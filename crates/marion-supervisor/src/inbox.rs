//! **Turn delivery, supervisor side: one inbox per node** — the queue a message waits in until the
//! node's next turn boundary, whoever it is from.
//!
//! Push (a child's end, announced to its parent) and steer (a parent's or the operator's message
//! into a child) are one mechanism: a message for a node's next turn. The row says *how* a turn is
//! taken ([`marion_harness::spec::TurnDelivery`]); this module is the part that is the same on
//! every row — the queue, who a message is from, what it reads as when it arrives, and the journal
//! records that audit it.
//!
//! # Why marion queues rather than writes
//!
//! S31 (`tests/fixtures/s31-turn-delivery/`) measured every typed surface misbehaving under a write
//! made while a turn is in flight — folded into the running turn with one completion for two
//! messages, or a first prompt that never answers. So every message waits here, and a
//! [`DeliveryPort`] — the node's driver, whichever lane it is — takes it at a turn boundary.
//!
//! # The late-steer race, and why [`Inboxes::take_or_seal`] is one call
//!
//! A driver that finds the queue empty at its node's last turn boundary ends the node. A steer that
//! lands between "the queue is empty" and "the node is gone" would be accepted and then never
//! delivered — a message marion said it would hand over and silently did not. `take_or_seal` makes
//! the check and the seal one step under the inbox's lock, so a concurrent [`Inboxes::enqueue`]
//! either lands before it (and is taken) or after it (and is refused, "node ended"). Never both
//! accepted and lost.
//!
//! # A node owed a child's end is held open
//!
//! A node that backgrounded a child is owed that child's end as a message (§7.6's gate, applied
//! at a turn boundary rather than at process exit). [`Inboxes::owe`] records the debt when the
//! child starts; [`Inboxes::announce`] queues the end and settles the debt in one step. While a
//! debt is open `take_or_seal` does not seal, and [`TurnSource::held`] tells the driver to wait
//! on its port instead of ending the node — so "the child ended" can never land between a
//! driver's "nothing queued" and its seal.
//!
//! # What the journal holds
//!
//! `MessageQueued` / `MessageDelivered` / `MessageDropped`, keyed by the message id — **the
//! message's length and SHA-256, never its text**. They are not barriers (`journal.rs` says why).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::SystemTime;

use marion_core::contract::{AgentId, TaskId};
use marion_core::journal::{
    MessageDelivered, MessageDropped, MessageQueued, MessageSource, RecordKind,
};
use marion_harness::spec::{MidTurn, TurnDelivery};

/// Marion's name for one message, shared by the records that audit it.
pub type MessageId = String;

/// **The node's driver, as the inbox sees it**: something that takes the next message at a turn
/// boundary. The inbox only ever tells it there is one. Each lane (duplex, ACP, continuation, pty,
/// bridge) implements it in its own phase; none is wired yet.
pub trait DeliveryPort: Send + Sync {
    /// A message is waiting. Called after it is queued, outside the inbox's lock.
    fn wake(&self);

    /// This port will never be woken again: its node's inbox closed, or a reopen of the same id
    /// replaced it. A driver that holds a thread for the port lets it go here. Called once,
    /// outside the inbox's lock; a no-op for a driver with nothing to release.
    fn closed(&self) {}
}

/// **One node's inbox, as that node's driver holds it** — the same queue as [`Inboxes`], bound
/// to the node, so a lane's driver never names another node's id.
///
/// `take_or_seal` is the only way a driver ends its node, for the late-steer race's reason, and
/// [`Self::held`] is how it tells "nothing now" from "nothing ever": a node owed a background
/// child's end (§7.6) is kept open, and its driver waits on its [`DeliveryPort`] instead of ending.
pub trait TurnSource: Send + Sync {
    /// The oldest waiting message, leaving the inbox open. For a delivery made mid-turn, where the
    /// node is not ending.
    fn take_next(&self) -> Option<Message>;
    /// The oldest waiting message, or — when there is none and nothing is owed — seal the inbox
    /// in the same step. See [`Inboxes::take_or_seal`].
    fn take_or_seal(&self) -> Option<Message>;
    /// A taken message reached the node, by `via`.
    fn delivered(&self, id: &str, via: &str);
    /// A taken message will never reach the node.
    fn dropped(&self, id: &str, reason: &str);
    /// Wake `port` for every message queued from now on (and once now if one waits).
    fn attach_port(&self, port: Arc<dyn DeliveryPort>);
    /// The inbox is still open after a `take_or_seal` found it empty: something is owed to it, so
    /// the driver must wait for its port rather than end the node. `false` for a source that
    /// never holds.
    fn held(&self) -> bool {
        false
    }
}

/// [`TurnSource`] over the supervisor's [`Inboxes`], for one node.
pub struct BoundInbox {
    inboxes: Arc<Inboxes>,
    agent: AgentId,
}

impl BoundInbox {
    pub fn new(inboxes: Arc<Inboxes>, agent: AgentId) -> Self {
        BoundInbox { inboxes, agent }
    }
}

impl TurnSource for BoundInbox {
    fn take_next(&self) -> Option<Message> {
        self.inboxes.take_next(&self.agent)
    }
    fn take_or_seal(&self) -> Option<Message> {
        self.inboxes.take_or_seal(&self.agent)
    }
    fn delivered(&self, id: &str, via: &str) {
        self.inboxes.delivered(&self.agent, id, via);
    }
    fn dropped(&self, id: &str, reason: &str) {
        self.inboxes.dropped(&self.agent, id, reason);
    }
    fn attach_port(&self, port: Arc<dyn DeliveryPort>) {
        self.inboxes.attach_port(&self.agent, port);
    }
    fn held(&self) -> bool {
        self.inboxes.held(&self.agent)
    }
}

/// **Everything a typed-turn driver needs to take more than the turn it was launched with**: the
/// node's inbox, and what its row does with a message that arrives while a turn is running
/// ([`MidTurn`], measured per harness and per ACP agent in S31).
#[derive(Clone)]
pub struct TurnFeed {
    pub source: Arc<dyn TurnSource>,
    pub mid_turn: MidTurn,
}

impl TurnFeed {
    /// The feed for a node whose row delivers by `delivery`. A strategy other than a typed turn
    /// cannot reach a typed driver's node mid-turn, so it is held for the boundary.
    pub fn new(source: Arc<dyn TurnSource>, delivery: TurnDelivery) -> Self {
        let mid_turn = match delivery {
            TurnDelivery::TypedTurn { mid_turn, .. } => mid_turn,
            _ => MidTurn::Queue,
        };
        TurnFeed { source, mid_turn }
    }

    /// A message that arrives mid-turn is written at once rather than held for the boundary.
    pub fn folds(&self) -> bool {
        self.mid_turn == MidTurn::Fold
    }
}

impl std::fmt::Debug for TurnFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnFeed")
            .field("mid_turn", &self.mid_turn)
            .finish_non_exhaustive()
    }
}

/// A [`DeliveryPort`] that **latches**: a wake with nobody waiting is kept until it is taken, so a
/// driver that checks between its own steps never misses one that landed in between.
#[derive(Default)]
pub struct Latch {
    set: Mutex<bool>,
    cv: Condvar,
}

impl Latch {
    /// Whether a wake landed since the last take, clearing it.
    pub fn take(&self) -> bool {
        std::mem::take(&mut *self.set.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Wait for a wake until `deadline` (forever with `None`), clearing it. `false` on timeout.
    pub fn wait_until(&self, deadline: Option<std::time::Instant>) -> bool {
        let mut set = self.set.lock().unwrap_or_else(|e| e.into_inner());
        while !*set {
            match deadline {
                None => set = self.cv.wait(set).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let left = d.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    set = self
                        .cv
                        .wait_timeout(set, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
        *set = false;
        true
    }
}

impl DeliveryPort for Latch {
    fn wake(&self) {
        *self.set.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }
}

/// Who a message is from, with what its rendering needs. Richer than the journal's
/// [`MessageSource`] (which it maps onto) by exactly the words the message is prefaced with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A client speaking for the operator — the TUI, the CLI, a top-level `marion mcp`.
    Operator,
    /// A node above the recipient, proved by its token.
    Ancestor {
        agent_id: AgentId,
        agent_type: String,
    },
    /// Marion itself: one of the recipient's backgrounded children (or a root it started) ended.
    ChildEnded {
        child: AgentId,
        task_id: TaskId,
        /// The terminal as the child's contract spells it (`completed`, `failed`, …).
        status: String,
        agent_type: String,
        /// Whether the ended node is a root, which has no contract (§9) and is named as one.
        root: bool,
    },
}

impl Source {
    /// The journal's spelling of who this is from.
    pub fn journal(&self) -> MessageSource {
        match self {
            Source::Operator => MessageSource::Operator,
            Source::Ancestor { agent_id, .. } => MessageSource::Ancestor(agent_id.clone()),
            Source::ChildEnded {
                child,
                task_id,
                status,
                ..
            } => MessageSource::ChildEnded {
                child: child.clone(),
                task_id: task_id.clone(),
                status: status.clone(),
            },
        }
    }
}

/// One queued message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub source: Source,
    pub text: String,
    pub queued_at: SystemTime,
}

/// Why a message was not accepted. Each has one sentence, [`Refusal::sentence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The node's inbox is sealed: its last turn has been taken and it is ending or ended.
    Ended,
    /// The row has no measured way to deliver a turn to a node of this shape.
    Unsupported { note: &'static str },
    /// The node has no inbox yet — it is still spawning.
    NotReady,
    /// `MessageQueued` could not be journaled, so the message was not accepted.
    Journal(String),
}

impl Refusal {
    pub fn sentence(&self) -> String {
        match self {
            Refusal::Ended => "the node has ended, so no later turn will take this message; \
                               `node/resume` relaunches it"
                .to_string(),
            Refusal::Unsupported { note } => format!(
                "this node's harness has no measured way to take a message into its next turn \
                 in this shape, so marion refuses rather than accept one it cannot hand over: \
                 {note}"
            ),
            Refusal::NotReady => "the node is still spawning and has no inbox yet; retry once \
                                  it is running"
                .to_string(),
            Refusal::Journal(e) => {
                format!("marion could not journal the message, so it did not accept it: {e}")
            }
        }
    }
}

/// **What a message reads as when it reaches the node's model** — one renderer for every source
/// and every lane, so a steer arrives worded the same way over a typed turn, a resume or a paste.
pub fn render(msg: &Message) -> String {
    match &msg.source {
        Source::Operator => format!("marion: message from the operator: {}", msg.text),
        Source::Ancestor {
            agent_id,
            agent_type,
        } => format!(
            "marion: message from your parent ({agent_type} {}): {}",
            crate::tree::short_id(&agent_id.0),
            msg.text
        ),
        Source::ChildEnded {
            task_id,
            agent_type,
            root,
            ..
        } => child_ended_text(agent_type, *root, &task_id.0, &msg.text),
    }
}

/// **The announcement that a backgrounded node ended** — what the per-child bridge pushes over
/// MCP today (`mcp.rs`'s `watch`) and what [`render`] makes of a [`Source::ChildEnded`], one text
/// for both. `body` is what the node's `wait` returns.
///
/// The push **announces and does not deliver**: the handle stays uncollected, so the parent's
/// `wait` returns this same document rather than "you already have this".
pub fn child_ended_text(agent_type: &str, root: bool, task_id: &str, body: &str) -> String {
    let node = if root { "root" } else { "child" };
    format!(
        "The {agent_type} {node} you backgrounded as task_id {task_id:?} has ended. This is what \
         its `wait` returns; a `wait` on that task_id still returns it.\n\n{body}"
    )
}

/// Where the inbox's records go. Injected so the unit tests read them back without a journal.
pub type Sink = Box<dyn Fn(RecordKind) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
struct Inbox {
    queue: VecDeque<Message>,
    port: Option<Arc<dyn DeliveryPort>>,
    sealed: bool,
    /// Announcements this node is owed: one per backgrounded child still running (§7.6). The
    /// inbox cannot seal while any is owed, which is what makes the hold race-free — "the child
    /// ended" and "its announcement is queued" are one step ([`Inboxes::announce`]).
    owed: usize,
}

/// Every node's inbox, keyed by the node. Held by the supervisor beside its node table.
pub struct Inboxes {
    boxes: Mutex<HashMap<AgentId, Inbox>>,
    sink: Sink,
}

impl Inboxes {
    pub fn new(sink: Sink) -> Self {
        Inboxes {
            boxes: Mutex::new(HashMap::new()),
            sink,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Inbox>> {
        self.boxes.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The node exists and can be queued for. A node opened again — a resumed process under the
    /// same id — gets a fresh, unsealed inbox: its old lifetime's queue was already resolved when
    /// that lifetime was closed.
    pub fn open(&self, agent: &AgentId) {
        let replaced = self.lock().insert(agent.clone(), Inbox::default());
        if let Some(port) = replaced.and_then(|old| old.port) {
            port.closed();
        }
    }

    /// The node's driver is up: it will be woken for every message queued from now on, and once
    /// now if any are already waiting. `false`, and the port left untouched, for a node with no
    /// open inbox — a driver that started a thread for it releases the thread itself.
    pub fn attach_port(&self, agent: &AgentId, port: Arc<dyn DeliveryPort>) -> bool {
        let (waiting, replaced) = {
            let mut boxes = self.lock();
            let Some(inbox) = boxes.get_mut(agent).filter(|b| !b.sealed) else {
                return false;
            };
            let replaced = inbox.port.replace(Arc::clone(&port));
            (!inbox.queue.is_empty(), replaced)
        };
        if let Some(old) = replaced {
            old.closed();
        }
        if waiting {
            port.wake();
        }
        true
    }

    /// Accept `text` for `agent`'s next turn, or say why not. On success `MessageQueued` is
    /// journaled (length and digest only) before the message is visible to a taker.
    ///
    /// The refusals are checked in the order that makes each answer the most useful one: an
    /// ended node says so first; a row with no strategy says so whether or not the node is up yet,
    /// since waiting would not change it; only then is a node with no inbox "still spawning".
    pub fn enqueue(
        &self,
        agent: &AgentId,
        delivery: TurnDelivery,
        source: Source,
        text: String,
    ) -> Result<MessageId, Refusal> {
        self.accept(agent, delivery, source, text, false)
    }

    /// Record that `agent` is owed one announcement — a backgrounded child of it has started —
    /// so its inbox stays open until [`Self::announce`] or [`Self::release`] settles it. `false`
    /// (and nothing recorded) for a node with no inbox or a sealed one: it has taken its last
    /// turn, so there is nothing to hold open.
    pub fn owe(&self, agent: &AgentId) -> bool {
        match self.lock().get_mut(agent) {
            Some(inbox) if !inbox.sealed => {
                inbox.owed += 1;
                true
            }
            _ => false,
        }
    }

    /// [`Self::enqueue`] for an owed announcement: the message is queued and the debt settled
    /// **in one step**, so a driver can never see the debt gone and the message not yet there.
    /// A refusal settles the debt too — the announcement was attempted and will not come again.
    pub fn announce(
        &self,
        agent: &AgentId,
        delivery: TurnDelivery,
        source: Source,
        text: String,
    ) -> Result<MessageId, Refusal> {
        let accepted = self.accept(agent, delivery, source, text, true);
        if accepted.is_err() {
            self.release(agent);
        }
        accepted
    }

    /// Settle one debt with no message: the announcement goes another way (the parent's own lane
    /// pushes it) or nowhere. Wakes the driver, which may be holding for exactly this.
    pub fn release(&self, agent: &AgentId) {
        let port = {
            let mut boxes = self.lock();
            let Some(inbox) = boxes.get_mut(agent) else {
                return;
            };
            inbox.owed = inbox.owed.saturating_sub(1);
            inbox.port.clone()
        };
        if let Some(port) = port {
            port.wake();
        }
    }

    /// The inbox exists and is not sealed — after a `take_or_seal` found it empty, that means an
    /// announcement is owed and the driver must wait. See [`TurnSource::held`].
    pub fn held(&self, agent: &AgentId) -> bool {
        self.lock().get(agent).is_some_and(|b| !b.sealed)
    }

    fn accept(
        &self,
        agent: &AgentId,
        delivery: TurnDelivery,
        source: Source,
        text: String,
        settles_debt: bool,
    ) -> Result<MessageId, Refusal> {
        let port = {
            let mut boxes = self.lock();
            if boxes.get(agent).is_some_and(|b| b.sealed) {
                return Err(Refusal::Ended);
            }
            if let TurnDelivery::None { note } = delivery {
                return Err(Refusal::Unsupported { note });
            }
            let Some(inbox) = boxes.get_mut(agent) else {
                return Err(Refusal::NotReady);
            };
            let id = mint_message_id();
            // Under the lock, so the intent record is on the journal before any taker can see the
            // message and write its resolution.
            (self.sink)(RecordKind::MessageQueued(MessageQueued {
                agent_id: agent.clone(),
                message_id: id.clone(),
                source: source.journal(),
                // Bounded far below `u32::MAX` at the protocol boundary (`MAX_STEER_BYTES`);
                // saturating rather than wrapping for a caller that bypassed it.
                len: u32::try_from(text.len()).unwrap_or(u32::MAX),
                sha256: sha256_hex(text.as_bytes()),
            }))
            .map_err(Refusal::Journal)?;
            inbox.queue.push_back(Message {
                id: id.clone(),
                source,
                text,
                queued_at: SystemTime::now(),
            });
            if settles_debt {
                inbox.owed = inbox.owed.saturating_sub(1);
            }
            (id, inbox.port.clone())
        };
        let (id, port) = port;
        if let Some(port) = port {
            port.wake();
        }
        Ok(id)
    }

    /// The oldest waiting message, if any. Leaves the inbox open.
    pub fn take_next(&self, agent: &AgentId) -> Option<Message> {
        self.lock().get_mut(agent)?.queue.pop_front()
    }

    /// The oldest waiting message — or, when there is none, **seal the inbox in the same step**,
    /// so no message can be accepted after the driver decided the node has taken its last turn.
    ///
    /// **Except while an announcement is owed**: then the inbox is left open and `None` means
    /// "not yet" ([`Self::held`] says which). Sealing over a debt would refuse the child's end a
    /// moment after marion promised to announce it.
    pub fn take_or_seal(&self, agent: &AgentId) -> Option<Message> {
        let mut boxes = self.lock();
        let inbox = boxes.get_mut(agent)?;
        let next = inbox.queue.pop_front();
        if next.is_none() && inbox.owed == 0 {
            inbox.sealed = true;
        }
        next
    }

    /// A taken message reached the node, by `via` (the lane's verb). The record is an audit, so a
    /// failure to write it is reported and does not undo a delivery that happened.
    pub fn delivered(&self, agent: &AgentId, id: &str, via: &str) {
        self.audit(RecordKind::MessageDelivered(MessageDelivered {
            agent_id: agent.clone(),
            message_id: id.to_string(),
            via: via.to_string(),
        }));
    }

    /// A taken message will never reach the node.
    pub fn dropped(&self, agent: &AgentId, id: &str, reason: &str) {
        self.audit(RecordKind::MessageDropped(MessageDropped {
            agent_id: agent.clone(),
            message_id: id.to_string(),
            reason: reason.to_string(),
        }));
    }

    /// The node's process ended: seal its inbox and drop every message still waiting, each with
    /// `reason`. A no-op for a node with no inbox.
    pub fn close(&self, agent: &AgentId, reason: &str) {
        let (port, stranded): (Option<Arc<dyn DeliveryPort>>, Vec<Message>) = {
            let mut boxes = self.lock();
            let Some(inbox) = boxes.get_mut(agent) else {
                return;
            };
            inbox.sealed = true;
            inbox.owed = 0;
            (inbox.port.take(), inbox.queue.drain(..).collect())
        };
        for m in stranded {
            self.dropped(agent, &m.id, reason);
        }
        if let Some(port) = port {
            port.closed();
        }
    }

    /// How many messages wait for `agent`.
    pub fn queued(&self, agent: &AgentId) -> usize {
        self.lock().get(agent).map_or(0, |b| b.queue.len())
    }

    fn audit(&self, kind: RecordKind) {
        if let Err(e) = (self.sink)(kind) {
            eprintln!("marion: a turn-delivery audit record was not journaled: {e}");
        }
    }
}

/// A message id: `m-` and 16 hex digits of entropy, unique for the life of any journal. Falls back
/// to the clock and a process counter if the entropy source is unreadable — an id need only be
/// unique, never secret.
fn mint_message_id() -> MessageId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        bytes = (nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).rotate_left(32)).to_be_bytes();
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("m-{hex}")
}

/// The lowercase hex SHA-256 of `bytes` — what `MessageQueued` records in place of the text.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    const TYPED: TurnDelivery = TurnDelivery::TypedTurn {
        mid_turn: marion_harness::spec::MidTurn::Queue,
        note: "t",
    };

    /// An inbox set whose records land in a vector the test reads back.
    pub(crate) fn recording() -> (Inboxes, Arc<Mutex<Vec<RecordKind>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink_log = Arc::clone(&log);
        let inboxes = Inboxes::new(Box::new(move |k| {
            sink_log.lock().unwrap().push(k);
            Ok(())
        }));
        (inboxes, log)
    }

    pub(crate) fn records(log: &Arc<Mutex<Vec<RecordKind>>>) -> Vec<RecordKind> {
        log.lock().unwrap().clone()
    }

    #[test]
    fn the_digest_is_standard_sha256() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// Messages come out in the order they went in, each once.
    #[test]
    fn messages_are_taken_first_in_first_out() {
        let (inboxes, _) = recording();
        let a = id("a");
        inboxes.open(&a);
        let ids: Vec<_> = ["one", "two", "three"]
            .into_iter()
            .map(|t| {
                inboxes
                    .enqueue(&a, TYPED, Source::Operator, t.into())
                    .unwrap()
            })
            .collect();
        assert_eq!(inboxes.queued(&a), 3);
        let taken: Vec<_> = std::iter::from_fn(|| inboxes.take_next(&a))
            .map(|m| (m.id, m.text))
            .collect();
        assert_eq!(
            taken,
            vec![
                (ids[0].clone(), "one".to_string()),
                (ids[1].clone(), "two".to_string()),
                (ids[2].clone(), "three".to_string()),
            ]
        );
        assert_ne!(ids[0], ids[1], "every message has its own id");
        assert_eq!(inboxes.queued(&a), 0);
    }

    /// A sealed inbox refuses with the one sentence that points at `node/resume`, and journals
    /// nothing for the refused message.
    #[test]
    fn a_sealed_inbox_refuses_and_names_resume() {
        let (inboxes, log) = recording();
        let a = id("a");
        inboxes.open(&a);
        assert_eq!(inboxes.take_or_seal(&a), None, "empty, so it seals");
        let refused = inboxes.enqueue(&a, TYPED, Source::Operator, "late".into());
        assert_eq!(refused, Err(Refusal::Ended));
        assert!(Refusal::Ended.sentence().contains("node/resume"));
        assert!(
            records(&log).is_empty(),
            "a refusal is not a queued message"
        );
    }

    /// A row with no measured strategy is refused by name, quoting the row's note — even before
    /// the node's inbox exists, because no later moment would change the answer.
    #[test]
    fn a_row_without_a_strategy_is_refused_quoting_its_note() {
        let (inboxes, log) = recording();
        let a = id("a");
        let none = TurnDelivery::None {
            note: "S31: the row's note",
        };
        for opened in [false, true] {
            if opened {
                inboxes.open(&a);
            }
            let r = inboxes.enqueue(&a, none, Source::Operator, "x".into());
            assert_eq!(
                r,
                Err(Refusal::Unsupported {
                    note: "S31: the row's note"
                })
            );
        }
        assert!(
            Refusal::Unsupported {
                note: "S31: the row's note"
            }
            .sentence()
            .contains("S31: the row's note")
        );
        assert!(records(&log).is_empty());
    }

    /// A node with no inbox yet is still spawning: refused with "retry", not "ended".
    #[test]
    fn a_node_with_no_inbox_yet_is_refused_as_not_ready() {
        let (inboxes, _) = recording();
        let r = inboxes.enqueue(&id("spawning"), TYPED, Source::Operator, "x".into());
        assert_eq!(r, Err(Refusal::NotReady));
        assert!(Refusal::NotReady.sentence().contains("retry"));
    }

    /// **The journal carries the length and digest, never the text** — for the queue, and for its
    /// two resolutions under the same id.
    #[test]
    fn queued_delivered_and_dropped_are_journaled_without_the_text() {
        let (inboxes, log) = recording();
        let a = id("a");
        inboxes.open(&a);
        let text = "a secret instruction";
        let m1 = inboxes
            .enqueue(
                &a,
                TYPED,
                Source::Ancestor {
                    agent_id: id("p"),
                    agent_type: "claude".into(),
                },
                text.into(),
            )
            .unwrap();
        let m2 = inboxes
            .enqueue(&a, TYPED, Source::Operator, "second".into())
            .unwrap();
        let first = inboxes.take_next(&a).unwrap();
        inboxes.delivered(&a, &first.id, "typed-turn");
        inboxes.close(&a, "the node ended before delivery");
        let got = records(&log);
        assert_eq!(
            got,
            vec![
                RecordKind::MessageQueued(MessageQueued {
                    agent_id: a.clone(),
                    message_id: m1.clone(),
                    source: MessageSource::Ancestor(id("p")),
                    len: text.len() as u32,
                    sha256: sha256_hex(text.as_bytes()),
                }),
                RecordKind::MessageQueued(MessageQueued {
                    agent_id: a.clone(),
                    message_id: m2.clone(),
                    source: MessageSource::Operator,
                    len: 6,
                    sha256: sha256_hex(b"second"),
                }),
                RecordKind::MessageDelivered(MessageDelivered {
                    agent_id: a.clone(),
                    message_id: m1,
                    via: "typed-turn".into(),
                }),
                RecordKind::MessageDropped(MessageDropped {
                    agent_id: a.clone(),
                    message_id: m2,
                    reason: "the node ended before delivery".into(),
                }),
            ]
        );
        for r in &got {
            let line = serde_json::to_string(r).unwrap();
            assert!(!line.contains(text) && !line.contains("second\""), "{line}");
        }
        assert_eq!(
            inboxes.enqueue(&a, TYPED, Source::Operator, "x".into()),
            Err(Refusal::Ended),
            "a closed inbox is sealed"
        );
    }

    /// A message that could not be journaled was not accepted: nothing waits for a taker.
    #[test]
    fn a_message_the_journal_refused_is_not_queued() {
        let inboxes = Inboxes::new(Box::new(|_| Err("disk full".into())));
        let a = id("a");
        inboxes.open(&a);
        let r = inboxes.enqueue(&a, TYPED, Source::Operator, "x".into());
        assert_eq!(r, Err(Refusal::Journal("disk full".into())));
        assert_eq!(inboxes.queued(&a), 0);
    }

    struct Counting(AtomicUsize);
    impl DeliveryPort for Counting {
        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The port is woken once per queued message, and once on attach when messages already wait.
    #[test]
    fn the_port_is_woken_for_each_message_and_on_attach_when_one_waits() {
        let (inboxes, _) = recording();
        let a = id("a");
        inboxes.open(&a);
        inboxes
            .enqueue(&a, TYPED, Source::Operator, "early".into())
            .unwrap();
        let port = Arc::new(Counting(AtomicUsize::new(0)));
        assert!(inboxes.attach_port(&a, port.clone()));
        assert_eq!(port.0.load(Ordering::SeqCst), 1, "a message was waiting");
        inboxes
            .enqueue(&a, TYPED, Source::Operator, "late".into())
            .unwrap();
        assert_eq!(port.0.load(Ordering::SeqCst), 2);
    }

    struct Closing(AtomicUsize);
    impl DeliveryPort for Closing {
        fn wake(&self) {}
        fn closed(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// **A port is told when it will never be woken again** — its node closed, or a reopen of the
    /// same id replaced its inbox — so a driver holding a thread for it can let it go. A port for
    /// a node with no inbox is refused, and told nothing: it was never attached.
    #[test]
    fn a_port_is_told_when_its_inbox_closes_or_is_replaced() {
        let (inboxes, _) = recording();
        let a = id("a");
        let orphan = Arc::new(Closing(AtomicUsize::new(0)));
        assert!(
            !inboxes.attach_port(&a, orphan.clone()),
            "no inbox, no attach"
        );
        inboxes.open(&a);
        let first = Arc::new(Closing(AtomicUsize::new(0)));
        assert!(inboxes.attach_port(&a, first.clone()));
        inboxes.open(&a);
        assert_eq!(first.0.load(Ordering::SeqCst), 1, "a reopen replaced it");
        let second = Arc::new(Closing(AtomicUsize::new(0)));
        assert!(inboxes.attach_port(&a, second.clone()));
        inboxes.close(&a, "ended");
        inboxes.close(&a, "ended again");
        assert_eq!(second.0.load(Ordering::SeqCst), 1, "closed once");
        assert_eq!(orphan.0.load(Ordering::SeqCst), 0);
    }

    /// A reopened node (a resume under the same id) starts unsealed and empty.
    #[test]
    fn reopening_a_closed_inbox_starts_a_fresh_lifetime() {
        let (inboxes, _) = recording();
        let a = id("a");
        inboxes.open(&a);
        inboxes.close(&a, "ended");
        inboxes.open(&a);
        assert!(
            inboxes
                .enqueue(&a, TYPED, Source::Operator, "x".into())
                .is_ok()
        );
    }

    /// **The late-steer race, closed.** An `enqueue` racing a `take_or_seal` on an empty inbox,
    /// released together by a barrier, many times: every round ends in exactly one of the two
    /// honest outcomes — the message was accepted *and* taken, or it was refused because the
    /// inbox sealed first. Accepted-and-lost is the outcome this function exists to make
    /// impossible, and it is what a check-then-seal in two lock acquisitions would produce.
    #[test]
    fn take_or_seal_and_a_concurrent_enqueue_never_lose_a_message() {
        let mut outcomes = [0usize; 2];
        for round in 0..400 {
            let (inboxes, _) = recording();
            let inboxes = Arc::new(inboxes);
            let a = id("a");
            inboxes.open(&a);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (i2, b2, a2) = (Arc::clone(&inboxes), Arc::clone(&barrier), a.clone());
            let sender = std::thread::spawn(move || {
                b2.wait();
                i2.enqueue(&a2, TYPED, Source::Operator, format!("m{round}"))
            });
            barrier.wait();
            let taken = inboxes.take_or_seal(&a);
            let sent = sender.join().unwrap();
            match (&sent, &taken) {
                (Ok(id), Some(m)) => {
                    assert_eq!(&m.id, id);
                    outcomes[0] += 1;
                }
                (Err(Refusal::Ended), None) => outcomes[1] += 1,
                other => {
                    panic!("round {round}: accepted and lost, or taken from nowhere: {other:?}")
                }
            }
        }
        assert_eq!(outcomes.iter().sum::<usize>(), 400);
    }

    fn ended(child: &str) -> Source {
        Source::ChildEnded {
            child: id(child),
            task_id: TaskId(format!("t-{child}")),
            status: "completed".into(),
            agent_type: "codex".into(),
            root: false,
        }
    }

    /// **A node owed a background child's end is held, not sealed**: `take_or_seal` finds nothing
    /// and leaves the inbox open while the announcement is owed, the announcement is then taken,
    /// and only after it does the empty inbox seal.
    #[test]
    fn an_inbox_owed_an_announcement_is_held_open_until_it_arrives() {
        let (inboxes, _) = recording();
        let (p, c) = (id("parent"), id("child"));
        inboxes.open(&p);
        assert!(inboxes.owe(&p), "an open inbox records what it is owed");
        assert_eq!(inboxes.take_or_seal(&p), None);
        assert!(inboxes.held(&p), "owed, so still open");
        let m = inboxes
            .announce(&p, TYPED, ended("child"), "the contract".into())
            .expect("an owed announcement is accepted");
        let taken = inboxes.take_or_seal(&p).expect("the announcement is taken");
        assert_eq!(taken.id, m);
        assert_eq!(taken.source, ended(c.0.as_str()));
        assert_eq!(inboxes.take_or_seal(&p), None);
        assert!(!inboxes.held(&p), "nothing owed and nothing queued: sealed");
        assert_eq!(
            inboxes.enqueue(&p, TYPED, Source::Operator, "late".into()),
            Err(Refusal::Ended)
        );
    }

    /// An announcement that will not be delivered by the inbox — the parent's lane pushes it
    /// itself, or has no way to take it — is released, which lets the inbox seal and wakes the
    /// driver so it notices.
    #[test]
    fn a_released_debt_lets_the_inbox_seal_and_wakes_the_driver() {
        let (inboxes, log) = recording();
        let p = id("parent");
        inboxes.open(&p);
        let port = Arc::new(Counting(AtomicUsize::new(0)));
        inboxes.attach_port(&p, port.clone());
        inboxes.owe(&p);
        assert_eq!(inboxes.take_or_seal(&p), None);
        assert!(inboxes.held(&p));
        inboxes.release(&p);
        assert_eq!(port.0.load(Ordering::SeqCst), 1, "the held driver is told");
        assert_eq!(inboxes.take_or_seal(&p), None);
        assert!(!inboxes.held(&p));
        assert!(records(&log).is_empty(), "a release is no message");
    }

    /// A sealed or unknown inbox records no debt, so it cannot be held open by one.
    #[test]
    fn a_sealed_or_unknown_inbox_is_owed_nothing() {
        let (inboxes, _) = recording();
        let p = id("parent");
        assert!(!inboxes.owe(&p), "no inbox");
        inboxes.open(&p);
        assert_eq!(inboxes.take_or_seal(&p), None);
        assert!(!inboxes.owe(&p), "sealed");
        assert_eq!(
            inboxes.announce(&p, TYPED, ended("c"), "x".into()),
            Err(Refusal::Ended)
        );
    }

    /// **The late-announcement race, closed** — the held driver's loop (`take_or_seal`, then
    /// `held`) against a concurrent `announce`, released together many times. Every round ends
    /// with the announcement taken: an owed inbox cannot seal, so "held says no" can only follow
    /// the announcement having been queued, and the next `take_or_seal` takes it.
    #[test]
    fn a_held_driver_never_loses_an_announcement_racing_its_boundary() {
        for round in 0..400 {
            let (inboxes, _) = recording();
            let inboxes = Arc::new(inboxes);
            let p = id("p");
            inboxes.open(&p);
            inboxes.owe(&p);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (i2, b2, p2) = (Arc::clone(&inboxes), Arc::clone(&barrier), p.clone());
            let child = std::thread::spawn(move || {
                b2.wait();
                i2.announce(&p2, TYPED, ended("c"), format!("m{round}"))
            });
            barrier.wait();
            let mut taken = None;
            loop {
                match inboxes.take_or_seal(&p) {
                    Some(m) => taken = Some(m),
                    None if inboxes.held(&p) => std::thread::yield_now(),
                    None => break,
                }
            }
            let sent = child.join().unwrap().expect("owed, so accepted");
            assert_eq!(taken.map(|m| m.id), Some(sent), "round {round}");
        }
    }

    /// A node's inbox, as its driver holds it: the same queue, bound to the one node.
    #[test]
    fn a_bound_inbox_is_the_nodes_own_queue() {
        let (inboxes, log) = recording();
        let inboxes = Arc::new(inboxes);
        let a = id("a");
        inboxes.open(&a);
        let turns: Arc<dyn TurnSource> = Arc::new(BoundInbox::new(Arc::clone(&inboxes), a.clone()));
        let port = Arc::new(Counting(AtomicUsize::new(0)));
        turns.attach_port(port.clone());
        let m = inboxes
            .enqueue(&a, TYPED, Source::Operator, "one".into())
            .unwrap();
        assert_eq!(port.0.load(Ordering::SeqCst), 1);
        let got = turns.take_next().unwrap();
        assert_eq!(got.id, m);
        turns.delivered(&got.id, "stream-json:mid-turn");
        assert!(matches!(
            records(&log).last(),
            Some(RecordKind::MessageDelivered(d)) if d.via == "stream-json:mid-turn" && d.message_id == m
        ));
        assert!(turns.held(), "open and unsealed");
        assert_eq!(turns.take_or_seal(), None);
        assert!(!turns.held());
    }

    #[test]
    fn a_message_renders_by_who_it_is_from() {
        let msg = |source| Message {
            id: "m".into(),
            source,
            text: "use the v2 API".into(),
            queued_at: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(
            render(&msg(Source::Operator)),
            "marion: message from the operator: use the v2 API"
        );
        assert_eq!(
            render(&msg(Source::Ancestor {
                agent_id: id("019f0000-5b04-7000-8000-000000000000"),
                agent_type: "claude".into(),
            })),
            "marion: message from your parent (claude 5b04): use the v2 API"
        );
        let ended = render(&msg(Source::ChildEnded {
            child: id("c"),
            task_id: TaskId("t-1".into()),
            status: "completed".into(),
            agent_type: "codex-impl".into(),
            root: false,
        }));
        assert_eq!(
            ended,
            child_ended_text("codex-impl", false, "t-1", "use the v2 API")
        );
        assert!(
            ended
                .starts_with("The codex-impl child you backgrounded as task_id \"t-1\" has ended.")
        );
        assert!(ended.ends_with("\n\nuse the v2 API"));
    }
}
