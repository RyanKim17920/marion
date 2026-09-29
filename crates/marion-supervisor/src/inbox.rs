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

/// The `via` of a child's end the node read for itself ([`Inboxes::received`]): no turn carried
/// it, and `note` says so.
const VIA_READ: &str = "wait";
const READ_NOTE: &str = "the node read this end itself (`wait` or `status`), so no turn carried it";

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
    /// Queue marion's one request that the node report ([`Inboxes::request_report`]). `false`
    /// where nothing was queued: asked already, sealed, or a source that never asks.
    fn request_report(&self) -> bool {
        false
    }
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
    fn request_report(&self) -> bool {
        self.inboxes.request_report(&self.agent)
    }
    fn attach_port(&self, port: Arc<dyn DeliveryPort>) {
        self.inboxes.attach_port(&self.agent, port);
    }
    fn held(&self) -> bool {
        self.inboxes.held(&self.agent)
    }
}

/// Whether a stretch of a node's stdout holds its `report` call, read the row's way (its adapter's
/// `parse_stream`). What a feed asks with before it lets a child end unreported.
pub type ReportReader = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// **Everything a typed-turn driver needs to take more than the turn it was launched with**: the
/// node's inbox, and what its row does with a message that arrives while a turn is running
/// ([`MidTurn`], measured per harness and per ACP agent in S31).
#[derive(Clone)]
pub struct TurnFeed {
    pub source: Arc<dyn TurnSource>,
    pub mid_turn: MidTurn,
    /// How to tell a turn that reported from one that did not — a child's feed only, since a root
    /// never reports. `None` asks for nothing ([`Self::ask_for_report`]).
    pub reports: Option<ReportReader>,
    /// How to tell a turn that ended on the harness's own failure claim (a usage limit, a refused
    /// key) — which marion does not answer with a request for a report, as the relaunch lane does
    /// not relaunch an involuntary stop. `None` reads no turn as failed.
    pub failures: Option<ReportReader>,
}

impl TurnFeed {
    /// The feed for a node whose row delivers by `delivery`. A strategy other than a typed turn
    /// cannot reach a typed driver's node mid-turn, so it is held for the boundary.
    pub fn new(source: Arc<dyn TurnSource>, delivery: TurnDelivery) -> Self {
        let mid_turn = match delivery {
            TurnDelivery::TypedTurn { mid_turn, .. } => mid_turn,
            _ => MidTurn::Queue,
        };
        TurnFeed {
            source,
            mid_turn,
            reports: None,
            failures: None,
        }
    }

    /// The same feed, able to tell a turn that failed ([`Self::failures`]).
    pub fn reading_failures(self, failures: ReportReader) -> Self {
        TurnFeed {
            failures: Some(failures),
            ..self
        }
    }

    /// The same feed, able to tell a reported turn from an unreported one.
    pub fn reading_reports(self, reports: ReportReader) -> Self {
        TurnFeed {
            reports: Some(reports),
            ..self
        }
    }

    /// **At the node's last boundary, one re-prompt for a report it never made** (§7.6's grace
    /// turn): `since` is what the node wrote since its last turn was delivered, and `time_left`
    /// whether its clock can carry one more. Queues marion's request when that stretch holds no
    /// report or failure claim and the node has not been asked before; the driver's `take_or_seal` then takes it
    /// as the next turn, the way it takes a steer. `true` iff a request was queued.
    pub fn ask_for_report(&self, since: &str, time_left: bool) -> bool {
        match &self.reports {
            Some(reported)
                if time_left
                    && !reported(since)
                    && !self.failures.as_ref().is_some_and(|failed| failed(since)) =>
            {
                self.source.request_report()
            }
            _ => false,
        }
    }

    /// **Whether `stretch` holds the node's report**, read the row's way — `false` for a feed that
    /// reads none. A driver that finds its inbox held only for an owed child's end asks this of
    /// the node's whole stream: a node that reported has concluded (§7.6's reported-early
    /// exemption), so it ends rather than wait, and the end goes to its nearest live ancestor.
    pub fn reported(&self, stretch: &str) -> bool {
        self.reports.as_ref().is_some_and(|r| r(stretch))
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
            .field("reads_reports", &self.reports.is_some())
            .finish_non_exhaustive()
    }
}

/// A [`DeliveryPort`] that **latches**: a wake with nobody waiting is kept until it is taken, so a
/// driver that checks between its own steps never misses one that landed in between.
///
/// A [`Self::ringing`] latch also rings a [`crate::wake::Pipe`] on every wake, for a driver that
/// waits in `poll(2)` on the pipe beside its other sources and then [`Self::take`]s. A close is a
/// wake too: a driver holding for an owed message must look again when the inbox seals.
#[derive(Default)]
pub struct Latch {
    set: Mutex<bool>,
    cv: Condvar,
    ring: Option<Arc<crate::wake::Pipe>>,
}

impl Latch {
    /// A latch that also rings `ring` on every wake.
    pub fn ringing(ring: Arc<crate::wake::Pipe>) -> Latch {
        Latch {
            ring: Some(ring),
            ..Latch::default()
        }
    }

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
        if let Some(ring) = &self.ring {
            ring.wake();
        }
    }

    fn closed(&self) {
        self.wake();
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
    /// Marion itself, asking a child whose turn ended without a `report` to make one.
    ReportRequested,
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
            Source::ReportRequested => MessageSource::ReportRequested,
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
        Source::ReportRequested => msg.text.clone(),
    }
}

/// **marion's one request for a missing report** ([`Inboxes::request_report`]): a best-effort
/// account of the work done, not a request to do more (§7.6's grace turn).
pub const REPORT_REQUEST: &str = "marion: your turn ended without a call to marion's `report` \
    tool, so whoever delegated this task has not heard what you did. Call `report` now, exactly \
    once, with a one-sentence `narrative` of all the work you did on this task (including any \
    since an earlier report) and the `result_commits` you made. Do not start new work.";

/// **The announcement that a backgrounded node ended** — what the per-child bridge pushes over
/// MCP today (`mcp.rs`'s `watch`) and what [`render`] makes of a [`Source::ChildEnded`], one text
/// for both. `body` is its few-line summary (`bridge::announcement_of`).
///
/// The push **announces and does not deliver**: the handle stays uncollected, so the parent's
/// `wait` returns the whole result rather than "you already have this".
pub fn child_ended_text(agent_type: &str, root: bool, task_id: &str, body: &str) -> String {
    let node = if root { "root" } else { "child" };
    format!(
        "The {agent_type} {node} you backgrounded as task_id {task_id:?} has ended; a `wait` on \
         that task_id returns its whole result.\n\n{body}"
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
    /// Children whose end this node read for itself (`wait`) before marion announced it: their
    /// announcement is resolved as it arrives rather than queued ([`Inboxes::received`]).
    received: std::collections::HashSet<AgentId>,
    /// marion has asked this node for its report ([`Inboxes::request_report`]), which it does
    /// once in the node's life — continuation generations share one inbox, so once per node.
    asked_for_report: bool,
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

    /// **`agent` read `child`'s end for itself** — its `wait` returned it, or its `status` said it
    /// finished — so announcing it too would spend a whole turn on news the node already has.
    /// A queued announcement is withdrawn; one not yet made is resolved the moment it is
    /// ([`Self::announce`]). Either way the journal records it delivered `via: "wait"`, and a
    /// driver held for it is woken. `true` iff a queued announcement was withdrawn.
    pub fn received(&self, agent: &AgentId, child: &AgentId) -> bool {
        let (withdrawn, port) = {
            let mut boxes = self.lock();
            let Some(inbox) = boxes.get_mut(agent).filter(|b| !b.sealed) else {
                return false;
            };
            let at = inbox.queue.iter().position(
                |m| matches!(&m.source, Source::ChildEnded { child: c, .. } if c == child),
            );
            match at.and_then(|i| inbox.queue.remove(i)) {
                Some(m) => (Some(m.id), inbox.port.clone()),
                None => {
                    inbox.received.insert(child.clone());
                    (None, None)
                }
            }
        };
        if let Some(id) = &withdrawn {
            self.delivered_noting(agent, id, VIA_READ, Some(READ_NOTE));
        }
        if let Some(port) = port {
            port.wake();
        }
        withdrawn.is_some()
    }

    /// **Queue marion's request that `agent` report** ([`REPORT_REQUEST`]), journaled like any
    /// message and taken at the node's boundary like one. **At most once per node**: `false`,
    /// and nothing queued, when it was asked before, or its inbox is sealed or absent — **or a
    /// backgrounded child's end is still owed to it**: that end is its next turn, and asking now
    /// would press it to report before its children are done (§7.6 holds first, then asks).
    pub fn request_report(&self, agent: &AgentId) -> bool {
        let port = {
            let mut boxes = self.lock();
            let Some(inbox) = boxes
                .get_mut(agent)
                .filter(|b| !b.sealed && !b.asked_for_report && b.owed == 0)
            else {
                return false;
            };
            let id = mint_message_id();
            let recorded = (self.sink)(RecordKind::MessageQueued(MessageQueued {
                agent_id: agent.clone(),
                message_id: id.clone(),
                source: MessageSource::ReportRequested,
                len: u32::try_from(REPORT_REQUEST.len()).unwrap_or(u32::MAX),
                sha256: sha256_hex(REPORT_REQUEST.as_bytes()),
            }));
            if let Err(e) = recorded {
                eprintln!("marion: `{}` was not asked for its report: {e}", agent.0);
                return false;
            }
            inbox.asked_for_report = true;
            inbox.queue.push_back(Message {
                id,
                source: Source::ReportRequested,
                text: REPORT_REQUEST.to_string(),
                queued_at: SystemTime::now(),
            });
            inbox.port.clone()
        };
        if let Some(port) = port {
            port.wake();
        }
        true
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
            // An end the node already read for itself is resolved here, never queued.
            let read = match &source {
                Source::ChildEnded { child, .. } if settles_debt => inbox.received.remove(child),
                _ => false,
            };
            if !read {
                inbox.queue.push_back(Message {
                    id: id.clone(),
                    source,
                    text,
                    queued_at: SystemTime::now(),
                });
            }
            if settles_debt {
                inbox.owed = inbox.owed.saturating_sub(1);
            }
            (id, inbox.port.clone(), read)
        };
        let (id, port, read) = port;
        if read {
            self.delivered_noting(agent, &id, VIA_READ, Some(READ_NOTE));
        }
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
        self.delivered_noting(agent, id, via, None);
    }

    /// [`Self::delivered`], saying why the delivery departed from the lane's usual rule.
    pub fn delivered_noting(&self, agent: &AgentId, id: &str, via: &str, note: Option<&str>) {
        self.audit(RecordKind::MessageDelivered(MessageDelivered {
            agent_id: agent.clone(),
            message_id: id.to_string(),
            via: via.to_string(),
            note: note.map(str::to_string),
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
                    note: None,
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

    /// **A child's end its parent already read with `wait` costs the parent no turn** — whichever
    /// comes first. A live codex root got a whole extra generation just to hear about a child it
    /// had waited on. Read after the announcement is queued, the queued message is withdrawn;
    /// read before, the announcement is never queued. Either way the debt is settled, the driver
    /// is woken, and the journal says the end reached the node through `wait`.
    #[test]
    fn a_childs_end_the_parent_already_read_is_withdrawn_or_never_queued() {
        let delivered_via_wait = |log: &Arc<Mutex<Vec<RecordKind>>>| {
            records(log)
                .iter()
                .filter(|r| matches!(r, RecordKind::MessageDelivered(d) if d.via == "wait"))
                .count()
        };
        // The end is queued first, then read.
        let (inboxes, log) = recording();
        let p = id("parent");
        inboxes.open(&p);
        inboxes.owe(&p);
        inboxes
            .announce(&p, TYPED, ended("child"), "the end".into())
            .unwrap();
        inboxes
            .enqueue(&p, TYPED, Source::Operator, "keep me".into())
            .unwrap();
        assert!(
            inboxes.received(&p, &id("child")),
            "the queued end is withdrawn"
        );
        assert_eq!(
            inboxes.take_next(&p).map(|m| m.text),
            Some("keep me".into())
        );
        assert_eq!(inboxes.take_or_seal(&p), None);
        assert!(!inboxes.held(&p), "nothing owed, nothing queued: sealed");
        assert_eq!(delivered_via_wait(&log), 1);

        // The end is read first, then announced.
        let (inboxes, log) = recording();
        inboxes.open(&p);
        inboxes.owe(&p);
        let port = Arc::new(Counting(AtomicUsize::new(0)));
        inboxes.attach_port(&p, port.clone());
        assert!(!inboxes.received(&p, &id("child")), "nothing queued yet");
        inboxes
            .announce(&p, TYPED, ended("child"), "the end".into())
            .expect("accepted, and resolved at once");
        assert_eq!(inboxes.queued(&p), 0, "no turn is spent on it");
        assert!(
            port.0.load(Ordering::SeqCst) >= 1,
            "the held driver is told"
        );
        assert_eq!(inboxes.take_or_seal(&p), None);
        assert!(!inboxes.held(&p), "the debt is settled");
        assert_eq!(delivered_via_wait(&log), 1);

        // Another child's end is still announced.
        let (inboxes, _) = recording();
        inboxes.open(&p);
        inboxes.owe(&p);
        inboxes.received(&p, &id("child"));
        inboxes
            .announce(&p, TYPED, ended("other"), "the other end".into())
            .unwrap();
        assert_eq!(inboxes.queued(&p), 1);
    }

    /// **A node that ended a turn without reporting is asked once, never twice.** The request is
    /// an ordinary queued message — journaled as marion's, taken at the boundary like any other —
    /// so every lane carries it the way it carries a steer; a second ask in the same node's life
    /// is refused, which is what bounds the re-prompt.
    #[test]
    fn a_report_is_requested_once_as_a_queued_message_from_marion() {
        let (inboxes, log) = recording();
        let a = id("a");
        assert!(
            !inboxes.request_report(&a),
            "no inbox, nothing to ask through"
        );
        inboxes.open(&a);
        inboxes.owe(&a);
        assert!(
            !inboxes.request_report(&a),
            "a child's end is still owed: that is its next turn, not a request to report"
        );
        inboxes.release(&a);
        assert!(inboxes.request_report(&a));
        assert!(!inboxes.request_report(&a), "one re-prompt per node");
        let m = inboxes
            .take_or_seal(&a)
            .expect("the request is the next turn");
        assert_eq!(m.source, Source::ReportRequested);
        let words = render(&m);
        assert!(
            words.starts_with("marion: ") && words.contains("`report`"),
            "{words}"
        );
        assert!(matches!(
            records(&log).as_slice(),
            [RecordKind::MessageQueued(MessageQueued {
                source: MessageSource::ReportRequested,
                ..
            })]
        ));
        assert_eq!(inboxes.take_or_seal(&a), None);
        assert!(!inboxes.request_report(&a), "sealed: nothing more is asked");
    }

    /// **The feed asks only where it can and should**: a node whose last turn holds no report,
    /// with time left, on a feed that knows how to read a report (a child's). A root's feed has
    /// no reader, since a root never reports.
    #[test]
    fn a_feed_asks_for_a_report_only_for_an_unreported_turn_with_time_left() {
        let (inboxes, _) = recording();
        let inboxes = Arc::new(inboxes);
        let feed = |agent: &str, reads: bool| {
            let a = id(agent);
            inboxes.open(&a);
            let feed = TurnFeed::new(Arc::new(BoundInbox::new(Arc::clone(&inboxes), a)), TYPED);
            if reads {
                feed.reading_reports(Arc::new(|since: &str| since.contains("REPORTED")))
            } else {
                feed
            }
        };
        assert!(!feed("reported", true).ask_for_report("… REPORTED …", true));
        assert!(
            !feed("late", true).ask_for_report("nothing", false),
            "no time left"
        );
        assert!(
            !feed("root", false).ask_for_report("nothing", true),
            "a root is never asked"
        );
        assert!(
            !feed("limited", true)
                .reading_failures(Arc::new(|since: &str| since.contains("LIMIT")))
                .ask_for_report("… LIMIT …", true),
            "a turn that ended on its own failure claim is not answered with a request"
        );
        let child = feed("child", true);
        assert!(child.ask_for_report("nothing", true));
        assert!(!child.ask_for_report("nothing", true), "once");
        assert_eq!(inboxes.queued(&id("child")), 1);
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
                .starts_with("The codex-impl child you backgrounded as task_id \"t-1\" has ended;")
        );
        assert!(ended.ends_with("\n\nuse the v2 API"));
    }
}
