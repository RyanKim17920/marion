//! The registry, answering `node/get`, `tree/subscribe`, and §7.3.2's `session/quit`.
//!
//! [`serve`](crate::serve) knows about frames and connections and nothing about nodes;
//! [`registry`](crate::registry) knows about nodes and nothing about clients. This is the seam, and
//! it is where three questions the registry deliberately refused to answer get answered — or are
//! refused again, in words, which is the other honest outcome.
//!
//! # Projecting a node is fallible, and that is the finding
//!
//! `registry.rs` states that it will not build a [`NodeSummary`] because two of its fields —
//! `timeout` and `name` — *"have no source in the journal at all"*, and that building one means
//! fabricating both. Neither is fabricated here:
//!
//! * **`timeout`** is resolved from the node's recorded `agent_type` through
//!   [`marion_core::agent_type::builtin`] — the same registry that decided the bound when the node
//!   was spawned. §3.1 makes the agent type the source of truth for it and §9 *"re-resolves it fresh
//!   on resume"*, so reading it from the type is what the spec already prescribes; a journalled copy
//!   would be the second source of truth, not the first.
//! * **`name`** is `None`, which is not a placeholder but the truth. §2's `node/rename` is the only
//!   thing that sets `Node.name`, it does not exist, and nothing has ever written one. When it lands
//!   it lands as a journal record and is read here; until then `None` is what the journal says.
//! * **`agent_type`, `harness`, `depth`** come from the `SpawnIntent`, which `marion-core` makes an
//!   `Option` precisely because a journal whose head was compacted away, or whose intent record was
//!   lost, has records *about* a node and no identity *for* it.
//!
//! So a node can be **unprojectable**, and [`summarize`] returns that rather than inventing a
//! summary. Three ways, each named: no intent at all, an `agent_type` this build does not know, and
//! a depth that will not fit `NodeSummary`'s `u8`. A `node/get` for such a node is a refusal that
//! says which fact is missing, and — see below — a `tree/subscribe` counts them rather than dropping
//! them into silence.
//!
//! # The snapshot and the subscription begin at the same instant
//!
//! §7.3.3 makes the replay-to-subscribe seam *the* correctness question, and
//! [`marion_core::proto::result::TreeSubscribeResult`] says why the two travel together: *"a client that
//! renders a tree and **then** starts listening has a window it cannot account for."*
//!
//! Here that is a lock, not a promise. [`RegistryHandle::subscribe`] takes the shared state's lock,
//! performs **one** read of the registry, and from that single view it (a) flushes to existing
//! subscribers everything that changed since the last flush, (b) builds this subscriber's snapshot,
//! and (c) records the snapshot as the point this subscriber has been told about. A notification
//! cannot be produced between (b) and (c), because nothing else can hold the lock, so the new
//! subscriber can neither miss an event nor be told twice about one it already has.
//!
//! # Quit acts here; departure never does
//!
//! §7.3 gives two events one name and makes confusing them the crash-safety failure. The explicit
//! [`Call::SessionQuit`] is handled under `quit`'s decision lock: validate a confirmed render,
//! durably append an intent, perform one per-node §6.7 kill, observe death, then durably confirm.
//! [`Handle::gone`] performs none of those steps. Even when the transport reports
//! [`ClientGone::Quit`], the disposition was already accepted or refused by the call; applying it
//! again at EOF would make every successful quit happen twice and every refused quit happen once.
//!
//! §5.7's exit is deliberately split once more. A successful call can make exit *eligible*, but
//! the serve loop alone knows whether the client count stayed at zero for the configured grace.
//! Only its later [`Handle::begin_idle_exit`] callback appends `SupervisorExited` and flips
//! `exiting`; this is why the record cannot be written while the response's socket is still open.
//!
//! # What is deliberately not built
//!
//! `TreeSubscribeResult` has nowhere to say *"and there are N nodes I could not describe"*. Rather
//! than omit them into silence — the accept-and-ignore shape §11 item 23 keeps naming — the count is
//! kept on the supervisor's side and exposed as [`RegistryHandle::unprojectable`], where a test can
//! see it and a `doctor` will read it. Naming the gap is not the same as closing it, and this one is
//! open until the vocabulary has a field for it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marion_core::agent_type;
use marion_core::contract::{AgentId, Isolation, ProcessExit, TaskContract, TaskId};
use marion_core::journal::{
    KillConfirmed, KillIntent, ReapConfirmed, ReapIntent, RecordKind, SpawnIntent, SupervisorExited,
};
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::notify::Event;
use marion_core::proto::result::{
    NodeAttachResult, NodeGetResult, SessionQuitResult, TreeSubscribeResult,
};
use marion_core::proto::{
    AttachMode, Call, ClientGone, DetachGuidance, FailureKind, KilledNode, MethodResult,
    NativeLaunchContext, NodeSummary, QuitDisposition, QuitOutcome, ReplayPoint, ResidentReason,
    RpcError, SpawnCaller, SupervisorDisposition,
};
use marion_core::registry::{Replay, ReplayedNode};

use crate::native_binding::{NativeBindingError, refuse_untrusted_native_launch};
use crate::registry::{LiveRegistry, Registry};
use crate::serve::{ConnId, Departure, Handle, Outbound, Peer};

/// Why a node the journal knows about cannot be described to a client.
///
/// An enum and not a `None`, because the three have different causes and different fixes, and a
/// client told only *"cannot describe it"* would have no idea whether to look at the journal, at
/// this build's agent types, or at a writer that produced a nonsense depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unprojectable {
    /// No `SpawnIntent`: the journal has records *about* this node and no identity *for* it. Its
    /// head was compacted, or the record was lost (§4.2's `Ordinal` loss is exactly this shape).
    NoIntent,
    /// The journal names an agent type this build does not have, so the node's `timeout` — §3.1's
    /// bound, which §9 re-resolves from the type — cannot be resolved from anywhere.
    UnknownAgentType(String),
    /// A depth `NodeSummary`'s `u8` cannot hold. §6.1's default `max_depth` is 3, so this is a
    /// writer producing nonsense rather than a deep tree, and saturating it would silently place the
    /// node somewhere it is not.
    DepthOutOfRange(u32),
}

/// Failures made at the native-launch boundary.
///
/// A public versioned value has already passed protocol-version validation during deserialization.
/// This early gate owns only root/child pairing; root binding runs after peer authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NativeLaunchGateError {
    #[error("native launch context belongs only on a root request")]
    ChildMisuse,
}

impl NativeLaunchGateError {
    fn into_rpc(self) -> RpcError {
        match self {
            Self::ChildMisuse => RpcError::refused(
                "native_launch",
                "native launch context belongs only on a root: a spawn carrying `caller` is a \
                 managed child whose launch marion derives from its agent type and contract. \
                 Refused rather than dropping the native context or replacing the managed launch.",
                "§6.1, §9, §11 item 23",
            ),
        }
    }
}

fn native_binding_error_into_rpc(error: NativeBindingError) -> RpcError {
    match error {
        NativeBindingError::UnsupportedPlatform(error) => RpcError::unsupported(
            "native_launch",
            format!(
                "this native launch request requires byte-exact operating-system values, but \
                 {error}. Refused before any launch state was created."
            ),
            "§9",
        ),
        error => RpcError::refused(
            "native_launch",
            format!("{error}. Refused before any launch state was created."),
            "§9, §11 item 23",
        ),
    }
}

fn native_launch_refusal(context: &NativeLaunchContext) -> RpcError {
    native_binding_error_into_rpc(refuse_untrusted_native_launch(context))
}

/// Refuse child/native pairing before token lookup or any launch state is consulted.
///
/// Root requests pass unchanged here; their opaque selector/program binding is deliberately later,
/// after [`root_spawn_authorized`] authenticates the socket peer.
pub(crate) fn validate_native_launch_boundary(
    caller: Option<&SpawnCaller>,
    native_launch: Option<&NativeLaunchContext>,
) -> Result<(), NativeLaunchGateError> {
    if native_launch.is_none() {
        return Ok(());
    }
    if caller.is_some() {
        return Err(NativeLaunchGateError::ChildMisuse);
    }

    Ok(())
}

impl Unprojectable {
    /// The refusal a client sees, with the citation for the fact that is missing.
    pub fn as_error(&self, agent: &AgentId) -> RpcError {
        match self {
            Unprojectable::NoIntent => RpcError::of(
                FailureKind::Internal,
                Some(&agent.0),
                "the journal has records about this node but no `SpawnIntent` for it, so marion \
                 does not know its agent type, its harness, its parent or its depth. Replay reports \
                 that absence rather than filling it in, and this call will not invent one \
                 (§4.2, §7.4).",
                "§4.2",
            ),
            Unprojectable::UnknownAgentType(t) => RpcError::not_found(
                t,
                format!(
                    "this node is recorded as agent type `{t}`, which this build of marion does \
                     not have. §3.1 makes the agent type the source of its timeout bound and §9 \
                     re-resolves that bound from the type rather than from a copy, so marion \
                     cannot describe the node without it."
                ),
                "§3.1",
            ),
            Unprojectable::DepthOutOfRange(d) => RpcError::of(
                FailureKind::Internal,
                Some(&agent.0),
                format!(
                    "this node is recorded at depth {d}, which is not a depth a tree marion built \
                     can reach (§6.1's default max_depth is 3). Reporting a saturated depth would \
                     place the node somewhere it is not."
                ),
                "§6.1",
            ),
        }
    }
}

/// §3.2's node, projected for a client — or the reason it cannot be.
///
/// Pure: it reads the replayed node, this build's agent-type registry and one fact the caller
/// supplies, and touches nothing else. That is what makes every arm above testable without a
/// socket, a journal or a thread.
///
/// `pane` is a **parameter and not a field of `node`** because it is not a journal fact: the
/// supervisor's live pty map is what knows whether a node has a display plane, and `ReplayedNode`
/// is a fold over records. Passing it in keeps replay honest — no record is invented for it — and
/// makes every caller state which answer it is giving, which is what stops a caller that has no
/// pty map from quietly defaulting one.
pub fn summarize(node: &ReplayedNode, pane: bool) -> Result<NodeSummary, Unprojectable> {
    let intent = node.intent.as_ref().ok_or(Unprojectable::NoIntent)?;
    // A built-in resolves here; a `.marion/agents.toml` row does not, because this runs under
    // the shared registry lock and reads no file. The type was only ever needed for the bound,
    // and every production writer journals the bound — so a recorded bound projects a node of
    // any type, and only an unresolvable type *with no recorded bound* is unprojectable.
    let ty = agent_type::builtin(&intent.agent_type);
    if ty.is_none() && intent.timeout_secs.is_none() {
        return Err(Unprojectable::UnknownAgentType(intent.agent_type.clone()));
    }
    let depth =
        u8::try_from(intent.depth).map_err(|_| Unprojectable::DepthOutOfRange(intent.depth))?;
    Ok(NodeSummary {
        agent_id: node.agent_id.clone(),
        parent_id: intent.parent_id.clone(),
        // Not a placeholder. See the module doc: nothing sets `Node.name` yet, so `None` is what
        // the journal says rather than what marion does not know.
        name: None,
        agent_type: intent.agent_type.clone(),
        harness: intent.harness,
        // §3.3's middle key component, straight off `Spawned`. `None` until the process exists,
        // which is the honest answer for a node that has not launched: there is no version yet.
        harness_version: node.harness_version.clone(),
        depth,
        state: node.state,
        reap_state: node.reap_state,
        // **The bound this node was launched under, and the agent type only where the journal is
        // silent.** §3.1 makes the type the *default*; the intent records what the launch actually
        // resolved (`--timeout`, `spawn`'s `timeout_secs`), and a pane that prints the default for
        // a node running under a different clock is telling an operator the wrong number in the one
        // place they look to decide whether a run has time left. `None` is an older journal or a
        // launch marion put no bound of its own on, and the type is the honest answer for both.
        timeout: match (intent.timeout_secs, ty) {
            (Some(secs), _) => marion_core::encoding::Duration::from_secs(secs),
            (None, Some(ty)) => ty.timeout,
            (None, None) => unreachable!("refused above"),
        },
        pane,
    })
}

/// What a subscriber has already been told about one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Told {
    state: NodeState,
    reap_state: ReapState,
}

/// One client following one node's `events.jsonl`.
///
/// The [`EventReader`](crate::events::EventReader) is owned here rather than shared, because a
/// cursor is per subscription: two clients attaching to one node at different moments have
/// different read points, and a shared reader would make the second one's replay depend on when
/// the first attached. The file is the shared thing; the position in it is not.
///
/// `conn` is kept alongside `out` so [`Handle::gone`] can drop this without asking the transport
/// anything — the same bookkeeping `subs` gets, for the same reason.
struct Attachment {
    conn: ConnId,
    agent_id: AgentId,
    reader: crate::events::EventReader,
    out: Outbound,
}

/// **Every node this supervisor holds a pty for, and who is typing into each.**
///
/// Separate from [`Shared`] rather than a field of it, for the reason [`RegistryHandle::nodes`]
/// gives: `shared` is taken and released inside one call, and writing a keystroke is a `write(2)`
/// on a pty master. Folding them together would put an operator's keyboard inside the lock every
/// `tree/subscribe` waits on — and a node whose harness has stopped reading its stdin would then
/// block the whole supervisor rather than one attach.
#[derive(Default)]
struct Panes {
    /// `Arc` because a keystroke is delivered outside the map's lock: the host is cloned out, the
    /// lock is released, and only then is the byte written. A `&PtyHost` would hold the map for
    /// the duration of the write.
    hosts: HashMap<AgentId, PaneEntry>,
    /// Write leases, **keyed by the connection**, which is what makes §7.3.1 automatic here.
    /// `WriteLease` releases the node's writer slot on `Drop`, so a client that was SIGKILLed hands
    /// its keyboard back when `gone` removes this entry — nobody has to write the cleanup, and a
    /// node whose one writer died is not permanently read-only.
    /// `Arc` so a keystroke can be written **outside** this map's lock. `PtyMaster::write_all`
    /// spins on `EAGAIN` — a full tty input buffer is backpressure from a harness that has not read
    /// yet, not a failure — so a write can take arbitrarily long, and holding the map across it
    /// would let one unread node stall every other client's attach. The lease still releases the
    /// node's writer slot when the last `Arc` drops, which is the entry leaving this map.
    leases: HashMap<ConnId, Vec<(AgentId, Arc<crate::pty::WriteLease>)>>,
    native_launches: Option<Arc<crate::native_bootstrap::PendingNativeLaunches>>,
    host_generations: HashMap<AgentId, u64>,
    next_host_generation: u64,
    completed_count: usize,
    completed_bytes: usize,
    next_completed_order: u64,
    next_completed_expiry: Option<std::time::Instant>,
    #[cfg(test)]
    completed_limit: Option<usize>,
    #[cfg(test)]
    completed_scan_count: usize,
}

/// A native ticket and writer slot validated together before the claim ACK.
///
/// The authority guard keeps ordinary attaches behind the pending gate, while `lease` reserves the
/// host's one writer slot. Neither requires holding `Panes` during socket I/O. Dropping this value
/// before commit restores the ticket and releases the slot, which makes any fallible relay
/// preparation (including lifecycle-thread creation) retryable. Commit is itself the final
/// fallible preparation step; a later acknowledgement failure rolls the whole launch back through
/// its prepared lifecycle rather than trying to resurrect a ticket whose writer was published.
pub(crate) struct PreparedNativeWriter {
    handle: Arc<RegistryHandle>,
    authority: Arc<crate::native_bootstrap::PendingNativeLaunches>,
    claim: Option<crate::native_bootstrap::PreparedNativeLaunchClaim>,
    agent_id: AgentId,
    host: Arc<crate::pty::PtyHost>,
    host_generation: u64,
    conn: ConnId,
    lease: Option<Arc<crate::pty::WriteLease>>,
}

impl PreparedNativeWriter {
    pub(crate) fn commit(
        mut self,
    ) -> Result<
        crate::native_bootstrap::NativeLaunchClaim,
        crate::native_bootstrap::NativeLaunchClaimError,
    > {
        let mut panes = lock(&self.handle.panes);
        let exact_host = panes
            .hosts
            .get(&self.agent_id)
            .is_some_and(|entry| entry.is_live() && Arc::ptr_eq(entry.host(), &self.host));
        let exact_generation =
            panes.host_generations.get(&self.agent_id) == Some(&self.host_generation);
        let exact_authority = panes
            .native_launches
            .as_ref()
            .is_some_and(|authority| Arc::ptr_eq(authority, &self.authority));
        if !exact_host
            || !exact_generation
            || !exact_authority
            || self.host.writer() != Some(self.conn)
        {
            return Err(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding);
        }
        let claim = self
            .claim
            .take()
            .expect("a prepared native writer commits once")
            .commit()?;
        panes.leases.entry(self.conn).or_default().push((
            self.agent_id.clone(),
            self.lease
                .take()
                .expect("a prepared native writer owns its lease"),
        ));
        Ok(claim)
    }
}

enum PaneEntry {
    Live(Arc<crate::pty::PtyHost>),
    Closing(Arc<crate::pty::PtyHost>),
    Completed {
        host: Arc<crate::pty::PtyHost>,
        charged_bytes: usize,
        completed_at: std::time::Instant,
        order: u64,
    },
    Replacing(Arc<crate::pty::PtyHost>),
}

/// **Why a client may or may not type into the pane it just attached to.** The three answers are
/// deliberately one type: `writable: false` alone has meant both "somebody else is typing into
/// this running node" and "this node has finished", and a client cannot act correctly on the pair
/// collapsed into one bit. A native relay that reads a finished pane as a stolen lease abandons
/// the replay it attached to carry (§5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneWriteHalf {
    /// This connection holds the write half.
    Held,
    /// The pane is live and its write half belongs to another connection, named where the host
    /// still knows it.
    Elsewhere(Option<u64>),
    /// The pane generation has ended. Nobody holds its write half and nobody can be given it.
    Ended,
}

impl PaneWriteHalf {
    /// Write this answer onto the attach result. The one place the three states become wire
    /// fields, so a caller cannot spell a busy writer and an ended pane the same way.
    fn describe(self, pane: &mut marion_core::proto::result::PaneAttach) {
        let (writable, held_by, ended) = match self {
            Self::Held => (true, None, false),
            Self::Elsewhere(owner) => (false, owner, false),
            Self::Ended => (false, None, true),
        };
        pane.writable = writable;
        pane.held_by = held_by;
        pane.ended = ended;
    }
}

impl PaneEntry {
    fn host(&self) -> &Arc<crate::pty::PtyHost> {
        match self {
            Self::Live(host) | Self::Closing(host) | Self::Replacing(host) => host,
            Self::Completed { host, .. } => host,
        }
    }

    fn live_host(&self) -> Option<&Arc<crate::pty::PtyHost>> {
        match self {
            Self::Live(host) => Some(host),
            Self::Closing(_) | Self::Completed { .. } | Self::Replacing(_) => None,
        }
    }

    fn replay_host(&self) -> Option<&Arc<crate::pty::PtyHost>> {
        match self {
            Self::Live(host) | Self::Closing(host) | Self::Completed { host, .. } => Some(host),
            Self::Replacing(_) => None,
        }
    }

    fn is_live(&self) -> bool {
        matches!(self, Self::Live(_))
    }
}

impl Panes {
    const MAX_COMPLETED: usize = 64;
    /// Cap on conservative cache-owned replay charge, not literal allocator/process RSS.
    const MAX_COMPLETED_BYTES: usize = 256 * 1024 * 1024;
    const COMPLETED_TTL: std::time::Duration = std::time::Duration::from_secs(300);

    fn completed_limit(&self) -> usize {
        #[cfg(test)]
        if let Some(limit) = self.completed_limit {
            return limit;
        }
        Self::MAX_COMPLETED
    }

    fn debit_completed(&mut self, entry: &PaneEntry) {
        let PaneEntry::Completed { charged_bytes, .. } = entry else {
            return;
        };
        self.completed_count = self
            .completed_count
            .checked_sub(1)
            .expect("every Completed insertion is debited exactly once");
        self.completed_bytes = self
            .completed_bytes
            .checked_sub(*charged_bytes)
            .expect("completed bytes are debited by their exact stored charge");
    }

    fn recompute_next_completed_expiry(&mut self) {
        self.next_completed_expiry = self
            .hosts
            .values()
            .filter_map(|entry| match entry {
                PaneEntry::Completed { completed_at, .. } => {
                    completed_at.checked_add(Self::COMPLETED_TTL)
                }
                _ => None,
            })
            .min();
    }

    fn remove_accounted(&mut self, id: &AgentId) -> Option<PaneEntry> {
        let entry = self.hosts.remove(id)?;
        self.debit_completed(&entry);
        Some(entry)
    }

    fn remove(&mut self, id: &AgentId) -> Option<PaneEntry> {
        let entry = self.remove_accounted(id)?;
        if matches!(entry, PaneEntry::Completed { .. }) {
            self.recompute_next_completed_expiry();
        }
        Some(entry)
    }

    fn replace(&mut self, id: AgentId, entry: PaneEntry) -> Option<PaneEntry> {
        let old = self.hosts.insert(id, entry);
        if let Some(old) = old.as_ref() {
            self.debit_completed(old);
        }
        self.recompute_next_completed_expiry();
        old
    }

    fn complete(
        &mut self,
        id: &AgentId,
        host: &Arc<crate::pty::PtyHost>,
        charged_bytes: usize,
        completed_at: std::time::Instant,
    ) -> bool {
        if !matches!(self.hosts.get(id), Some(PaneEntry::Closing(current)) if Arc::ptr_eq(current, host))
        {
            return false;
        }
        // A candidate that cannot fit by itself must not evict valid older completions merely to
        // discover that it still cannot fit.
        if charged_bytes > Self::MAX_COMPLETED_BYTES {
            return false;
        }
        let Some(completed_count) = self.completed_count.checked_add(1) else {
            return false;
        };
        let Some(completed_bytes) = self.completed_bytes.checked_add(charged_bytes) else {
            return false;
        };
        let Some(next_order) = self.next_completed_order.checked_add(1) else {
            return false;
        };
        let order = self.next_completed_order;
        self.next_completed_order = next_order;
        self.completed_count = completed_count;
        self.completed_bytes = completed_bytes;
        self.hosts.insert(
            id.clone(),
            PaneEntry::Completed {
                host: Arc::clone(host),
                charged_bytes,
                completed_at,
                order,
            },
        );
        if let Some(expiry) = completed_at.checked_add(Self::COMPLETED_TTL)
            && self
                .next_completed_expiry
                .is_none_or(|next_expiry| expiry < next_expiry)
        {
            self.next_completed_expiry = Some(expiry);
        }
        true
    }

    fn take_completed_victims(&mut self, now: std::time::Instant) -> Vec<Arc<crate::pty::PtyHost>> {
        if self.completed_count == 0 {
            return Vec::new();
        }
        if self.completed_within_budget(now) {
            return Vec::new();
        }
        #[cfg(test)]
        {
            self.completed_scan_count += 1;
        }
        let mut victims = self.take_expired_completed(now);
        self.take_oldest_completed_over_budget(&mut victims);
        self.recompute_next_completed_expiry();
        victims
    }

    /// **Nothing is past its deadline and nothing is over budget**, so no Completed pane is a
    /// victim and the scan below is skipped entirely.
    fn completed_within_budget(&self, now: std::time::Instant) -> bool {
        self.next_completed_expiry
            .is_some_and(|next_expiry| now < next_expiry)
            && self.completed_count <= self.completed_limit()
            && self.completed_bytes <= Self::MAX_COMPLETED_BYTES
    }

    /// Every Completed pane at or past [`Self::COMPLETED_TTL`], taken out of the accounting.
    fn take_expired_completed(&mut self, now: std::time::Instant) -> Vec<Arc<crate::pty::PtyHost>> {
        let expired = self
            .hosts
            .iter()
            .filter_map(|(id, entry)| match entry {
                PaneEntry::Completed { completed_at, .. }
                    if now.saturating_duration_since(*completed_at) >= Self::COMPLETED_TTL =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter_map(|id| self.remove_accounted(&id))
            .map(|entry| Arc::clone(entry.host()))
            .collect::<Vec<_>>()
    }

    /// **Oldest first**, until the retained Completed panes are back inside both budgets.
    fn take_oldest_completed_over_budget(&mut self, victims: &mut Vec<Arc<crate::pty::PtyHost>>) {
        while self.completed_count > self.completed_limit()
            || self.completed_bytes > Self::MAX_COMPLETED_BYTES
        {
            let Some(oldest) = self
                .hosts
                .iter()
                .filter_map(|(id, entry)| match entry {
                    PaneEntry::Completed { order, .. } => Some((id.clone(), *order)),
                    _ => None,
                })
                .min_by_key(|(_, order)| *order)
                .map(|(id, _)| id)
            else {
                break;
            };
            if let Some(entry) = self.remove_accounted(&oldest) {
                victims.push(Arc::clone(entry.host()));
            }
        }
    }

    /// `conn`'s lease on `id`, if it holds one. The lease **is** the permission — there is no
    /// separate boolean to disagree with it — so a caller that gets `None` here has been refused.
    fn lease(&self, conn: ConnId, id: &AgentId) -> Option<Arc<crate::pty::WriteLease>> {
        self.leases
            .get(&conn)?
            .iter()
            .find(|(a, _)| a == id)
            .map(|(_, l)| Arc::clone(l))
    }

    fn has_live(&self, id: &AgentId) -> bool {
        self.hosts.get(id).is_some_and(PaneEntry::is_live)
    }

    fn assign_host_generation(&mut self, id: &AgentId) -> Option<u64> {
        let generation = self.next_host_generation.checked_add(1)?;
        self.next_host_generation = generation;
        self.host_generations.insert(id.clone(), generation);
        Some(generation)
    }

    fn revoke_native_launch(&mut self, id: &AgentId) {
        self.host_generations.remove(id);
        if let Some(launches) = self.native_launches.as_ref() {
            launches.revoke_agent(id);
        }
    }
}

#[derive(Default)]
struct Shared {
    subs: Vec<Outbound>,
    attached: Vec<Attachment>,
    clients: HashSet<ConnId>,
    told: HashMap<AgentId, Told>,
    unprojectable: usize,
}

/// **One node this supervisor owns**, which until §11 item 28 step 4 was a sentence with nothing
/// behind it: `detach.rs`'s stage 3 held no `Command`, no `Child`, no pid and no pipe, and every
/// node in the fleet was owned by a bridge process the harness had started.
///
/// **A thread per node, not a poll loop**, and that is the choice worth defending. `run_duplex` is
/// a blocking, ordered protocol driver — `duplex.rs`'s own doc opens by naming the drift it exists
/// to prevent — and rewriting it as a state machine an event loop could step would be a second
/// implementation of the most-measured code in this repo. `background.rs` already proves the shape
/// under §6.1 step 2's concurrency gate, and this supervisor is already thread-per-thing (accept
/// loop, connection, follower). **The cost is stated rather than hidden: roughly 3N+2 threads for a
/// fleet of N nodes** — the driver, its stdout drain and its stderr drain, plus the accept loop and
/// the follower.
pub struct NodeHandle {
    /// §9's contract this node runs under. Kept because it is how a caller names the run in every
    /// other vocabulary — the contract file on disk, the branch, a `wait`.
    ///
    /// **`None` for a root, and that is §9 rather than an omission**: *"a root has no
    /// `TaskContract`"*. Minting an id here for a root would name a contract file nothing will ever
    /// write, and every vocabulary this field exists to serve — the file, the `marion/<task>` ref,
    /// a `wait` — would then resolve to nothing.
    task_id: Option<TaskId>,
    /// §5.4's per-node capability, minted here and written into exactly one other place: the MCP
    /// declaration this node's own bridge reads. See [`RegistryHandle::claim`].
    token: String,
    /// **The tree this node lives in**, and therefore the tree its own children branch from —
    /// [`crate::run::SpawnRequest::repo`] for the child of this node's next `agent/spawn`.
    ///
    /// One supervisor serves `/r` and every linked worktree of `/r` (§2 keys on the git common
    /// dir), so the repository cannot be a field of the supervisor; it has to be remembered per
    /// node and inherited down the tree from whichever root stated it.
    ///
    /// **In memory, and deliberately not in the journal.** This entry's lifetime is exactly the
    /// lifetime of the capability token beside it: both are minted at [`RegistryHandle::claim`]
    /// and both die with the process. A caller whose supervisor has restarted cannot present a
    /// token this supervisor minted, so its `agent/spawn` is refused before the repository is ever
    /// consulted — which means a repository that did not survive the restart cannot be a
    /// repository any authorized spawn needed. Persisting it would add a second, longer-lived
    /// copy of a fact that is only ever read under the shorter lifetime, and a journal that
    /// recorded a path would then have to be right about it after the tree moved on disk.
    repo: PathBuf,
    /// `None` until the process exists. The window is one `write(2)` plus one fsync wide — step 1
    /// made `Spawned` durable at `command.spawn()`, and this is filled from the same hook.
    pid: Option<i32>,
    /// The node's own process group, read with `getpgid(2)` rather than assumed equal to the pid.
    /// `socket.rs` already records why assuming it is wrong: a reader that *computes* `getpgid(pid)`
    /// and then treats it as `pid` has checked nothing. `None` where the call failed.
    pgid: Option<i32>,
    /// When the process came into existence — not when the node was claimed. The two differ by a
    /// worktree, a config document and a `--version` probe, and only the second is a fact about a
    /// process.
    started_at: Option<std::time::SystemTime>,
    /// The node's thread. Kept for §5.7's exit, which needs the *thread* joined and not merely its
    /// answer read — `background.rs` argues the same distinction for the same reason.
    join: Option<std::thread::JoinHandle<()>>,
    /// What the node's own thread produced. `None` means **still running**, and that is the reading
    /// §5.7's exit predicate takes as a second guard beside the journal's.
    outcome: Option<NodeOutcome>,
}

/// What a node's thread produced, in the vocabulary of the kind of node it was.
///
/// Two arms rather than one, because §9 gives a root and a child different results and collapsing
/// them would mean either fabricating a `TaskContract` for a node that has none, or filing a root's
/// success under `Err`. Only [`NodeHandle::running`] reads this today; it is kept whole so that the
/// distinction survives to whatever reads it next.
#[derive(Debug)]
enum NodeOutcome {
    /// §9's contract, or why `run_spawn` could not produce one.
    Child(Box<Result<TaskContract, crate::spawn::SpawnError>>),
    /// A root ran to a terminal reading, or marion's own sentence for why it did not. The reading
    /// itself is not carried: a root's result **is** its stream and its exit (§9), both of which
    /// are on disk in `events.jsonl` and the journal, and a second copy in memory would be a copy
    /// that disappears when the supervisor does.
    Root(Result<(), String>),
}

/// **Run a node's body so that a panic in it is still an outcome.**
///
/// Neither node thread had this, and the hole it left is the worst shape §5.7 has: a panic after
/// `SpawnObserver::identified` has claimed the node, but before `mark_finished`, left
/// [`NodeHandle::outcome`] `None` **for ever**. `None` means *still running*, so
/// [`RegistryHandle::running_nodes`] counted the node, [`RegistryHandle::idle_exit_eligible`]
/// refused, [`RegistryHandle::join_finished_nodes`] never reaped the thread, and the supervisor
/// could not exit for the rest of its life. Not a lost result — a permanent phantom.
///
/// **A panicking node is `Exited`-with-a-reason, not a new disposition.** `SpawnError::Panicked`
/// already existed for exactly this, with a carefully written sentence, and was **constructed
/// nowhere** — a documented variant describing a mechanism that did not run. `handler.rs`'s own
/// comment at [`RegistryHandle::join_finished_nodes`] asserted that a panic *"already reached the
/// caller as `SpawnError::Panicked`"*, which was false. Making that true is better than deleting
/// the claim and much better than adding a third `NodeOutcome` arm: the two arms exist to keep a
/// root's and a child's *vocabularies* apart (§9 gives them different results), and a panic is a
/// failure **within** each vocabulary, not a third kind of node. A third arm would force every
/// reader — [`RegistryHandle::owned_failure`], [`NodeHandle::running`] — to grow a case for
/// something both can already say.
///
/// **The payload is printed rather than folded into the sentence.** `SpawnError::Panicked`'s
/// message is about the *kind* of node and points the reader at the journal, and a panic message is
/// neither; a supervisor that swallowed its own defect's text entirely would make the one record
/// that says what went wrong unavailable anywhere.
///
/// The journal is left to the run's own unwind. `run.rs`'s `AbortOnDrop` is armed across the whole
/// child run and `Drop` runs on an unwind, so a `SpawnAborted` is already there — which is what the
/// variant's own doc says, and is why this does not write a record of its own over a node whose
/// process may still be alive.
/// `panicked` spells the panic in the caller's own error vocabulary — [`spawn_panicked`] for a
/// child, [`root_panicked`] for a root — rather than this function choosing one for both. The two
/// arms of [`NodeOutcome`] exist precisely because those vocabularies differ, and a shared helper
/// that picked one would put a `SpawnError` on a node that has no spawn result or a bare sentence
/// on one that does.
fn caught<T, E>(
    what: &str,
    panicked: impl FnOnce(&str) -> E,
    body: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(outcome) => outcome,
        Err(payload) => {
            eprintln!(
                "marion-supervisor: the thread running the {what} node panicked, which is a defect \
                 in marion: {}. The node is resolved as a failed run so this supervisor can still \
                 exit (§5.7); its journal records end with the abort the unwind wrote.",
                panic_text(&payload)
            );
            Err(panicked(what))
        }
    }
}

/// The tests' arming switch for the injected panic in [`NodeOwner::identified`].
///
/// A module with a guard rather than a bare `static`, because the lib tests share one process and
/// run in parallel: an arming that outlived its test would panic an unrelated spawn. The guard
/// disarms on drop, including on an unwind, and the mutex means only one test is inside the window
/// at a time.
#[cfg(test)]
mod panic_after_claim {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// **Armed for one tree, not globally**, and that distinction is the whole of this type.
    ///
    /// A bare on/off switch was wrong in a way a mutex cannot fix: serializing the *armings* stops
    /// two injecting tests from overlapping, but does nothing about the other tests in this binary
    /// that spawn a real child at the same time — one of them picked up the injected panic and
    /// failed on a spawn it had every right to expect to work. The switch now carries the repo the
    /// arming test owns, which is its own `scratch` directory and therefore unique to it, so the
    /// panic can only fire inside that test's own node.
    static ARMED: Mutex<Option<PathBuf>> = Mutex::new(None);
    static ONE_AT_A_TIME: OnceLock<Mutex<()>> = OnceLock::new();

    pub(super) fn armed_for(repo: &Path) -> bool {
        ARMED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            .is_some_and(|armed| armed == repo)
    }

    pub(super) struct Armed(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl Drop for Armed {
        fn drop(&mut self) {
            *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    pub(super) fn arm(repo: &Path) -> Armed {
        let g = ONE_AT_A_TIME
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = Some(repo.to_path_buf());
        Armed(g)
    }
}

/// A child's panic, in `run_spawn`'s vocabulary.
fn spawn_panicked(agent_type: &str) -> crate::spawn::SpawnError {
    crate::spawn::SpawnError::Panicked(agent_type.to_string())
}

/// A root's panic, in marion's own — §9 gives a root no `TaskContract`, so there is no
/// `SpawnError` to be had and [`NodeOutcome::Root`] carries a sentence.
fn root_panicked(agent_type: &str) -> String {
    format!(
        "marion's own thread running the {agent_type} root panicked, so there is no reading of \
         what the root did. This is a defect in marion, not in the request; the root's process may \
         have been left running, and the journal resolves the node with whatever the unwind wrote \
         rather than with a record of what it did."
    )
}

/// The best sentence available for a panic payload, which is a `&str` or a `String` for every panic
/// `panic!` produces and opaque otherwise.
fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic payload of an unprintable type".to_string())
}

impl NodeHandle {
    /// Whether this node is still running, as *this table* sees it.
    ///
    /// Deliberately not "the journal says non-terminal". A thread that is running but whose
    /// terminal journal write failed is exactly the case a journal-only predicate would let the
    /// supervisor exit through, and it is the case that leaves a live process behind.
    fn running(&self) -> bool {
        self.outcome.is_none()
    }
}

/// **Constant-time byte comparison, for the one value where a timing difference is a signal.**
///
/// `==` on `String` returns at the first differing byte, so an attacker who can call `agent/spawn`
/// repeatedly learns a token one byte at a time. The token is 32 hex characters; a prefix oracle
/// turns that from infeasible into a few hundred calls. The whole slice is always read.
fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // The length is not a secret — it is a constant of this build — so comparing it first leaks
    // nothing, and it is what lets the loop below be a fixed-width fold.
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

unsafe extern "C" {
    fn getpgid(pid: i32) -> i32;
}

/// **The wall clock an `agent/spawn` gets when the caller states none.**
///
/// The same 900 the bridge's `spawn` tool has always defaulted to, kept as one constant rather than
/// a second literal so the two surfaces cannot drift into promising different bounds for the same
/// absent field. `run::effective_timeout` clamps it exactly as it clamps a stated one.
///
/// **Public since §11 item 28 step 5**, and read by the bridge rather than re-spelled there. The
/// bridge no longer resolves this number — it sends the caller's `Option` untouched and the
/// resolution happens here — but it still has to know how long a `wait` on the resulting child may
/// block, and a `900` written beside this one is how the tool's promise and the node's actual
/// clock come to disagree.
pub const DEFAULT_SPAWN_TIMEOUT_SECS: u64 = 900;

/// **How long `agent/spawn` will hold its caller waiting for a process to exist.**
///
/// Not the node's wall clock, and deliberately much shorter than one: this bounds only the span
/// between `SpawnIntent` and `command.spawn()` — the worktree, the config documents, `compile`, and
/// `harness_version`'s own five-second probe, which §6.1 step 3 puts on the pre-launch path. A
/// minute is roughly an order of magnitude above the slowest of those.
///
/// It exists because the alternative is a JSON-RPC call with no bound at all. `git worktree add`
/// blocked on a repository lock is the measured case (`background.rs` found it blocking a `wait`
/// forever), and a call that never returns is worse here than there: `serve` answers each
/// connection on its own thread, but a client with no answer has no way to learn whether the node
/// it asked for exists.
///
/// **Expiry is never a verdict on the node.** The thread keeps running, the table keeps the entry,
/// and the refusal says which node marion has stopped waiting for — see [`launch_bound_expired`].
const LAUNCH_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// **32 bytes from `/dev/urandom`, as hex** — §5.4's per-node capability.
///
/// Not `run::entropy()`, which is 10 bytes and is sized for *uniqueness* in a UUIDv7 whose other
/// half is a millisecond timestamp. Uniqueness and unguessability are different requirements, and
/// reusing an id generator for a credential is how the second silently inherits the first's budget.
///
/// A failure to read `/dev/urandom` yields `None`, and [`RegistryHandle::claim`]'s caller turns
/// that into a node with no token — which is a node whose bridge can never spawn, and never a node
/// with a predictable one.
fn mint_token() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)) {
        Ok(()) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        // **A token nothing can present, rather than one anything can guess.** The empty string
        // never matches: `tokens_match` compares lengths first, and every real `SpawnCaller` must
        // carry a non-empty `node_token` to deserialize at all. So the node runs and cannot spawn,
        // which is the safe direction for a machine whose entropy source is unreadable.
        Err(_) => String::new(),
    }
}

/// **A fixed decoy of a real token's shape**, so a `SpawnCaller` naming a node this supervisor does
/// not own takes the same comparison path as one naming a node it does. Minted once per process and
/// never written anywhere, so it matches nothing.
fn decoy_token() -> &'static str {
    static DECOY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DECOY.get_or_init(mint_token)
}

/// What a node's own thread tells the call that started it. Three moments, in this order.
enum Progress {
    /// The node has an identity and its `SpawnIntent` is durable.
    Identified(AgentId),
    /// A process exists and `Spawned { pid: Some(_) }` is journaled.
    Started,
    /// `run_spawn` returned, whichever way. Sent last and always, so a launch that fails before
    /// either of the above cannot leave the call waiting out [`LAUNCH_BOUND`] for nothing.
    Finished,
}

/// The supervisor, watching one of its own nodes launch. See [`crate::run::SpawnObserver`].
struct NodeOwner {
    handle: Arc<RegistryHandle>,
    /// `None` for a root — see [`NodeHandle::task_id`].
    task_id: Option<TaskId>,
    /// The tree this node is being launched in, carried so [`RegistryHandle::claim`] can record it
    /// on the entry the node's *own* children will inherit from. See [`NodeHandle::repo`].
    repo: PathBuf,
    tx: std::sync::mpsc::Sender<Progress>,
    /// The id this spawn minted, once it has one — read by the thread body after `run_spawn`
    /// returns, so the outcome can be filed under the node it belongs to.
    identified: Mutex<Option<AgentId>>,
}

impl NodeOwner {
    fn identified_id(&self) -> Option<AgentId> {
        lock(&self.identified).clone()
    }
}

/// **The pane's half of the same ownership**, on §3.4's display axis (§9's M3 criterion C1).
///
/// `root::launch_terminal` opens the pty and reaps the child; this supervisor is the only process
/// that can *serve* it, because `node/attach` is answered here. The two calls are what make a
/// launched pane an attachable one, and the window between them is the node's whole life.
impl crate::root::PaneOwner for NodeOwner {
    fn opened(&self, agent_id: &AgentId, host: Arc<crate::pty::PtyHost>) {
        self.handle.register_pane(agent_id, host);
    }

    fn closing(&self, agent_id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.handle.closing_pane(agent_id, host);
    }

    fn completed(&self, agent_id: &AgentId, host: &Arc<crate::pty::PtyHost>, charged_bytes: usize) {
        self.handle.completed_pane(agent_id, host, charged_bytes);
    }

    /// Permanent invalidation is the failure/removal path; ordinary completion keeps replay state.
    fn failed(&self, agent_id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.handle.failed_pane(agent_id, host);
    }
}

impl crate::run::SpawnObserver for NodeOwner {
    fn identified(&self, agent_id: &AgentId) -> Option<String> {
        let token = self
            .handle
            .claim(agent_id, self.task_id.clone(), self.repo.clone());
        *lock(&self.identified) = Some(agent_id.clone());
        // Ignored: a receiver dropped before this fires means the call that started the spawn has
        // already given up on it, and the node goes on running either way. Panicking here would
        // unwind through `AbortOnDrop` and journal `SpawnAborted` over a node that is fine.
        let _ = self.tx.send(Progress::Identified(agent_id.clone()));
        // **The one place the tests can make a node's thread panic in the window that matters.**
        // Here and not at the top of the thread body, because the defect is specifically a panic
        // *after* the node is claimed and *before* `mark_finished` — a panic before the claim has
        // no node to strand. Placed after the `Progress::Identified` send so the calling frame gets
        // its id and its refusal comes from the thread's epilogue rather than from `LAUNCH_BOUND`.
        // `cfg(test)`, so no production build carries a branch that panics on purpose.
        #[cfg(test)]
        if panic_after_claim::armed_for(&self.repo) {
            panic!("injected: a node thread panicking after its claim");
        }
        (!token.is_empty()).then_some(token)
    }

    fn started(&self, agent_id: &AgentId, pid: i32) {
        self.handle.mark_started(agent_id, pid);
        let _ = self.tx.send(Progress::Started);
    }

    /// §7.6's subtree scan, off the registry this supervisor already follows. Refreshed first: the
    /// registry is a follower of the journal, and a descendant's `Exited` that landed since the last
    /// poll is exactly the transition a held node is waiting to see.
    fn live_descendants(&self, agent_id: &AgentId) -> Option<Vec<AgentId>> {
        self.handle.live.refresh();
        Some(
            self.handle
                .live
                .read(|r| crate::descendant_gate::live_descendants(r.tree(), agent_id)),
        )
    }
}

/// **What authorizes `agent/spawn` with no caller — open question 3, decided.**
///
/// The answer is: **filesystem permission on the socket, checked against peer credentials, and
/// deliberately not a token.** The argument, in the order the pieces matter.
///
/// *A token cannot be the answer here.* §5.4's capability is *"a per-node capability token bound to
/// its `AgentId`"*. A spawn with `caller: None` is a client creating a **root**: there is no node
/// yet, so there is nothing to bind a token to and nothing node-wise to prove. Any durable secret
/// invented for this path would be a file under `<state>`, readable by exactly the set of processes
/// that can already `connect(2)` to a socket in the same tree — so it would authenticate the same
/// set it excludes, at the cost of a secret at rest. It would look like security and be a mode.
///
/// *Peer credentials are necessary and not sufficient, and both halves are load-bearing.*
/// `getpeereid` answers **which user**; §5.4's question is **which node**. §11 item 28's open
/// question 3 records a code-verified audit of a third-party harness with this exact topology whose
/// authorization keyed on a client-asserted origin field, reachable by any process of the same
/// user — and notes that peer credentials *would not have saved them*, because the attacker's
/// credentials matched. That is why this check is confined to the one call where there is no node
/// to ask about: every spawn that names a caller goes through [`RegistryHandle::resolve_caller`]
/// and proves it with the token, and this check is not a substitute for that anywhere.
///
/// *What it does buy.* `socket.rs` creates the socket and its directory `0700` and the socket
/// `0600` (`a_created_socket_directory_is_private_and_so_is_the_socket`), so the kernel already
/// excludes other users at `connect`. This makes that exclusion a **checked** property of the
/// supervisor rather than an inherited property of a mode bit somebody could change, and it turns
/// the one case the mode bits cannot cover — a socket whose permissions were widened, deliberately
/// or by an umask accident — from a silent grant into a named refusal.
///
/// *An unreadable peer is a refusal.* [`Peer::Unknown`] means `getpeereid` failed, and a check that
/// cannot be made is not a check that passed.
fn root_spawn_authorized(peer: Peer) -> Result<(), RpcError> {
    let own = crate::socket::own_uid();
    match peer {
        Peer::Uid(uid) if uid == own => Ok(()),
        Peer::Uid(uid) => Err(RpcError::refused(
            "caller",
            format!(
                "this connection's peer runs as uid {uid} and this supervisor runs as uid {own}. A \
                 spawn with no `caller` creates a **root**, which is the one call on this socket \
                 that starts work rather than describing it, and marion authorizes it by \
                 filesystem permission on the socket: §2's path is under `<state>` (or \
                 `/tmp/marion-<uid>`), created 0700 with the socket 0600, so another user reaching \
                 it means the permissions are not what marion set. There is deliberately no token \
                 for this path — §5.4's capability binds to an `AgentId` and a root has none yet, \
                 and any secret at rest here would be readable by exactly the processes it would \
                 be excluding (§11 item 28, open question 3)."
            ),
            "§2, §5.4",
        )),
        Peer::Unknown => Err(RpcError::refused(
            "caller",
            "marion could not read this connection's peer credentials, so it cannot establish that \
             the caller is this supervisor's own user — and a check that could not be made is not \
             a check that passed. A spawn with no `caller` creates a root, which is authorized by \
             filesystem permission on the socket and by nothing else (§11 item 28, open question \
             3), so there is no weaker evidence to fall back to.",
            "§2, §5.4",
        )),
    }
}

/// §6.1 step 2's refusal, in the caller's own vocabulary rather than as an internal error.
fn gate_refusal(e: marion_core::agent_type::SpawnGateError) -> RpcError {
    RpcError::refused(
        "caller",
        format!(
            "{e}. The bound and the value that broke it are read from the **caller's** agent type \
             and the supervisor's own registry, never from the call — §6.1 step 2 says the gates \
             read the caller's type, and a caller that could state its own depth or child count \
             could state one that passes."
        ),
        "§6.1, §3.1",
    )
}

fn spawn_refused_before_the_node_existed() -> RpcError {
    RpcError::refused(
        "agent_type",
        "marion refused this spawn before it minted a node: the agent type is not a built-in and \
         not a row of the tree's .marion/agents.toml, or the requested writable scope is outside \
         that type's ceiling. Nothing was journaled and nothing was started, so there is no node \
         to look up. (§3.1, §5.4)",
        "§3.1",
    )
}

/// The agent types the tree at `repo` can spawn, or the file's own refusal as a frame's error.
///
/// [`crate::run::agent_types`]'s one non-`Ok` arm is a `.marion/agents.toml` that exists and cannot
/// be used; that is the operator's file, so the refusal carries its path and reason verbatim.
fn tree_types(repo: &Path) -> Result<marion_core::agent_type::AgentTypes, RpcError> {
    crate::run::agent_types(repo)
        .map_err(|e| RpcError::refused("agent_type", e.to_string(), "§3.1"))
}

/// A journaled node's type, re-resolved from today's table for the tree it runs in — and refused
/// if the type has since moved to another harness.
///
/// The journal records the name and the harness the node launched under. A `.marion/agents.toml`
/// row is the operator's and may have been edited since; a row that now names another harness
/// would relaunch a session that harness has never seen, under a node claiming to be the same
/// one. The built-ins cannot move, so this only ever fires on a user row.
fn recorded_type(
    repo: &Path,
    intent: &SpawnIntent,
) -> Result<marion_core::agent_type::AgentType, RpcError> {
    let ty = tree_types(repo)?
        .resolve(&intent.agent_type)
        .ok_or_else(|| {
            Unprojectable::UnknownAgentType(intent.agent_type.clone()).as_error(&intent.agent_id)
        })?;
    if ty.harness != intent.harness {
        return Err(RpcError::refused(
            "agent_type",
            format!(
                "`{}` was journaled as {} and {} now says {}; marion will not resume a node under \
                 a different harness than it recorded",
                intent.agent_type,
                intent.harness,
                crate::run::AGENT_TYPES_FILE,
                ty.harness
            ),
            "§3.1, §8",
        ));
    }
    Ok(ty)
}

/// A **child**'s launch failed between its intent and its process, and this is why.
///
/// `why` is the sentence `run_spawn`'s thread filed as its outcome — a worktree git refused, a
/// configuration document that would not write, an adapter that would not compile the launch
/// because the row promised a tool the harness has none of. It is quoted rather than summarised:
/// the parent reading this `spawn` result is the one who can change the row, and "a worktree, a
/// configuration document, or the harness `--version` probe" told it nothing it could act on. The
/// journal's `SpawnAborted` beside the intent carries the same sentence (`run::AbortOnDrop`).
///
/// [`launch_failed_reason`] supplies the fallback for a thread that filed no reason.
fn spawn_failed_before_the_process_existed(agent_id: &AgentId, why: Option<&str>) -> RpcError {
    RpcError::of(
        FailureKind::Internal,
        Some(&agent_id.0),
        format!(
            "node `{}` was journaled and its launch failed before any process existed: {}. Its \
             `SpawnIntent` is resolved by a `SpawnAborted` beside it, which after §11 item 28 step \
             1 is evidence that **no process exists**, not merely consistent with it (§7.2). A \
             worktree may be left behind; a process is not.",
            agent_id.0,
            launch_failed_reason(why)
        ),
        "§7.2",
    )
}

/// The sentence a launch failure is reported with: the thread's own, or an honest statement that
/// it filed none — which is itself the fault to report, never a reason invented in its place.
fn launch_failed_reason(why: Option<&str>) -> String {
    match why {
        Some(w) => w.to_string(),
        None => "marion's own thread for it filed no reason, which is itself the fault to report"
            .to_string(),
    }
}

/// [`spawn_failed_before_the_process_existed`], for a **root**.
///
/// The same shape, kept separate because the two have different evidence to offer and different
/// readers. A root's caller is a **person at `marion run`**, and the failures they hit are
/// marion's own refusals — an unrecordable working tree, a `<state>` inside the repository, a
/// harness with no root surface — each of which already names the directory and the remedy.
/// Dropping that on the floor and answering "the launch failed" is a refusal an operator cannot
/// act on.
fn root_launch_failed(agent_id: &AgentId, why: Option<&str>) -> RpcError {
    let why = launch_failed_reason(why);
    RpcError::of(
        FailureKind::Internal,
        Some(&agent_id.0),
        format!(
            "root `{}` was journaled and its launch failed before any process existed: {why}",
            agent_id.0
        ),
        "§7.2",
    )
}

/// **Never a verdict on the node**, which is why this is separate from the failure above.
///
/// marion has stopped waiting; the node has not stopped launching. The thread runs on, the table
/// keeps the entry, and the node's own wall clock still bounds it. Saying "the spawn failed" here
/// would be the false-receipt shape this codebase keeps deleting — in the direction that leaves a
/// live process a caller believes is dead.
fn launch_bound_expired(agent_id: Option<&AgentId>) -> RpcError {
    let subject = match agent_id {
        Some(id) => format!("node `{}`", id.0),
        None => "this spawn".to_string(),
    };
    RpcError::of(
        FailureKind::Internal,
        agent_id.map(|i| i.0.as_str()),
        format!(
            "marion started {subject} and did not observe a process within {}s, so it stopped \
             holding this call. **This is not a statement that the spawn failed**: the node's \
             thread is still running, the supervisor still owns it, and its own wall clock still \
             bounds it. Watch it through `tree/subscribe` and `node/attach`; a `SpawnAborted` or a \
             `Spawned` will say which way it went.",
            LAUNCH_BOUND.as_secs()
        ),
        "§6.1",
    )
}

/// One bounded wait on a launch thread's progress channel: whatever time is left of `deadline`, so
/// the two waits a launch makes share one `LAUNCH_BOUND` rather than each getting their own.
fn recv_progress(
    progress: &std::sync::mpsc::Receiver<Progress>,
    deadline: std::time::Instant,
) -> Result<Progress, std::sync::mpsc::RecvTimeoutError> {
    progress.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
}

/// A launch's first wait: the node's id, or the refusal that it never had one.
///
/// Anything but `Identified` here means the launch refused before it minted an id — there is no
/// node and nothing to report on — or the bound expired first. Shared by the child and root paths
/// because the two answers are the same sentence on both.
fn await_identified(
    progress: &std::sync::mpsc::Receiver<Progress>,
    deadline: std::time::Instant,
) -> Result<AgentId, RpcError> {
    match recv_progress(progress, deadline) {
        Ok(Progress::Identified(id)) => Ok(id),
        Ok(_) => Err(spawn_refused_before_the_node_existed()),
        Err(_) => Err(launch_bound_expired(None)),
    }
}

trait QuitRuntime: Send + Sync {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool;
}

struct SystemQuitRuntime;

impl QuitRuntime for SystemQuitRuntime {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
        crate::kill::kill_process_tree_and_wait(pid)
    }
}

/// A [`Handle`](crate::serve::Handle) backed by a running registry.
///
/// Descriptions still come only from the journal. Quit is the deliberately different half: it
/// uses the journal's PID to apply §6.7's process-tree kill, observes death, and writes that new
/// fact back before any client can learn it. The handle remembers only connection/exit bookkeeping,
/// never a second copy of node state.
pub struct RegistryHandle {
    /// This handle, as its own nodes' threads hold it. A node's thread outlives the `Handle::call`
    /// that started it, so it cannot borrow; `Weak` rather than `Arc` because the alternative is a
    /// cycle that never drops.
    me: std::sync::Weak<RegistryHandle>,
    live: Arc<LiveRegistry>,
    shared: Mutex<Shared>,
    runtime: Arc<dyn QuitRuntime>,
    /// **The nodes this supervisor owns** — §11 item 28's whole point. See [`NodeHandle`].
    ///
    /// Separate from [`Self::shared`] rather than a field of it, because the two are held for
    /// different lengths of time and by different threads: `shared` is taken and released inside a
    /// single call, while this one is taken by a node's own thread at two moments spread across a
    /// launch. Folding them together would put a spawning node's `getpgid` inside the lock a
    /// client's `tree/subscribe` waits on.
    nodes: Mutex<HashMap<AgentId, NodeHandle>>,
    /// §3.4's display plane, per node. See [`Panes`] for why this is not in `shared`.
    panes: Mutex<Panes>,
    /// Completion-based monotonic time for the bounded pane replay cache. Cloned before invoking
    /// it so an injected clock never runs under the global Panes lock.
    pane_clock: Mutex<Arc<dyn Fn() -> std::time::Instant + Send + Sync>>,
    /// Serializes same-id replacement cleanup without holding the global pane registry lock.
    pane_replacement: Mutex<()>,
    #[cfg(test)]
    pane_listener_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pane_delivery_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pane_attach_selection_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// **What `agent/spawn` runs a node in**, or `None` for a supervisor that cannot spawn.
    ///
    /// **Production fills this.** `detach.rs`'s stage 3 builds [`RegistryHandle::owning`] from what
    /// its `Launch` carries: `project_dir` from `(state, project root)`, `bridge` from
    /// `current_exe`, and `base_url`/`auth` from argv — carried rather than re-read from the
    /// environment, because `main::auth_from_env` turns an absent key into `Canned`, and a
    /// supervisor that silently downgraded a live fleet to the canned endpoint would be the exact
    /// silent-degradation shape this codebase refuses.
    ///
    /// The repository is **not** here and cannot be: one supervisor serves a repository and all of
    /// its linked worktrees (§2 keys on the git common dir), while a worktree is made from a
    /// specific tree's HEAD. It is per-spawn — [`crate::run::SpawnRequest::repo`], resolved from
    /// the caller's own [`NodeHandle::repo`].
    ///
    /// So `None` now means only *"this handle was built by [`RegistryHandle::new`]"* — a describing
    /// handle, which is what `restart.rs`'s and the unit tests' fixtures want and what any future
    /// read-only surface would want. A handle in that state refuses `agent/spawn` rather than
    /// failing somewhere further in, and the refusal says which constructor was used.
    spawn_env: Option<crate::run::Env>,
    /// **One spawn decision at a time**, held from the gate evaluation until the child's
    /// `SpawnIntent` is durable — and released before the worktree, the compile and the launch.
    ///
    /// §6.1 step 2's concurrency gate reads a count off the registry, and the registry is a
    /// *follower*: it learns of a node when it reads the journal, not when the journal is written.
    /// Two `agent/spawn` calls for one caller that both evaluated the gate before either wrote its
    /// intent would both pass a bound only one of them fits under. This is the same shape
    /// [`Self::quit`] uses and for the same reason — it makes a read and the write derived from it
    /// one indivisible decision, not a throughput device.
    spawn_decision: Mutex<()>,
    quit: Mutex<()>,
    /// An explicit `session/quit` arrived and left nothing in §5.7's exclusion list holding.
    ///
    /// **Not** what makes exit permissible — [`RegistryHandle::idle_exit_eligible`] answers that
    /// from §5.7's own two clauses and nothing else, because a supervisor whose client was
    /// SIGKILLed is in exactly the state §5.7 permits an exit from and has no way to say so
    /// (§7.3.1). What this flag decides is only the *grace*: §5.7 justifies the wait as one that
    /// *"should outlast an operator closing one window to open another"*, and a client that called
    /// `session/quit` has said the opposite in as many words. So a departure marion cannot read
    /// waits the full grace, and a decision marion was told about does not.
    quit_waived_grace: AtomicBool,
    /// Whether the log already carries the reason [`crate::registry::Status::Stopped`] was reached.
    /// The predicate is asked on every pass of the accept loop; the fault is reported once.
    stopped_reported: AtomicBool,
    exiting: AtomicBool,
}

/// Assemble the root domain input from the validated socket request and supervisor environment.
///
/// Pure by construction: it clones values already in memory and performs no adapter lookup,
/// compilation, argv synthesis, filesystem access, PTY allocation, or launch. Production uses
/// this same seam; while the native transport gate is closed, its focused test proves the opaque
/// context is nevertheless threaded through unchanged for the slice that will eventually open it.
/// **The working tree a resumed root runs in**, from the project root this supervisor serves.
///
/// A resume has no `repo` from a client — §8 rebuilds the launch from the node's journal, and the
/// journal keys on the git common dir (§2), not the working tree. A harness resumes a session only
/// from the **same cwd** it was created in (claude silently starts fresh otherwise), so the cwd
/// matters and must be the working tree, not the `.git` directory the socket keys on. For a
/// standard repository the working tree is the parent of `.git`, which is what this recovers.
///
/// **The known limit, stated rather than hidden:** a linked worktree's common dir is not its
/// working tree's parent, so a root started in one is not yet resumable — its cwd would need
/// recording. Every root a person starts with `marion run` in a plain checkout is, which is M2's
/// slice. A `node/resume` of a worktree root reaches the harness with the wrong cwd and the harness
/// refuses or starts fresh; until the cwd is journaled, that is the honest boundary.
fn resumable_root_cwd(project_root: &std::path::Path) -> PathBuf {
    match project_root.file_name() {
        Some(name) if name == ".git" => project_root
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| project_root.to_path_buf()),
        _ => project_root.to_path_buf(),
    }
}

fn root_spec_from_spawn(
    p: &marion_core::proto::params::AgentSpawnParams,
    repo: PathBuf,
    env: &crate::run::Env,
    agent_type: &marion_core::agent_type::AgentType,
) -> crate::root::RootSpec {
    crate::root::RootSpec {
        agent_type: p.agent_type.clone(),
        prompt: p.prompt.clone(),
        native_launch: p.native_launch.as_deref().cloned(),
        repo,
        state: env.state.clone(),
        base_url: env.base_url.clone(),
        bridge: env.bridge.clone(),
        // §3.1's precedence, the same one a child's spawn gets: the stated model, else the agent
        // type's own `model` key.
        model: p.model.clone().or_else(|| agent_type.model.clone()),
        // Absent is `false` — marion looks. See `AgentSpawnParams::no_change_record`.
        no_change_record: p.no_change_record.unwrap_or(false),
        auth: env.auth,
        // Absent is `false` — a node gets a pane because a run asked for one. See
        // `AgentSpawnParams::pane`.
        pane: p.pane.unwrap_or(false),
        // A client `agent/spawn` is always a fresh run: resume is `node/resume`'s path, which
        // builds its own `RootSpec` from the node's journal.
        resume: None,
        // §9's node-level bound, resolved from what the client stated and the agent type — by the
        // same function `marion run` used to call in-process, so the number an operator typed and
        // the number the node runs under are one resolution. It rides on the spec because
        // `root::prepare` journals it onto the node's `SpawnIntent`, and `launch_root` enforces
        // the same value: one clock, enforced and reported.
        bound_secs: crate::root::blocked_bound_secs(p.timeout_secs, agent_type.timeout.0.as_secs()),
    }
}

/// What one pane-directed [`marion_core::proto::Input`] asks of its pane, once the variant that
/// carries it has been read off the frame.
#[derive(Clone, Copy)]
enum PaneDelivery<'a> {
    LegacyWrite(&'a [u8]),
    OpaqueWrite(&'a [u8]),
    Resize { cols: u16, rows: u16 },
}

impl RegistryHandle {
    /// A handle that describes nodes and does not own any. See [`Self::spawn_env`].
    pub fn new(live: Arc<LiveRegistry>) -> Arc<RegistryHandle> {
        Self::build(live, Arc::new(SystemQuitRuntime), None)
    }

    /// A handle that **owns the nodes it spawns**: §2's `agent/spawn`, answered rather than refused.
    ///
    /// The environment is passed in rather than derived because it cannot be derived — see
    /// [`Self::spawn_env`].
    pub fn owning(live: Arc<LiveRegistry>, env: crate::run::Env) -> Arc<RegistryHandle> {
        Self::build(live, Arc::new(SystemQuitRuntime), Some(env))
    }

    fn build(
        live: Arc<LiveRegistry>,
        runtime: Arc<dyn QuitRuntime>,
        spawn_env: Option<crate::run::Env>,
    ) -> Arc<RegistryHandle> {
        // `new_cyclic` rather than a `Mutex<Option<Weak<_>>>` filled in afterwards: a node's thread
        // outlives the call that started it and has to hold the handle it reports to, so the
        // reference is a property of the value and not a step a construction site could forget.
        Arc::new_cyclic(|me| RegistryHandle {
            me: me.clone(),
            live,
            shared: Mutex::new(Shared::default()),
            runtime,
            nodes: Mutex::new(HashMap::new()),
            spawn_env,
            panes: Mutex::new(Panes::default()),
            pane_clock: Mutex::new(Arc::new(std::time::Instant::now)),
            pane_replacement: Mutex::new(()),
            #[cfg(test)]
            pane_listener_hook: Mutex::new(None),
            #[cfg(test)]
            pane_delivery_hook: Mutex::new(None),
            #[cfg(test)]
            pane_attach_selection_hook: Mutex::new(None),
            spawn_decision: Mutex::new(()),
            quit: Mutex::new(()),
            quit_waived_grace: AtomicBool::new(false),
            stopped_reported: AtomicBool::new(false),
            exiting: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    fn with_runtime(live: Arc<LiveRegistry>, runtime: Arc<dyn QuitRuntime>) -> Arc<RegistryHandle> {
        Self::build(live, runtime, None)
    }

    fn pane_now(&self) -> std::time::Instant {
        let clock = Arc::clone(&lock(&self.pane_clock));
        clock()
    }

    fn retire_pane_hosts(hosts: Vec<Arc<crate::pty::PtyHost>>) {
        for host in hosts {
            host.clear_legacy_listeners();
            host.invalidate_pane_streams();
        }
    }

    fn prune_completed_panes(&self) {
        let now = self.pane_now();
        let victims = lock(&self.panes).take_completed_victims(now);
        Self::retire_pane_hosts(victims);
    }

    #[cfg(test)]
    fn set_pane_clock_for_test(&self, clock: Arc<dyn Fn() -> std::time::Instant + Send + Sync>) {
        *lock(&self.pane_clock) = clock;
    }

    #[cfg(test)]
    fn completed_usage_for_test(&self) -> (usize, usize) {
        let panes = lock(&self.panes);
        (panes.completed_count, panes.completed_bytes)
    }

    #[cfg(test)]
    fn next_completed_order_for_test(&self) -> u64 {
        lock(&self.panes).next_completed_order
    }

    #[cfg(test)]
    fn completed_scan_count_for_test(&self) -> usize {
        lock(&self.panes).completed_scan_count
    }

    #[cfg(test)]
    fn reset_completed_scan_count_for_test(&self) {
        lock(&self.panes).completed_scan_count = 0;
    }

    #[cfg(test)]
    fn set_completed_limit_for_test(&self, limit: usize) {
        lock(&self.panes).completed_limit = Some(limit);
    }

    /// How many nodes the journal knows about that marion could not describe to a client.
    ///
    /// See the module doc: `TreeSubscribeResult` has no field for this, so rather than dropping the
    /// nodes into silence the supervisor counts them here. It is the honest half of an admitted gap,
    /// not a substitute for closing it.
    pub fn unprojectable(&self) -> usize {
        lock(&self.shared).unprojectable
    }

    pub fn subscribers(&self) -> usize {
        lock(&self.shared).subs.len()
    }

    /// Push everything that changed since the last flush to every subscriber.
    ///
    /// Returns how many notifications were produced — **per event, not per delivery** — so a caller
    /// can tell "nothing changed" from "nothing was delivered", which are the same zero from the
    /// socket's side and different problems.
    pub fn flush(&self) -> usize {
        // **Before the shared lock, never inside it.** See [`Panes`]: the two are separate maps
        // precisely so an operator's keystroke is not written under the lock every `tree/subscribe`
        // waits on, and taking them in the other order here would rebuild that coupling.
        let panes = self.pane_ids();
        let mut g = lock(&self.shared);
        let events = self.live.read(|r| collect(r, &mut g, &panes));
        deliver(&mut g, &events);
        events.len()
    }

    /// The nodes this supervisor holds a pty for, as a snapshot.
    ///
    /// Cloned out under the panes lock and read afterwards, so no caller holds two locks at once.
    /// The window that opens — a pane created between this read and the projection — resolves at
    /// the next flush, and the alternative is the lock coupling [`Panes`] exists to avoid.
    fn pane_ids(&self) -> HashSet<AgentId> {
        lock(&self.panes)
            .hosts
            .iter()
            .filter_map(|(id, entry)| entry.is_live().then_some(id.clone()))
            .collect()
    }

    /// §2's `tree/subscribe`: the snapshot, and the point live notifications begin from.
    fn subscribe(&self, out: &Outbound) -> TreeSubscribeResult {
        let panes = self.pane_ids();
        let mut g = lock(&self.shared);
        // **One read, three uses.** See the module doc: catching up existing subscribers, building
        // this one's snapshot, and recording what it has been told all happen against the same view,
        // under one lock, so there is no instant at which a notification could slip between the
        // snapshot and the subscription.
        let (events, nodes, read_point) = self.live.read(|r| {
            let events = collect(r, &mut g, &panes);
            let nodes = project(r.tree(), &mut g, &panes);
            (events, nodes, r.read_point())
        });
        deliver(&mut g, &events);
        g.subs.push(out.clone());
        TreeSubscribeResult { nodes, read_point }
    }

    fn node_get(&self, id: &AgentId) -> Result<NodeGetResult, RpcError> {
        let pane = lock(&self.panes).has_live(id);
        self.live.read(|r| match r.tree().get(id) {
            None => Err(RpcError::not_found(
                &id.0,
                format!(
                    "this project's journal records no node `{}`. The registry is current as of \
                     {} records read; a node spawned by another process appears here once its \
                     `SpawnIntent` is on disk (§6.1 step 7).",
                    id.0,
                    r.read_point().records
                ),
                "§3.2",
            )),
            Some(node) => summarize(node, pane)
                .map(|node| NodeGetResult { node })
                .map_err(|e| e.as_error(id)),
        })
    }

    /// §2's `node/attach` — §7.3.3's re-attach, **both legs and no seam between them**.
    ///
    /// `events.rs` argues the shape and this is where it is spent: replay and subscribe are one
    /// [`EventReader`] cursor over one file, so *"replay to the journal's own read point, then
    /// subscribe from there"* is not a procedure anybody has to implement correctly — the reader
    /// returns the intact prefix and its own byte offset in a single read, this method sends that
    /// prefix, and every later poll continues from that same offset. **A gap or a duplicate at the
    /// join is unreachable because there is no join.**
    ///
    /// So this needs none of the locking [`Self::subscribe`] needs, and that difference is
    /// structural rather than an omission. `tree/subscribe` derives *two* things — a snapshot and
    /// the told-set that decides what the next notification will be — and must derive them from one
    /// view or a notification can slip between them. An attach derives one: the reader's own read.
    ///
    /// **The replayed events go out as `node/event` notifications, before the response frame.**
    /// [`marion_core::proto::result::NodeAttachResult`] has no field for them and deliberately so: a
    /// replayed event and a live one are the same event from the same file, and giving the replay a
    /// second shape on the wire would ask every client to write the splice this module exists to
    /// make impossible. Because they precede the response, the [`ReplayPoint`] in the answer is a
    /// statement in the past tense — *everything up to here has already been sent to you* — which
    /// is stronger than a promise about what will arrive.
    ///
    /// **The cursor is kept even for a node that has already exited**, though the mode says the
    /// node is finished. The two are not in tension: the mode describes the *node*, the cursor
    /// describes this reader. A node marion has just journaled as `Exited` may still have its
    /// `Lifecycle::Exited` bookend in flight to its own `events.jsonl` — different files, different
    /// writers, no ordering between them — and dropping the cursor on the strength of the journal
    /// would lose precisely the record that separates a node that finished from one cut mid-turn.
    /// A cursor on a file nobody will append to costs one `stat` per tick and delivers nothing.
    fn node_attach(
        &self,
        id: &AgentId,
        pane_stream_v1: bool,
        out: &Outbound,
    ) -> Result<NodeAttachResult, RpcError> {
        let (mut summary, state, reap_state, project) = self.live.read(|r| {
            let Some(node) = r.tree().get(id) else {
                return Err(RpcError::not_found(
                    &id.0,
                    format!(
                        "this project's journal records no node `{}`, so there is no stream to \
                         attach to. The registry is current as of {} records read; a node spawned \
                         by another process appears here once its `SpawnIntent` is on disk \
                         (§6.1 step 7).",
                        id.0,
                        r.read_point().records
                    ),
                    "§3.2",
                ));
            };
            let summary = summarize(node, false).map_err(|e| e.as_error(id))?;
            Ok((summary, node.state, node.reap_state, r.project()))
        })?;

        let Some(project) = project else {
            return Err(RpcError::internal(format!(
                "the supervisor cannot work out which project directory node `{}`'s stream lives \
                 under: its registry was booted over a journal path with no `<state>/<hash>/` \
                 above it. This is a misconfigured supervisor, not a missing node.",
                id.0
            )));
        };
        let events_path = project.agent(id).events();
        let (reader, replayed) = crate::events::EventReader::open_path(&events_path)
            .map_err(|e| attach_io_failure(id, &events_path, &e))?;

        // **"Nobody recorded this" is not "it said nothing", and a terminal node is where the two
        // stop being distinguishable by waiting.** `EventReader::ever_written` is the split, and
        // §4.1's whole vocabulary exists so marion does not report an unrecorded node as an empty
        // transcript. For a node that is still running the answer is to attach anyway — the file
        // appears on its first frame and the reader is already watching for it. For one that has
        // exited, no frame is coming, and `ReplayOnly(records: 0)` would be marion asserting it
        // observed silence it never observed.
        if !reader.ever_written() && state.is_exited() {
            return Err(RpcError::of(
                FailureKind::NotFound,
                Some(&id.0),
                format!(
                    "node `{}` has exited and marion has no `{}` for it, so there is nothing to \
                     replay and nothing more will be written. This is **not** an empty transcript: \
                     it is a node whose stream was never recorded — the recorder could not open the \
                     file, or this node ran under a build that did not record one (§4.1). Reporting \
                     it as a node that said nothing would be a claim marion cannot make.",
                    id.0,
                    events_path.display()
                ),
                "§7.3.3",
            ));
        }

        let point = reader.read_point();
        let mode = attach_mode(state, reap_state, point);
        // A versioned pane reservation is the first side effect. If its bounded cursor, entropy,
        // or retained generation is unavailable, no NodeEvent, cursor, listener, or lease has
        // moved. It must never silently downgrade to the lossy legacy stream.
        let (mut reserved_pane, reserved_host) = if pane_stream_v1 {
            self.attach_pane_v1(id, out)?
        } else {
            (None, None)
        };
        // Sent before the reader is parked, so nothing appended between the two can be delivered
        // ahead of the replay it comes after.
        let live = deliver_events(out, id, &replayed);
        if !live {
            if let Some(host) = reserved_host {
                self.rollback_pane_attach(id, out.conn(), &host);
            }
            return Err(RpcError::internal(format!(
                "node `{}`'s attach connection closed while its durable event prefix was queued; \
                 the pane reservation and write lease were rolled back",
                id.0
            )));
        }
        #[cfg(test)]
        if let Some(hook) = lock(&self.pane_attach_selection_hook).take() {
            hook();
        }
        if pane_stream_v1 {
            self.commit_pane_v1_reservation(id, out, &mut reserved_pane, reserved_host.as_ref())?;
        }
        lock(&self.shared).attached.push(Attachment {
            conn: out.conn(),
            agent_id: id.clone(),
            reader,
            out: out.clone(),
        });
        // Legacy keeps its historical NodeEvent-before-NodePty ordering. V1 already reserved a
        // paced cursor, but sends no pane frame until the response has advertised its exact token.
        let pane = if pane_stream_v1 {
            reserved_pane
        } else {
            self.attach_pane(id, out)
        };
        summary.pane = pane.is_some();
        Ok(NodeAttachResult {
            node: summary,
            mode,
            pane,
        })
    }

    /// **One look at the pane this attach reserved**, under the registry lock: is the
    /// retained generation still the current one, and is this connection its writer?
    fn pane_reservation_snapshot(
        &self,
        id: &AgentId,
        conn: ConnId,
        host: &Arc<crate::pty::PtyHost>,
        descriptor: &marion_core::proto::result::PaneReadyDescriptorV1,
    ) -> (bool, PaneWriteHalf) {
        let panes = lock(&self.panes);
        let (same_generation, write_half) = match panes.hosts.get(id) {
            Some(PaneEntry::Live(current)) if Arc::ptr_eq(current, host) => {
                let writable = panes.lease(conn, id).is_some();
                (
                    true,
                    if writable {
                        PaneWriteHalf::Held
                    } else {
                        PaneWriteHalf::Elsewhere(host.writer().map(|owner| owner.0))
                    },
                )
            }
            Some(PaneEntry::Closing(current)) if Arc::ptr_eq(current, host) => {
                (true, PaneWriteHalf::Ended)
            }
            Some(PaneEntry::Completed { host: current, .. }) if Arc::ptr_eq(current, host) => {
                (true, PaneWriteHalf::Ended)
            }
            _ => (false, PaneWriteHalf::Ended),
        };
        // Panes remains held through exact Pending validation. Closing therefore
        // linearizes wholly before this snapshot (read-only) or wholly after the attach
        // commit; it cannot revoke the lease between metadata and token validation.
        let exact_pending =
            same_generation && host.pane_replay_reserved(conn, &descriptor.token, descriptor.cut);
        (exact_pending, write_half)
    }

    /// **The versioned reservation's commit point**: the pane a v1 attach reserved is
    /// still the current generation and still exactly Pending, or the reservation is
    /// rolled back and the attach refused as a conflict.
    fn commit_pane_v1_reservation(
        &self,
        id: &AgentId,
        out: &Outbound,
        reserved_pane: &mut Option<marion_core::proto::result::PaneAttach>,
        reserved_host: Option<&Arc<crate::pty::PtyHost>>,
    ) -> Result<(), RpcError> {
        if let (Some(pane), Some(host)) = (reserved_pane.as_mut(), reserved_host) {
            let descriptor = pane
                .pane_ready
                .as_ref()
                .expect("a versioned reservation carries its Ready descriptor");
            let (valid_reservation, write_half) =
                self.pane_reservation_snapshot(id, out.conn(), host, descriptor);
            if !valid_reservation {
                self.rollback_pane_attach(id, out.conn(), host);
                return Err(RpcError::conflict(
                    &id.0,
                    format!(
                        "node `{}`'s pane generation changed while attach was being prepared; \
                         retry against the current generation",
                        id.0
                    ),
                    "§5.3",
                ));
            }
            write_half.describe(pane);
        }
        Ok(())
    }

    /// Claim the write half for `conn` on a **live** host, or say who has it. Re-attaching from
    /// the connection that already holds it answers `Held` with the lease it already had rather
    /// than issuing a second one: two live leases for one connection would each clear the slot on
    /// drop, and the first drop would silently open the node to a third client.
    fn take_write_half(
        panes: &mut Panes,
        host: &Arc<crate::pty::PtyHost>,
        id: &AgentId,
        conn: ConnId,
    ) -> PaneWriteHalf {
        if panes.lease(conn, id).is_some() {
            return PaneWriteHalf::Held;
        }
        match host.lease_writer(conn) {
            Ok(lease) => {
                panes
                    .leases
                    .entry(conn)
                    .or_default()
                    .push((id.clone(), Arc::new(lease)));
                PaneWriteHalf::Held
            }
            Err(crate::pty::WriterBusy::HeldBy(owner)) => PaneWriteHalf::Elsewhere(Some(owner.0)),
        }
    }

    /// The **display plane's** half of `node/attach` (§5.3), or `None` for a node with no pty.
    ///
    /// Three things happen here and they are deliberately one step, in this order:
    ///
    /// 1. **Listen first.** Registering the byte fan-out before claiming the keyboard means the
    ///    client that *is* refused the write half still sees the node — a read-only attach is the
    ///    point of the refusal, not a consolation for it. Doing it the other way round would leave
    ///    a window in which this client could type and not yet see the echo.
    /// 2. **Claim the write half, once.** `lease_writer` refuses the second caller by name; the
    ///    lease is parked under this connection so it is released by `gone` dropping it, which is
    ///    what makes a crashed client's node writable again with no cleanup code anywhere.
    /// 3. **Report the pty's current size**, because it is not the client's: the master was sized
    ///    before the child existed, by a supervisor that could not know what terminal would
    ///    eventually attach. A client that rendered without asking would render somebody else's
    ///    geometry.
    ///
    /// **Re-attaching from the same connection does not re-lease.** `lease_writer` refuses a
    /// connection that already holds the slot rather than issuing a second lease, so the answer to
    /// a second `node/attach` from a writer is `writable: true` with the lease it already had —
    /// two live leases for one connection would each clear the slot on drop, and the first drop
    /// would silently open the node to a third client.
    fn attach_pane(
        &self,
        id: &AgentId,
        out: &Outbound,
    ) -> Option<marion_core::proto::result::PaneAttach> {
        self.prune_completed_panes();
        // Held through listener and lease commit. `forget_pane` cannot invalidate a host between
        // those two halves, and neither host operation performs a delivery callback while this
        // global registry lock is held.
        let mut panes = lock(&self.panes);
        let host = panes.hosts.get(id)?.live_host().cloned()?;
        host.unlisten(out.conn());
        host.listen(out.clone());
        #[cfg(test)]
        if let Some(hook) = lock(&self.pane_listener_hook).take() {
            hook();
        }
        // The size the master really has, not the size it was asked for: `PtyMaster::size` reads
        // `TIOCGWINSZ` back, so a client is told what the child will see rather than what marion
        // intended it to see.
        let size = host
            .master()
            .size()
            .unwrap_or_else(|_| host.master().intended_size());
        let native_reserved = panes
            .native_launches
            .as_ref()
            .is_some_and(|launches| launches.has_pending(id));
        // A pending native launch is a write half already promised to a claimant that has not
        // arrived: read-only, but the node is running, so it is `Elsewhere` and not `Ended`.
        let write_half = if native_reserved {
            PaneWriteHalf::Elsewhere(None)
        } else {
            Self::take_write_half(&mut panes, &host, id, out.conn())
        };
        let mut pane = marion_core::proto::result::PaneAttach {
            cols: size.cols,
            rows: size.rows,
            writable: false,
            held_by: None,
            ended: false,
            pane_ready: None,
        };
        write_half.describe(&mut pane);
        Some(pane)
    }

    fn attach_pane_v1(
        &self,
        id: &AgentId,
        out: &Outbound,
    ) -> Result<
        (
            Option<marion_core::proto::result::PaneAttach>,
            Option<Arc<crate::pty::PtyHost>>,
        ),
        RpcError,
    > {
        self.prune_completed_panes();
        let mut panes = lock(&self.panes);
        let Some(entry) = panes.hosts.get(id) else {
            return Ok((None, None));
        };
        let Some(host) = entry.replay_host().cloned() else {
            return Err(RpcError::refused(
                &id.0,
                format!(
                    "node `{}`'s pane generation is being replaced; retry attach",
                    id.0
                ),
                "§5.3",
            ));
        };
        let is_live = entry.is_live();
        let descriptor = host
            .reserve_pane_replay(out.conn(), out.clone())
            .map_err(|error| match error {
                crate::pty::PaneReplayReservationError::Busy => RpcError::refused(
                    &id.0,
                    format!(
                        "node `{}` could not reserve another bounded pane replay: {error}",
                        id.0
                    ),
                    "§5.3",
                ),
                crate::pty::PaneReplayReservationError::Unavailable(_)
                | crate::pty::PaneReplayReservationError::Entropy => RpcError::internal(format!(
                    "node `{}` could not create an exact pane replay: {error}",
                    id.0
                )),
            })?;
        let native_reserved = panes
            .native_launches
            .as_ref()
            .is_some_and(|launches| launches.has_pending(id));
        let write_half = if !is_live {
            PaneWriteHalf::Ended
        } else if native_reserved {
            PaneWriteHalf::Elsewhere(None)
        } else {
            Self::take_write_half(&mut panes, &host, id, out.conn())
        };
        let size = host
            .master()
            .size()
            .unwrap_or_else(|_| host.master().intended_size());
        let mut pane = marion_core::proto::result::PaneAttach {
            cols: size.cols,
            rows: size.rows,
            writable: false,
            held_by: None,
            ended: false,
            pane_ready: Some(descriptor),
        };
        write_half.describe(&mut pane);
        Ok((Some(pane), Some(host)))
    }

    fn rollback_pane_attach(&self, id: &AgentId, conn: ConnId, host: &Arc<crate::pty::PtyHost>) {
        {
            let mut panes = lock(&self.panes);
            if panes
                .hosts
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(entry.host(), host))
                && let Some(leases) = panes.leases.get_mut(&conn)
            {
                leases.retain(|(agent_id, _)| agent_id != id);
                if leases.is_empty() {
                    panes.leases.remove(&conn);
                }
            }
        }
        host.unlisten(conn);
    }

    /// Register a node's pty with this supervisor, so `node/attach` can find it.
    ///
    /// The one route in. A `PtyHost` that is never registered is a recording nobody can watch, and
    /// a pane registered for a node the journal does not know about is unattachable — `node_attach`
    /// resolves the node from the registry *before* it looks here, so the journal stays the
    /// authority on what exists.
    pub fn register_pane(&self, id: &AgentId, host: Arc<crate::pty::PtyHost>) {
        self.prune_completed_panes();
        let _replacement = lock(&self.pane_replacement);
        let replacement = Arc::clone(&host);
        let (old, revoked_leases) = {
            let mut panes = lock(&self.panes);
            if let Some(existing) = panes.hosts.get(id) {
                if Arc::ptr_eq(existing.host(), &replacement) {
                    // Live is idempotent; lifecycle states never move backwards through register.
                    return;
                }
            } else {
                panes.hosts.insert(id.clone(), PaneEntry::Live(host));
                let _ = panes.assign_host_generation(id);
                return;
            }
            panes.revoke_native_launch(id);
            let old = panes.replace(id.clone(), PaneEntry::Replacing(Arc::clone(&replacement)));
            let mut revoked = Vec::new();
            for leases in panes.leases.values_mut() {
                let mut kept = Vec::with_capacity(leases.len());
                for (agent_id, lease) in std::mem::take(leases) {
                    if &agent_id == id {
                        revoked.push(lease);
                    } else {
                        kept.push((agent_id, lease));
                    }
                }
                *leases = kept;
            }
            panes.leases.retain(|_, leases| !leases.is_empty());
            (old, revoked)
        };
        drop(revoked_leases);
        let old = old.expect("an existing generation was replaced above");
        let old_host = Arc::clone(old.host());
        old_host.clear_legacy_listeners();
        old_host.invalidate_pane_streams();
        drop(old);

        let published = {
            let mut panes = lock(&self.panes);
            let matching = panes.hosts.get(id).is_some_and(
                |entry| matches!(entry, PaneEntry::Replacing(current) if Arc::ptr_eq(current, &replacement)),
            );
            if matching {
                panes.replace(id.clone(), PaneEntry::Live(Arc::clone(&replacement)));
                let _ = panes.assign_host_generation(id);
            }
            matching
        };
        if !published {
            replacement.clear_legacy_listeners();
            replacement.invalidate_pane_streams();
        }
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn install_pending_native_launches(
        &self,
        launches: Arc<crate::native_bootstrap::PendingNativeLaunches>,
    ) -> Result<(), Arc<crate::native_bootstrap::PendingNativeLaunches>> {
        let mut panes = lock(&self.panes);
        if panes.native_launches.is_some() {
            return Err(launches);
        }
        panes.native_launches = Some(launches);
        Ok(())
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn reserve_pending_native_launch(
        self: &Arc<Self>,
        binding: crate::native_bootstrap::NativeLaunchBinding,
    ) -> Result<
        crate::native_bootstrap::PendingNativeLaunchReceipt,
        crate::native_bootstrap::NativeLaunchReservationError,
    > {
        let panes = lock(&self.panes);
        if panes.hosts.contains_key(binding.agent_id()) {
            return Err(crate::native_bootstrap::NativeLaunchReservationError::HostAlreadyVisible);
        }
        let launches = panes
            .native_launches
            .as_ref()
            .ok_or(crate::native_bootstrap::NativeLaunchReservationError::UnknownTicket)?;
        launches.reserve(binding)
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn publish_pending_native_launch(
        &self,
        receipt: &crate::native_bootstrap::NativeLaunchReceipt,
    ) -> Result<u64, crate::native_bootstrap::NativeLaunchReservationError> {
        let panes = lock(&self.panes);
        if !matches!(
            panes.hosts.get(receipt.agent_id()),
            Some(PaneEntry::Live(_))
        ) {
            return Err(crate::native_bootstrap::NativeLaunchReservationError::HostUnavailable);
        }
        let generation = panes
            .host_generations
            .get(receipt.agent_id())
            .copied()
            .ok_or(crate::native_bootstrap::NativeLaunchReservationError::HostUnavailable)?;
        let launches = panes
            .native_launches
            .as_ref()
            .ok_or(crate::native_bootstrap::NativeLaunchReservationError::UnknownTicket)?;
        launches.publish(receipt, generation)?;
        Ok(generation)
    }

    #[allow(
        dead_code,
        reason = "dark until an authenticated native claim transport activates"
    )]
    pub(crate) fn claim_pending_native_writer(
        &self,
        ticket: &crate::native_bootstrap::NativeLaunchTicket,
        agent_id: &AgentId,
        claimant: crate::native_bootstrap::NativeClaimant,
    ) -> Result<
        crate::native_bootstrap::NativeLaunchClaim,
        crate::native_bootstrap::NativeLaunchClaimError,
    > {
        // Dark internal seam only: `NativeClaimant` has no production issuer and no public wire
        // vocabulary carries a launch ticket. Production NativeBootstrapService remains disabled.
        let mut panes = lock(&self.panes);
        let host = panes
            .hosts
            .get(agent_id)
            .and_then(PaneEntry::live_host)
            .cloned()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)?;
        let host_generation = panes
            .host_generations
            .get(agent_id)
            .copied()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)?;
        let launches = panes
            .native_launches
            .as_ref()
            .cloned()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::UnknownTicket)?;
        // Panes stays held from exact ticket consumption through writer publication. An ordinary
        // attach therefore sees either the pending gate or the completed lease, never the seam.
        let conn = claimant.conn();
        let (claim, lease) =
            launches.claim_with(ticket, agent_id, host_generation, claimant, || {
                host.lease_writer(conn)
                    .map_err(|_| crate::native_bootstrap::NativeLaunchClaimError::WriterBusy)
            })?;
        panes
            .leases
            .entry(conn)
            .or_default()
            .push((agent_id.clone(), Arc::new(lease)));
        Ok(claim)
    }

    /// Validate a same-socket native claim and reserve its exact writer slot without consuming the
    /// ticket. The returned guard performs the final, non-I/O writer commit only after every other
    /// fallible relay resource is ready and before the service delivers the claim acknowledgement;
    /// post-acknowledgement relay start is therefore infallible.
    pub(crate) fn prepare_pending_native_writer(
        self: &Arc<Self>,
        ticket: &crate::native_bootstrap::NativeLaunchTicket,
        agent_id: &AgentId,
        claimant: crate::native_bootstrap::NativeClaimant,
    ) -> Result<PreparedNativeWriter, crate::native_bootstrap::NativeLaunchClaimError> {
        let panes = lock(&self.panes);
        let host = panes
            .hosts
            .get(agent_id)
            .and_then(PaneEntry::live_host)
            .cloned()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)?;
        let host_generation = panes
            .host_generations
            .get(agent_id)
            .copied()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)?;
        let authority = panes
            .native_launches
            .as_ref()
            .cloned()
            .ok_or(crate::native_bootstrap::NativeLaunchClaimError::UnknownTicket)?;
        let claim = authority.prepare_claim(ticket, agent_id, host_generation, claimant)?;
        let conn = claimant.conn();
        let lease = host
            .lease_writer(conn)
            .map_err(|_| crate::native_bootstrap::NativeLaunchClaimError::WriterBusy)?;
        Ok(PreparedNativeWriter {
            handle: Arc::clone(self),
            authority,
            claim: Some(claim),
            agent_id: agent_id.clone(),
            host,
            host_generation,
            conn,
            lease: Some(Arc::new(lease)),
        })
    }

    /// Stop admitting new live-pane operations while keeping the exact host registered through
    /// reader drain. A stale close callback cannot transition a replacement host with the same id.
    pub fn closing_pane(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        // Serialize with same-id publication. A replacement briefly occupies `Replacing(new)`
        // while the old generation's admitted legacy deliveries drain; a fast new child may exit
        // in that interval. Waiting here ensures its Closing transition happens after publication
        // instead of being discarded and later overwritten by Live.
        let _replacement = lock(&self.pane_replacement);
        let mut panes = lock(&self.panes);
        if !matches!(panes.hosts.get(id), Some(PaneEntry::Live(current)) if Arc::ptr_eq(current, host))
        {
            return;
        }
        panes.revoke_native_launch(id);
        panes
            .hosts
            .insert(id.clone(), PaneEntry::Closing(Arc::clone(host)));
        host.seal_controls();
        for leases in panes.leases.values_mut() {
            leases.retain(|(agent_id, _)| agent_id != id);
        }
    }

    /// Publish a drained host as read-only replay state. Only the matching closing generation can
    /// complete, so an old launcher cannot overwrite a newly registered pane with the same id.
    pub fn completed_pane(
        &self,
        id: &AgentId,
        host: &Arc<crate::pty::PtyHost>,
        charged_bytes: usize,
    ) {
        let now = self.pane_now();
        let (completed, victims) = {
            let mut panes = lock(&self.panes);
            let matching = matches!(panes.hosts.get(id), Some(PaneEntry::Closing(current)) if Arc::ptr_eq(current, host));
            if !matching {
                return;
            }
            let completed = panes.complete(id, host, charged_bytes, now);
            let mut victims = panes.take_completed_victims(now);
            if !completed && let Some(refused) = panes.remove(id) {
                victims.push(Arc::clone(refused.host()));
            }
            (completed, victims)
        };
        if completed {
            host.clear_legacy_listeners();
        }
        Self::retire_pane_hosts(victims);
    }

    /// Permanently invalidate one failed host generation without touching a replacement that now
    /// owns the same agent id.
    pub fn failed_pane(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        let removed = {
            let mut panes = lock(&self.panes);
            let matching = panes
                .hosts
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(entry.host(), host));
            if matching {
                panes.revoke_native_launch(id);
                for leases in panes.leases.values_mut() {
                    leases.retain(|(agent_id, _)| agent_id != id);
                }
                panes.remove(id)
            } else {
                None
            }
        };
        host.clear_legacy_listeners();
        host.invalidate_pane_streams();
        drop(removed);
    }

    /// Forget a node's pty. Called when the node ends: the host is dropped by its owner, and
    /// leaving a dangling entry here would answer a later `node/attach` with a pane onto a master
    /// that has already been closed.
    pub fn forget_pane(&self, id: &AgentId) {
        let host = {
            let mut panes = lock(&self.panes);
            panes.revoke_native_launch(id);
            let host = panes.remove(id).map(|entry| Arc::clone(entry.host()));
            for leases in panes.leases.values_mut() {
                leases.retain(|(a, _)| a != id);
            }
            host
        };
        if let Some(host) = host {
            host.clear_legacy_listeners();
            host.invalidate_pane_streams();
        }
    }

    /// How many nodes this supervisor holds a pty for. Diagnostic, and the assertion surface for
    /// `register_pane`/`forget_pane`.
    pub fn panes(&self) -> usize {
        lock(&self.panes).hosts.len()
    }

    /// §2's inbound notifications, delivered.
    ///
    /// **Both are gated on the write lease, including the resize**, and that is not over-caution.
    /// A resize is not a private view setting: it is `TIOCSWINSZ` on the one master, so a
    /// read-only attacher that could resize would reflow the pane of the operator who *is* typing,
    /// from under them, with no way for either to tell where it came from. §5.3 gives a node one
    /// writer; the geometry is part of what that means.
    ///
    /// Legacy refusals remain silent for compatibility: there is no response envelope, and the
    /// client was already told at `node/attach` that it may not type. Pane-v1 opaque input is
    /// stronger: it is accepted only from that connection's live negotiated slot, and any refusal
    /// visibly ends that exact socket before a terminal `End` can claim success.
    fn deliver_input(&self, conn: ConnId, input: &marion_core::proto::Input) {
        self.deliver_input_with_out(conn, input, None);
    }

    /// **The pane-v1 replay handshake**, completed by read-only viewers too.
    fn deliver_pane_ready(&self, conn: ConnId, ready: &marion_core::proto::NodePaneReadyV1) {
        // Read-only viewers complete this handshake too, so it is deliberately independent of
        // the keyboard lease. Registry expiry runs first: a token cannot revive a Completed
        // host at or beyond its exact cache deadline.
        self.prune_completed_panes();
        let host = {
            let panes = lock(&self.panes);
            panes
                .hosts
                .get(&ready.agent_id)
                .and_then(PaneEntry::replay_host)
                .cloned()
        };
        if let Some(host) = host {
            host.pane_ready(conn, &ready.token, ready.cut);
        }
    }
    /// **One opaque keystroke's admitted delivery**: selection and admission under the
    /// registry lock, master I/O after it, and the slot outbound failed on either refusal.
    fn deliver_opaque_input(
        &self,
        conn: ConnId,
        input: &marion_core::proto::Input,
        id: &AgentId,
        bytes: &[u8],
        wire_out: Option<&Outbound>,
    ) {
        // Selection, pane-v1 generation validation, and control admission are one registry
        // transaction. The admission owns the exact slot outbound; durability and master I/O
        // happen only after the global Panes lock is released.
        let selected = {
            let panes = lock(&self.panes);
            let lease = panes.lease(conn, id).ok_or_else(|| {
                "opaque pane input requires this connection's live write lease".to_string()
            });
            lease.and_then(|lease| {
                let host = panes
                    .hosts
                    .get(id)
                    .and_then(PaneEntry::live_host)
                    .cloned()
                    .ok_or_else(|| "opaque pane input requires a live pane".to_string())?;
                let admission = host
                    .admit_opaque_input(&lease, conn)
                    .map_err(|error| error.to_string())?;
                Ok((host, admission))
            })
        };
        let (host, admission) = match selected {
            Ok(selected) => selected,
            Err(error) => {
                if let Some(out) = wire_out {
                    out.fail(crate::serve::Departure::PaneInputFailed {
                        agent_id: id.0.clone(),
                        error: error.clone(),
                    });
                }
                eprintln!("marion: {} on node {}: {error}", input.method(), id.0);
                return;
            }
        };
        #[cfg(test)]
        if let Some(hook) = lock(&self.pane_delivery_hook).take() {
            hook();
        }
        if let Err(error) = host.write_opaque_input_admitted(admission, bytes) {
            // The admission fails its authoritative slot outbound before releasing the input
            // delivery barrier, including error and unwind paths.
            eprintln!("marion: {} on node {}: {error}", input.method(), id.0);
        }
    }
    /// **Which pane and what it is being asked to do**, read off the frame alone.
    fn classify_pane_delivery(input: &marion_core::proto::Input) -> (&AgentId, PaneDelivery<'_>) {
        match input {
            marion_core::proto::Input::NodePtyWrite {
                agent_id, bytes, ..
            } => (agent_id, PaneDelivery::LegacyWrite(bytes.as_bytes())),
            marion_core::proto::Input::NodePaneWrite(params) => (
                &params.agent_id,
                PaneDelivery::OpaqueWrite(params.bytes.as_bytes()),
            ),
            marion_core::proto::Input::NodeResize {
                agent_id,
                cols,
                rows,
            } => (
                agent_id,
                PaneDelivery::Resize {
                    cols: *cols,
                    rows: *rows,
                },
            ),
            marion_core::proto::Input::NodePaneReady(_) => unreachable!("handled above"),
        }
    }

    /// **A leased legacy write or resize**: the lease and host are taken out from under
    /// the registry lock in one look, and the lock is released before the write.
    fn deliver_leased_input(
        &self,
        conn: ConnId,
        input: &marion_core::proto::Input,
        id: &AgentId,
        delivery: PaneDelivery<'_>,
    ) {
        // Both taken out from under the lock in one look, and the lock released before the write:
        // see `Panes::leases`. A harness that has stopped reading its stdin must stall one attach,
        // never the supervisor.
        let (host, lease) = {
            let panes = lock(&self.panes);
            let Some(lease) = panes.lease(conn, id) else {
                return;
            };
            match panes.hosts.get(id).and_then(PaneEntry::live_host) {
                Some(h) => (Arc::clone(h), lease),
                None => return,
            }
        };
        #[cfg(test)]
        if let Some(hook) = lock(&self.pane_delivery_hook).take() {
            hook();
        }
        let outcome = match delivery {
            PaneDelivery::LegacyWrite(bytes) => host.write_input(&lease, bytes),
            PaneDelivery::OpaqueWrite(_) => unreachable!("opaque input returned above"),
            PaneDelivery::Resize { cols, rows } => {
                host.resize(crate::pty::WinSize::new(cols, rows))
            }
        };
        if let Err(e) = outcome {
            eprintln!("marion: {} on node {}: {e}", input.method(), id.0);
        }
    }

    fn deliver_input_with_out(
        &self,
        conn: ConnId,
        input: &marion_core::proto::Input,
        wire_out: Option<&Outbound>,
    ) {
        if let marion_core::proto::Input::NodePaneReady(ready) = input {
            self.deliver_pane_ready(conn, ready);
            return;
        }
        let (id, delivery) = Self::classify_pane_delivery(input);
        if let PaneDelivery::OpaqueWrite(bytes) = delivery {
            self.deliver_opaque_input(conn, input, id, bytes, wire_out);
            return;
        }
        self.deliver_leased_input(conn, input, id, delivery);
    }

    /// How many node streams this supervisor is following on behalf of a client.
    pub fn attachments(&self) -> usize {
        lock(&self.shared).attached.len()
    }

    /// **The subscribe leg's engine**: advance every attached cursor and push what it found.
    ///
    /// Returns how many events were read — per event, not per delivery, for the reason
    /// [`Self::flush`] gives.
    ///
    /// One `open` and one `stat` per attachment per call, which is [`EventReader::poll`]'s stated
    /// cost when there is nothing new, and nothing new is the common case. §11 item 27 is the entry
    /// that makes this cheaper; nothing here depends on it landing.
    pub fn pump_attached(&self) -> usize {
        let mut g = lock(&self.shared);
        let mut read = 0usize;
        g.attached.retain_mut(|a| {
            let mut fresh = Vec::new();
            read += a.reader.poll(&mut fresh);
            deliver_events(&a.out, &a.agent_id, &fresh)
        });
        read
    }

    // ----------------------------------------------------------------------------------------
    // §2's `agent/spawn`, and the node table it fills.
    // ----------------------------------------------------------------------------------------

    /// **Take ownership of a node the instant it has an identity**, and mint it §5.4's capability.
    ///
    /// Called from [`crate::run::SpawnObserver::identified`], which fires with the `SpawnIntent`
    /// already durable and no side effect yet taken. The token returned is written into the node's
    /// MCP declaration a few lines later, so this is the last moment it can be decided.
    ///
    /// `repo` is the tree this node lives in, remembered so this node's own children can branch
    /// from it — see [`NodeHandle::repo`]. It is the caller's `repo`, inherited rather than
    /// re-derived, because "which tree" is a fact about the subtree and not about the spawn.
    ///
    /// `pub(crate)` rather than private because it is also the whole of what a test needs to put a
    /// node in this table — and a test that reached in through a back door would be asserting
    /// against a binding production does not make.
    pub(crate) fn claim(
        &self,
        agent_id: &AgentId,
        task_id: Option<TaskId>,
        repo: PathBuf,
    ) -> String {
        let token = mint_token();
        lock(&self.nodes).insert(
            agent_id.clone(),
            NodeHandle {
                task_id,
                token: token.clone(),
                repo,
                pid: None,
                pgid: None,
                started_at: None,
                join: None,
                outcome: None,
            },
        );
        token
    }

    /// **A native root this supervisor claimed has reached its end**, and its table entry says so.
    ///
    /// A managed node's entry is finished by the thread that ran it ([`Self::mark_finished`] from
    /// `run_spawn`'s return). A native node has no such thread — the supervisor owns its pane, not
    /// its turn — so its recorder closes the entry instead, at the same two moments the journal
    /// gets a terminal record. Without this, [`Self::idle_exit_eligible`]'s second guard would read
    /// a native session that ended hours ago as a running node and no supervisor with a native
    /// launch in its history could ever leave.
    ///
    /// [`NodeOutcome::Root`] because a native node **is** a root: its result is its stream and its
    /// exit, both on disk (§9).
    pub(crate) fn finished_native(&self, agent_id: &AgentId, outcome: Result<(), String>) {
        self.mark_finished(agent_id, NodeOutcome::Root(outcome));
    }

    /// A process exists for a node this supervisor owns. Called from
    /// [`crate::run::SpawnObserver::started`], immediately after `Spawned { pid: Some(_) }` is
    /// journaled.
    fn mark_started(&self, agent_id: &AgentId, pid: i32) {
        // SAFETY: reads the process group of a pid this process just created; cannot fail other
        // than by returning -1, which is recorded as "not known" rather than as a group id.
        let pgid = unsafe { getpgid(pid) };
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.pid = Some(pid);
            node.pgid = (pgid > 0).then_some(pgid);
            node.started_at = Some(std::time::SystemTime::now());
        }
    }

    /// `run_spawn` returned. **The entry is kept, not removed**: it is what tells a later caller
    /// "that node finished" from "no such node", and §5.7's exit predicate reads liveness off it
    /// rather than membership.
    fn mark_finished(&self, agent_id: &AgentId, outcome: NodeOutcome) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.outcome = Some(outcome);
        }
    }

    /// **Why a node this supervisor owns did not finish cleanly**, in one sentence.
    ///
    /// `None` for a node still running, a node that finished cleanly, and a node this supervisor
    /// does not own — three states this deliberately does not distinguish, because the question it
    /// answers is *"is there a fault to report about this node"* and the other three surfaces
    /// ([`Self::owned_running`], [`Self::owned_nodes`]) answer the rest. The sentence is marion's
    /// own for a root and the spawn path's for a child; neither is derived from an exit code.
    pub fn owned_failure(&self, agent_id: &AgentId) -> Option<String> {
        match lock(&self.nodes).get(agent_id)?.outcome.as_ref()? {
            NodeOutcome::Child(r) => r.as_ref().as_ref().err().map(|e| e.to_string()),
            NodeOutcome::Root(r) => r.as_ref().err().cloned(),
        }
    }

    /// How many nodes this supervisor owns, finished or not. For tests and a future `doctor`.
    pub fn owned_nodes(&self) -> usize {
        lock(&self.nodes).len()
    }

    /// How many of them are still running, by [`NodeHandle::running`]'s reading.
    pub fn running_nodes(&self) -> usize {
        lock(&self.nodes).values().filter(|n| n.running()).count()
    }

    /// The pid this supervisor recorded for a node it owns, if a process exists yet.
    pub fn owned_pid(&self, agent_id: &AgentId) -> Option<i32> {
        lock(&self.nodes).get(agent_id).and_then(|n| n.pid)
    }

    /// Whether one node this supervisor owns is still running. `None` if it owns no such node.
    pub fn owned_running(&self, agent_id: &AgentId) -> Option<bool> {
        lock(&self.nodes).get(agent_id).map(|n| n.running())
    }

    /// The node's own process group, as `getpgid(2)` reported it — §6.7's `killpg` target.
    pub fn owned_pgid(&self, agent_id: &AgentId) -> Option<i32> {
        lock(&self.nodes).get(agent_id).and_then(|n| n.pgid)
    }

    /// §9's contract this node runs under.
    /// **`None` for a node this supervisor does not own *and* for a root**, which has no contract
    /// at all (§9). The two are told apart by [`Self::owned_running`], which answers `None` only
    /// for the first.
    pub fn owned_task_id(&self, agent_id: &AgentId) -> Option<TaskId> {
        lock(&self.nodes)
            .get(agent_id)
            .and_then(|n| n.task_id.clone())
    }

    /// When the node's process came into existence.
    pub fn owned_started_at(&self, agent_id: &AgentId) -> Option<std::time::SystemTime> {
        lock(&self.nodes).get(agent_id).and_then(|n| n.started_at)
    }

    /// **Join every finished node's thread**, and answer whether any refused to be joined.
    ///
    /// §5.7's exit needs the *thread* joined and not merely its answer read — `background.rs` makes
    /// the same distinction for the same reason: a thread that has sent its outcome is still
    /// running its own epilogue (dropping the `Child`, removing the worktree, unwinding
    /// `AbortOnDrop`), and a process that exits underneath that leaves the epilogue undone.
    ///
    /// Only *finished* nodes, so this can never block: a handle whose `outcome` is set has had
    /// `run_spawn` return on that thread, so the join is a formality. A running node is not joined
    /// here because [`Handle::idle_exit_eligible`] has already refused to exit while one exists.
    fn join_finished_nodes(&self) {
        let joins: Vec<_> = lock(&self.nodes)
            .values_mut()
            .filter(|n| !n.running())
            .filter_map(|n| n.join.take())
            .collect();
        for j in joins {
            // A panicking node's thread is not this supervisor's failure to report at exit — the
            // panic already became this node's outcome, and so its terminal state, in `caught`.
            // That claim was false until `caught` existed: nothing constructed `SpawnError::Panicked`
            // anywhere, and a panicking thread left `outcome: None` for ever.
            let _ = j.join();
        }
    }

    /// **§6.1 step 2's third argument, read off the registry rather than off a caller's table.**
    ///
    /// `background.rs` used to argue the opposite and was right at the time: a bridge's table was
    /// authoritative *by construction*, because every child of a node went through that node's own
    /// bridge, while a journal read could not see a child between "thread started" and "process
    /// observed" — `Spawned` was written after the whole run. **Step 1 inverted the premise.**
    /// `SpawnIntent` is journaled before any side effect and `Spawned` at `command.spawn()`, so a
    /// node counted here exists from the first instant it exists at all, and the count survives a
    /// restart, which no in-process table does.
    ///
    /// Counted from the **intent**, not from `Spawned`: a child whose worktree is still being made
    /// occupies its parent's slot exactly as much as one already running, and counting the case it
    /// cannot rule out is `background.rs`'s own over-count-is-the-safe-direction rule.
    fn live_children_of(&self, parent: &AgentId) -> u32 {
        let n = self.live.read(|r| {
            r.tree()
                .nodes()
                .iter()
                .filter(|n| {
                    n.parent_id().as_ref() == Some(&parent)
                        && !n.state.is_exited()
                        && n.reap_state == ReapState::Live
                        && !Self::abandoned(n)
                })
                .count()
        });
        // Saturating for `Background::live_children`'s reason: a count that wrapped to 0 would
        // silently *open* the gate.
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// **Who is asking, checked rather than believed** — F2, and the whole reason `SpawnCaller`
    /// carries a token instead of a depth.
    ///
    /// `serve_conn` performs no peer-credential check: `Handle::call` receives a [`ConnId`] and
    /// nothing about the process on the other end. So an `agent/spawn` that took the caller's word
    /// for its own `depth` would let any process that can `connect(2)` assert `depth: 0` and spawn,
    /// and §6.1 step 2's gates — the ones `depth_gate.rs`'s seven tests exist for — would be
    /// advisory. Here the caller states only *which node it is*, proves it with the secret marion
    /// wrote into that node's own declaration, and every gated fact is read back out of the
    /// registry marion itself wrote.
    fn resolve_caller(
        &self,
        c: &marion_core::proto::SpawnCaller,
    ) -> Result<crate::run::Caller, RpcError> {
        // **The token is checked before anything else is said about the node**, so a caller who
        // guesses an `AgentId` learns nothing from the shape of the refusal beyond "no".
        // The caller's repository rides out with the answer: it is the tree whose
        // `.marion/agents.toml` defines the caller's own type, read below, outside the registry lock.
        let repo = {
            let nodes = lock(&self.nodes);
            // **A node this supervisor does not own is still compared**, against a decoy of the
            // same shape, so "no such node" and "wrong token" take the same path and cost the same.
            // Returning early on the absent case would turn the *existence* of a node into an
            // oracle a caller could probe with ids alone.
            let (stored, repo) = match nodes.get(&c.agent_id) {
                Some(n) => (n.token.clone(), Some(n.repo.clone())),
                None => (decoy_token().to_string(), None),
            };
            if tokens_match(&stored, &c.node_token) & repo.is_some() {
                repo
            } else {
                None
            }
        };
        let Some(repo) = repo else {
            return Err(RpcError::refused(
                &c.agent_id.0,
                "this supervisor did not mint that node token, so it cannot tell the caller it \
                 claims to be from any other process that can reach this socket. §5.4 binds a \
                 capability token to an `AgentId`; the supervisor holds the binding and there is \
                 no round trip that could establish one for a node it does not own. A node whose \
                 supervisor has restarted is in this case and it is not a mistake the caller made \
                 — its parent is `Orphaned` (§7.2) and needs an operator, not a retry.",
                "§5.4, §6.1",
            ));
        };
        let (type_name, depth) = self.live.read(|r| {
            let node = r.tree().get(&c.agent_id).ok_or_else(|| {
                RpcError::internal(format!(
                    "this supervisor owns node `{}` and its journal has no `SpawnIntent` for it, \
                     so §6.1 step 2's gates have no agent type and no depth to read. Refusing \
                     rather than assuming a default: an assumed `max_depth` is a constant \
                     pretending to be a lookup.",
                    c.agent_id.0
                ))
            })?;
            let intent = node
                .intent
                .as_ref()
                .ok_or_else(|| Unprojectable::NoIntent.as_error(&c.agent_id))?;
            Ok::<_, RpcError>((intent.agent_type.clone(), intent.depth))
        })?;
        let agent_type = tree_types(&repo)?.resolve(&type_name).ok_or_else(|| {
            Unprojectable::UnknownAgentType(type_name.clone()).as_error(&c.agent_id)
        })?;
        Ok(crate::run::Caller {
            agent_id: c.agent_id.0.clone(),
            agent_type,
            depth,
            live_children: self.live_children_of(&c.agent_id),
        })
    }

    /// §2's `agent/spawn` — see [`agent_spawn`](Self::agent_spawn)'s doc for the whole shape.
    ///
    /// `peer` is the connection's credentials and is consulted on exactly one path — a spawn with
    /// no caller. See [`root_spawn_authorized`] for what that decides and, more importantly, for
    /// what it deliberately does not.
    /// **§11 item 28 step 6's `caller`/`repo` pairing**, answered from the frame alone.
    fn check_caller_repo_pairing(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        // **The `caller`/`repo` pairing, before anything else and before any state is consulted.**
        // It is a property of the frame alone, so it is answered from the frame alone — and
        // answering it first is what keeps both halves reachable no matter which other refusals
        // this build still owes (see the step-6 arm below).
        //
        // `deny_unknown_fields` makes an *unknown* key a refusal already; these are the two ways a
        // known key can be wrong.
        match (p.caller.as_ref(), p.repo.as_ref()) {
            (None, None) => {
                return Err(RpcError::refused(
                    "repo",
                    "a spawn with no `caller` is a client creating a root, and only the client \
                     knows which tree the root is of. §2 keys this supervisor on `git rev-parse \
                     --git-common-dir`, so it serves a repository *and every linked worktree of \
                     it* over one socket and one journal — `<state>/<project-hash>` names the \
                     project, never the working tree, and no derivation here could recover which \
                     of them was meant. A worktree branched from the wrong tree's HEAD is a real \
                     branch off real commits with nothing reporting a problem, so the field is \
                     required rather than defaulted to the supervisor's own project root.",
                    "§2, §6.6",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(RpcError::refused(
                    "repo",
                    "a spawn with a `caller` must not state a `repo`: the supervisor already knows \
                     which tree that node lives in, and a caller that states it is a caller that \
                     can lie about it. This is the same rule that keeps `agent_type` and `depth` \
                     off `SpawnCaller` — every gated fact about a caller is derived from what the \
                     supervisor minted, never from what the frame asserts. A node that could name \
                     its own repository could branch its children off a tree its parent never \
                     entitled it to touch.",
                    "§5.4, §6.1",
                ));
            }
            (Some(_), None) | (None, Some(_)) => {}
        }
        Ok(())
    }
    /// **A root's workspace is not a choice**: `isolation` on a spawn with no `caller`.
    fn check_root_isolation(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        // **The other half of the same pairing**, and refused for §11 item 23's reason rather than
        // ignored: §9's change record exists only for a root, because only a root runs in the
        // operator's own checkout. A child's writes are judged against the worktree marion made it,
        // so a caller that asked marion not to snapshot and was told nothing would have been given
        // a wrong answer that looks like a right one.
        // **The mirror of the two below**: `isolation` is a *child's* field, and stating it for a
        // root is a request marion cannot serve in either direction. A root **is** the operator's
        // checkout — the tree they named with `--repo` — and §9's change record is built on exactly
        // that; accepting `shared-cwd` and doing what marion already does would be an
        // accept-and-ignore, and honouring `worktree` would branch the operator's own run into a
        // directory they never asked marion to make.
        if p.caller.is_none()
            && let Some(iso) = p.isolation
        {
            return Err(RpcError::refused(
                "isolation",
                format!(
                    "a spawn without a `caller` creates a **root**, and a root's workspace is not a \
                     choice: it is the operator's own checkout at the `repo` this call names, which \
                     is what §9's change record measures. `isolation: {:?}` is therefore either a \
                     description of what marion already does or a request to run the operator's own \
                     run somewhere else. Refused rather than dropped, because a parameter that is \
                     accepted and not acted on tells the caller their choice was honoured (§11 item \
                     23). `isolation` belongs on a child spawn, where the parent really is choosing \
                     between §6.6's two workspaces.",
                    iso.as_wire()
                ),
                "§6.6, §9, §11 item 23",
            ));
        }
        Ok(())
    }
    /// **A root has no caller's cwd to share**: `allow_concurrent_writes` without a `caller`.
    fn check_root_allow_concurrent_writes(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        if p.caller.is_none() && p.allow_concurrent_writes.is_some() {
            return Err(RpcError::refused(
                "allow_concurrent_writes",
                "a spawn without a `caller` creates a root, and §6.6's write-conflict rule is about \
                 a *child* sharing a caller's cwd. A root has no parent to share with, so there is \
                 no guard here to lift and nothing for this field to permit. Refused rather than \
                 dropped (§11 item 23).",
                "§6.6, §11 item 23",
            ));
        }
        Ok(())
    }
    /// **§9's change record is a root's**: `no_change_record` on a spawn with a `caller`.
    fn check_child_no_change_record(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        if p.caller.is_some() && p.no_change_record.is_some() {
            return Err(RpcError::refused(
                "no_change_record",
                "a spawn with a `caller` must not state `no_change_record`: §9's change record is \
                 a root's, because only a root runs in the operator's own checkout. A child runs \
                 in a worktree marion made for it (§6.6) and its writes are judged against that \
                 worktree's own scope, so there is no snapshot of anybody's tree here to decline. \
                 Refused rather than dropped, because a parameter that is accepted and not acted \
                 on tells the caller their choice was honoured (§11 item 23).",
                "§9, §11 item 23",
            ));
        }
        Ok(())
    }
    /// **A contracted child in a pane could only time out**: `pane` on a spawn with a `caller`.
    fn check_child_pane(p: &marion_core::proto::params::AgentSpawnParams) -> Result<(), RpcError> {
        // **The third field on the same rule, refused for a reason of its own.** Not symmetry with
        // the two above: a pane is a TUI, and a TUI takes no turn until a human presses return,
        // while a child is defined by a `TaskContract` it must `report` against inside a wall
        // clock. A contracted child in a pane is therefore a task that can only ever time out, and
        // its contract would record that as the child's failure rather than as marion's category
        // error. `run::run_spawn` refuses `LaunchPath::Terminal` again one layer down; this is the
        // refusal in the frame that asked for it.
        if p.caller.is_some() && p.pane.is_some() {
            return Err(RpcError::refused(
                "pane",
                "a spawn with a `caller` must not state `pane`: a pane is a terminal an operator \
                 attaches to and drives by keystrokes, and a child is defined by the \
                 `TaskContract` it must report against inside its wall clock. A TUI takes no turn \
                 until somebody presses return, so a contracted child in a pane is a task that can \
                 only time out — and the contract would record that as the child's failure. Spawn \
                 it without a pane and watch its structured events through `node/attach`, or start \
                 it as a root. Refused rather than dropped (§11 item 23).",
                "§9 M3, §11 item 23",
            ));
        }
        Ok(())
    }
    fn agent_spawn(
        &self,
        p: &marion_core::proto::params::AgentSpawnParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        if p.caller.is_some() {
            validate_native_launch_boundary(p.caller.as_ref(), p.native_launch.as_deref())
                .map_err(NativeLaunchGateError::into_rpc)?;
        }
        let me = self.me.upgrade().ok_or_else(|| {
            RpcError::internal(
                "this supervisor is being dropped and will not start a node it could not then own",
            )
        })?;
        Self::check_caller_repo_pairing(p)?;
        Self::check_root_isolation(p)?;
        Self::check_root_allow_concurrent_writes(p)?;
        Self::check_child_no_change_record(p)?;
        Self::check_child_pane(p)?;
        let Some(env) = self.spawn_env.clone() else {
            return Err(RpcError::unimplemented(
                "agent/spawn",
                "this handle was built by `RegistryHandle::new`, which describes nodes and owns \
                 none, so there is no environment to run one in. A detached supervisor is built by \
                 `RegistryHandle::owning` and does serve this method; a handle in this state is a \
                 describing fixture. Refused here rather than further in, where the failure would \
                 be about a directory instead of about a build.",
                "§2",
            ));
        };
        let Some(caller_id) = p.caller.as_ref() else {
            // **A client creating a root** — §11 item 28 step 6. Answered on its own path rather
            // than folded into the child one: `run_spawn` writes `parent_id: Some(caller)`
            // unconditionally, so serving a root through it would put a node in the tree whose
            // parent is a fabrication. `root::prepare` owns everything that makes a root a root —
            // its agent-dir layout, `ROOT_DEPTH`, §9's change record, `parent_id: None`.
            return self.spawn_root(me, env, p, peer);
        };

        // **Held from here to the child's durable intent, and no further.** See
        // [`Self::spawn_decision`]: the registry is a follower, so two callers that both evaluated
        // the gate before either wrote its intent would both pass a bound only one fits under.
        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        let caller = self.resolve_caller(caller_id)?;
        // **The child's tree is its caller's tree**, read off the entry `resolve_caller` has just
        // proved this caller owns. Not derived from `<project-hash>` — that is the git common dir
        // shared by every linked worktree of this repository, so deriving it would branch a
        // feature-worktree node's children off the main tree's HEAD. See [`NodeHandle::repo`].
        let repo = lock(&self.nodes)
            .get(&caller_id.agent_id)
            .map(|n| n.repo.clone())
            .ok_or_else(|| {
                // Unreachable through `resolve_caller`, which compared this caller's token against
                // this same table. Kept as a refusal rather than an `expect` because the lock is
                // dropped and retaken between the two reads, and the honest answer to "the entry
                // went away" is not a panic in a supervisor that owns other nodes.
                RpcError::refused(
                    &caller_id.agent_id.0,
                    "this supervisor no longer owns that node, so it cannot say which tree a \
                     child of it would branch from.",
                    "§5.4",
                )
            })?;
        // §6.1 step 2, before every side effect — the same pure function `run_spawn` calls, run
        // here as well so the refusal arrives in the frame that asked for it rather than as a node
        // that was never going to start. Two call sites of one function, never two rules.
        agent_type::check_spawn_gates(&caller.agent_type, caller.depth, caller.live_children)
            .map_err(gate_refusal)?;

        // Minted here rather than by the caller, for §9's reason: the contract id names a run
        // marion performed, and a caller-chosen one would let two runs share a contract file.
        let task_id = crate::run::entropy()
            .map(|e| marion_core::contract::new_task_id(crate::run::unix_millis(), e))
            .map_err(|e| {
                RpcError::internal(format!(
                    "marion could not mint a task id for this spawn, so nothing was started: {e}"
                ))
            })?;
        let req = crate::run::SpawnRequest {
            agent_type: p.agent_type.clone(),
            prompt: p.prompt.clone(),
            repo: repo.clone(),
            acceptance_criteria: p.acceptance_criteria.clone(),
            verification: p.verification.clone(),
            writable_scope: p.writable_scope.clone(),
            // **Resolved here, not defaulted in the params.** `params.rs` argues why the wire
            // carries `Option`; this is the one place that turns absence into a number, and
            // `effective_timeout` clamps it exactly as it clamps a stated one.
            timeout_secs: p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS),
            model: p.model.clone(),
            // **Absence resolved here, once.** §3.1's agent-type key defaults to `shared-cwd`;
            // marion resolves an absent *`spawn` parameter* to `Worktree` instead, and
            // `contract::Isolation` carries the argument: silence must not silently *remove*
            // containment from every caller that names nothing.
            isolation: p.isolation.unwrap_or(Isolation::Worktree),
            // Absent is `false` — marion holds §6.6's guard. See `AgentSpawnParams`.
            allow_concurrent_writes: p.allow_concurrent_writes.unwrap_or(false),
            // A client `agent/spawn` is always a fresh run: resume is `node/resume`'s path, which
            // reconstructs this same request from the journal rather than from a caller.
            resume: None,
        };

        // Kept out of the thread's move, because the answer names it: the composing client reads
        // `contracts/<task_id>.json` and cannot mint this id itself (see `AgentSpawnResult`).
        let answered_task_id = task_id.clone();
        let (agent_id, state) = self.launch_child(me, env, req, task_id, caller, repo, decision)?;
        Ok(marion_core::proto::result::AgentSpawnResult {
            state,
            agent_id,
            // §9's contract, named before it exists: this call answers at `Spawned`, and the id is
            // what lets the caller find the file when the run ends.
            task_id: Some(answered_task_id),
        })
    }

    /// **The child launcher, driven from a fully-built [`crate::run::SpawnRequest`]** — the mirror
    /// of [`Self::launch_root`], and for the same reason: `agent/spawn` and `node/resume` are one
    /// launch path, differing only in the request (`agent/spawn` builds a fresh one, `node/resume`
    /// reconstructs a node's own with its id, its session and the workspace it ran in on it). A
    /// second launcher beside this one is how a resumed child ends up gated, journaled or claimed
    /// differently from a spawned one.
    ///
    /// `repo` is the tree this node's own children branch from, `decision` is §6.1 step 2's
    /// serialization — held until the node's intent is durable and dropped here, not by the caller,
    /// so the window it covers is the same on both paths. Returns once the process exists, with the
    /// node's state as the journal has it.
    #[allow(clippy::too_many_arguments)]
    fn launch_child(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        req: crate::run::SpawnRequest,
        task_id: TaskId,
        caller: crate::run::Caller,
        repo: PathBuf,
        decision: std::sync::MutexGuard<'_, ()>,
    ) -> Result<(AgentId, NodeState), RpcError> {
        let (tx, progress) = std::sync::mpsc::channel();
        let observer = NodeOwner {
            handle: me.clone(),
            task_id: Some(task_id.clone()),
            // The child inherits its caller's tree and hands the same one to *its* children.
            repo,
            tx,
            identified: Mutex::new(None),
        };
        // **Everything the thread needs is owned**, for `Background::start`'s reason: this work
        // outlives the JSON-RPC frame that asked for it, so it cannot borrow from this stack frame.
        let owner = me.clone();
        let join = std::thread::spawn(move || {
            let agent_type = req.agent_type.clone();
            // **A panic here must still resolve the node.** See [`caught`]: without this the
            // outcome stays `None` for ever and the supervisor can never exit.
            let outcome = caught(&agent_type, spawn_panicked, || {
                crate::run::run_spawn_watched(&env, &req, &task_id, &caller, &observer)
            });
            // The agent id is known only if `identified` fired. A spawn refused above it — an
            // unknown agent type, a scope outside the ceiling — never minted a node, so there is
            // nothing to file the outcome under and nothing holding the supervisor open.
            if let Some(agent_id) = observer.identified_id() {
                owner.mark_finished(&agent_id, NodeOutcome::Child(Box::new(outcome)));
            }
            // Sent last and unconditionally, so a launch that failed before either earlier moment
            // cannot leave the call waiting out `LAUNCH_BOUND` for something that will not come.
            let _ = observer.tx.send(Progress::Finished);
        });

        // **The response is sent when the process exists, not when the run finishes.** A child
        // spawn that blocked until its contract was written would put a minutes-long call on the
        // JSON-RPC surface, which `background.rs` records as taking a whole bridge down when it
        // hangs.
        let deadline = std::time::Instant::now() + LAUNCH_BOUND;
        let recv = |deadline: std::time::Instant| {
            progress.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        };
        let agent_id = match recv(deadline) {
            Ok(Progress::Identified(id)) => id,
            // `run_spawn` refused before it minted an id — an unknown agent type, or a writable
            // scope outside that type's ceiling. There is no node and nothing to report on.
            Ok(_) => return Err(spawn_refused_before_the_node_existed()),
            Err(_) => return Err(launch_bound_expired(None)),
        };
        // The node is in the table and its intent is durable, so no later spawn can miss it in the
        // count. Everything from here on is this one node's own launch, which no other caller's
        // gate depends on.
        drop(decision);
        // The join handle is filed now rather than at `spawn`, because the table's key is the id
        // this call has only just learned. Nothing races: `Progress::Identified` is sent from
        // inside `claim`, so the entry exists before this line can run.
        //
        // **A call that gave up before this point leaves the thread detached**, and that is
        // deliberate rather than overlooked: the node is still claimed, so §5.7 still refuses to
        // exit while it runs, and the alternative — holding the handle somewhere keyed by nothing
        // — would be a second table to keep consistent with this one.
        if let Some(node) = lock(&self.nodes).get_mut(&agent_id) {
            node.join = Some(join);
        }
        match recv(deadline) {
            Ok(Progress::Started) => {}
            // The launch failed between the intent and the process: no worktree, a config that
            // would not compile, a row promising a tool the harness lacks. The thread filed its
            // reason through `mark_finished` before it sent `Finished`, so it is there to quote.
            Ok(_) => {
                return Err(spawn_failed_before_the_process_existed(
                    &agent_id,
                    self.owned_failure(&agent_id).as_deref(),
                ));
            }
            Err(_) => return Err(launch_bound_expired(Some(&agent_id))),
        }
        self.live.refresh();
        Ok((agent_id.clone(), self.spawned_state(&agent_id)))
    }

    /// **§11 item 28 step 6: a client creating a root, served.**
    ///
    /// The same shape step 4 established for a child and for the same reasons — a thread per node,
    /// the node claimed the instant it has an identity, the caller answered when a **process
    /// exists** rather than when the run finishes — over `root::prepare`/`root::launch_owned`
    /// instead of `run_spawn`, because those are what make a root a root (§9: no parent, no
    /// contract, `ROOT_DEPTH`, the change record over the operator's own checkout).
    ///
    /// **What is different from the child path, stated rather than left to be noticed:**
    ///
    /// * **No §6.1 step 2 gate.** Those gates read the *caller's* type and depth, and there is no
    ///   caller. A root is depth 0 by definition and has no parent whose `max_concurrent_children`
    ///   it could exceed. The authorization that does apply is [`root_spawn_authorized`], which is
    ///   about the connection rather than about the tree.
    /// * **No [`Self::spawn_decision`].** That lock exists to make a gate evaluation and the write
    ///   derived from it indivisible; with no gate there is nothing to serialize, and holding it
    ///   across a root's `prepare` — which snapshots a working tree — would block every child spawn
    ///   in the fleet on one `git` walk.
    /// * **No task id.** §9: a root has no `TaskContract`. See [`NodeHandle::task_id`].
    /// * **The outcome is filed on every exit path**, including a `prepare` that failed before the
    ///   `SpawnIntent` was journaled. A root is claimed *before* its intent (see
    ///   `root::prepare_watched`), so a thread that returned without filing one would leave a
    ///   claimed, never-running node holding this supervisor open against §5.7 for ever.
    fn spawn_root(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        p: &marion_core::proto::params::AgentSpawnParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        root_spawn_authorized(peer)?;
        if let Some(context) = p.native_launch.as_deref() {
            return Err(native_launch_refusal(context));
        }
        let repo = p.repo.clone().ok_or_else(|| {
            // Unreachable: the frame-shape match above refuses `(None, None)` by name before any
            // state is consulted. A refusal rather than an `expect`, because a supervisor owning a
            // fleet must not be taken down by a shape it has already answered.
            RpcError::internal(
                "a root spawn reached the launcher with no repository, which the frame-shape check \
                 refuses by name",
            )
        })?;
        // **The repository must be one this socket serves** — before `NodeOwner::claim` and before
        // any side effect, because everything after this point writes somewhere.
        //
        // `root_spawn_authorized` answers *who* is calling and nothing about *what they named*. A
        // same-uid process that dials this project's socket and states another project's `repo`
        // would otherwise be obeyed: `root::prepare_watched` keys the agent directory and the
        // `SpawnIntent` on `ProjectDir::new(state, project_root(&spec.repo))`, so the node would be
        // journaled under the *named* project while this supervisor's `LiveRegistry` went on
        // following its own. marion would answer "spawned" for a root that never appears in
        // `tree/subscribe`, `session/quit` or any other tree operation on the socket that started
        // it — an authorization hole and a broken contract in one.
        //
        // The comparison is on the **project key**, not on the path, and that is what keeps §2's
        // worktree rule intact: `project_root` resolves to the git common dir, so every linked
        // worktree of one repository hashes to the same `<project-hash>` and a legitimate worktree
        // root is accepted. What it rejects is a `repo` in a *different* repository, which is the
        // only case that could put a record in another project's journal.
        let named =
            marion_core::paths::ProjectDir::new(&env.state, &crate::socket::project_root(&repo));
        if named != env.project_dir {
            return Err(RpcError::refused(
                "repo",
                format!(
                    "this supervisor serves the project keyed at {}, and {} keys to {}. §2 keys a \
                     supervisor and its state on the project root — the git common dir — so one \
                     socket serves one repository and every linked worktree of it, and no other. A \
                     root created here would be journaled under the project it named while this \
                     supervisor kept following its own, so marion would report a node that no \
                     `tree/subscribe` on this socket could ever show. Dial the supervisor for that \
                     repository instead; refused before the node is claimed, so nothing was written \
                     under either project.",
                    env.project_dir.path().display(),
                    repo.display(),
                    named.path().display(),
                ),
                "§2, §5.4",
            ));
        }
        // Resolved here so an unknown type is refused **in the frame that asked for it** rather
        // than arriving as a node that was never going to start. `root::prepare` refuses it again
        // one layer down; two call sites of one lookup, never two rules.
        let agent_type = tree_types(&repo)?
            .resolve(&p.agent_type)
            .ok_or_else(spawn_refused_before_the_node_existed)?;
        // §9's node-level bound is resolved inside the spec — see [`root_spec_from_spawn`] — so
        // the value the launch enforces is the value `prepare` journals.
        let spec = root_spec_from_spawn(p, repo.clone(), &env, &agent_type);

        let (agent_id, state) = self.launch_root(me, spec, repo)?;
        Ok(marion_core::proto::result::AgentSpawnResult {
            state,
            agent_id,
            // §9: a root has no `TaskContract`, so there is no file to name. See
            // [`NodeHandle::task_id`], which is `None` here for the same reason.
            task_id: None,
        })
    }

    /// **The root launcher, driven from a fully-built [`RootSpec`]** — spawn and resume are one
    /// launch path, differing only in the spec (`agent/spawn` builds a fresh one, `node/resume`
    /// reconstructs a node's own with an id and a session on it). The thread owns the node for its
    /// whole life through [`NodeOwner`]; `repo` is the tree its children branch from. Returns once
    /// the process exists, with the node's state as the registry has it at that instant —
    /// `Running` once `journal::confirm_spawned`'s records have been followed, `Spawning` if the
    /// tail has not caught up yet — the same instant `agent/spawn` and `node/resume` both answer
    /// at.
    fn launch_root(
        &self,
        me: Arc<RegistryHandle>,
        spec: crate::root::RootSpec,
        repo: PathBuf,
    ) -> Result<(AgentId, NodeState), RpcError> {
        // §9's bound, off the spec that journaled it: the clock this launch is held to and the
        // clock the node's `SpawnIntent` records are one number by construction.
        let bound = std::time::Duration::from_secs(spec.bound_secs);
        let (tx, progress) = std::sync::mpsc::channel();
        let owner = me.clone();
        let join = std::thread::spawn(move || {
            let observer = NodeOwner {
                handle: me,
                // §9: a root has no contract.
                task_id: None,
                // The tree the client named, remembered so this root's own children branch from it
                // — which is the whole of what step 5 needs from step 6.
                repo,
                tx,
                identified: Mutex::new(None),
            };
            // **Wrapped for [`caught`]'s reason**, in the root's own vocabulary: `NodeOutcome::Root`
            // carries marion's sentence rather than a `SpawnError`, so the panic becomes that
            // sentence. The alternative — leaving the root path uncaught because only the child path
            // has an `AbortOnDrop` — would keep the whole defect alive on the one node that holds
            // the supervisor open on its own.
            let outcome = caught(&spec.agent_type, root_panicked, || -> Result<(), String> {
                let node = crate::root::prepare_watched(&spec, &observer)
                    .map_err(|e| format!("marion could not prepare the root node: {e}"))?;
                // The trait's second hook, called by the launcher at `command.spawn()`. In
                // scope explicitly rather than through a blanket import, so the two halves of
                // `SpawnObserver` are visibly the same trait here as on the child path.
                use crate::run::SpawnObserver as _;
                let started = |pid: i32| observer.started(&node.agent_id, pid);
                crate::root::launch_owned(
                    &node,
                    bound,
                    crate::root::MCP_READY_TIMEOUT,
                    // No live sink here: this runs inside `marion-supervisor`, and the client
                    // watches through `node/attach` over the socket rather than through a pipe
                    // this process would have to own. `events.jsonl` is what both legs read.
                    None,
                    Some(&started),
                    // §9's M3: where a pane goes if this run asked for one. The same observer,
                    // because it is the same ownership — this supervisor answers `node/attach`,
                    // so it is the only process for which a registered pane means anything.
                    Some(&observer),
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
            });
            // **Filed whenever there is a node to file it under**, which after `identified` there
            // always is. See this function's doc for why a missing outcome would be a supervisor
            // that can never exit.
            if let Some(agent_id) = observer.identified_id() {
                owner.mark_finished(&agent_id, NodeOutcome::Root(outcome));
            }
            // Last and unconditional, so a launch that failed before either earlier moment cannot
            // leave the call waiting out `LAUNCH_BOUND` for something that will not come.
            let _ = observer.tx.send(Progress::Finished);
        });

        let deadline = std::time::Instant::now() + LAUNCH_BOUND;
        // `prepare_watched` calls `identified` before its first side effect, so nothing but
        // minting the id itself can fail ahead of it — an unreadable entropy source.
        let agent_id = await_identified(&progress, deadline)?;
        self.file_join(&agent_id, join);
        self.await_root_started(&progress, deadline, &agent_id)?;
        self.live.refresh();
        let state = self.spawned_state(&agent_id);
        Ok((agent_id, state))
    }

    /// **Park a launch thread's handle under the node it has just identified.**
    ///
    /// Filed now rather than at `spawn`, because the table's key is the id the launch has only just
    /// learned. Nothing races: `Progress::Identified` is sent from inside `claim`, so the entry
    /// exists before this can run. A launch that gave up before this point leaves the thread
    /// detached, and that is deliberate rather than overlooked: the node is still claimed, so §5.7
    /// still refuses to exit while it runs, and the alternative — holding the handle somewhere
    /// keyed by nothing — would be a second table to keep consistent with this one.
    fn file_join(&self, agent_id: &AgentId, join: std::thread::JoinHandle<()>) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.join = Some(join);
        }
    }

    /// The root launch's second wait: a process exists, or the reason there is none.
    ///
    /// The launch failed between the identity and the process: a working tree marion could not
    /// snapshot, a `<state>` inside the repository, a configuration document that would not
    /// compile, an unsupported root surface.
    ///
    /// **Answered with the reason, not merely with the fact.** Every one of those is a `RootError`
    /// marion wrote as a sentence for an operator — it names the directory, the declaration and the
    /// way through — and until step 6 `marion run` printed it from the error in its own hand. The
    /// supervisor holds it now, so a client that got the generic sentence back would be told a run
    /// failed and never told what to change. [`Self::owned_failure`] is where the thread filed it,
    /// and it is filed before `Progress::Finished` is sent, so it is there by the time this reads.
    fn await_root_started(
        &self,
        progress: &std::sync::mpsc::Receiver<Progress>,
        deadline: std::time::Instant,
        agent_id: &AgentId,
    ) -> Result<(), RpcError> {
        match recv_progress(progress, deadline) {
            Ok(Progress::Started) => Ok(()),
            Ok(_) => Err(root_launch_failed(
                agent_id,
                self.owned_failure(agent_id).as_deref(),
            )),
            Err(_) => Err(launch_bound_expired(Some(agent_id))),
        }
    }

    /// **The state a launch answers with, read back off the registry rather than asserted.** The
    /// state this returns is the state the journal says, which is the point of answering at
    /// `Spawned` rather than before it: a client that renders `Running` — or a `Spawning` the
    /// registry's tail has not yet moved past — is rendering a record it could have read itself.
    fn spawned_state(&self, agent_id: &AgentId) -> NodeState {
        self.live
            .read(|r| r.tree().get(agent_id).map(|n| n.state))
            .unwrap_or(NodeState::Spawning)
    }

    /// **`node/resume` — relaunch a lost node under its own id** (`plan-restart-resume.md` step 6).
    ///
    /// §8's rule is that a lost session is *relaunched*, never re-opened: the process died with the
    /// supervisor that held it, and every harness's resume starts a **new** process against the
    /// single-writer transcript. So this is not `node/prompt` reaching a live channel — there is
    /// none — it is `agent/spawn`'s own launch path fed a [`RootSpec`] rebuilt from the node's
    /// journal, with the node's own id and the session its stream named carried onto it.
    ///
    /// The preflight is in the order §7.2 and §6.7 impose, each refusal naming the node rather than
    /// guessing past it:
    ///
    /// 1. the node exists, and its intent says what it was — a node with no `SpawnIntent` has no
    ///    type, no harness and no depth, and a launch rebuilt from that would be a guess;
    /// 2. its process is finished — an `Orphaned`/`ReapedIdle`/exited node — never a live one;
    /// 3. its stream named a session, else there is nothing to resume and the refusal says so;
    /// 4. its recorded process is provably not still running: `AliveAndOurs` is killed first (the
    ///    same confirmed group kill `session/quit` uses), `CannotTell` refuses rather than risk a
    ///    second live process against one transcript, `Gone` proceeds.
    ///
    /// Then the node's own depth selects the launcher — [`Self::relaunch_root`] or
    /// [`Self::relaunch_child`], which carries the three further refusals a child needs and a root
    /// cannot need. Both go through the launcher `agent/spawn` uses at that depth: **one spawn
    /// path**, with the resume as a parameter to it.
    fn node_resume(
        &self,
        p: &marion_core::proto::params::NodeResumeParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::NodeResumeResult, RpcError> {
        // A resume starts work under a root's authority — the same filesystem-permission gate
        // `agent/spawn`'s root path answers, for the same reason (a resumed root has an id, but the
        // caller asking to resume it is not that node).
        root_spawn_authorized(peer)?;
        let me = self.me.upgrade().ok_or_else(|| {
            RpcError::internal(
                "this supervisor is being dropped and will not relaunch a node it could not own",
            )
        })?;
        let Some(env) = self.spawn_env.clone() else {
            return Err(RpcError::unimplemented(
                "node/resume",
                "this handle describes nodes and owns none, so there is no environment to relaunch \
                 one in. A detached supervisor built by `RegistryHandle::owning` serves this.",
                "§2",
            ));
        };
        self.live.refresh();
        let node = self
            .live
            .read(|r| r.tree().get(&p.agent_id).cloned())
            .ok_or_else(|| {
                RpcError::refused(
                    "agent_id",
                    format!(
                        "no node `{}` is on this project's journal, so there is nothing to resume.",
                        p.agent_id.0
                    ),
                    "§2, §7.2",
                )
            })?;
        // **A node with no intent has no depth, and a relaunch of it would be a guess.** Its
        // agent type, its harness, its parent and its position are all on that one record; a
        // journal whose head was compacted away yields a node marion has records *about* and no
        // identity *for*, which is not something to rebuild a launch from.
        let Some(depth) = node.depth() else {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` has records on this journal but no `SpawnIntent`, so marion cannot say \
                     what it was — its type, its harness, or where it sat in the tree. Refused by \
                     name rather than relaunched as a guess.",
                    p.agent_id.0
                ),
                "§4.3, §7.2",
            ));
        };
        // **A live node is not resumed — it is attached to.** Only a node whose fate is decided (an
        // orphan the last restart marked, an idle node deliberately reaped, or one that exited) has
        // a process to relaunch in place of.
        let resumable = matches!(node.reap_state, ReapState::Orphaned | ReapState::ReapedIdle)
            || node.state.is_exited();
        if !resumable {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` is still live ({:?}); marion holds its channel, so a new turn is \
                     `node/prompt` and reaching it is `node/attach`. Resume relaunches a node whose \
                     process is gone, never one that is running.",
                    p.agent_id.0, node.state,
                ),
                "§8, §7.2",
            ));
        }
        let Some(session) = node.harness_session.clone() else {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` named no harness session — its stream never carried one, or it ran on a \
                     harness marion reads no session from — so marion cannot hand a resume back to \
                     the harness. Refused by name rather than started fresh under a resumed \
                     session's id, which would misdescribe the run.",
                    p.agent_id.0
                ),
                "§8",
            ));
        };
        // **The recorded process must be provably not still running before a second one is started
        // against the same single-writer transcript** (principle 8). A node with no recorded pid
        // never reached `command.spawn()` under the lost supervisor, so there is nothing to signal.
        if let Some(pid) = node.pid {
            match crate::procid::resolve(node.start_id.as_ref(), crate::procid::read(pid)) {
                crate::procid::Resolution::AliveAndOurs => {
                    self.journal_append(RecordKind::KillIntent(KillIntent {
                        agent_id: node.agent_id.clone(),
                        was: node.state,
                    }))
                    .map_err(journal_failure_before_signal)?;
                    if !self.runtime.kill_process_tree_and_wait(pid) {
                        return Err(RpcError::internal(format!(
                            "`{}` was still running its own process and marion journaled the intent \
                             to kill it before resuming, but could not observe its PID dead; the \
                             intent remains unconfirmed and marion will not start a second process \
                             against one transcript (§6.7, §8)",
                            node.agent_id.0
                        )));
                    }
                    self.journal_append(RecordKind::KillConfirmed(KillConfirmed {
                        agent_id: node.agent_id.clone(),
                        exit: ProcessExit {
                            code: None,
                            signal: Some(9),
                            description: "marion sent SIGKILL to the surviving process before \
                                          resuming its node"
                                .into(),
                        },
                    }))
                    .map_err(journal_failure_after_signal)?;
                }
                crate::procid::Resolution::CannotTell(_) => {
                    return Err(RpcError::conflict(
                        &node.agent_id.0,
                        "marion cannot prove `node`'s recorded process is gone — a process wears \
                         its PID and no recorded identity settles whether it is the same one — so \
                         it will not start a second process that might run against a transcript a \
                         first is still writing. Nothing was signalled or launched.",
                        "§6.7, §8",
                    ));
                }
                crate::procid::Resolution::Gone => {}
            }
        }
        // **The tree this node's own children branch from**, and it is the same value at every
        // depth: `marion run` gives the root the project's working tree, and every `agent/spawn`
        // hands a child its caller's, so one tree runs the whole fleet. A resumed node at any depth
        // therefore hands its own children what it would have handed them in its first life.
        let repo = resumable_root_cwd(&env.project_root);
        let (agent_id, state) = if depth == 0 {
            self.relaunch_root(me, &node, &env, &p.prompt, repo, session)?
        } else {
            self.relaunch_child(me, &node, &env, &p.prompt, repo, session)?
        };
        Ok(marion_core::proto::result::NodeResumeResult {
            agent_id,
            state,
            // The lifetime this relaunch begins: replay folds a second `Spawned` as generation two,
            // so the count the caller reads is the recorded one plus this launch.
            spawn_generation: node.spawn_generation + 1,
        })
    }

    /// [`Self::node_resume`]'s root arm: [`crate::root::RootSpec`] rebuilt from the node's journal,
    /// through the same [`Self::launch_root`] `agent/spawn` uses.
    fn relaunch_root(
        &self,
        me: Arc<RegistryHandle>,
        node: &marion_core::registry::ReplayedNode,
        env: &crate::run::Env,
        prompt: &str,
        repo: PathBuf,
        session: String,
    ) -> Result<(AgentId, NodeState), RpcError> {
        // §6.1 step 5's type resolution, for the wall clock the relaunch runs under — the same
        // number `marion run` and `agent/spawn` resolve, from the node's own recorded type.
        let agent_type = node
            .intent
            .as_ref()
            .map(|i| recorded_type(&repo, i))
            .transpose()?
            .ok_or_else(spawn_refused_before_the_node_existed)?;
        let spec = crate::root::RootSpec {
            agent_type: node.agent_type().unwrap_or_default().to_string(),
            prompt: prompt.to_string(),
            native_launch: None,
            repo: repo.clone(),
            state: env.state.clone(),
            base_url: env.base_url.clone(),
            bridge: env.bridge.clone(),
            model: node.model.clone(),
            no_change_record: false,
            auth: env.auth,
            // **From the recorded value, never inferred** (`plan-restart-resume.md` step 6): the
            // shape the node's session was observed in. A pane names no session, so a resumable
            // node is always headless today; the field keeps that a checked fact.
            pane: node.harness_pane,
            resume: Some((node.agent_id.clone(), session)),
            // **The node's own bound, from its own journal** — the same discipline the two fields
            // above follow. A relaunch that re-resolved this from the agent type would put a root
            // the operator had launched with `--timeout 300` back under §3.1's 900 s, which is a
            // node quietly given a different clock than the one it was started with. `None` — an
            // intent written before the field existed — falls back to the type, which is the only
            // thing a journal that never recorded a bound can honestly say.
            bound_secs: crate::root::blocked_bound_secs(
                node.intent.as_ref().and_then(|i| i.timeout_secs),
                agent_type.timeout.0.as_secs(),
            ),
        };
        self.launch_root(me, spec, repo)
    }

    /// [`Self::node_resume`]'s child arm: a [`crate::run::SpawnRequest`] rebuilt from the node's
    /// journal, through the same [`Self::launch_child`] `agent/spawn` uses.
    ///
    /// Three more refusals than the root arm, each about something a root cannot lack, and in this
    /// order because the tree question settles whether there is a relaunch to place at all before
    /// anything asks the filesystem where it would go:
    ///
    /// 1. **its parent's fate is decided** — principle 8. A **Live** parent holds this child: its
    ///    own `spawn` is owed the outcome, and a relaunch from outside would put a second process
    ///    under one contract. A parent that is `Orphaned`, `ReapedIdle` or exited holds nothing,
    ///    and its child resumes: the child reports to the *supervisor* (the report tool writes the
    ///    contract and the journal), so a decided parent costs the report no destination;
    /// 2. **its workspace is on the journal** — a child's cwd is a linked worktree marion made or
    ///    the caller's own directory, and neither is derivable from anything else;
    /// 3. **that workspace still exists** — `run::cleanup` and `marion worktree reap` both remove a
    ///    child's tree, and a harness handed a session from a directory it has never seen starts
    ///    fresh, which marion would then have called a resume.
    ///
    /// **The design documents are silent on the third**, and this is the reading taken rather than
    /// invented: nothing in §7 or `plan-restart-resume.md` gives a child resume a parent rule at
    /// all. The alternative reading — refuse when the parent is *not* live, on the grounds that the
    /// child "reports into nothing" — was rejected because the premise is false in this codebase,
    /// and because it would make the case a supervisor SIGKILL actually produces (a whole tree
    /// orphaned at once) the one case resume could not serve.
    ///
    /// **Three limits, stated rather than hidden.** A child's `writable_scope`, its
    /// `acceptance_criteria` and its `verification` are not journaled — they live on the contract,
    /// which an orphaned child never got far enough to write — so the second life runs with the
    /// empty scope, which is its agent type's ceiling (`run::requested_scope`, still checked
    /// against that ceiling), an empty criteria list and no verification commands. Narrowing them
    /// again would need a second record, not a guess here.
    fn relaunch_child(
        &self,
        me: Arc<RegistryHandle>,
        node: &marion_core::registry::ReplayedNode,
        env: &crate::run::Env,
        prompt: &str,
        repo: PathBuf,
        session: String,
    ) -> Result<(AgentId, NodeState), RpcError> {
        // **§7.5's immutable parent link, read back.** The intent is the only record that names it,
        // and a child that has one always has a parent — `run_spawn` writes `parent_id: Some(..)`
        // unconditionally.
        let parent_id = node
            .intent
            .as_ref()
            .and_then(|i| i.parent_id.clone())
            .ok_or_else(|| {
                RpcError::refused(
                    "agent_id",
                    format!(
                        "`{}` records a depth below the root and no parent, so marion cannot say \
                         whose child it is or what gates its relaunch. Refused by name.",
                        node.agent_id.0
                    ),
                    "§7.5",
                )
            })?;
        let parent = self
            .live
            .read(|r| r.tree().get(&parent_id).cloned())
            .ok_or_else(|| {
                RpcError::refused(
                    "agent_id",
                    format!(
                        "`{}` names `{}` as its parent and no such node is on this journal, so the \
                         relaunch would have no place in the tree. Refused by name.",
                        node.agent_id.0, parent_id.0,
                    ),
                    "§7.5",
                )
            })?;
        let parent_decided = matches!(
            parent.reap_state,
            ReapState::Orphaned | ReapState::ReapedIdle
        ) || parent.state.is_exited();
        if !parent_decided {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}`'s parent `{}` is still live ({:?}); marion holds its channel and its own \
                     `spawn` owns this child, so the child's outcome is owed to a call that is \
                     still waiting for it. A second process under one contract is what resume \
                     exists not to do. Resume this child once its parent's fate is decided, or \
                     reach it through its parent.",
                    node.agent_id.0, parent_id.0, parent.state,
                ),
                "§8, §7.2",
            ));
        }
        let Some(workspace) = node.launch_workspace.clone() else {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` is a child, and this journal does not record which directory it ran in \
                     — its session record predates the field, or no frame ever named a session. A \
                     harness resumes a conversation only from the tree that created it, so marion \
                     refuses by name rather than relaunching it somewhere the session has never \
                     been.",
                    node.agent_id.0
                ),
                "§6.6, §8",
            ));
        };
        if !workspace.path().is_dir() {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` ran in {}, and that tree no longer exists — a reap or a cleanup removed \
                     it. Its session id outlived its workspace; handing the id back from anywhere \
                     else would start a fresh run wearing a resumed node's name, so marion refuses \
                     by name instead.",
                    node.agent_id.0,
                    workspace.path().display(),
                ),
                "§6.6, §8",
            ));
        }
        let parent_type = parent
            .intent
            .as_ref()
            .map(|i| recorded_type(&repo, i))
            .transpose()?
            .ok_or_else(spawn_refused_before_the_node_existed)?;
        let agent_type = node
            .intent
            .as_ref()
            .map(|i| recorded_type(&repo, i))
            .transpose()?
            .ok_or_else(spawn_refused_before_the_node_existed)?;
        // **The task this run is still under**, from the node's own intent (§9: one contract per
        // run of one task). A fresh id here would file the second life's audit record under a task
        // nothing asked for.
        let task_id = node
            .intent
            .as_ref()
            .and_then(|i| i.task_id.clone())
            .ok_or_else(|| {
                RpcError::refused(
                    "agent_id",
                    format!(
                        "`{}` is a child with no task on its intent, so §9's contract for its \
                         second life would name nothing. Refused by name.",
                        node.agent_id.0
                    ),
                    "§9",
                )
            })?;
        // Held for the same window `agent/spawn` holds it: §6.1 step 2's gate evaluation and the
        // intent derived from it are one decision, and this launch is gated exactly as a fresh
        // child's is (`run_spawn_watched` calls `check_spawn_gates` with this caller).
        let decision = lock(&self.spawn_decision);
        let caller = crate::run::Caller {
            agent_id: parent_id.0.clone(),
            agent_type: parent_type,
            depth: parent.depth().unwrap_or(0),
            live_children: self.live_children_of(&parent_id),
        };
        let req = crate::run::SpawnRequest {
            agent_type: agent_type.name.clone(),
            prompt: prompt.to_string(),
            repo: repo.clone(),
            // Not journaled; see this function's doc for why they are empty rather than invented.
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec![],
            timeout_secs: agent_type.timeout.0.as_secs(),
            model: node.model.clone(),
            // Read off the recorded workspace, so the answer and the directory cannot disagree.
            isolation: match workspace {
                marion_core::contract::Workspace::Worktree { .. } => Isolation::Worktree,
                marion_core::contract::Workspace::SharedCwd { .. } => Isolation::SharedCwd,
            },
            allow_concurrent_writes: false,
            resume: Some(crate::run::ChildResume {
                agent_id: node.agent_id.clone(),
                session,
                workspace,
            }),
        };
        self.launch_child(me, env.clone(), req, task_id, caller, repo, decision)
    }

    /// §7.3.2's voluntary path. The mutex is not throughput machinery; it makes the rendered-set
    /// comparison and the first intent one indivisible decision. Without it, two clients can both
    /// confirm the same live render and each signal it after the other's confirmation.
    fn session_quit(&self, disposition: &QuitDisposition) -> Result<SessionQuitResult, RpcError> {
        let _decision = lock(&self.quit);
        self.live.refresh();
        let answered = match disposition {
            QuitDisposition::KillTree { confirmed } => self.kill_tree(confirmed),
            QuitDisposition::DetachAll => Ok(SessionQuitResult {
                outcome: self.detach_all(),
            }),
            QuitDisposition::ReapIdleDetachBusy => self.reap_idle_detach_busy(),
        };
        // **Recorded on the disposition, never on its answer.** A quit that was told `Resident` is
        // not a quit that failed — the client still left, and the clause holding the supervisor can
        // clear a millisecond later. Reading the waiver off the answer would make an operator who
        // quit while one node was mid-turn wait §5.7's full grace after it finished, while an
        // operator who quit a second later did not; the two said the same thing.
        //
        // A **refused** quit sets nothing: `kill_tree` rejects a stale render before it signals
        // anything, and that client has not left, it has been told to render again.
        if answered.is_ok() {
            self.quit_waived_grace.store(true, Ordering::SeqCst);
        }
        answered
    }

    fn nodes(&self) -> Vec<marion_core::registry::ReplayedNode> {
        self.live.read(|r| r.tree().nodes().to_vec())
    }

    fn journal_path(&self) -> PathBuf {
        self.live.read(|r| r.path().to_path_buf())
    }

    /// Append one record through **the supervisor's single journal handle** (`journal::append_at`).
    ///
    /// This used to open a `Journal` of its own, with a fresh `writer_id`, once per call. Nothing
    /// depended on that and two fields were the worse for it: `seq` restarted at 0 on every RPC, and
    /// `mono_ns` — §4.2's anchor between a record and `pty.cast` — was measured from an origin a few
    /// microseconds old, so it was ~0 on essentially every record the handler wrote. The node
    /// threads were already sharing one handle via `journal::record`; this is the same handle, with
    /// the error handed back rather than printed, because `session/quit` orders a kill against a
    /// durable intent and has to know when the append failed.
    ///
    /// A failed *open* now surfaces as the first append's failure rather than as a distinct refusal.
    /// That is the honest report: nothing had been signalled at that point either way, and a
    /// disposition with nothing to journal no longer fails on a file it never needed to touch.
    fn journal_append(&self, kind: RecordKind) -> Result<(), crate::journal::JournalError> {
        crate::journal::append_at(&self.journal_path(), kind).map(|_| ())
    }

    /// [`Self::journal_append`], then fold the record into the live tree **before returning**.
    ///
    /// For a record whose reader is about to arrive on another connection — a native node's
    /// `SpawnIntent`, which the relay's `node/attach` resolves moments after the bootstrap
    /// answers — the follower's poll interval is a race, and ordering rather than elapsed time is
    /// the contract (`LiveRegistry::refresh`).
    pub(crate) fn journal_now(&self, kind: RecordKind) -> Result<(), crate::journal::JournalError> {
        self.journal_append(kind)?;
        self.live.refresh();
        Ok(())
    }

    fn guidance(&self) -> DetachGuidance {
        let socket = self
            .journal_path()
            .parent()
            .map(|p| p.join("supervisor.sock"))
            .unwrap_or_else(|| PathBuf::from("supervisor.sock"));
        DetachGuidance {
            reattach: format!(
                "Reconnect to {} and call tree/subscribe. Until then, any named gate_exposed node \
                 that reaches a permission request burns its bound and is denied unattended; the \
                 far side receives an is_error:true tool_result (§7.3.2, §11 item 22, S9).",
                socket.display()
            ),
            stop_fleet: format!(
                "Reconnect to {} and call session/quit with KillTree confirmed against a fresh \
                 tree/subscribe render.",
                socket.display()
            ),
        }
    }

    fn active(nodes: &[marion_core::registry::ReplayedNode]) -> Vec<AgentId> {
        nodes
            .iter()
            .filter(|n| !n.state.is_exited() && n.reap_state == ReapState::Live)
            .map(|n| n.agent_id.clone())
            .collect()
    }

    /// §5.7's exclusion list, and the one thing it is **not** about.
    ///
    /// Every clause below asks the same underlying question — *is there work here that exiting
    /// would strand?* A node whose `SpawnAborted` is journaled has none: `RecordKind::SpawnAborted`
    /// is *"the intent's other resolution"*, written when marion abandoned the spawn, and §7.2 is
    /// emphatic that *"a node marion decided the fate of is never `Orphaned`"*. There is no process,
    /// there never was one, and nothing about it can ever change again.
    ///
    /// It has to be filtered explicitly because it does not show up as one. `NodeState` has no
    /// "never started" variant and `registry.rs` deliberately does not synthesise one — the abort is
    /// a separate recorded fact, not a state transition — so an aborted node's state stays
    /// `Spawning` forever and satisfies **two** clauses below on its own.
    ///
    /// **This was measured, not reasoned about.** Before the detached supervisor existed, nothing
    /// acted on this function's answer for longer than one process's life; wiring §10's split made
    /// a `marion run` whose root failed to launch leave a supervisor that would never exit, holding
    /// a project directory that had already been deleted. That is the *inverse* of what §5.7's
    /// exclusion list is for.
    ///
    /// **The exclusion is exactly as wide as that argument and no wider.** *"There is no process
    /// and there never was one"* is a claim about the journal's **other** records, not about the
    /// abort on its own. `run.rs`'s `AbortOnDrop` is armed across the whole synchronous child run —
    /// from before the process exists to after it is reaped — so an unwind anywhere in between
    /// writes `SpawnAborted` beside a `Spawned` that names a live pid, and a `std::process::Child`
    /// dropped rather than waited on does not kill what it holds. See [`Self::abandoned`].
    ///
    /// **§7.2's `Orphaned` is deliberately not a second exclusion**, and the temptation to make it
    /// one is worth answering rather than leaving to be rediscovered.
    ///
    /// The case for excluding it looks strong: the node was recorded before this process existed,
    /// marion holds no `Child` and no channel for it, §7.6 already counts it terminal for gating,
    /// and holding is permanent — a supervisor booting over a journal a crashed one left is
    /// `Resident` from its first instant. Every clause of that is true and the conclusion is still
    /// wrong, because it reads `Orphaned` as *"nothing here to act on"* when §7.2 defines it as the
    /// opposite: *"both are 'marion does not know', **both require the same user resolution**"*.
    /// The resolution is the operator's, over this socket — and a supervisor that exited to avoid
    /// holding has taken the resolution away rather than performed it. `Spawned` can carry a real
    /// pid, in which case a confirmed `session/quit` KillTree signals the surviving process; that
    /// is a live node this supervisor is the only handle on.
    /// `detached_supervisor.rs`'s `a_supervisor_holding_a_non_terminal_node_refuses_to_exit_
    /// until_that_node_finishes` boots over exactly that journal, with a process it really started.
    ///
    /// So `Orphaned` is the *reason* to stay, not a reason to leave — and the honest cost is that a
    /// node whose `Spawned` recorded no pid can be neither killed nor resolved, so it holds forever
    /// with no path out. That is §11 item 28's absent pid, not a residency rule to loosen; loosening
    /// it here would trade a supervisor that cannot exit for a fleet that cannot be stopped.
    fn resident_reason(nodes: &[marion_core::registry::ReplayedNode]) -> Option<ResidentReason> {
        let holding: Vec<_> = nodes.iter().filter(|n| !Self::abandoned(n)).collect();
        // **The intent holds only while the death is unobserved.** §7.2's crash window is *"the
        // supervisor died before the kill landed"*, and what makes it a window is that a process
        // may still be running. A terminal record for the node shuts it: §7.2 resolves an
        // unconfirmed intent *by checking for the process*, and an observed exit or a confirmed
        // kill **is** that check, already made. No `ReapConfirmed` follows — nothing here
        // fabricates a record — so the intent stays outstanding on the tree forever, and reading
        // it without asking about the exit beside it is a supervisor that can never leave.
        //
        // Reachable in one supervisor's life and without a crash: `reap_idle_detach_busy` journals
        // the intent, signals, and returns rather than confirming when it cannot observe the death;
        // a later confirmed `session/quit` KillTree then writes the `KillConfirmed` that does
        // observe it.
        if holding
            .iter()
            .any(|n| n.reap_intent.is_some() && !n.state.is_exited())
        {
            Some(ResidentReason::UnconfirmedReapIntent)
        } else if holding
            .iter()
            .any(|n| matches!(n.state, NodeState::Blocked(_)))
        {
            Some(ResidentReason::BlockedNode)
        } else if holding.iter().any(|n| n.state == NodeState::Spawning) {
            // An intent whose confirmation has not landed. A managed node's `Spawned` is followed
            // by `StateChanged(Running)` (`journal::confirm_spawned`), so a node marion holds a
            // process for reads as `NonTerminalNode` below, not here; both keep the supervisor
            // resident, and the word names which of the two the operator is looking at.
            Some(ResidentReason::SpawnOutstanding)
        } else if holding.iter().any(|n| !n.state.is_exited()) {
            Some(ResidentReason::NonTerminalNode)
        } else {
            None
        }
    }

    /// §5.7's exit predicate as this supervisor can actually answer it — the exclusion list, and
    /// **whether the list is being read off a tree that is still the journal's**.
    ///
    /// `registry.rs` stops following at a line it cannot parse and is right to (§7.4): *"an
    /// authority may not keep serving a tree from a file it no longer recognises."* What that costs
    /// one level up is not in §5.7 at all — the exclusion list is then evaluated against the prefix
    /// as it stood *before* the corruption, so a node that has since exited is reported
    /// non-terminal forever and nothing short of a signal ends the process.
    ///
    /// Failing closed is the right half of that and is kept. What is fixed here is the **answer**:
    /// the operator was told `Resident(NonTerminalNode)` and sent looking for a node, when the
    /// truth is that marion stopped reading. [`ResidentReason::RegistryStopped`] says so, and the
    /// reason and offset — which the `Copy` enum cannot carry — go to the supervisor's log once.
    ///
    /// **This does not clear the condition** and is not meant to; see §11 item 29.
    fn residency(&self) -> Option<ResidentReason> {
        if let Some(reason) = self.registry_stopped() {
            if !self.stopped_reported.swap(true, Ordering::SeqCst) {
                eprintln!(
                    "marion-supervisor: this project's journal stopped being followable and this \
                     supervisor is answering §5.7 from the tree as it stood before that point: \
                     {reason}. It will not exit while that reading holds (§7.4, §11 item 29)."
                );
            }
            return Some(ResidentReason::RegistryStopped);
        }
        Self::resident_reason(&self.nodes())
    }

    /// The reason the registry gave up following, if it has.
    fn registry_stopped(&self) -> Option<String> {
        self.live.read(|r| match r.status() {
            crate::registry::Status::Stopped { reason } => Some(reason.clone()),
            _ => None,
        })
    }

    /// A node marion abandoned **before there was anything to abandon**, which is the only shape
    /// §7.2's *"a node marion decided the fate of is never `Orphaned`"* licenses excluding.
    ///
    /// Four facts and not one, because the abort record alone does not carry the claim. `Spawned`
    /// is written when the process exists and carries its pid; either of those present means the
    /// abort was written *over* a live child — `run.rs`'s guard covers the whole run and unwinds
    /// through the reap, and dropping a `Child` does not signal it. A reap intent present is §7.2's
    /// own crash window and holds regardless of how the spawn ended.
    ///
    /// **The first sentence became true rather than aspirational with §11 item 28 step 1**, and
    /// this predicate got stricter in the safe direction as a result. `run_spawn` now appends
    /// `Spawned { pid: Some(_) }` between `command.spawn()` and the child's first byte of stdin,
    /// so the two middle clauses have teeth on a child: any failure *after* the process exists
    /// leaves a record marion cannot mistake for a spawn that never happened. **It is true of a
    /// root too**, and this sentence used to say it was not: item 28's step 6 gave `root.rs`'s
    /// `launch_inner` an `on_started` hook, so a root's `Spawned` is written at `command.spawn()`
    /// with a real pid exactly like a child's. The `pid: None` arm survives only for a launch that
    /// never reached a process.
    ///
    /// The b600d82 case — `SpawnIntent` then `SpawnAborted`, nothing else, which is exactly what a
    /// `marion run` whose root failed to launch journals — still satisfies all four, and now does
    /// so for a stronger reason: a launch that fails before there is a process writes no `Spawned`
    /// at all, so the shape is *evidence* that no process exists rather than merely consistent
    /// with it. `background_spawn.rs`'s
    /// `a_launch_that_fails_before_the_process_exists_journals_no_spawned_record` produces that
    /// shape from a real failing launch; the residency reading of it is
    /// `an_aborted_spawn_does_not_keep_the_supervisor_resident_but_an_outstanding_one_does`.
    fn abandoned(n: &marion_core::registry::ReplayedNode) -> bool {
        n.spawn_aborted.is_some()
            && !n.spawn_confirmed
            && n.pid.is_none()
            && n.reap_intent.is_none()
    }

    fn detach_all(&self) -> QuitOutcome {
        let nodes = self.nodes();
        let detached = Self::active(&nodes);
        let supervisor = match self.residency() {
            Some(reason) => SupervisorDisposition::Resident(reason),
            None => SupervisorDisposition::Exiting,
        };
        QuitOutcome::Detached {
            gate_exposed: detached.clone(),
            detached,
            guidance: self.guidance(),
            supervisor,
        }
    }

    fn kill_tree(&self, confirmed: &[AgentId]) -> Result<SessionQuitResult, RpcError> {
        let nodes = self.nodes();
        let targets: Vec<_> = nodes.iter().filter(|n| !n.state.is_exited()).collect();
        let mut expected: Vec<_> = targets.iter().map(|n| n.agent_id.0.clone()).collect();
        let mut stated: Vec<_> = confirmed.iter().map(|id| id.0.clone()).collect();
        expected.sort();
        stated.sort();
        if expected != stated {
            return Err(RpcError::refused(
                "confirmed",
                format!(
                    "the confirmed kill list does not equal the supervisor's current non-terminal \
                     set; confirmed {stated:?}, current {expected:?}. Nothing was signalled. Render \
                     the list again and confirm that exact set (§7.3.2)."
                ),
                "§7.3.2",
            ));
        }
        if let Some(node) = targets
            .iter()
            .find(|n| n.reap_state != ReapState::ReapedIdle && n.pid.is_none())
        {
            return Err(RpcError::conflict(
                &node.agent_id.0,
                "the confirmed node has no recorded PID yet, so marion cannot prove a signal \
                 reaches it. Nothing was signalled; retry after its spawn resolves.",
                "§6.7, §7.3.2",
            ));
        }

        let mut killed = Vec::with_capacity(targets.len());
        for node in targets {
            self.journal_append(RecordKind::KillIntent(KillIntent {
                agent_id: node.agent_id.clone(),
                was: node.state,
            }))
            .map_err(journal_failure_before_signal)?;
            let signal = node.reap_state != ReapState::ReapedIdle;
            if signal
                && !self.runtime.kill_process_tree_and_wait(
                    node.pid.expect("preflight required a signal target PID"),
                )
            {
                return Err(RpcError::internal(format!(
                    "marion journaled the kill intent for `{}` and signalled its per-node process \
                     tree, but could not observe its PID dead; the intent remains unconfirmed and \
                     the supervisor will not exit (§6.7, §5.7)",
                    node.agent_id.0
                )));
            }
            self.journal_append(RecordKind::KillConfirmed(KillConfirmed {
                agent_id: node.agent_id.clone(),
                exit: ProcessExit {
                    code: None,
                    signal: signal.then_some(9),
                    description: if signal {
                        "marion sent SIGKILL for confirmed session/quit KillTree".into()
                    } else {
                        "confirmed session/quit retired an already ReapedIdle node; no process \
                         existed to signal"
                            .into()
                    },
                },
            }))
            .map_err(journal_failure_after_signal)?;
            killed.push(KilledNode {
                agent_id: node.agent_id.clone(),
                was: node.state,
            });
        }
        self.live.refresh();
        Ok(SessionQuitResult {
            outcome: QuitOutcome::Killed {
                nodes: killed,
                supervisor: SupervisorDisposition::Exiting,
            },
        })
    }

    fn reap_idle_detach_busy(&self) -> Result<SessionQuitResult, RpcError> {
        let nodes = self.nodes();
        let reaping: Vec<_> = nodes
            .iter()
            .filter(|n| {
                n.state == NodeState::Idle
                    && n.reap_state == ReapState::Live
                    && n.reap_intent.is_none()
                    // A non-terminal child is the node its parent's blocking `spawn` is waiting
                    // on. §7.2 names that as a separate refusal even if the child happens to be
                    // between turns and reports `Idle`; reaping it would strand the caller because
                    // ReapedIdle writes no Completion. Roots have no waiting spawn by construction.
                    && n.parent_id().is_none()
            })
            .collect();
        if let Some(node) = reaping.iter().find(|n| n.pid.is_none()) {
            return Err(RpcError::conflict(
                &node.agent_id.0,
                "the idle node has no recorded PID, so marion cannot perform and confirm §7.2's \
                 reap without inventing an observation. Nothing was reaped.",
                "§7.2",
            ));
        }
        let reaped_ids: Vec<_> = reaping.iter().map(|n| n.agent_id.clone()).collect();
        let detached: Vec<_> = nodes
            .iter()
            .filter(|n| {
                !n.state.is_exited()
                    && n.reap_state == ReapState::Live
                    && !reaped_ids.contains(&n.agent_id)
            })
            .map(|n| n.agent_id.clone())
            .collect();
        for node in reaping {
            self.journal_append(RecordKind::ReapIntent(ReapIntent {
                agent_id: node.agent_id.clone(),
                reason: "session/quit reaped an idle node before detaching busy work".into(),
            }))
            .map_err(journal_failure_before_signal)?;
            let pid = node.pid.expect("preflight required every reap PID");
            if !self.runtime.kill_process_tree_and_wait(pid) {
                return Err(RpcError::internal(format!(
                    "marion journaled the reap intent for `{}` and signalled its per-node process \
                     tree, but could not observe PID {pid} dead; the intent remains unconfirmed and \
                     the supervisor will not exit (§7.2, §5.7)",
                    node.agent_id.0
                )));
            }
            self.journal_append(RecordKind::ReapConfirmed(ReapConfirmed {
                agent_id: node.agent_id.clone(),
            }))
            .map_err(journal_failure_after_signal)?;
        }
        self.live.refresh();
        let supervisor = self
            .residency()
            .map(SupervisorDisposition::Resident)
            .unwrap_or(SupervisorDisposition::Exiting);
        let mut guidance = self.guidance();
        // §7.3.2(c) requires the reaped list and the fact that it is resumable. The ids already
        // have a typed field; the latter does not, so omitting it here would make a structurally
        // complete response still leave out the operator's most important recovery fact.
        guidance.reattach.push_str(
            " Nodes named in `reaped` are ReapedIdle, retain their transcripts and ownership, and \
             are resumable.",
        );
        Ok(SessionQuitResult {
            outcome: QuitOutcome::ReapedAndDetached {
                reaped: reaped_ids,
                gate_exposed: detached.clone(),
                detached,
                guidance,
                supervisor,
            },
        })
    }
}

/// §7.3.3's three answers, chosen from the two fields that decide them and nothing else.
///
/// Any non-`Live` reap state is tested **before** the exit. `ReapedIdle` because §7.3.2's
/// disposition (c) reaps a node that is idle rather than one that is finished, and a reaped node
/// whose journal also shows an exit is still the one the operator can bring back. `Orphaned`
/// (§7.2) because this supervisor booted over a record it did not write and holds no channel for
/// the node — `ResubscribeFrom` would promise live events nobody can deliver — while the record
/// itself is complete and the node is the operator's to bring back. Collapsing either into
/// `ReplayOnly` would tell a client its only option is to read, when the node is resumable.
fn attach_mode(state: NodeState, reap_state: ReapState, point: ReplayPoint) -> AttachMode {
    if reap_state != ReapState::Live {
        AttachMode::ReplayResumable(point)
    } else if state.is_exited() {
        AttachMode::ReplayOnly(point)
    } else {
        AttachMode::ResubscribeFrom(point)
    }
}

/// Send a run of a node's events to one client. `false` means the connection is finished.
///
/// Every field a client needs to place the event in the stream comes off the event itself; nothing
/// is derived from the moment of delivery. In particular `ts` is the writer's, not this process's
/// clock — a replayed event that claimed to have happened when it was replayed would make the
/// detached window look like it never existed.
fn deliver_events(
    out: &Outbound,
    agent_id: &AgentId,
    events: &[marion_core::event::Event],
) -> bool {
    for e in events {
        let payload = serde_json::to_value(&e.payload).unwrap_or_else(|err| {
            // Unreachable for a payload this process just read out of an encoded line, and stated
            // rather than defaulted to `null`: a client must be able to tell "the node said
            // nothing" from "marion could not re-encode what the node said".
            serde_json::json!({ "marion_unencodable": err.to_string() })
        });
        let ok = out.notify(Event::NodeEvent {
            agent_id: agent_id.clone(),
            agent_seq: e.agent_seq,
            ts: e.ts,
            provenance: e.provenance.clone(),
            src_seq: e.src_seq.clone(),
            payload,
        });
        if !ok {
            return false;
        }
    }
    true
}

/// A node's stream exists and could not be read — a fault in the supervisor's own storage, not
/// something the caller did, so `Internal` and not a refusal.
fn attach_io_failure(
    id: &AgentId,
    path: &std::path::Path,
    error: &crate::events::EventError,
) -> RpcError {
    RpcError::internal(format!(
        "node `{}`'s event stream at {} could not be read: {error}. A missing file is not this \
         case — that is a node marion never recorded, and it is reported as such.",
        id.0,
        path.display()
    ))
}

fn journal_failure_before_signal(error: crate::journal::JournalError) -> RpcError {
    RpcError::internal(format!(
        "session/quit could not durably journal its intent, so it refused before signalling the \
         node: {error}"
    ))
}

fn journal_failure_after_signal(error: crate::journal::JournalError) -> RpcError {
    RpcError::internal(format!(
        "session/quit changed a process but could not durably journal its confirmation; its intent \
         remains for restart recovery and the supervisor will not exit: {error}"
    ))
}

impl Handle for RegistryHandle {
    fn connected(&self, conn: ConnId) {
        lock(&self.shared).clients.insert(conn);
    }

    fn call(&self, _conn: ConnId, call: &Call, out: &Outbound) -> Result<MethodResult, RpcError> {
        match call {
            Call::NodeGet(p) => self.node_get(&p.agent_id).map(MethodResult::NodeGet),
            Call::TreeSubscribe(_) => Ok(MethodResult::TreeSubscribe(self.subscribe(out))),
            Call::NodeAttach(p) => self
                .node_attach(&p.agent_id, p.pane_stream.is_some(), out)
                .map(MethodResult::NodeAttach),
            // **Not keyed by the connection**, and that is §7.3.1 restated on the way *in*
            // rather than defended on the way out: a node this supervisor owns is not a resource
            // of the client that asked for it, so nothing here records which connection called.
            // That is what makes `gone` able to touch nothing — there is no per-connection node
            // list for it to reap, structurally, rather than by a rule someone must remember.
            Call::AgentSpawn(p) => self
                .agent_spawn(p, out.peer())
                .map(MethodResult::AgentSpawn),
            Call::SessionQuit(p) => self
                .session_quit(&p.disposition)
                .map(MethodResult::SessionQuit),
            Call::NodeResume(p) => self
                .node_resume(p, out.peer())
                .map(MethodResult::NodeResume),
            // Everything else is specified and not built. `Unimplemented` and not `Unsupported`,
            // per `error.rs`: the gap is marion's, not the harness's, and the operator's next move
            // is to check the milestone rather than the node.
            other => Err(RpcError::unimplemented(
                other.method().as_str(),
                format!(
                    "`{}` is specified (§2) and not built. This supervisor answers `node/get`, \
                     `tree/subscribe`, `node/attach`, `agent/spawn`, `session/quit` and \
                     `node/resume`; the remaining methods land with the milestone that needs them.",
                    other.method().as_str()
                ),
                "§2",
            )),
        }
    }

    /// §7.3.1, and the whole of what this handler does about a departure.
    ///
    /// **Nothing happens to any node here**, in either case. For [`ClientGone::SocketClosed`] that
    /// is §7.3.1's invariant: no state, journal, reap, or orphan transition. For
    /// [`ClientGone::Quit`], the explicit call already completed or refused the disposition; doing
    /// it at departure would repeat a successful kill and, worse, turn a refused stale
    /// confirmation into an unconfirmed kill triggered by EOF.
    ///
    /// What does happen is bookkeeping this connection's subscription is dropped, so a supervisor
    /// with no clients holds no queues. It may make a previously requested exit eligible for the
    /// serve loop's grace timer, but `gone` itself neither journals nor exits.
    ///
    /// **After §11 item 28 step 4 that invariant covers a *bridge* connection, and that is the
    /// whole win.** While every node was owned by the bridge process the harness started, a
    /// SIGKILL of that bridge — which s16 measured as what a Claude Code harness really sends,
    /// uncatchable, ~450 ms after its SIGTERM — orphaned a live process at pid 1 with an
    /// unresolved `SpawnIntent` and no pid to recover it by. Once the *supervisor* owns the node,
    /// the same SIGKILL kills a courier: the supervisor sees `Departure::Eof`, this method runs,
    /// and the node's thread, process, worktree and `events.jsonl` are untouched.
    ///
    /// **`self.nodes` is not mentioned below, and that is structural rather than remembered.**
    /// `Handle::call` never records which connection asked for a node, so there is no
    /// per-connection node list here to reap even by accident. Asserted in
    /// `a_departing_client_does_not_touch_a_node_the_supervisor_owns` rather than left to this
    /// comment, because a comment is not a test.
    fn gone(&self, conn: ConnId, gone: &ClientGone, _why: &Departure) {
        debug_assert!(
            gone.nodes_must_be_untouched() || gone.disposition().is_some(),
            "§7.3.1 admits exactly two readings and both are handled"
        );
        let mut g = lock(&self.shared);
        g.subs.retain(|s| s.conn() != conn);
        // A node stream this connection was following. Dropping the cursor is the whole of it:
        // §7.3.1's invariant is about nodes, and a reader is not one. The node goes on running and
        // goes on writing its `events.jsonl`, which is what makes the *next* client's attach a
        // replay rather than a hole.
        g.attached.retain(|a| a.conn != conn);
        g.clients.remove(&conn);
        drop(g);
        // The display plane's half of the same invariant, and it is the half §7.3.1 is really
        // about: a client that was SIGKILLed while holding a node's keyboard must not leave that
        // node permanently read-only. Dropping the leases releases the writer slots — `WriteLease`
        // does it on `Drop`, so there is no cleanup here to forget — and `unlisten` stops the byte
        // fan-out to a channel nobody is drawing. **The node is not touched**: its process, its
        // pty, its recording and its size are exactly as they were.
        let mut panes = lock(&self.panes);
        panes.leases.remove(&conn);
        for entry in panes.hosts.values() {
            entry.host().unlisten(conn);
        }
    }

    /// §2's inbound table. Direct in-process callers retain the original compatibility surface;
    /// real transports use [`Self::input_with_out`] so pane-v1 failure is visible on the exact
    /// connection.
    fn input(&self, conn: ConnId, input: &marion_core::proto::Input) {
        self.deliver_input(conn, input);
    }

    fn input_with_out(&self, conn: ConnId, input: &marion_core::proto::Input, out: &Outbound) {
        self.deliver_input_with_out(conn, input, Some(out));
    }

    /// §2's notifications, driven by the accept loop's heartbeat — see [`Handle::tick`] for why
    /// that loop and not a fourth thread.
    ///
    /// Both halves, in this order. `flush` pushes what the *journal* said (a node appeared, a node
    /// changed state); `pump_attached` pushes what a *node* said. Journal first because a client
    /// that learns of an event on a node it has not been told exists has to hold it, and §2 puts
    /// `tree/node-added` before anything else about a node for exactly that reason.
    fn tick(&self) {
        self.prune_completed_panes();
        self.flush();
        self.pump_attached();
    }

    fn exiting(&self) -> bool {
        self.exiting.load(Ordering::SeqCst)
    }

    /// §5.7's exit predicate, in full and with nothing added to it.
    ///
    /// *"With **zero clients and zero non-terminal nodes**, the supervisor MAY exit after an idle
    /// grace period"* — two clauses, and neither of them is *"and some client asked nicely first"*.
    /// This used to be gated on an explicit `session/quit` having arrived, which meant the one
    /// departure §7.3.1 is actually about — a client that was killed and could say nothing — left
    /// a supervisor that could never exit at all, over a journal with nothing left in it. That is
    /// not §7.3.1's invariant. §7.3.1 is about **nodes**, and this touches none: an empty registry
    /// has nothing to touch, and a non-empty one is what [`Self::resident_reason`] answers with.
    ///
    /// The waiting is the accept loop's, and it is what covers the window before the starting
    /// client has connected: a supervisor is published and dialled within milliseconds, and §5.7's
    /// grace is five minutes. See [`crate::serve::DEFAULT_IDLE_GRACE`] — a supervisor launched with
    /// a grace of zero really could leave before its launcher arrived, which is one of the reasons
    /// `marion run` no longer asks for one.
    /// **The node table is a second guard, not a replacement for the journal's.**
    ///
    /// §5.7's clause is *"zero non-terminal nodes"*, and [`Self::residency`] answers it from the
    /// journal — which is the right primary source, because it is the only one that survives a
    /// restart and the only one that knows about nodes this process did not start.
    ///
    /// What it cannot see is a node whose thread is running and whose **terminal journal write
    /// failed**. `journal::record` does not propagate its error, so a full disk or a revoked
    /// directory leaves a node that is running with a tree that says it finished — and a
    /// journal-only predicate would let the supervisor exit straight through it, leaving exactly
    /// the untracked live process §9's M2 criteria forbid. The table cannot be wrong in that
    /// direction: an entry's `outcome` is set by the node's own thread, in this process, after
    /// `run_spawn` has returned.
    ///
    /// It is a **second** guard rather than the guard, because it is wrong in the other direction:
    /// it is empty after a restart, and it never knew about a node another process started. Both
    /// have to hold.
    fn idle_exit_eligible(&self) -> bool {
        if !lock(&self.shared).clients.is_empty() {
            return false;
        }
        if self.running_nodes() > 0 {
            return false;
        }
        self.live.refresh();
        self.residency().is_none()
    }

    fn idle_exit_grace_waived(&self) -> bool {
        self.quit_waived_grace.load(Ordering::SeqCst)
    }

    fn begin_idle_exit(&self) -> bool {
        let _decision = lock(&self.quit);
        if !self.idle_exit_eligible() {
            return false;
        }
        // Every node this supervisor owns has finished — that is what the predicate above just
        // established — so this is the epilogue, not a wait. See [`Self::join_finished_nodes`].
        self.join_finished_nodes();
        let result = self
            .journal_append(RecordKind::SupervisorExited(SupervisorExited {}))
            .map_err(journal_failure_after_signal);
        match result {
            Ok(()) => {
                self.live.refresh();
                self.exiting.store(true, Ordering::SeqCst);
                true
            }
            Err(error) => {
                eprintln!("marion: {error}");
                false
            }
        }
    }
}

/// The tree, as summaries, counting what could not be described.
///
/// `panes` is the set of nodes this supervisor holds a pty for, read **before** the shared lock was
/// taken — see [`RegistryHandle::pane_ids`] for why it is a snapshot passed in rather than a map
/// consulted here.
fn project(tree: &Replay, g: &mut Shared, panes: &HashSet<AgentId>) -> Vec<NodeSummary> {
    let mut out = Vec::new();
    let mut lost = 0usize;
    for n in tree.nodes() {
        match summarize(n, panes.contains(&n.agent_id)) {
            Ok(s) => out.push(s),
            Err(_) => lost += 1,
        }
    }
    g.unprojectable = lost;
    out
}

/// What has changed since the last time anybody was told, in journal order.
///
/// Two events and not one: §2's `tree/node-added` exists because *"a client that learned of nodes
/// only from state changes would show a tree that is missing exactly the nodes currently being
/// created — the ones an operator is most likely watching."*
///
/// Every `ts` is the journal's, never this process's clock — see
/// [`marion_core::registry::ReplayedNode::first_ts`] for why a follower's `now()` is the wrong
/// answer on a field a client renders as when the thing occurred.
fn collect(r: &Registry, g: &mut Shared, panes: &HashSet<AgentId>) -> Vec<Event> {
    let mut events = Vec::new();
    for n in r.tree().nodes() {
        let now = Told {
            state: n.state,
            reap_state: n.reap_state,
        };
        match g.told.get(&n.agent_id) {
            None => {
                // A node marion cannot describe produces no `tree/node-added` — there is no summary
                // to put in one — but it is still recorded as told, so it is not re-examined on
                // every flush. `project` is what counts it.
                if let Ok(node) = summarize(n, panes.contains(&n.agent_id)) {
                    events.push(Event::NodeAdded {
                        node,
                        ts: journal_ts(n.first_ts),
                    });
                }
                g.told.insert(n.agent_id.clone(), now);
            }
            Some(before) if *before != now => {
                events.push(Event::NodeState {
                    agent_id: n.agent_id.clone(),
                    state: n.state,
                    reap_state: n.reap_state,
                    ts: journal_ts(n.state_ts),
                });
                g.told.insert(n.agent_id.clone(), now);
            }
            Some(_) => {}
        }
    }
    events
}

/// The journal's own time for a transition.
///
/// The `None` case is reachable and is not a decision this function may duck: a node replayed from
/// records written before `first_ts`/`state_ts` existed has neither. The epoch is used rather than
/// `now()` deliberately — a timestamp a client can *see* is wrong is better than one that is wrong
/// and plausible, which is the field-name-lies class this codebase refuses elsewhere.
fn journal_ts(ts: Option<marion_core::encoding::SystemTime>) -> marion_core::encoding::SystemTime {
    ts.unwrap_or_else(|| marion_core::encoding::SystemTime::from_unix_millis(0))
}

/// Send to every subscriber, dropping the ones that have gone.
///
/// [`Outbound::send`] never blocks, so this cannot be slowed by a client — see `serve.rs`: a full
/// queue is a verdict about that client, and §5.7 is what makes it the right one.
fn deliver(g: &mut Shared, events: &[Event]) {
    if events.is_empty() {
        return;
    }
    g.subs.retain(|s| {
        events.iter().all(|e| {
            s.send(&marion_core::proto::Frame::Notification(
                marion_core::proto::Notification::new(e.clone()),
            ))
        })
    });
}

/// A [`RegistryHandle`] flushed by a thread of its own.
///
/// The registry has its own follower (`LiveRegistry`) and this is a second loop over the result of
/// the first, which is deliberate: the follower's job is to be *current*, this one's is to be
/// *heard*, and a follower that also pushed would have a client's socket inside the lock that keeps
/// the tree current.
pub struct Broadcast {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Broadcast {
    /// Flush every `interval`.
    ///
    /// **The loop reads the stop flag before its flush and returns after it** — `LiveRegistry`'s
    /// rule, for the same reason: the transitions written just before a shutdown are exactly the
    /// ones a watching client cares about, and a loop that returned on the flag before flushing
    /// would drop them.
    pub fn start(handle: Arc<RegistryHandle>, interval: std::time::Duration) -> Broadcast {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                loop {
                    let done = stop.load(std::sync::atomic::Ordering::SeqCst);
                    handle.flush();
                    if done {
                        return;
                    }
                    std::thread::sleep(interval);
                }
            })
        };
        Broadcast {
            stop,
            thread: Some(thread),
        }
    }

    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Broadcast {
    fn drop(&mut self) {
        self.halt();
    }
}

/// A poisoned lock is taken, not unwrapped — `registry.rs`'s rule and §5.7's requirement.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The read point the registry is currently serving, for a caller that wants it without a client.
pub fn read_point(live: &LiveRegistry) -> ReplayPoint {
    live.read(|r| r.read_point())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_bootstrap::fakes::{ManualClock, SequenceRng};
    use marion_core::contract::{ExitStatus, ProcessExit};
    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::journal::{
        Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, StateChanged, WriterId, encode,
    };
    use marion_core::node::BlockReason;
    use marion_core::proto::Frame;
    use marion_testsupport::{append, scratch, until};
    use std::io::Write;
    use std::path::Path;
    use std::time::Duration;

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    /// **The child's launch refusal carries the thread's own sentence, as the root's does.** A
    /// codex row granting `read` is refused at `compile` with a sentence naming the tool and the
    /// harness; the answer to `agent/spawn` must quote it, not replace it with "a worktree, a
    /// configuration document, or the harness `--version` probe".
    #[test]
    fn a_child_launch_failure_quotes_the_reason_the_thread_filed() {
        let why = "compiling the child's launch: codex: no mapping for marion tool `read`; this \
                   harness's adapter provides none";
        let err = spawn_failed_before_the_process_existed(&id("a1"), Some(why));
        let msg = err.to_string();
        assert!(msg.contains(why), "the sentence survives: {msg}");
        assert!(msg.contains("a1"), "and the node is named: {msg}");
        let none = spawn_failed_before_the_process_existed(&id("a1"), None).to_string();
        assert!(
            none.contains("filed no reason"),
            "a thread that filed nothing is reported as such: {none}"
        );
    }

    fn line(seq: u64, ms: u64, kind: RecordKind) -> Vec<u8> {
        encode(&JournalRecord {
            writer: WriterId("w".into()),
            seq,
            ts: SystemTime::from_unix_millis(ms),
            mono_ns: seq,
            provenance: marion_core::ir::Provenance::marion(),
            src_seq: None,
            kind,
        })
        .expect("a record encodes")
    }

    /// The harness comes from the agent type, as a real writer's would — and the projection reads
    /// it back from the **journal**, not from the type, because the journal records what was
    /// actually launched.
    fn intent(agent: &str, parent: Option<&str>, ty: &str, depth: u32) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id(agent),
            parent_id: parent.map(id),
            agent_type: ty.into(),
            harness: agent_type::builtin(ty)
                .map(|t| t.harness)
                .unwrap_or(Harness::Codex),
            depth,
            task_id: None,
            timeout_secs: None,
        })
    }

    fn replay_of(records: &[Vec<u8>]) -> Replay {
        let mut r = Replay::default();
        for b in records {
            r.extend(b);
        }
        r
    }

    fn node_of(records: &[Vec<u8>], agent: &str) -> ReplayedNode {
        replay_of(records).get(&id(agent)).unwrap().clone()
    }

    /// The projection, on a node the journal fully describes — including the two fields
    /// `registry.rs` refused to fabricate.
    #[test]
    fn a_summary_falls_back_to_the_agent_type_and_names_nothing_it_was_not_told() {
        let n = node_of(
            &[line(0, 1, intent("child", Some("root"), "codex-impl", 1))],
            "child",
        );
        let s = summarize(&n, false).expect("a fully described node projects");
        assert_eq!(s.agent_id, id("child"));
        assert_eq!(s.parent_id, Some(id("root")));
        assert_eq!(s.agent_type, "codex-impl");
        assert_eq!(s.harness, Harness::Codex);
        assert_eq!(s.depth, 1);
        assert_eq!(s.state, NodeState::Spawning);
        assert_eq!(
            s.timeout,
            agent_type::builtin("codex-impl").unwrap().timeout,
            "this intent records no bound — an older journal, or a launch marion timed nothing of \
             — so §3.1's agent-type default is the only thing marion can honestly report"
        );
        assert_eq!(
            s.timeout,
            marion_core::encoding::Duration::from_secs(agent_type::DEFAULT_TIMEOUT_SECS),
            "and that fallback really is §3.1's 900 s default, not a value invented here"
        );
        assert_eq!(
            s.name, None,
            "nothing sets `Node.name` yet, so `None` is what the journal says rather than a \
             placeholder for what marion does not know"
        );
    }

    /// **A recorded bound outranks the agent type's, because it is the one the node is under.**
    ///
    /// The sibling above is the fallback; this is the normal case. `marion run --timeout 300` and
    /// `spawn`'s `timeout_secs` both resolve a bound before the process exists, and the intent
    /// records it — so the number `marion tree`'s detail pane prints is the clock the node is
    /// actually being held to, and not its type's default wearing that clock's name.
    #[test]
    fn a_summary_reports_the_bound_the_launch_resolved() {
        let mut i = match intent("child", Some("root"), "codex-impl", 1) {
            RecordKind::SpawnIntent(i) => i,
            other => panic!("{other:?}"),
        };
        i.timeout_secs = Some(300);
        let n = node_of(&[line(0, 1, RecordKind::SpawnIntent(i))], "child");
        let s = summarize(&n, false).expect("a fully described node projects");
        assert_eq!(s.timeout, marion_core::encoding::Duration::from_secs(300));
        assert_ne!(
            s.timeout,
            agent_type::builtin("codex-impl").unwrap().timeout,
            "the type's default is what this node is *not* running under"
        );
    }

    /// **NC — a node marion cannot describe is refused by name, never summarized with invented
    /// fields.**
    ///
    /// Three ways, three sentences. The failure this rules out is the tempting one: default the
    /// missing fields (`Harness::Codex`, depth 0, the 900 s bound) and hand back a summary that
    /// reads exactly like a real node's. A client cannot tell those apart, which is the
    /// partial-presented-as-complete shape this repo keeps refusing.
    #[test]
    fn an_undescribable_node_is_named_rather_than_filled_in() {
        // (1) records about a node, no identity for it.
        let orphan = node_of(
            &[line(
                0,
                1,
                RecordKind::Spawned(Spawned {
                    agent_id: id("no-intent"),
                    harness_version: "0.9.0".into(),
                    model: None,
                    pid: Some(3),
                    start_id: None,
                }),
            )],
            "no-intent",
        );
        assert_eq!(summarize(&orphan, false), Err(Unprojectable::NoIntent));
        let e = Unprojectable::NoIntent.as_error(&id("no-intent"));
        assert_eq!(e.kind(), Some(FailureKind::Internal));
        assert!(e.message.contains("SpawnIntent"), "{e}");

        // (2) an agent type this build does not have — so §3.1's bound has no source.
        let unknown = node_of(&[line(0, 1, intent("a", None, "codex-turbo", 0))], "a");
        assert_eq!(
            summarize(&unknown, false),
            Err(Unprojectable::UnknownAgentType("codex-turbo".into()))
        );
        let e = Unprojectable::UnknownAgentType("codex-turbo".into()).as_error(&id("a"));
        assert_eq!(
            e.kind(),
            Some(FailureKind::NotFound),
            "`error.rs` names an agent type among the things NotFound is for"
        );
        assert!(e.message.contains("codex-turbo"), "{e}");

        // (3) a depth `NodeSummary`'s u8 cannot hold. §6.1's default max_depth is 3, so this is a
        // writer producing nonsense — and saturating it would place the node somewhere it is not.
        let deep = node_of(&[line(0, 1, intent("a", None, "codex-impl", 300))], "a");
        assert_eq!(
            summarize(&deep, false),
            Err(Unprojectable::DepthOutOfRange(300))
        );
        assert!(
            summarize(
                &node_of(&[line(0, 1, intent("a", None, "codex-impl", 255))], "a"),
                false
            )
            .is_ok(),
            "255 fits, so the boundary is the type's and not an arbitrary cap"
        );
    }

    /// **A user-defined type is projected from its recorded bound.** `summarize` runs under the
    /// shared registry lock and reads no file, so it cannot resolve a `.marion/agents.toml` row —
    /// but every production writer journals the bound the node launched under, and that is the
    /// one thing the type was needed for. Only a journal that names no built-in *and* records no
    /// bound is unprojectable: an intent written before the field existed, for a type this build
    /// cannot look up.
    #[test]
    fn an_unknown_type_projects_from_its_recorded_bound_and_not_otherwise() {
        let intent_with = |bound: Option<u64>| {
            RecordKind::SpawnIntent(SpawnIntent {
                agent_id: id("r"),
                parent_id: None,
                agent_type: "reviewer".into(),
                harness: Harness::Codex,
                depth: 1,
                task_id: None,
                timeout_secs: bound,
            })
        };
        let bounded = node_of(&[line(0, 1, intent_with(Some(120)))], "r");
        let s = summarize(&bounded, false).expect("the recorded bound is enough");
        assert_eq!(s.agent_type, "reviewer");
        assert_eq!(s.harness, Harness::Codex);
        assert_eq!(s.timeout, marion_core::encoding::Duration::from_secs(120));
        let unbounded = node_of(&[line(0, 1, intent_with(None))], "r");
        assert_eq!(
            summarize(&unbounded, false),
            Err(Unprojectable::UnknownAgentType("reviewer".into()))
        );
    }

    /// **A resume re-resolves the recorded type from today's file, and refuses if the harness
    /// moved.** The journal records the name and the harness; the file is the operator's and may
    /// have been edited since. A row that now names another harness would relaunch a session that
    /// harness has never seen under a node that claims to be the same one.
    #[test]
    fn a_recorded_type_is_resumed_only_under_the_harness_it_was_journaled_with() {
        let root = scratch("handler-recorded-type");
        let repo = marion_testsupport::fixture_repo(&root);
        let file = repo.join(crate::run::AGENT_TYPES_FILE);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let row = |harness: &str| {
            format!(
                "[[agent]]\nname = \"reviewer\"\nharness = \"{harness}\"\ndescription = \"r\"\n"
            )
        };
        let intent = SpawnIntent {
            agent_id: id("r"),
            parent_id: None,
            agent_type: "reviewer".into(),
            harness: Harness::Codex,
            depth: 1,
            task_id: None,
            timeout_secs: Some(60),
        };
        std::fs::write(&file, row("codex")).unwrap();
        assert_eq!(
            recorded_type(&repo, &intent).unwrap().harness,
            Harness::Codex
        );
        std::fs::write(&file, row("gemini")).unwrap();
        let e = recorded_type(&repo, &intent).unwrap_err();
        assert!(
            e.message.contains("journaled as codex") && e.message.contains("now says gemini"),
            "{e}"
        );
        std::fs::remove_file(&file).unwrap();
        let e = recorded_type(&repo, &intent).unwrap_err();
        assert_eq!(e.kind(), Some(FailureKind::NotFound), "{e}");
        // A built-in is resolved regardless of the file, and the file's own refusal is its own.
        let builtin = SpawnIntent {
            agent_type: "claude".into(),
            harness: Harness::ClaudeCode,
            ..intent.clone()
        };
        assert_eq!(
            recorded_type(&repo, &builtin).unwrap().harness,
            Harness::ClaudeCode
        );
        std::fs::write(&file, "[[agent]\n").unwrap();
        let e = recorded_type(&repo, &builtin).unwrap_err();
        assert!(e.message.contains("agents.toml"), "{e}");
    }

    /// Build a handle over a journal file, plus the recording sink a subscriber would be.
    struct Fx {
        _dir: marion_testsupport::Scratch,
        path: std::path::PathBuf,
        handle: Arc<RegistryHandle>,
    }

    fn fx(tag: &str) -> Fx {
        fx_with(tag, vec![intent("root", None, "claude", 0)])
    }

    fn fx_with(tag: &str, records: Vec<RecordKind>) -> Fx {
        fx_with_runtime(tag, records, Arc::new(SystemQuitRuntime))
    }

    /// **The registry boots before the records are written, which is the production order.**
    ///
    /// `marion run` starts the supervisor and *then* journals its root, so every node these tests
    /// are about is a node the supervisor watched arrive. Writing the journal first and booting
    /// over it is a different situation entirely — §7.2's restart, where a node already `Live` at
    /// boot is one this supervisor has no record of deciding and is marked `Orphaned`
    /// (`restart.rs`). A fixture in that shape would have every test below asserting over a tree of
    /// orphans while claiming to describe a live fleet. `registry.rs` covers the restart order
    /// directly.
    fn fx_with_runtime(tag: &str, records: Vec<RecordKind>, runtime: Arc<dyn QuitRuntime>) -> Fx {
        let dir = scratch(tag);
        let path = dir.join("journal.jsonl");
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot_path(&path).unwrap(),
            std::time::Duration::from_millis(2),
        ));
        for (seq, kind) in records.into_iter().enumerate() {
            append(&path, &line(seq as u64, 1_000 + seq as u64, kind));
        }
        assert!(
            live.read(|r| r.restart_marks().is_empty()),
            "the supervisor booted over an empty journal; it lost nothing"
        );
        live.refresh();
        Fx {
            _dir: dir,
            path,
            handle: RegistryHandle::with_runtime(live, runtime),
        }
    }

    /// Records the process-tree operations selected by a disposition without asking the host
    /// process table to cooperate. The real runtime delegates to `run.rs`; these tests are about
    /// the handler's selection and ordering, so an injected observation makes a missed or extra
    /// per-node operation an exact assertion rather than a timing-dependent survivor check.
    #[derive(Default)]
    struct RecordingRuntime {
        killed: Mutex<Vec<i32>>,
    }

    impl RecordingRuntime {
        fn killed(&self) -> Vec<i32> {
            lock(&self.killed).clone()
        }
    }

    impl QuitRuntime for RecordingRuntime {
        fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
            lock(&self.killed).push(pid);
            true
        }
    }

    fn recording_fx_with(tag: &str, records: Vec<RecordKind>) -> (Fx, Arc<RecordingRuntime>) {
        let runtime = Arc::new(RecordingRuntime::default());
        let fx = fx_with_runtime(tag, records, runtime.clone());
        (fx, runtime)
    }

    fn spawned(agent: &str, pid: i32) -> RecordKind {
        RecordKind::Spawned(Spawned {
            agent_id: id(agent),
            harness_version: "test".into(),
            model: None,
            pid: Some(pid),
            start_id: None,
        })
    }

    fn state(agent: &str, state: NodeState) -> RecordKind {
        RecordKind::StateChanged(StateChanged {
            agent_id: id(agent),
            state,
        })
    }

    fn quit(
        fx: &Fx,
        disposition: marion_core::proto::QuitDisposition,
    ) -> Result<marion_core::proto::result::SessionQuitResult, RpcError> {
        let out = crate::serve::sink(ConnId(9));
        match fx.handle.call(
            ConnId(9),
            &Call::SessionQuit(marion_core::proto::params::SessionQuitParams { disposition }),
            &out,
        )? {
            MethodResult::SessionQuit(r) => Ok(r),
            other => panic!("wrong result: {}", other.method().as_str()),
        }
    }

    /// The file's records, decoded — the envelope, not just the payload. `journal_tags` answers
    /// *what* was written; this answers *who wrote it and in what order*, which is a different
    /// question and the one §4.2's `seq` and `mono_ns` are the answer to.
    fn journal_records(path: &Path) -> Vec<JournalRecord> {
        std::fs::read(path)
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| marion_core::journal::decode(l).expect("a record marion wrote decodes"))
            .collect()
    }

    fn journal_tags(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                value["kind"]
                    .as_object()
                    .and_then(|o| o.keys().next())
                    .cloned()
                    .unwrap_or_else(|| value["kind"].as_str().unwrap_or("unknown").to_string())
            })
            .collect()
    }

    /// A pair of `Outbound`s is only obtainable from a live connection, so the subscription tests
    /// run a real server over a real socket — which is also the only way to assert that the
    /// notification reaches a *client* rather than a channel.
    struct Wired {
        fx: Fx,
        server: Option<crate::serve::Server>,
        dir: std::path::PathBuf,
        sock: std::path::PathBuf,
    }

    impl Wired {
        fn new(tag: &str) -> Wired {
            let fx = fx(tag);
            let dir = std::path::PathBuf::from(format!("/tmp/mh-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let paths = crate::socket::socket_paths(&dir, Path::new("/p"), 1);
            let crate::socket::Acquired::Serving(serving) = crate::socket::acquire(&paths).unwrap()
            else {
                panic!("nothing was listening")
            };
            let server = crate::serve::Server::start(
                serving,
                Arc::clone(&fx.handle) as Arc<dyn crate::serve::Handle>,
            );
            Wired {
                fx,
                server: Some(server),
                sock: paths.socket().to_path_buf(),
                dir,
            }
        }

        fn dial(&self) -> std::os::unix::net::UnixStream {
            let s = std::os::unix::net::UnixStream::connect(&self.sock).expect("dial");
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            s
        }
    }

    impl Drop for Wired {
        fn drop(&mut self) {
            if let Some(s) = self.server.take() {
                s.stop();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn call(s: &mut std::os::unix::net::UnixStream, call: Call, id: i64) {
        let f = Frame::Request(marion_core::proto::Request::new(
            marion_core::proto::RequestId::Number(id),
            call,
        ));
        s.write_all(f.to_line().as_bytes()).unwrap();
        s.flush().unwrap();
    }

    fn next_frame(r: &mut std::io::BufReader<std::os::unix::net::UnixStream>) -> Frame {
        use std::io::BufRead;
        let mut line = String::new();
        assert!(r.read_line(&mut line).unwrap() > 0, "the socket closed");
        Frame::from_line(&line).expect("well-formed")
    }

    /// `node/get` over the socket, against a tree that came out of a journal — the request/response
    /// shape, end to end.
    #[test]
    fn node_get_answers_from_the_journal_and_refuses_a_node_it_has_no_record_of() {
        let w = Wired::new("handler-get");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());

        call(
            &mut c,
            Call::NodeGet(marion_core::proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_core::proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::NodeGet(got) = marion_core::proto::Method::NodeGet
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(got.node.agent_id, id("root"));
        assert_eq!(got.node.harness, Harness::ClaudeCode);
        assert_eq!(got.node.depth, 0);

        // A node the journal does not record is a refusal that says how much has been read, so an
        // operator can tell "no such node" from "not yet".
        call(
            &mut c,
            Call::NodeGet(marion_core::proto::params::NodeGetParams {
                agent_id: id("nobody"),
            }),
            2,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_core::proto::Outcome::Error(e) = resp.outcome else {
            panic!("expected a refusal")
        };
        assert_eq!(e.kind(), Some(FailureKind::NotFound));
        assert!(e.is_refusal(), "a missing node is the caller's business");
        assert!(e.message.contains("records read"), "{e}");
    }

    /// **The subscription shape, against the real journal**: a snapshot, then a notification for a
    /// node that appeared afterwards — written by a *second* writer, which is the case the registry
    /// tails the file for in the first place.
    #[test]
    fn tree_subscribe_returns_a_snapshot_and_then_narrates_what_the_journal_says_next() {
        let w = Wired::new("handler-sub");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let _b = Broadcast::start(
            Arc::clone(&w.fx.handle),
            std::time::Duration::from_millis(2),
        );

        call(
            &mut c,
            Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_core::proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_core::proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(snap.nodes.len(), 1, "the root the journal already had");
        assert_eq!(snap.nodes[0].agent_id, id("root"));
        assert_eq!(snap.read_point.records, 1);

        // A different process appends a child and then moves it.
        append(
            &w.fx.path,
            &line(1, 2_000, intent("child", Some("root"), "codex-impl", 1)),
        );
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("a node that appeared must arrive as tree/node-added")
        };
        let Event::NodeAdded { node, ts } = n.event else {
            panic!("expected tree/node-added, got {:?}", n.event.method())
        };
        assert_eq!(node.agent_id, id("child"));
        assert_eq!(node.parent_id, Some(id("root")));
        assert_eq!(
            ts,
            SystemTime::from_unix_millis(2_000),
            "the journal's time, not the follower's — a client renders this as when it happened"
        );

        append(
            &w.fx.path,
            &line(
                2,
                3_500,
                RecordKind::StateChanged(StateChanged {
                    agent_id: id("child"),
                    state: NodeState::Running,
                }),
            ),
        );
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("expected a notification")
        };
        let Event::NodeState {
            agent_id,
            state,
            reap_state,
            ts,
        } = n.event
        else {
            panic!("expected node/state")
        };
        assert_eq!(agent_id, id("child"));
        assert_eq!(state, NodeState::Running);
        assert_eq!(
            reap_state,
            ReapState::Live,
            "§7.6 gates on the disjunction, so the two travel in one message"
        );
        assert_eq!(ts, SystemTime::from_unix_millis(3_500));
    }

    /// **NC — a node the snapshot could not describe is excluded *and counted*, never silently
    /// dropped.**
    ///
    /// `TreeSubscribeResult` has nowhere to say "and there are N I could not describe", so the count
    /// lives on the supervisor. The assertion is that it is not zero: a gap that is admitted is a
    /// different thing from a gap that is invisible, and the invisible version is the one §11 item
    /// 23 keeps naming.
    #[test]
    fn a_node_the_snapshot_cannot_describe_is_counted_rather_than_dropped_into_silence() {
        let w = Wired::new("handler-lost");
        // A node with records and no identity, written by a second writer.
        append(
            &w.fx.path,
            &line(
                1,
                2_000,
                RecordKind::Spawned(Spawned {
                    agent_id: id("headless"),
                    harness_version: "0.9.0".into(),
                    model: None,
                    pid: Some(9),
                    start_id: None,
                }),
            ),
        );
        assert!(until(
            || w.fx.handle.live.read(|r| r.tree().nodes().len()) == 2
        ));

        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        call(
            &mut c,
            Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_core::proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_core::proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(
            snap.nodes
                .iter()
                .map(|n| n.agent_id.clone())
                .collect::<Vec<_>>(),
            [id("root")],
            "a node with no identity is not described"
        );
        assert_eq!(
            w.fx.handle.unprojectable(),
            1,
            "and it is not invisible either"
        );
    }

    /// A subscriber that goes away stops being one, so a supervisor with no clients holds no queues
    /// — and, per §7.3.1, nothing else happens at all.
    #[test]
    fn a_departed_client_stops_being_a_subscriber_and_nothing_else_changes() {
        let w = Wired::new("handler-gone");
        let before = w.fx.handle.live.read(|r| r.tree().nodes().len());
        {
            let mut c = w.dial();
            let mut r = std::io::BufReader::new(c.try_clone().unwrap());
            call(
                &mut c,
                Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
                1,
            );
            next_frame(&mut r);
            assert_eq!(w.fx.handle.subscribers(), 1);
        }
        assert!(
            until(|| w.fx.handle.subscribers() == 0),
            "a closed connection is not a subscriber"
        );
        assert_eq!(
            w.fx.handle.live.read(|r| r.tree().nodes().len()),
            before,
            "§7.3.1: from the registry's point of view nothing happened"
        );
    }

    /// **NC — disposition (b) changes no node and cannot end the supervisor.**
    ///
    /// The journal bytes are the authority on the first half: comparing a summary before and after
    /// could miss an appended record that happens not to project into state. The second call on the
    /// same socket is the authority on the second half: a `Resident` word in a response is not proof
    /// that the server actually remained resident.
    #[test]
    fn detach_all_leaves_the_journal_and_node_untouched_and_the_supervisor_serving() {
        let fx = fx("handler-quit-detach");
        let before = std::fs::read(&fx.path).unwrap();
        let state = fx
            .handle
            .live
            .read(|r| r.tree().get(&id("root")).unwrap().state);
        let out = crate::serve::sink(ConnId(1));

        let result = fx
            .handle
            .call(
                ConnId(1),
                &Call::SessionQuit(marion_core::proto::params::SessionQuitParams {
                    disposition: marion_core::proto::QuitDisposition::DetachAll,
                }),
                &out,
            )
            .expect("detach is implemented, not refused");
        let MethodResult::SessionQuit(result) = result else {
            panic!("wrong result type")
        };
        let marion_core::proto::QuitOutcome::Detached {
            detached,
            gate_exposed,
            guidance,
            supervisor,
        } = result.outcome
        else {
            panic!("detach returned another disposition's outcome")
        };
        assert_eq!(detached, [id("root")]);
        assert_eq!(gate_exposed, [id("root")]);
        assert!(guidance.reattach.contains("tree/subscribe"));
        assert!(guidance.stop_fleet.contains("session/quit"));
        assert!(guidance.reattach.contains("denied unattended"));
        assert!(guidance.reattach.contains("is_error:true"));
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::SpawnOutstanding
            )
        );
        assert_eq!(std::fs::read(&fx.path).unwrap(), before);
        assert_eq!(
            fx.handle
                .live
                .read(|tree| tree.tree().get(&id("root")).unwrap().state),
            state
        );

        assert!(matches!(
            fx.handle.call(
                ConnId(1),
                &Call::NodeGet(marion_core::proto::params::NodeGetParams {
                    agent_id: id("root")
                }),
                &out,
            ),
            Ok(MethodResult::NodeGet(_))
        ));
        assert!(!fx.handle.exiting());
        assert!(!fx.handle.idle_exit_eligible());
    }

    /// **NC — disposition (a) is unreachable when the confirmation is not the live render.**
    ///
    /// The empty runtime trace is the negative control: checking only for a refusal could pass
    /// after a buggy implementation signalled first and noticed the mismatch second. It and the
    /// byte-identical journal prove the refusal preceded every side effect.
    #[test]
    fn kill_tree_refuses_a_missing_or_stale_confirmed_list_before_signalling_anything() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-kill-unconfirmed",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 101),
                state("root", NodeState::Running),
            ],
        );
        let before = std::fs::read(&fx.path).unwrap();

        let error = quit(
            &fx,
            marion_core::proto::QuitDisposition::KillTree { confirmed: vec![] },
        )
        .expect_err("an empty render did not confirm a live root");
        assert_eq!(error.kind(), Some(FailureKind::Refused));
        assert!(error.message.contains("confirmed"), "{error}");
        assert!(
            runtime.killed().is_empty(),
            "the mismatch was checked after signalling"
        );
        assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    }

    /// Disposition (a), positively: every live node is killed through its own recorded PID, its
    /// prior activity is returned, each intent precedes its confirmation, and only then is the
    /// supervisor exit recorded. Two distinct recorded PIDs make a one-node or one-group
    /// implementation visible in the runtime trace.
    #[test]
    fn kill_tree_kills_each_non_terminal_node_and_journals_each_pair_before_exit() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-kill",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 101),
                state("root", NodeState::Running),
                intent("child", Some("root"), "codex-impl", 1),
                spawned("child", 202),
                state("child", NodeState::Blocked(BlockReason::Permission)),
                intent("done", Some("root"), "codex-impl", 1),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );

        let disposition = marion_core::proto::QuitDisposition::KillTree {
            confirmed: vec![id("child"), id("root")],
        };
        fx.handle.connected(ConnId(9));
        let result = quit(&fx, disposition.clone()).expect("the exact set was confirmed");
        let marion_core::proto::QuitOutcome::Killed { nodes, supervisor } = result.outcome else {
            panic!("kill returned another disposition's outcome")
        };
        assert_eq!(
            nodes,
            [
                marion_core::proto::KilledNode {
                    agent_id: id("root"),
                    was: NodeState::Running,
                },
                marion_core::proto::KilledNode {
                    agent_id: id("child"),
                    was: NodeState::Blocked(BlockReason::Permission),
                },
            ]
        );
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Exiting
        );
        assert_eq!(runtime.killed(), [101, 202], "one operation per live node");
        let mut tags = journal_tags(&fx.path);
        assert_eq!(
            &tags[8..],
            ["KillIntent", "KillConfirmed", "KillIntent", "KillConfirmed",],
            "one intent/act/confirm pair per node"
        );
        assert!(
            !fx.handle.begin_idle_exit(),
            "§5.7 forbids the exit record while even the quitting client remains"
        );
        fx.handle.gone(
            ConnId(9),
            &ClientGone::Quit(disposition),
            &Departure::QuitCompleted,
        );
        assert!(
            fx.handle.begin_idle_exit(),
            "after the configured grace, zero clients and zero non-terminals permit exit"
        );
        tags = journal_tags(&fx.path);
        assert_eq!(tags.last().unwrap(), "SupervisorExited");
        assert!(fx.handle.exiting());
        let replay = crate::journal::read_path(&fx.path).unwrap();
        assert_eq!(
            replay.get(&id("root")).unwrap().state,
            NodeState::Exited(ExitStatus::Cancelled)
        );
        assert_eq!(
            replay.get(&id("child")).unwrap().state,
            NodeState::Exited(ExitStatus::Cancelled)
        );
        assert_eq!(
            replay.get(&id("done")).unwrap().state,
            NodeState::Exited(ExitStatus::Ok),
            "quitting does not rewrite an existing terminal"
        );
    }

    /// **Disposition (a) reaches a real process and leaves it dead** — the other half of §11 item
    /// 28 step 1, and the half no other test in this file can make.
    ///
    /// Every other `KillTree` test injects [`RecordingRuntime`], which records a pid and signals
    /// nothing. That is deliberate — they are about *selection and ordering* — but it means the
    /// whole set could stay green over a `kill_process_tree_and_wait` that did nothing at all. This
    /// one runs the **real** [`SystemQuitRuntime`] against a **real** process started in its own
    /// process group, which is what a child marion spawns is (`run_bounded_with`'s
    /// `process_group(0)`), and asserts the process is gone afterwards.
    ///
    /// It is only reachable because `Spawned` now carries a pid. Until item 28 step 1 every
    /// production writer recorded `pid: None`, the preflight below refused whenever there was
    /// anything to kill, and this disposition could not fire on a real fleet at all — a
    /// mutation-audited path with no production path to it. The refusal itself is pinned
    /// separately, over the record shape rather than over a live process, by
    /// `kill_tree_refuses_a_confirmed_node_whose_pid_is_not_recorded_yet`.
    ///
    /// **Two negative controls, because "everything is already dead" passes vacuously.** The
    /// confirmed set is asserted non-empty before the call, and the process is asserted *alive*
    /// before it — a `kill_tree` over an all-terminal tree signals nothing and succeeds, and would
    /// satisfy every other assertion here.
    ///
    /// Liveness is read three-valued through `ps`: the test is the killed process's parent, so
    /// between the signal and the reap it is a **zombie**, which `kill(pid, 0)` reports as alive
    /// and which `kill_process_tree_and_wait` correctly counts as dead.
    #[test]
    fn kill_tree_over_the_real_runtime_leaves_the_recorded_process_dead() {
        use marion_testsupport::{Liveness, liveness};
        use std::os::unix::process::CommandExt;

        // Its own group, so the group-addressed kill reaches it and cannot reach the test runner:
        // `signal_targets` refuses marion's own pgid, so without this the signal lands nowhere.
        let mut victim = std::process::Command::new("sleep")
            .arg("600")
            .process_group(0)
            .spawn()
            .expect("a `sleep` starts");
        let pid = victim.id() as i32;
        assert_eq!(
            liveness(pid),
            Liveness::Alive,
            "NC: the victim must be alive before the kill, or every assertion below is vacuous"
        );

        let fx = fx_with(
            "handler-quit-kill-real",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", pid),
                state("root", NodeState::Running),
            ],
        );
        let confirmed = vec![id("root")];
        assert!(
            !confirmed.is_empty(),
            "NC: an empty confirmed set means an all-terminal tree, which this disposition \
             satisfies without signalling anything"
        );

        let result = quit(
            &fx,
            marion_core::proto::QuitDisposition::KillTree { confirmed },
        );
        // Reap before asserting, unconditionally: a failure here must not also leak the victim.
        let outcome = result.map(|r| r.outcome);
        let after = liveness(pid);
        let _ = victim.kill();
        let _ = victim.wait();

        let marion_core::proto::QuitOutcome::Killed { nodes, .. } =
            outcome.expect("the exact live set was confirmed")
        else {
            panic!("kill returned another disposition's outcome")
        };
        assert_eq!(nodes.len(), 1, "one live node, one kill: {nodes:?}");
        assert_ne!(
            after,
            Liveness::Alive,
            "the journal named pid {pid}, marion said it killed it, and it is still running"
        );
        assert_ne!(
            after,
            Liveness::CannotTell,
            "`ps` could not be asked, so nothing here was measured"
        );
        assert_eq!(
            liveness(pid),
            Liveness::Gone,
            "and once reaped it is absent outright"
        );
    }

    /// **NC — disposition (c) applies §7.2's predicate, not a convenient approximation.**
    ///
    /// Idle processes die and get exactly one reap pair. Running, every `Blocked(_)`, and a
    /// spawning node survive untouched and are returned with detach guidance; an already-terminal
    /// node appears in neither list. The exact runtime trace is the mutation control against an
    /// implementation that simply sweeps every PID it can see.
    #[test]
    fn reap_idle_detach_busy_reaps_only_idle_and_detaches_every_refusal_class() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-reap",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 11),
                state("idle", NodeState::Idle),
                intent("running", Some("idle"), "codex-impl", 1),
                spawned("running", 22),
                state("running", NodeState::Running),
                intent("permission", Some("idle"), "codex-impl", 1),
                spawned("permission", 33),
                state("permission", NodeState::Blocked(BlockReason::Permission)),
                intent("elicitation", Some("idle"), "codex-impl", 1),
                spawned("elicitation", 44),
                state("elicitation", NodeState::Blocked(BlockReason::Elicitation)),
                intent("descendants", Some("idle"), "codex-impl", 1),
                spawned("descendants", 55),
                state("descendants", NodeState::Blocked(BlockReason::Descendants)),
                intent("waiting-parent", None, "claude", 0),
                spawned("waiting-parent", 66),
                state(
                    "waiting-parent",
                    NodeState::Blocked(BlockReason::Descendants),
                ),
                intent("idle-spawn-target", Some("waiting-parent"), "codex-impl", 1),
                spawned("idle-spawn-target", 77),
                state("idle-spawn-target", NodeState::Idle),
                intent("spawning", Some("idle"), "codex-impl", 1),
                intent("done", Some("idle"), "codex-impl", 1),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );

        let result = quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
            .expect("the default disposition is implemented");
        let marion_core::proto::QuitOutcome::ReapedAndDetached {
            reaped,
            detached,
            gate_exposed,
            guidance,
            supervisor,
        } = result.outcome
        else {
            panic!("reap returned another disposition's outcome")
        };
        assert_eq!(reaped, [id("idle")]);
        assert_eq!(
            detached,
            [
                id("running"),
                id("permission"),
                id("elicitation"),
                id("descendants"),
                id("waiting-parent"),
                id("idle-spawn-target"),
                id("spawning"),
            ]
        );
        assert_eq!(gate_exposed, detached);
        assert!(guidance.reattach.contains("tree/subscribe"));
        assert!(guidance.reattach.contains("reaped"));
        assert!(guidance.reattach.contains("resumable"));
        assert!(guidance.stop_fleet.contains("session/quit"));
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::BlockedNode
            )
        );
        assert!(!fx.handle.idle_exit_eligible());
        assert_eq!(runtime.killed(), [11], "§7.2 refusal classes were detached");
        let tags = journal_tags(&fx.path);
        assert_eq!(&tags[24..], ["ReapIntent", "ReapConfirmed"]);
        let replay = crate::journal::read_path(&fx.path).unwrap();
        assert_eq!(
            replay.get(&id("idle")).unwrap().reap_state,
            ReapState::ReapedIdle
        );
        for agent in [
            "running",
            "permission",
            "elicitation",
            "descendants",
            "waiting-parent",
            "idle-spawn-target",
            "spawning",
        ] {
            let node = replay.get(&id(agent)).unwrap();
            assert_eq!(node.reap_state, ReapState::Live, "{agent}");
            assert!(node.reap_intent.is_none(), "{agent}");
        }
    }

    /// **NC — a spawn marion *abandoned* is not a spawn that is *outstanding*.**
    ///
    /// The node's state is `Spawning` and stays `Spawning` forever, because `registry.rs` records
    /// the abort as a separate fact rather than as a transition — so on the unfiltered reading this
    /// one node satisfies two of §5.7's clauses at once and the supervisor can never exit.
    ///
    /// That is not a hypothetical. A `marion run` whose root failed to launch journals exactly
    /// these two records (`root.rs`'s `Err` arm), and with the detached supervisor wired up it left
    /// a process resident over a project directory the run had already deleted. §7.2's rule is the
    /// argument in one line: *"a node marion decided the fate of is never `Orphaned`"* — its fate
    /// is decided, there is no process, and there is nothing for exiting to strand.
    ///
    /// The second half is what stops the fix from being a blanket "ignore `Spawning`": an intent
    /// with **no** abort beside it still holds the supervisor, because that one really is
    /// outstanding.
    #[test]
    fn an_aborted_spawn_does_not_keep_the_supervisor_resident_but_an_outstanding_one_does() {
        let aborted = fx_with(
            "handler-quit-spawn-aborted",
            vec![
                intent("root", None, "claude", 0),
                RecordKind::SpawnAborted(marion_core::journal::SpawnAborted {
                    agent_id: id("root"),
                    reason: "the harness binary was not found".into(),
                }),
            ],
        );
        let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&aborted, marion_core::proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Exiting,
            "an abandoned spawn strands nothing, so §5.7's exclusion list does not name it"
        );
        assert!(
            aborted.handle.idle_exit_eligible(),
            "and the accept loop may act on that"
        );

        let outstanding = fx_with(
            "handler-quit-spawn-outstanding",
            vec![intent("root", None, "claude", 0)],
        );
        let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&outstanding, marion_core::proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::SpawnOutstanding
            ),
            "an intent with no resolution beside it is exactly what §5.7 means by outstanding"
        );
        assert!(!outstanding.handle.idle_exit_eligible());
    }

    /// **NC — an abort written *after* the process existed is not evidence that it does not.**
    ///
    /// The narrow reading above — *no `Spawned`, no pid, so nothing to strand* — is the whole of
    /// what an abandoned spawn licenses. `run.rs`'s `AbortOnDrop` stays armed across the entire
    /// synchronous child run, and a `Child` that is dropped rather than reaped does **not** kill
    /// the process it holds, so a panic anywhere between `command.spawn()` and the disarm writes
    /// `SpawnAborted` beside a `Spawned` that names a live pid. Discarding that node would let the
    /// supervisor exit over a process it can name.
    #[test]
    fn an_abort_written_after_the_child_was_spawned_still_holds_the_supervisor() {
        let fx = fx_with(
            "handler-quit-abort-after-spawn",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 4242),
                RecordKind::SpawnAborted(marion_core::journal::SpawnAborted {
                    agent_id: id("root"),
                    reason: "marion left the spawn path before the child reached a terminal record"
                        .into(),
                }),
            ],
        );
        let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::SpawnOutstanding
            ),
            "the journal names pid 4242 and never says it died; exiting here strands it"
        );
        assert!(
            !fx.handle.idle_exit_eligible(),
            "and the accept loop must not act on the discarded reading either"
        );
    }

    /// **NC — a registry that stopped following says *that*, not whichever stale clause the frozen
    /// prefix happens to satisfy.**
    ///
    /// Every node here is terminal, so the honest answer to §5.7 is `Exiting` and the *only* thing
    /// keeping this supervisor is that it can no longer read the file it would answer from
    /// (§7.4). Failing closed is right and is unchanged. What is asserted is the sentence: an
    /// operator told `NonTerminalNode` goes looking for a node that finished, while the fact is
    /// that marion stopped reading at a byte — and only one of those two is actionable.
    #[test]
    fn a_registry_that_stopped_following_is_reported_as_that_and_not_as_a_stale_node() {
        let fx = fx_with(
            "handler-quit-registry-stopped",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 12),
                RecordKind::Exited(Exited {
                    agent_id: id("root"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "finished".into(),
                    },
                }),
            ],
        );
        let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Exiting,
            "the control: with the journal readable, nothing here holds it"
        );

        append(&fx.path, b"this is a complete line and not a record\n");
        let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::RegistryStopped
            ),
            "§7.4: the tree is frozen, so no clause read off it may be quoted as the reason"
        );
        assert!(
            !fx.handle.idle_exit_eligible(),
            "and the accept loop fails closed on the same reading"
        );
    }

    /// **NC — a detach that found work still arms the exit that the work later releases.**
    ///
    /// §5.7's predicate is evaluated by the accept loop, continuously, not once at the instant a
    /// client asked. A quit whose answer is `Resident` is not a quit that failed: the client still
    /// left, and the clause that held the supervisor can clear a millisecond later. If the answer
    /// at that one instant decided whether the timer may ever start, a fleet that finishes just
    /// after the last window closes keeps a supervisor forever.
    #[test]
    fn a_detach_that_found_work_still_arms_the_exit_that_work_later_releases() {
        let fx = fx_with(
            "handler-quit-resident-then-released",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 77),
                state("root", NodeState::Running),
            ],
        );
        fx.handle.connected(ConnId(9));
        let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::NonTerminalNode
            )
        );
        fx.handle.gone(
            ConnId(9),
            &ClientGone::Quit(marion_core::proto::QuitDisposition::DetachAll),
            &Departure::QuitCompleted,
        );
        assert!(
            !fx.handle.idle_exit_eligible(),
            "while the node runs, the node is the answer"
        );

        append(
            &fx.path,
            &line(
                3,
                1_003,
                RecordKind::Exited(Exited {
                    agent_id: id("root"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "the node finished a moment after the window closed".into(),
                    },
                }),
            ),
        );
        assert!(
            fx.handle.idle_exit_eligible(),
            "nothing in §5.7's exclusion list holds any more, and no second client is coming to \
             ask again"
        );
    }

    /// **NC — §5.7's exit predicate is zero clients and zero non-terminal nodes, and nothing else.**
    ///
    /// A dropped socket is not a quit (§2, §7.3.1) and this changes nothing about that: no node is
    /// touched, nothing is journaled about the departure, and every clause of the exclusion list
    /// still decides the answer. What it must not do is make the *supervisor's own* lifetime
    /// conditional on a client having been polite — a TUI that was SIGKILLed leaves a supervisor
    /// with nothing to supervise, and §5.7 says that supervisor MAY go.
    #[test]
    fn a_client_that_vanished_without_quitting_still_leaves_an_empty_supervisor_free_to_exit() {
        let fx = fx_with(
            "handler-exit-after-socket-closed",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 55),
                RecordKind::Exited(Exited {
                    agent_id: id("root"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "the run finished".into(),
                    },
                }),
            ],
        );
        fx.handle.connected(ConnId(9));
        assert!(
            !fx.handle.idle_exit_eligible(),
            "a client is attached, which is §5.7's absolute clause"
        );
        fx.handle
            .gone(ConnId(9), &ClientGone::SocketClosed, &Departure::Eof);
        assert!(
            fx.handle.idle_exit_eligible(),
            "zero clients and zero non-terminal nodes is the whole predicate (§5.7)"
        );
        assert_eq!(
            journal_tags(&fx.path),
            ["SpawnIntent", "Spawned", "Exited"],
            "§7.3.1: nothing is journaled about a client's death"
        );
    }

    /// `ReapedIdle` is resumable and therefore not `Exited(_)`. §5.7's zero-non-terminal rule is
    /// literal: reaping the last process does not permit the supervisor to journal an exit while
    /// that resumable node remains in the registry.
    #[test]
    fn a_reaped_idle_node_still_keeps_the_supervisor_resident() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-reaped-resident",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 88),
                state("idle", NodeState::Idle),
            ],
        );
        let outcome = quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
            .unwrap()
            .outcome;
        let marion_core::proto::QuitOutcome::ReapedAndDetached { supervisor, .. } = outcome else {
            panic!("wrong disposition outcome")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::NonTerminalNode
            )
        );
        assert_eq!(runtime.killed(), [88]);
        assert!(!fx.handle.idle_exit_eligible());
        assert!(
            journal_tags(&fx.path)
                .iter()
                .all(|tag| tag != "SupervisorExited")
        );
    }

    #[test]
    fn an_unconfirmed_reap_intent_forbids_the_supervisor_exit_record() {
        let fx = fx_with(
            "handler-quit-unconfirmed-reap",
            vec![
                intent("idle", None, "claude", 0),
                state("idle", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("idle"),
                    reason: "a prior supervisor decided to reap".into(),
                }),
            ],
        );
        let outcome = quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
            .unwrap()
            .outcome;
        let marion_core::proto::QuitOutcome::Detached { supervisor, .. } = outcome else {
            panic!("wrong disposition outcome")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Resident(
                marion_core::proto::ResidentReason::UnconfirmedReapIntent
            )
        );
        assert!(!fx.handle.idle_exit_eligible());
        assert!(
            journal_tags(&fx.path)
                .iter()
                .all(|tag| tag != "SupervisorExited")
        );
    }

    /// **The §7.2 restart order**, which every other fixture here deliberately inverts: the journal
    /// is written and *then* the supervisor boots over it, so its nodes are marked `Orphaned`.
    fn restart_fx_with(tag: &str, records: Vec<RecordKind>) -> Fx {
        let dir = scratch(tag);
        let path = dir.join("journal.jsonl");
        for (seq, kind) in records.into_iter().enumerate() {
            append(&path, &line(seq as u64, 1_000 + seq as u64, kind));
        }
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot_path(&path).unwrap(),
            std::time::Duration::from_millis(2),
        ));
        assert!(
            !live.read(|r| r.restart_marks().is_empty()),
            "the point of this fixture is a boot that marked something",
        );
        Fx {
            _dir: dir,
            path,
            handle: RegistryHandle::with_runtime(live, Arc::new(SystemQuitRuntime)),
        }
    }

    /// **An `Orphaned` node holds the supervisor resident**, which is the clause of
    /// [`RegistryHandle::resident_reason`] most likely to be *removed* by someone reasoning
    /// correctly from the wrong premise. The whole argument is on that function; this pins it.
    ///
    /// §7.2 is what makes it right: an orphan is a node that *"requires user resolution"*, and the
    /// resolution is the operator's over this socket. The supervisor is the handle on it — with a
    /// pid on record, a confirmed KillTree signals the surviving process. Exiting instead of
    /// holding does not avoid the problem, it discards the only means of fixing it.
    ///
    /// `detached_supervisor.rs` covers the same claim end to end with a process it really started;
    /// this covers it where the predicate lives, so the reasoning is refuted in the unit suite
    /// rather than eight seconds into an integration run.
    #[test]
    fn an_orphaned_node_holds_the_supervisor_resident_because_it_is_the_operators_to_resolve() {
        let fx = restart_fx_with(
            "handler-resident-orphan",
            vec![
                intent("lost", None, "claude", 0),
                spawned("lost", 55),
                state("lost", NodeState::Running),
            ],
        );
        assert_eq!(
            fx.handle
                .live
                .read(|r| r.tree().get(&id("lost")).unwrap().reap_state),
            ReapState::Orphaned,
            "the fixture's premise: the boot marked it",
        );
        assert_eq!(
            fx.handle.residency(),
            Some(marion_core::proto::ResidentReason::NonTerminalNode),
            "§7.2: marion does not know, and the operator is the one who resolves that",
        );
        assert!(!fx.handle.idle_exit_eligible());

        // And it is released by the same thing that releases any node: a recorded fate. Which also
        // retracts the marking (`marion_core::registry::Replay::apply`), so the two agree.
        append(
            &fx.path,
            &line(
                3,
                1_003,
                RecordKind::Exited(Exited {
                    agent_id: id("lost"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "exited cleanly".into(),
                    },
                }),
            ),
        );
        fx.handle.live.refresh();
        assert_eq!(fx.handle.residency(), None);
        assert_eq!(
            fx.handle
                .live
                .read(|r| r.tree().get(&id("lost")).unwrap().reap_state),
            ReapState::Live,
            "the marking went with the premise it rested on",
        );
    }

    /// **A reap intent stops holding the supervisor once the death it was about is observed.**
    ///
    /// §7.2's crash window is *"the supervisor died before the kill landed"*, and §5.7 holds the
    /// supervisor for it because a process may still be running. Once a terminal record for that
    /// node is on the journal the window is shut: the process was observed dead, which is the very
    /// check §7.2 says resolves the intent. Holding on the stale intent after that is a supervisor
    /// that can never exit — reachable in one supervisor's life, as this fixture's order shows:
    /// the reap's signal went out and could not be observed (so no `ReapConfirmed` was written),
    /// and the operator then confirmed a `session/quit` KillTree, which did observe it.
    #[test]
    fn a_reap_intent_stops_holding_the_supervisor_once_the_node_is_observed_dead() {
        let fx = fx_with(
            "handler-quit-reap-intent-then-killed",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 91),
                state("idle", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("idle"),
                    reason: "session/quit reaped an idle node before detaching busy work".into(),
                }),
                RecordKind::KillConfirmed(KillConfirmed {
                    agent_id: id("idle"),
                    exit: ProcessExit {
                        code: None,
                        signal: Some(9),
                        description: "confirmed session/quit killed the node's process tree".into(),
                    },
                }),
            ],
        );
        assert_eq!(
            fx.handle.residency(),
            None,
            "every node's death is on the record; nothing is left for this supervisor to hold",
        );
        assert!(fx.handle.idle_exit_eligible());
    }

    /// **NC — EOF has no default disposition.** An idle node is the sharp control because the
    /// explicit default would reap it; `gone(SocketClosed)` must leave both its runtime trace and
    /// every journal byte alone.
    #[test]
    fn a_dropped_socket_is_not_any_quit_disposition_and_does_not_apply_the_default() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-eof",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 77),
                state("idle", NodeState::Idle),
            ],
        );
        let before = std::fs::read(&fx.path).unwrap();
        fx.handle
            .gone(ConnId(44), &ClientGone::SocketClosed, &Departure::Eof);
        assert!(runtime.killed().is_empty());
        assert_eq!(std::fs::read(&fx.path).unwrap(), before);
        assert_eq!(
            fx.handle
                .live
                .read(|r| r.tree().get(&id("idle")).unwrap().reap_state),
            ReapState::Live
        );
    }

    /// Observes the journal at the instant of each signal, which no ordinary assertion can: the
    /// finished file records *intent, confirm* for both a correct implementation and one that
    /// appends the confirmation before sending anything. §4.3's order is what separates them, and
    /// a confirmation that precedes its act is a durable claim marion never earned.
    #[derive(Default)]
    struct OrderingRuntime {
        path: Mutex<Option<PathBuf>>,
        at_signal: Mutex<Vec<Vec<String>>>,
    }

    impl QuitRuntime for OrderingRuntime {
        fn kill_process_tree_and_wait(&self, _pid: i32) -> bool {
            let path = lock(&self.path).clone().expect("the fixture set its path");
            lock(&self.at_signal).push(journal_tags(&path));
            true
        }
    }

    /// A runtime that signals and cannot observe death, which is the failure `run.rs`'s bounded
    /// wait returns rather than asserting away.
    struct UnobservableRuntime;

    impl QuitRuntime for UnobservableRuntime {
        fn kill_process_tree_and_wait(&self, _pid: i32) -> bool {
            false
        }
    }

    /// **NC — (a)'s PID preflight refuses rather than signals into the dark.**
    ///
    /// A confirmed node whose spawn has not resolved has no recorded PID. Proceeding would append
    /// a durable kill intent for a process marion cannot address, which is the half-happened kill
    /// the confirmed list exists to prevent — and the refusal is a `Conflict`, because the
    /// operator's next move is to re-render, not to file a bug.
    #[test]
    fn kill_tree_refuses_a_confirmed_node_whose_pid_is_not_recorded_yet() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-kill-no-pid",
            vec![intent("root", None, "claude", 0)],
        );
        let before = std::fs::read(&fx.path).unwrap();

        let error = quit(
            &fx,
            marion_core::proto::QuitDisposition::KillTree {
                confirmed: vec![id("root")],
            },
        )
        .expect_err("a node with no PID cannot be proven to have been reached");
        assert_eq!(error.kind(), Some(FailureKind::Conflict));
        assert!(error.message.contains("no recorded PID"), "{error}");
        assert!(runtime.killed().is_empty(), "nothing was signalled");
        assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    }

    /// **NC — each node's confirmation is appended after its signal, not before it.**
    ///
    /// The finished journal is identical either way, so the assertion has to be made *during* the
    /// signal. §7.2's recovery reads an intent without a confirmation as "marion may have killed
    /// this"; a confirmation written first would make the opposite claim durable in the one window
    /// where it is false.
    #[test]
    fn every_kill_is_signalled_before_its_confirmation_becomes_durable() {
        let runtime = Arc::new(OrderingRuntime::default());
        let fx = fx_with_runtime(
            "handler-quit-kill-order",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 301),
                state("root", NodeState::Running),
                intent("child", Some("root"), "codex-impl", 1),
                spawned("child", 302),
                state("child", NodeState::Running),
            ],
            runtime.clone(),
        );
        *lock(&runtime.path) = Some(fx.path.clone());

        quit(
            &fx,
            marion_core::proto::QuitDisposition::KillTree {
                confirmed: vec![id("root"), id("child")],
            },
        )
        .expect("the exact set was confirmed");

        let snapshots = lock(&runtime.at_signal).clone();
        assert_eq!(snapshots.len(), 2, "one signal per node");
        for (i, tags) in snapshots.iter().enumerate() {
            assert_eq!(
                tags.last().map(String::as_str),
                Some("KillIntent"),
                "node {i}'s intent is durable at the moment it is signalled"
            );
            assert_eq!(
                tags.iter().filter(|t| *t == "KillConfirmed").count(),
                i,
                "node {i} was not confirmed before it was signalled"
            );
        }
    }

    /// **NC — a node §7.2 already reaped is retired without a second signal.**
    ///
    /// `ReapedIdle` is not `Exited`, so (a) must still account for it, but its process is already
    /// gone. Signalling its recorded PID again would address whatever now owns that number, and
    /// recording `signal: 9` would claim marion did something it did not do.
    #[test]
    fn kill_tree_retires_an_already_reaped_node_without_signalling_it_again() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-kill-reaped",
            vec![
                intent("reaped", None, "claude", 0),
                spawned("reaped", 501),
                state("reaped", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("reaped"),
                    reason: "an earlier session/quit reaped it".into(),
                }),
                RecordKind::ReapConfirmed(ReapConfirmed {
                    agent_id: id("reaped"),
                }),
                intent("live", None, "claude", 0),
                spawned("live", 502),
                state("live", NodeState::Running),
            ],
        );

        quit(
            &fx,
            marion_core::proto::QuitDisposition::KillTree {
                confirmed: vec![id("live"), id("reaped")],
            },
        )
        .expect("both non-terminal nodes were confirmed");
        assert_eq!(
            runtime.killed(),
            [502],
            "a ReapedIdle node has no process left to signal"
        );
        let replay = crate::journal::read_path(&fx.path).unwrap();
        assert_eq!(
            replay
                .get(&id("reaped"))
                .unwrap()
                .exit
                .as_ref()
                .unwrap()
                .signal,
            None,
            "the record does not claim a signal marion never sent"
        );
        assert_eq!(
            replay
                .get(&id("live"))
                .unwrap()
                .exit
                .as_ref()
                .unwrap()
                .signal,
            Some(9)
        );
    }

    /// **One supervisor is one writer, across every RPC it serves — `seq` and `mono_ns` say so or
    /// they are decoration.**
    ///
    /// `JournalRecord.seq` is documented as *"this writer's ordinal, from 0, gapless by
    /// construction"* and `mono_ns` as monotonic since the writer started, existing (§4.2) to anchor
    /// a record against `pty.cast`. The handler used to open a `Journal` per `session/quit` with a
    /// fresh `writer_id`, which made both untrue in a way nothing could see: replay seeds a writer's
    /// expected ordinal from the first record it reads, so a crowd of one-RPC writers raises **no**
    /// `SeqGap` — it just quietly stops being a timeline.
    ///
    /// Three call paths, three RPCs, one file: the reap, the kill, and the supervisor's own exit
    /// record. Reading the writers and ordinals back off the bytes is the only way to see it, since
    /// the tree replay is identical either way — which is exactly why it went unnoticed.
    #[test]
    fn records_written_across_separate_rpcs_share_one_writer_and_one_rising_sequence() {
        let (fx, runtime) = recording_fx_with(
            "handler-one-writer",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 71),
                state("idle", NodeState::Idle),
                intent("busy", None, "claude", 0),
                spawned("busy", 72),
                state("busy", NodeState::Running),
            ],
        );
        let seeded = journal_records(&fx.path).len();

        // RPC 1 — reaps the idle root, detaches the busy one.
        quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
            .expect("one idle root is reapable");
        // RPC 2 — a different handler method, over the same registry. The reaped node is still
        // non-terminal by `state`, so §7.3.2 requires it in the confirmed set.
        quit(
            &fx,
            marion_core::proto::QuitDisposition::KillTree {
                confirmed: vec![id("idle"), id("busy")],
            },
        )
        .expect("the exact non-terminal set was confirmed");
        // RPC 3 — not a client call at all, and the third place that used to mint its own writer.
        assert!(fx.handle.begin_idle_exit());
        assert_eq!(
            runtime.killed(),
            [71, 72],
            "the reaped root was not re-signalled"
        );

        let written: Vec<_> = journal_records(&fx.path).into_iter().skip(seeded).collect();
        assert_eq!(
            journal_tags(&fx.path)[seeded..],
            [
                "ReapIntent",
                "ReapConfirmed",
                "KillIntent",
                "KillConfirmed",
                "KillIntent",
                "KillConfirmed",
                "SupervisorExited",
            ],
            "the fixture is only interesting if all three call paths really wrote"
        );

        let writers: std::collections::BTreeSet<_> =
            written.iter().map(|r| r.writer.0.clone()).collect();
        assert_eq!(
            writers.len(),
            1,
            "three RPCs, one supervisor process, one writer identity; got {writers:?}"
        );
        assert_ne!(
            writers.iter().next().unwrap(),
            "w",
            "and it is the supervisor's own identity, not the fixture's seeded one"
        );
        assert_eq!(
            written.iter().map(|r| r.seq).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6],
            "`seq` continues across RPCs. Restarting at 0 per call raises no `SeqGap` — replay \
             seeds a new writer's expectation from whatever ordinal it first sees — so this \
             assertion is the only thing that can catch it"
        );
        assert!(
            written.windows(2).all(|w| w[0].mono_ns <= w[1].mono_ns),
            "one writer, one `Instant` origin, so §4.2's `pty.cast` anchor advances rather than \
             resetting: {:?}",
            written.iter().map(|r| r.mono_ns).collect::<Vec<_>>()
        );
    }

    /// **NC — a kill marion cannot observe dead is a refusal, not a confirmation.**
    ///
    /// This is the one path `run.rs`'s bounded wait exists to produce, and it is the path that
    /// must not end in an exit: an unconfirmed intent is exactly what §7.2's recovery needs to
    /// find, and a supervisor that left anyway would take that recovery with it.
    #[test]
    fn a_kill_that_cannot_be_observed_dead_leaves_the_intent_unconfirmed_and_no_exit() {
        let fx = fx_with_runtime(
            "handler-quit-kill-unobserved",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 909),
                state("root", NodeState::Running),
            ],
            Arc::new(UnobservableRuntime),
        );

        let error = quit(
            &fx,
            marion_core::proto::QuitDisposition::KillTree {
                confirmed: vec![id("root")],
            },
        )
        .expect_err("marion did not observe the process dead");
        assert_eq!(error.kind(), Some(FailureKind::Internal));
        let tags = journal_tags(&fx.path);
        assert_eq!(tags.last().map(String::as_str), Some("KillIntent"));
        assert!(
            !tags.iter().any(|t| t == "KillConfirmed"),
            "nothing confirmed a death nobody saw: {tags:?}"
        );
        assert!(!fx.handle.idle_exit_eligible());
        assert!(!fx.handle.begin_idle_exit());
        assert!(!fx.handle.exiting());
    }

    /// **NC — (c) refuses each busy class on that node's own state.**
    ///
    /// Every refusal class here is a *root*, so §7.2's "a node a spawn is blocked on" guard cannot
    /// stand in for the state predicate. Without this, `reaping` could test nothing but parentage
    /// and still detach every busy node in a tree-shaped fixture.
    #[test]
    fn reap_idle_detach_busy_refuses_each_busy_root_on_its_own_state() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-reap-roots",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 1),
                state("idle", NodeState::Idle),
                intent("running", None, "claude", 0),
                spawned("running", 2),
                state("running", NodeState::Running),
                intent("permission", None, "claude", 0),
                spawned("permission", 3),
                state("permission", NodeState::Blocked(BlockReason::Permission)),
                intent("elicitation", None, "claude", 0),
                spawned("elicitation", 4),
                state("elicitation", NodeState::Blocked(BlockReason::Elicitation)),
                intent("descendants", None, "claude", 0),
                spawned("descendants", 5),
                state("descendants", NodeState::Blocked(BlockReason::Descendants)),
                intent("spawning", None, "claude", 0),
            ],
        );

        let marion_core::proto::QuitOutcome::ReapedAndDetached {
            reaped, detached, ..
        } = quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
            .expect("no busy root blocks the reap of an idle one")
            .outcome
        else {
            panic!("reap returned another disposition's outcome")
        };
        assert_eq!(reaped, [id("idle")]);
        assert_eq!(
            detached,
            [
                id("running"),
                id("permission"),
                id("elicitation"),
                id("descendants"),
                id("spawning"),
            ]
        );
        assert_eq!(runtime.killed(), [1], "only the idle root was signalled");
    }

    /// **NC — an idle root already under, or past, a reap is not reaped a second time.**
    ///
    /// An unconfirmed intent means some other actor may already be mid-reap, and a `ReapedIdle`
    /// node has no process left; either way a second intent/confirm pair would journal an act that
    /// did not happen to a process that is not there.
    #[test]
    fn reap_idle_detach_busy_skips_an_idle_root_already_under_or_past_a_reap() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-reap-twice",
            vec![
                intent("fresh", None, "claude", 0),
                spawned("fresh", 10),
                state("fresh", NodeState::Idle),
                intent("intended", None, "claude", 0),
                spawned("intended", 20),
                state("intended", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("intended"),
                    reason: "someone else decided to reap it".into(),
                }),
                intent("already", None, "claude", 0),
                spawned("already", 30),
                state("already", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("already"),
                    reason: "an earlier session/quit reaped it".into(),
                }),
                RecordKind::ReapConfirmed(ReapConfirmed {
                    agent_id: id("already"),
                }),
            ],
        );

        let marion_core::proto::QuitOutcome::ReapedAndDetached {
            reaped, detached, ..
        } = quit(&fx, marion_core::proto::QuitDisposition::ReapIdleDetachBusy)
            .expect("one idle root was reapable")
            .outcome
        else {
            panic!("reap returned another disposition's outcome")
        };
        assert_eq!(reaped, [id("fresh")]);
        assert_eq!(runtime.killed(), [10]);
        assert_eq!(
            detached,
            [id("intended")],
            "a ReapedIdle node is no longer something a client can be detached from"
        );
    }

    /// **NC — (b) names the nodes an operator is walking away from, and not the ones that finished.**
    ///
    /// A `detached` list padded with terminal nodes tells the operator that work is still out there
    /// when it is not, which is the same lie as omitting a live one, in the other direction.
    #[test]
    fn detach_names_only_the_nodes_that_are_still_someones_agent() {
        let fx = fx_with(
            "handler-quit-detach-list",
            vec![
                intent("live", None, "claude", 0),
                spawned("live", 61),
                state("live", NodeState::Idle),
                intent("done", None, "claude", 0),
                spawned("done", 62),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );

        let marion_core::proto::QuitOutcome::Detached {
            detached,
            gate_exposed,
            ..
        } = quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
            .expect("detach is implemented")
            .outcome
        else {
            panic!("detach returned another disposition's outcome")
        };
        assert_eq!(detached, [id("live")]);
        assert_eq!(gate_exposed, [id("live")]);
    }

    /// **NC — a departure decides nothing, and *"nothing"* is about nodes and about time, not
    /// about the supervisor's right to leave an empty project.**
    ///
    /// §7.3.1 is a rule about **agents**: *"a crashed, SIGKILLed, or otherwise vanished client MUST
    /// leave every node exactly as it was"*, and *"nothing is journaled about the client's death"*.
    /// Both are asserted here, byte for byte.
    ///
    /// What §7.3.1 does **not** say is that the supervisor must outlive its own emptiness. §5.7's
    /// permission is two clauses — *"zero clients and zero non-terminal nodes"* — and neither
    /// mentions a quit. Reading one in is what left a supervisor immortal after every client that
    /// died rather than resigned, which is not a stricter reading of the crash invariant but a leak
    /// wearing its name; `handler-exit-after-socket-closed` above is the same fact stated
    /// positively.
    ///
    /// The distinction that *does* survive is timing, and it is asserted here: a departure marion
    /// could not read does not waive §5.7's grace, because for all marion knows the replacement
    /// window is already opening. Only an explicit `session/quit` does.
    #[test]
    fn a_departure_decides_nothing_and_does_not_shorten_the_wait() {
        let fx = fx_with(
            "handler-quit-eof-eligibility",
            vec![
                intent("done", None, "claude", 0),
                spawned("done", 7),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );
        let before = std::fs::read(&fx.path).unwrap();

        fx.handle.connected(ConnId(3));
        fx.handle
            .gone(ConnId(3), &ClientGone::SocketClosed, &Departure::Eof);
        assert_eq!(
            std::fs::read(&fx.path).unwrap(),
            before,
            "§7.3.1: nothing is journaled about a client's death, and no node moved"
        );
        assert!(
            !fx.handle.exiting(),
            "`gone` itself never commits to an exit; §5.7 splits the decision from the record"
        );
        assert!(
            !fx.handle.idle_exit_grace_waived(),
            "§7.3.1: a close said nothing, so it cannot have said 'and do not wait'"
        );
        assert!(
            fx.handle.idle_exit_eligible(),
            "§5.7's two clauses are both satisfied; the accept loop still owes the whole grace"
        );
    }

    /// **NC — `exiting` follows the exit record; it does not precede it.**
    ///
    /// §5.7's record is what distinguishes *finished and left* from *died*. A supervisor that
    /// committed to exiting and only then failed to journal would produce exactly the ambiguity
    /// the record exists to remove, and would do it on the one path — a journal marion cannot write
    /// — where the evidence is least recoverable.
    #[test]
    fn the_supervisor_does_not_commit_to_exiting_before_its_record_is_durable() {
        let fx = fx_with(
            "handler-quit-exit-undurable",
            vec![
                intent("done", None, "claude", 0),
                spawned("done", 8),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );
        let marion_core::proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_core::proto::QuitDisposition::DetachAll)
                .expect("detach is implemented")
                .outcome
        else {
            panic!("detach returned another disposition's outcome")
        };
        assert_eq!(
            supervisor,
            marion_core::proto::SupervisorDisposition::Exiting
        );
        assert!(fx.handle.idle_exit_eligible());

        // A path the journal cannot be appended to. Nothing else about the decision changes, so
        // the only reason to stay is the one under test.
        std::fs::remove_file(&fx.path).unwrap();
        std::fs::create_dir(&fx.path).unwrap();
        assert!(!fx.handle.begin_idle_exit());
        assert!(
            !fx.handle.exiting(),
            "an exit that could not be recorded did not happen"
        );
    }

    /// A method that is specified and not built says so — `Unimplemented`, not `Unsupported`, and
    /// not silence.
    #[test]
    fn a_specified_but_unbuilt_method_is_refused_with_the_milestone_named() {
        let w = Wired::new("handler-unimpl");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        call(
            &mut c,
            Call::NodeCancel(marion_core::proto::params::NodeCancelParams {
                agent_id: id("root"),
            }),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_core::proto::Outcome::Error(e) = resp.outcome else {
            panic!("expected a refusal")
        };
        assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
        assert!(e.message.contains("node/cancel"), "{e}");
    }

    /// **NC — a subscriber is told about a change exactly once, and a subscription that starts late
    /// is not told about what its own snapshot already contained.**
    ///
    /// §7.3.3's seam, as a test rather than as an argument: the snapshot and the point notifications
    /// begin from are taken under one lock from one read, so there is no instant between them for an
    /// event to be lost in or duplicated across.
    #[test]
    fn a_snapshot_and_its_subscription_meet_exactly_with_no_gap_and_no_overlap() {
        let w = Wired::new("handler-seam");
        // Two clients: one subscribes before the child appears, one after.
        let mut early = w.dial();
        let mut er = std::io::BufReader::new(early.try_clone().unwrap());
        call(
            &mut early,
            Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
            1,
        );
        next_frame(&mut er);

        append(
            &w.fx.path,
            &line(1, 2_000, intent("child", Some("root"), "codex-impl", 1)),
        );
        assert!(until(
            || w.fx.handle.live.read(|r| r.tree().nodes().len()) == 2
        ));

        let mut late = w.dial();
        let mut lr = std::io::BufReader::new(late.try_clone().unwrap());
        call(
            &mut late,
            Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
            1,
        );

        // The late subscriber's snapshot already has the child…
        let Frame::Response(resp) = next_frame(&mut lr) else {
            panic!("expected a response")
        };
        let marion_core::proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_core::proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(snap.nodes.len(), 2, "the snapshot is current, not stale");

        // …and the early subscriber was told about it, as the flush that the late subscribe
        // performed on its way in.
        let Frame::Notification(n) = next_frame(&mut er) else {
            panic!("the early subscriber must hear about the child")
        };
        assert_eq!(n.event.method(), "tree/node-added");

        // Now a change after both are subscribed reaches both, once each.
        append(
            &w.fx.path,
            &line(
                2,
                3_000,
                RecordKind::Exited(Exited {
                    agent_id: id("child"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "clean exit".into(),
                    },
                }),
            ),
        );
        assert!(until(|| w.fx.handle.flush() == 0
            && w.fx.handle.live.read(|r| r
                .tree()
                .get(&id("child"))
                .unwrap()
                .state
                .is_exited())));
        for r in [&mut er, &mut lr] {
            let Frame::Notification(n) = next_frame(r) else {
                panic!("both subscribers hear the exit")
            };
            let Event::NodeState { state, ts, .. } = n.event else {
                panic!("expected node/state")
            };
            assert_eq!(state, NodeState::Exited(ExitStatus::Ok));
            assert_eq!(ts, SystemTime::from_unix_millis(3_000));
        }
        // And exactly once: a second flush produces nothing, so nothing further arrives.
        assert_eq!(w.fx.handle.flush(), 0, "a told transition is not re-told");
    }

    // ---------------------------------------------------------------------------------------
    // §2's `node/attach` — §7.3.3's re-attach. `events.rs` owns the cursor and proves it loses and
    // repeats nothing across a torn seam; what these assert is the thing that module cannot: that
    // the cursor is wired to a **client**, over a socket, on a connection the client already had.
    // ---------------------------------------------------------------------------------------

    use crate::events::{Draft, EventWriter};
    use marion_core::event::{Payload, PayloadKind};
    use marion_core::ir::Source;

    /// Where a node's stream lives, derived the way the handler derives it — from the journal.
    fn events_of(fx: &Fx, agent: &str) -> std::path::PathBuf {
        fx.path
            .parent()
            .unwrap()
            .join("agents")
            .join(agent)
            .join("events.jsonl")
    }

    /// Append `n` events a test can recognise by name, continuing whatever ordinal the file is at.
    fn say(path: &Path, agent: &str, tags: &[&str]) {
        let mut w = EventWriter::open_path(path, &id(agent)).expect("the stream opens");
        for t in tags {
            w.record(Draft::observed(Payload::Raw((*t).into()), Source::Protocol));
        }
        w.sync()
            .expect("the bytes are on disk before the test looks for them");
    }

    /// The `node/event` notifications a client has been sent, as `(agent_seq, payload text)`.
    fn heard(events: &[Event]) -> Vec<(u64, String)> {
        events
            .iter()
            .map(|e| {
                let Event::NodeEvent {
                    agent_seq, payload, ..
                } = e
                else {
                    panic!("expected node/event, got {}", e.method())
                };
                (
                    *agent_seq,
                    payload["Raw"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// Send `node/attach` and read every frame up to and including its response.
    ///
    /// The notifications come **first** by construction — the handler sends the replay before it
    /// returns — so a helper that read the response first would hang, which is itself the assertion
    /// that the ordering is what `node_attach`'s doc says.
    fn attach(
        c: &mut std::os::unix::net::UnixStream,
        r: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
        agent: &str,
        rid: i64,
    ) -> (Vec<Event>, marion_core::proto::Outcome) {
        call(
            c,
            Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
                agent_id: id(agent),
                pane_stream: None,
            }),
            rid,
        );
        let mut notes = Vec::new();
        loop {
            match next_frame(r) {
                Frame::Notification(n) => notes.push(n.event),
                Frame::Response(resp) => return (notes, resp.outcome),
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    fn attached_ok(
        outcome: marion_core::proto::Outcome,
    ) -> marion_core::proto::result::NodeAttachResult {
        let marion_core::proto::Outcome::Result(body) = outcome else {
            panic!("node/attach was refused: {outcome:?}")
        };
        let MethodResult::NodeAttach(r) = marion_core::proto::Method::NodeAttach
            .decode_result(&body)
            .expect("the result decodes")
        else {
            panic!("wrong result type")
        };
        r
    }

    fn refusal(outcome: marion_core::proto::Outcome) -> RpcError {
        match outcome {
            marion_core::proto::Outcome::Error(e) => e,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------------------------
    // §5.3's display plane, end to end over the socket
    // -----------------------------------------------------------------------------------------

    /// A real pty running `script`, not yet registered as `agent`'s pane.
    ///
    /// A **real child on a real pty**, not a fake: everything these tests are about — that a
    /// keystroke reaches the process, that a resize reaches it as a `SIGWINCH`, that detaching
    /// leaves it running — is a claim about a process, and a stub would let all four pass while
    /// none of them was true.
    fn unregistered_pane(w: &Wired, agent: &str, script: &str) -> Arc<crate::pty::PtyHost> {
        use crate::pty::{PtyHost, PtyMaster, StdinPlan, WinSize, spawn_pty};
        let size = WinSize::new(80, 24);
        let master = PtyMaster::open(size).expect("a pty");
        let cast = w.dir.join(format!("{agent}.cast"));
        let host = PtyHost::start(
            id(agent),
            master,
            &cast,
            size,
            "xterm-256color",
            std::time::Instant::now(),
        )
        .expect("a recording");
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd.env("TERM", "xterm-256color");
        let child = spawn_pty(
            marion_harness::ExecutionSurfaces::opaque()
                .display_plane()
                .expect("`opaque` owns a pty"),
            &mut cmd,
            host.master(),
            StdinPlan::TerminalSlave,
            None,
        )
        .expect("the child starts");
        host.adopt(child);
        Arc::new(host)
    }

    fn pane(w: &Wired, agent: &str, script: &str) -> Arc<crate::pty::PtyHost> {
        let host = unregistered_pane(w, agent, script);
        w.fx.handle.register_pane(&id(agent), Arc::clone(&host));
        host
    }

    fn drained_zero_output_pane(w: &Wired, agent: &str) -> Arc<crate::pty::PtyHost> {
        let host = pane(w, agent, "exit 0");
        assert!(
            until(|| host.poll_exited_unreaped().unwrap()),
            "the zero-output child did not exit"
        );
        w.fx.handle.closing_pane(&id(agent), &host);
        host.shutdown().unwrap();
        host.completed_replay_charge()
            .expect("the zero-output host retained its End");
        host
    }

    fn native_launch_binding(agent: &str) -> crate::native_bootstrap::NativeLaunchBinding {
        crate::native_bootstrap::NativeLaunchBinding::new(
            id(agent),
            PathBuf::from("/project"),
            crate::native_bootstrap::PeerIdentity::current_for_tty_test(),
            ConnId(500),
            crate::native_bootstrap::TerminalFingerprint::new(1, 2, 3),
            crate::native_bootstrap::TerminalGeometry {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            },
            crate::native_bootstrap::NativeLaunchDescriptor::new("atlas", "atlas", "atlas-native"),
            crate::native_bootstrap::context_hash(
                &crate::native_bootstrap::DirectNativeRequestContext::new(
                    PathBuf::from("/project"),
                    PathBuf::from("/project"),
                    "atlas".into(),
                    Vec::new(),
                    "xterm".into(),
                    1,
                ),
            ),
        )
    }

    fn native_claimant(conn: ConnId) -> crate::native_bootstrap::NativeClaimant {
        crate::native_bootstrap::NativeClaimant::new(
            conn,
            crate::native_bootstrap::PeerIdentity::current_for_tty_test(),
        )
    }

    #[test]
    fn pending_native_reservation_gates_both_attach_modes_until_atomic_ticket_claim() {
        let w = Wired::new("handler-native-writer-priority");
        let host = unregistered_pane(&w, "root", "sleep 30");
        let launches = Arc::new(
            crate::native_bootstrap::PendingNativeLaunches::with_sources(
                Arc::new(SequenceRng::default()),
                Arc::new(ManualClock::default()),
                Duration::from_secs(60),
            ),
        );
        w.fx.handle
            .install_pending_native_launches(Arc::clone(&launches))
            .unwrap_or_else(|_| panic!("native launch authority installs once"));
        let binding = native_launch_binding("root");
        let pending =
            w.fx.handle
                .reserve_pending_native_launch(binding.clone())
                .unwrap();
        w.fx.handle.register_pane(&id("root"), Arc::clone(&host));
        let generation =
            w.fx.handle
                .publish_pending_native_launch(pending.receipt())
                .unwrap();

        for pane_stream_v1 in [false, true] {
            let (out, _rx) = crate::serve::capture(ConnId(601 + u64::from(pane_stream_v1)));
            let attach = if pane_stream_v1 {
                w.fx.handle
                    .attach_pane_v1(&id("root"), &out)
                    .unwrap()
                    .0
                    .expect("pane exists")
            } else {
                w.fx.handle
                    .attach_pane(&id("root"), &out)
                    .expect("pane exists")
            };
            assert!(!attach.writable);
            assert_eq!(host.writer(), None);
        }

        let claim =
            w.fx.handle
                .claim_pending_native_writer(
                    pending.receipt().ticket(),
                    binding.agent_id(),
                    native_claimant(ConnId(700)),
                )
                .unwrap();
        assert_eq!(claim.host_generation(), generation);
        assert_eq!(claim.conn(), ConnId(700));
        assert_eq!(host.writer(), Some(ConnId(700)));
        assert_eq!(
            w.fx.handle.claim_pending_native_writer(
                pending.receipt().ticket(),
                binding.agent_id(),
                native_claimant(ConnId(701)),
            ),
            Err(crate::native_bootstrap::NativeLaunchClaimError::UnknownTicket)
        );
        w.fx.handle
            .gone(ConnId(700), &ClientGone::SocketClosed, &Departure::Eof);
        assert_eq!(
            host.writer(),
            None,
            "claim lease leaked after claimant departure"
        );
        host.shutdown().unwrap();
    }

    #[test]
    fn unacknowledged_native_claim_releases_lease_and_preserves_ticket_for_retry() {
        let w = Wired::new("handler-native-claim-ack-rollback");
        let host = unregistered_pane(&w, "root", "sleep 30");
        let launches = Arc::new(
            crate::native_bootstrap::PendingNativeLaunches::with_sources(
                Arc::new(SequenceRng::default()),
                Arc::new(ManualClock::default()),
                Duration::from_secs(60),
            ),
        );
        w.fx.handle
            .install_pending_native_launches(Arc::clone(&launches))
            .unwrap_or_else(|_| panic!("native launch authority installs once"));
        let binding = native_launch_binding("root");
        let pending =
            w.fx.handle
                .reserve_pending_native_launch(binding.clone())
                .unwrap();
        let ticket = crate::native_bootstrap::NativeLaunchTicket::for_test([
            0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ]);
        w.fx.handle.register_pane(&id("root"), Arc::clone(&host));
        w.fx.handle
            .publish_pending_native_launch(pending.receipt())
            .unwrap();

        let prepared =
            w.fx.handle
                .prepare_pending_native_writer(
                    &ticket,
                    binding.agent_id(),
                    native_claimant(ConnId(710)),
                )
                .unwrap();
        assert_eq!(host.writer(), Some(ConnId(710)));
        drop(prepared);
        assert_eq!(host.writer(), None, "failed acknowledgement leaked lease");
        assert!(launches.has_pending(binding.agent_id()));

        let prepared =
            w.fx.handle
                .prepare_pending_native_writer(
                    &ticket,
                    binding.agent_id(),
                    native_claimant(ConnId(711)),
                )
                .unwrap();
        let claim = prepared.commit().unwrap();
        assert_eq!(claim.conn(), ConnId(711));
        assert_eq!(host.writer(), Some(ConnId(711)));
        w.fx.handle
            .gone(ConnId(711), &ClientGone::SocketClosed, &Departure::Eof);
        assert_eq!(host.writer(), None);
        host.shutdown().unwrap();
    }

    #[test]
    fn blocked_native_claim_ack_does_not_block_an_unrelated_pane_attach() {
        let w = Wired::new("handler-native-blocked-claim-ack");
        let root = unregistered_pane(&w, "root", "sleep 30");
        let other = unregistered_pane(&w, "other", "sleep 30");
        w.fx.handle.register_pane(&id("other"), Arc::clone(&other));
        let launches = Arc::new(
            crate::native_bootstrap::PendingNativeLaunches::with_sources(
                Arc::new(SequenceRng::default()),
                Arc::new(ManualClock::default()),
                Duration::from_secs(60),
            ),
        );
        w.fx.handle
            .install_pending_native_launches(Arc::clone(&launches))
            .unwrap_or_else(|_| panic!("native launch authority installs once"));
        let binding = native_launch_binding("root");
        let pending =
            w.fx.handle
                .reserve_pending_native_launch(binding.clone())
                .unwrap();
        let ticket = crate::native_bootstrap::NativeLaunchTicket::for_test([
            0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ]);
        w.fx.handle.register_pane(&id("root"), Arc::clone(&root));
        w.fx.handle
            .publish_pending_native_launch(pending.receipt())
            .unwrap();
        let prepared =
            w.fx.handle
                .prepare_pending_native_writer(
                    &ticket,
                    binding.agent_id(),
                    native_claimant(ConnId(720)),
                )
                .unwrap();

        let (ack_waiting_tx, ack_waiting_rx) = std::sync::mpsc::sync_channel(1);
        let (ack_release_tx, ack_release_rx) = std::sync::mpsc::sync_channel(1);
        let blocked_ack = std::thread::spawn(move || {
            ack_waiting_tx.send(()).unwrap();
            // **Bounded, because this thread is joined.** The release always arrives on the happy
            // path, but a panic on the main thread before it — an assertion this test is built to
            // report — would leave this thread parked forever and `blocked_ack.join()` below
            // parked behind it, on a fixture that owns two real pty children. A bound turns that
            // into a failure the harness can print.
            let _ = ack_release_rx.recv_timeout(Duration::from_secs(10));
            drop(prepared);
        });
        ack_waiting_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("claim reached its blocked acknowledgement");

        let attach_handle = Arc::clone(&w.fx.handle);
        let (attach_done_tx, attach_done_rx) = std::sync::mpsc::sync_channel(1);
        let attach = std::thread::spawn(move || {
            let (out, _rx) = crate::serve::capture(ConnId(721));
            let writable = attach_handle
                .attach_pane(&id("other"), &out)
                .expect("unrelated pane stays visible")
                .writable;
            attach_done_tx.send(writable).unwrap();
        });
        let unrelated_writable = attach_done_rx.recv_timeout(Duration::from_millis(250));
        ack_release_tx.send(()).unwrap();
        blocked_ack.join().unwrap();
        attach.join().unwrap();
        assert_eq!(
            unrelated_writable,
            Ok(true),
            "claim acknowledgement held the global Panes lock"
        );
        assert_eq!(root.writer(), None);
        assert!(launches.has_pending(binding.agent_id()));
        w.fx.handle
            .gone(ConnId(721), &ClientGone::SocketClosed, &Departure::Eof);
        root.shutdown().unwrap();
        other.shutdown().unwrap();
    }

    #[test]
    fn native_ticket_is_revoked_by_replacement_and_terminal_lifecycle() {
        let w = Wired::new("handler-native-writer-replacement");
        let launches = Arc::new(
            crate::native_bootstrap::PendingNativeLaunches::with_sources(
                Arc::new(SequenceRng::default()),
                Arc::new(ManualClock::default()),
                Duration::from_secs(60),
            ),
        );
        w.fx.handle
            .install_pending_native_launches(Arc::clone(&launches))
            .unwrap_or_else(|_| panic!("native launch authority installs once"));
        let binding = native_launch_binding("root");

        let old = unregistered_pane(&w, "root", "sleep 30");
        let pending =
            w.fx.handle
                .reserve_pending_native_launch(binding.clone())
                .unwrap();
        w.fx.handle.register_pane(&id("root"), Arc::clone(&old));
        w.fx.handle
            .publish_pending_native_launch(pending.receipt())
            .unwrap();
        let replacement = unregistered_pane(&w, "root", "sleep 30");
        w.fx.handle
            .register_pane(&id("root"), Arc::clone(&replacement));
        assert_eq!(
            w.fx.handle.claim_pending_native_writer(
                pending.receipt().ticket(),
                binding.agent_id(),
                native_claimant(ConnId(800))
            ),
            Err(crate::native_bootstrap::NativeLaunchClaimError::UnknownTicket)
        );
        assert_eq!(replacement.writer(), None);

        w.fx.handle.forget_pane(&id("root"));
        let closing_host = unregistered_pane(&w, "root", "sleep 30");
        let closing =
            w.fx.handle
                .reserve_pending_native_launch(binding.clone())
                .unwrap();
        w.fx.handle
            .register_pane(&id("root"), Arc::clone(&closing_host));
        w.fx.handle
            .publish_pending_native_launch(closing.receipt())
            .unwrap();
        w.fx.handle.closing_pane(&id("root"), &closing_host);
        assert_eq!(
            w.fx.handle.claim_pending_native_writer(
                closing.receipt().ticket(),
                binding.agent_id(),
                native_claimant(ConnId(801)),
            ),
            Err(crate::native_bootstrap::NativeLaunchClaimError::WrongBinding)
        );
        assert_eq!(closing_host.writer(), None);
        closing_host.shutdown().unwrap();
        replacement.shutdown().unwrap();
        old.shutdown().unwrap();
    }

    fn write_keys(s: &mut std::os::unix::net::UnixStream, agent: &str, bytes: &str) {
        let f = Frame::Input(marion_core::proto::ClientNotification::new(
            marion_core::proto::Input::NodePtyWrite {
                agent_id: id(agent),
                bytes: bytes.into(),
            },
        ));
        s.write_all(f.to_line().as_bytes()).unwrap();
        s.flush().unwrap();
    }

    fn write_opaque_keys(s: &mut std::os::unix::net::UnixStream, agent: &str, bytes: &[u8]) {
        let f = Frame::Input(marion_core::proto::ClientNotification::new(
            marion_core::proto::Input::NodePaneWrite(marion_core::proto::NodePaneWriteV1 {
                agent_id: id(agent),
                bytes: marion_core::proto::OpaquePaneBytesV1::new(bytes),
            }),
        ));
        s.write_all(f.to_line().as_bytes()).unwrap();
        s.flush().unwrap();
    }

    fn send_resize(s: &mut std::os::unix::net::UnixStream, agent: &str, cols: u16, rows: u16) {
        let f = Frame::Input(marion_core::proto::ClientNotification::new(
            marion_core::proto::Input::NodeResize {
                agent_id: id(agent),
                cols,
                rows,
            },
        ));
        s.write_all(f.to_line().as_bytes()).unwrap();
        s.flush().unwrap();
    }

    #[test]
    fn a_forged_pane_ready_before_advertisement_is_inert() {
        let w = Wired::new("handler-pane-ready-dark");
        let host = pane(&w, "root", "sleep 30");
        assert_eq!(w.fx.handle.panes(), 1);
        assert_eq!(host.listeners(), 0);
        assert_eq!(w.fx.handle.attachments(), 0);
        assert_eq!(w.fx.handle.subscribers(), 0);

        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let forged = Frame::Input(marion_core::proto::ClientNotification::new(
            marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id: id("root"),
                token: marion_core::proto::PaneReadyTokenV1::new([0x5a; 32]),
                cut: 0,
            }),
        ));
        c.write_all(forged.to_line().as_bytes()).unwrap();
        c.flush().unwrap();

        // A later request on the same connection is the processing barrier and the no-crash proof.
        // It must be the next frame: the forged notification produces no notification of its own.
        call(
            &mut c,
            Call::NodeGet(marion_core::proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            9,
        );
        assert!(matches!(next_frame(&mut r), Frame::Response(_)));
        assert_eq!(w.fx.handle.panes(), 1);
        assert_eq!(host.listeners(), 0);
        assert_eq!(w.fx.handle.attachments(), 0);
        assert_eq!(w.fx.handle.subscribers(), 0);
    }

    /// Forgetting the registry entry invalidates the host generation, including clones already
    /// held by an attach path. The production change that must make this fail is removing the host
    /// from `Panes` without first cancelling its pending pane replay: the stale clone can still
    /// activate and emit the retained prefix after the node was forgotten.
    #[test]
    fn forgetting_a_pane_cancels_pending_replay_on_cloned_hosts() {
        let w = Wired::new("handler-pane-forget-replay");
        let host = pane(&w, "root", "printf 'prefix'; sleep 30");
        assert!(
            until(|| host.bytes_read() >= b"prefix".len() as u64),
            "the retained prefix never arrived"
        );
        let conn = ConnId(103);
        let (out, rx) = crate::serve::capture(conn);
        let descriptor = host
            .begin_pane_replay(conn, out.clone())
            .expect("the pre-forget replay is reserved");

        w.fx.handle.forget_pane(&id("root"));
        host.pane_ready(conn, &descriptor.token, descriptor.cut);

        assert!(
            rx.try_iter().next().is_none(),
            "a forgotten host activated its old retained replay"
        );
        assert!(
            host.begin_pane_replay(conn, out).is_none(),
            "a cloned forgotten host minted a new replay generation"
        );
        host.shutdown().unwrap();
    }

    /// A process may exit before its owner reaches the close callback. The production change that
    /// must make this fail is removing or invalidating the host before `shutdown` joins the reader:
    /// the late retained tail and terminal `End` then cannot be replayed from the completed pane.
    #[test]
    fn a_fast_exit_keeps_its_tail_and_end_available_for_late_internal_replay() {
        let w = Wired::new("handler-pane-fast-exit-retention");
        let host = pane(&w, "root", "printf 'fast-tail'");
        assert!(
            until(|| matches!(host.try_wait(), Ok(Some(_)))),
            "the controlled child did not exit"
        );

        let (progress, _progress_rx) = std::sync::mpsc::channel();
        let owner = NodeOwner {
            handle: Arc::clone(&w.fx.handle),
            task_id: None,
            repo: w.dir.clone(),
            tx: progress,
            identified: Mutex::new(None),
        };
        <NodeOwner as crate::root::PaneOwner>::closing(&owner, &id("root"), &host);
        host.shutdown().expect("the exited child and reader join");
        let charge = host
            .completed_replay_charge()
            .expect("the drained replay is eligible");
        <NodeOwner as crate::root::PaneOwner>::completed(&owner, &id("root"), &host, charge);
        assert_eq!(w.fx.handle.panes(), 1, "the completed pane stays retained");

        let conn = ConnId(1_106);
        let (out, rx) = crate::serve::capture(conn);
        let descriptor = host
            .begin_pane_replay(conn, out)
            .expect("the completed host remains available for a late internal replay");
        host.pane_ready(conn, &descriptor.token, descriptor.cut);

        let frames = rx
            .try_iter()
            .map(|line| {
                Frame::from_line(std::str::from_utf8(&line).expect("outbound is NDJSON"))
                    .expect("outbound frame decodes")
            })
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        let mut ends = 0;
        let mut initial_geometry = 0;
        for (seq, frame) in frames.iter().enumerate() {
            let Frame::Notification(note) = frame else {
                panic!("captured outbound item is not a notification: {frame:?}")
            };
            let Event::NodePaneFrame(frame) = &note.event else {
                panic!("captured outbound item is not a pane frame: {frame:?}")
            };
            assert_eq!(frame.seq, seq as u64, "the replay sequence stays dense");
            match &frame.frame {
                marion_core::proto::PaneFrameKindV1::Output { bytes } => {
                    output.extend_from_slice(bytes.as_bytes());
                }
                marion_core::proto::PaneFrameKindV1::End {} => ends += 1,
                marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 } => {
                    initial_geometry += 1;
                }
                other => panic!("unexpected replay geometry: {other:?}"),
            }
        }
        assert_eq!(output, b"fast-tail");
        assert_eq!(initial_geometry, 1, "replay must seed geometry once");
        assert_eq!(ends, 1, "completion emits exactly one End: {frames:?}");
        assert!(matches!(
            frames.last(),
            Some(Frame::Notification(note))
                if matches!(&note.event, Event::NodePaneFrame(frame)
                    if matches!(frame.frame, marion_core::proto::PaneFrameKindV1::End {}))
        ));
    }

    /// Completed retention expires from the completion instant, not the last replay. The exact
    /// 300-second boundary is expired, and eviction must invalidate the host only after releasing
    /// the global Panes lock so callbacks cannot deadlock the supervisor.
    #[test]
    fn completed_pane_ttl_is_exact_and_never_refreshed_by_replay() {
        let w = Wired::new("handler-pane-completed-ttl");
        let now = Arc::new(Mutex::new(std::time::Instant::now()));
        let clock_now = Arc::clone(&now);
        w.fx.handle
            .set_pane_clock_for_test(Arc::new(move || *lock(&clock_now)));
        let completed_at = *lock(&now);

        let host = pane(&w, "root", "printf 'ttl-tail'");
        assert!(
            until(|| host.poll_exited_unreaped().unwrap()),
            "the controlled child did not exit"
        );
        w.fx.handle.closing_pane(&id("root"), &host);
        host.shutdown().unwrap();
        let charge = host.completed_replay_charge().unwrap();
        w.fx.handle.completed_pane(&id("root"), &host, charge);
        assert_eq!(w.fx.handle.completed_usage_for_test(), (1, charge));

        *lock(&now) = completed_at + std::time::Duration::from_secs(299);
        let conn = ConnId(1_122);
        let (out, _rx) = crate::serve::capture(conn);
        assert!(
            host.begin_pane_replay(conn, out).is_some(),
            "the completed pane remains replayable before its TTL"
        );

        *lock(&now) = completed_at + std::time::Duration::from_secs(300);
        let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&invalidated_outside_panes);
        let handle = Arc::clone(&w.fx.handle);
        host.set_invalidation_hook(Box::new(move || {
            observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
        }));
        crate::serve::Handle::tick(w.fx.handle.as_ref());

        assert_eq!(w.fx.handle.completed_usage_for_test(), (0, 0));
        assert_eq!(w.fx.handle.panes(), 0);
        assert!(
            invalidated_outside_panes.load(Ordering::SeqCst),
            "TTL victim invalidation ran under the Panes lock"
        );
        let (out, _rx) = crate::serve::capture(ConnId(1_123));
        assert!(
            host.begin_pane_replay(ConnId(1_123), out).is_none(),
            "TTL eviction left a cloned completed host replayable"
        );
    }

    /// Cache retirement is connection-fatal for every negotiated pane stream. Silently cancelling
    /// the cursor would leave the client waiting forever for an End that cannot arrive.
    #[test]
    fn completed_cache_eviction_visibly_departs_pending_replay() {
        let w = Wired::new("handler-pane-completed-visible-eviction");
        let now = Arc::new(Mutex::new(std::time::Instant::now()));
        let clock_now = Arc::clone(&now);
        w.fx.handle
            .set_pane_clock_for_test(Arc::new(move || *lock(&clock_now)));
        let host = drained_zero_output_pane(&w, "root");
        let charge = host.completed_replay_charge().unwrap();
        w.fx.handle.completed_pane(&id("root"), &host, charge);
        let conn = ConnId(1_202);
        let (out, _captured) = crate::serve::capture(conn);
        host.begin_pane_replay(conn, out.clone())
            .expect("pending completed replay");

        *lock(&now) += std::time::Duration::from_secs(300);
        w.fx.handle.prune_completed_panes();

        assert_eq!(
            out.departed(),
            Some(crate::serve::Departure::PaneReplayEvicted {
                agent_id: "root".into(),
            })
        );
    }

    #[test]
    fn pane_ticks_with_no_completed_entries_do_not_scan_the_pane_map() {
        let w = Wired::new("handler-pane-empty-tick-fast-path");
        w.fx.handle.reset_completed_scan_count_for_test();

        for _ in 0..32 {
            crate::serve::Handle::tick(w.fx.handle.as_ref());
        }

        assert_eq!(
            w.fx.handle.completed_scan_count_for_test(),
            0,
            "the 5ms heartbeat scanned/allocated for an empty completed cache"
        );
    }

    #[test]
    fn pane_ticks_scan_once_at_the_cached_completion_expiry() {
        let mut w = Wired::new("handler-pane-tick-expiry-deadline");
        w.server.take().expect("test server").stop();
        let now = Arc::new(Mutex::new(std::time::Instant::now()));
        let clock_now = Arc::clone(&now);
        w.fx.handle
            .set_pane_clock_for_test(Arc::new(move || *lock(&clock_now)));
        let completed_at = *lock(&now);
        let host = drained_zero_output_pane(&w, "root");
        w.fx.handle.completed_pane(&id("root"), &host, 1_024);
        w.fx.handle.reset_completed_scan_count_for_test();

        *lock(&now) = completed_at + std::time::Duration::from_secs(299);
        for _ in 0..32 {
            crate::serve::Handle::tick(w.fx.handle.as_ref());
        }
        assert_eq!(
            w.fx.handle.completed_scan_count_for_test(),
            0,
            "pre-expiry heartbeats scanned the completed cache"
        );

        *lock(&now) = completed_at + std::time::Duration::from_secs(300);
        crate::serve::Handle::tick(w.fx.handle.as_ref());
        assert_eq!(
            w.fx.handle.completed_scan_count_for_test(),
            1,
            "the exact deadline should perform one expiration scan"
        );
        assert_eq!(w.fx.handle.completed_usage_for_test(), (0, 0));
    }

    #[test]
    fn completed_pane_count_cap_evicts_the_oldest_zero_output_entry() {
        let w = Wired::new("handler-pane-completed-count-cap");
        let now = std::time::Instant::now();
        w.fx.handle.set_pane_clock_for_test(Arc::new(move || now));
        w.fx.handle.set_completed_limit_for_test(2);
        let mut first = None;
        let mut charge_each = None;

        for index in 0..3 {
            let agent = format!("completed-{index:02}");
            let host = pane(&w, &agent, "exit 0");
            assert!(until(|| host.poll_exited_unreaped().unwrap()));
            w.fx.handle.closing_pane(&id(&agent), &host);
            host.shutdown().unwrap();
            let charge = host.completed_replay_charge().unwrap();
            assert!(charge > 0, "End-only completion must carry a real charge");
            if let Some(expected) = charge_each {
                assert_eq!(
                    charge, expected,
                    "identical zero-output hosts charge equally"
                );
            } else {
                charge_each = Some(charge);
            }
            if index == 0 {
                first = Some(Arc::clone(&host));
            }
            if index == 2 {
                let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
                let observed = Arc::clone(&invalidated_outside_panes);
                let handle = Arc::clone(&w.fx.handle);
                first
                    .as_ref()
                    .unwrap()
                    .set_invalidation_hook(Box::new(move || {
                        observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
                    }));
                w.fx.handle.completed_pane(&id(&agent), &host, charge);
                assert!(
                    invalidated_outside_panes.load(Ordering::SeqCst),
                    "oldest count victim was invalidated under Panes"
                );
            } else {
                w.fx.handle.completed_pane(&id(&agent), &host, charge);
            }
        }

        let charge_each = charge_each.unwrap();
        assert_eq!(w.fx.handle.completed_usage_for_test(), (2, 2 * charge_each));
        let panes = lock(&w.fx.handle.panes);
        assert!(
            !panes.hosts.contains_key(&id("completed-00")),
            "the oldest completion survived insertion past the injected cap"
        );
        assert!(panes.hosts.contains_key(&id("completed-02")));
        drop(panes);
        let first = first.unwrap();
        let (out, _rx) = crate::serve::capture(ConnId(1_124));
        assert!(
            first.begin_pane_replay(ConnId(1_124), out).is_none(),
            "the count victim remained replayable through a stale Arc"
        );
    }

    #[test]
    fn individually_oversized_completion_preserves_the_existing_cache() {
        const BYTE_CAP: usize = 256 * 1024 * 1024;
        let w = Wired::new("handler-pane-completed-oversized");
        let now = std::time::Instant::now();
        w.fx.handle.set_pane_clock_for_test(Arc::new(move || now));

        let existing = drained_zero_output_pane(&w, "existing");
        w.fx.handle.completed_pane(&id("existing"), &existing, 1024);
        assert_eq!(w.fx.handle.completed_usage_for_test(), (1, 1024));

        let oversized = drained_zero_output_pane(&w, "oversized");
        let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&invalidated_outside_panes);
        let handle = Arc::clone(&w.fx.handle);
        oversized.set_invalidation_hook(Box::new(move || {
            observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
        }));
        w.fx.handle
            .completed_pane(&id("oversized"), &oversized, BYTE_CAP + 1);

        assert_eq!(w.fx.handle.completed_usage_for_test(), (1, 1024));
        let panes = lock(&w.fx.handle.panes);
        assert!(matches!(
            panes.hosts.get(&id("existing")),
            Some(PaneEntry::Completed { host, .. }) if Arc::ptr_eq(host, &existing)
        ));
        assert!(!panes.hosts.contains_key(&id("oversized")));
        drop(panes);
        assert!(
            invalidated_outside_panes.load(Ordering::SeqCst),
            "the oversized candidate was invalidated under Panes"
        );
        let (out, _rx) = crate::serve::capture(ConnId(1_125));
        assert!(
            oversized.begin_pane_replay(ConnId(1_125), out).is_none(),
            "the rejected oversized host remained replayable"
        );
    }

    #[test]
    fn completed_byte_cap_evicts_oldest_until_the_exact_sum_fits() {
        const HUNDRED_MIB: usize = 100 * 1024 * 1024;
        let w = Wired::new("handler-pane-completed-byte-cap");
        let now = std::time::Instant::now();
        w.fx.handle.set_pane_clock_for_test(Arc::new(move || now));

        let oldest = drained_zero_output_pane(&w, "bytes-0");
        w.fx.handle
            .completed_pane(&id("bytes-0"), &oldest, HUNDRED_MIB);
        let middle = drained_zero_output_pane(&w, "bytes-1");
        w.fx.handle
            .completed_pane(&id("bytes-1"), &middle, HUNDRED_MIB);
        assert_eq!(w.fx.handle.completed_usage_for_test(), (2, 2 * HUNDRED_MIB));

        let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&invalidated_outside_panes);
        let handle = Arc::clone(&w.fx.handle);
        oldest.set_invalidation_hook(Box::new(move || {
            observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
        }));
        let newest = drained_zero_output_pane(&w, "bytes-2");
        w.fx.handle
            .completed_pane(&id("bytes-2"), &newest, HUNDRED_MIB);

        assert_eq!(
            w.fx.handle.completed_usage_for_test(),
            (2, 2 * HUNDRED_MIB),
            "one oldest 100MiB entry is exactly enough to restore the 256MiB cap"
        );
        let panes = lock(&w.fx.handle.panes);
        assert!(!panes.hosts.contains_key(&id("bytes-0")));
        assert!(panes.hosts.contains_key(&id("bytes-1")));
        assert!(panes.hosts.contains_key(&id("bytes-2")));
        drop(panes);
        assert!(
            invalidated_outside_panes.load(Ordering::SeqCst),
            "the byte-cap victim was invalidated under Panes"
        );
    }

    #[test]
    fn completed_order_exhaustion_rejects_only_the_new_candidate() {
        let w = Wired::new("handler-pane-completed-order-exhaustion");
        let existing = drained_zero_output_pane(&w, "order-existing");
        w.fx.handle
            .completed_pane(&id("order-existing"), &existing, 1024);
        {
            let mut panes = lock(&w.fx.handle.panes);
            panes.next_completed_order = u64::MAX;
        }

        let candidate = drained_zero_output_pane(&w, "order-candidate");
        let invalidated_outside_panes = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&invalidated_outside_panes);
        let handle = Arc::clone(&w.fx.handle);
        candidate.set_invalidation_hook(Box::new(move || {
            observed.store(handle.panes.try_lock().is_ok(), Ordering::SeqCst);
        }));
        w.fx.handle
            .completed_pane(&id("order-candidate"), &candidate, 2048);

        assert_eq!(w.fx.handle.completed_usage_for_test(), (1, 1024));
        assert_eq!(w.fx.handle.next_completed_order_for_test(), u64::MAX);
        let panes = lock(&w.fx.handle.panes);
        assert!(matches!(
            panes.hosts.get(&id("order-existing")),
            Some(PaneEntry::Completed { host, .. }) if Arc::ptr_eq(host, &existing)
        ));
        assert!(!panes.hosts.contains_key(&id("order-candidate")));
        drop(panes);
        assert!(
            invalidated_outside_panes.load(Ordering::SeqCst),
            "the refused order-exhausted candidate was invalidated under Panes"
        );
    }

    /// A failed close belongs to one host generation, not to the agent id forever. The production
    /// change that must make this fail is routing failure through id-only `forget_pane`: a stale
    /// old host then removes and invalidates the replacement that already owns the same id.
    #[test]
    fn a_stale_failed_close_cannot_remove_a_replacement_pane() {
        let w = Wired::new("handler-pane-stale-close-failure");
        let old = pane(&w, "root", "sleep 30");
        let (progress, _progress_rx) = std::sync::mpsc::channel();
        let owner = NodeOwner {
            handle: Arc::clone(&w.fx.handle),
            task_id: None,
            repo: w.dir.clone(),
            tx: progress,
            identified: Mutex::new(None),
        };
        <NodeOwner as crate::root::PaneOwner>::closing(&owner, &id("root"), &old);
        let replacement = pane(&w, "root", "sleep 30");

        <NodeOwner as crate::root::PaneOwner>::failed(&owner, &id("root"), &old);

        let panes = lock(&w.fx.handle.panes);
        let current = panes.hosts.get(&id("root")).expect("replacement remains");
        assert!(Arc::ptr_eq(current.host(), &replacement));
        drop(panes);
        let conn = ConnId(1_107);
        let (out, _captured) = crate::serve::capture(conn);
        assert!(
            old.begin_pane_replay(conn, out).is_none(),
            "the failed old generation remains replayable"
        );

        w.fx.handle.forget_pane(&id("root"));
        old.shutdown().unwrap();
        replacement.shutdown().unwrap();
    }

    #[test]
    fn stale_generation_callbacks_cannot_change_completed_accounting_or_replacement() {
        let w = Wired::new("handler-pane-stale-completed-accounting");
        let old = pane(&w, "root", "printf old");
        assert!(until(|| old.poll_exited_unreaped().unwrap()));
        w.fx.handle.closing_pane(&id("root"), &old);
        old.shutdown().unwrap();
        let old_charge = old.completed_replay_charge().unwrap();
        w.fx.handle.completed_pane(&id("root"), &old, old_charge);
        assert_eq!(w.fx.handle.completed_usage_for_test(), (1, old_charge));
        assert_eq!(w.fx.handle.next_completed_order_for_test(), 1);

        let replacement = pane(&w, "root", "sleep 30");
        assert_eq!(w.fx.handle.completed_usage_for_test(), (0, 0));
        let order_after_replacement = w.fx.handle.next_completed_order_for_test();

        w.fx.handle.completed_pane(&id("root"), &old, old_charge);
        w.fx.handle.failed_pane(&id("root"), &old);

        assert_eq!(w.fx.handle.completed_usage_for_test(), (0, 0));
        assert_eq!(
            w.fx.handle.next_completed_order_for_test(),
            order_after_replacement,
            "a stale completion must not consume cache order"
        );
        let panes = lock(&w.fx.handle.panes);
        let current = panes.hosts.get(&id("root")).expect("replacement remains");
        assert!(matches!(current, PaneEntry::Live(host) if Arc::ptr_eq(host, &replacement)));
        drop(panes);

        w.fx.handle.forget_pane(&id("root"));
        replacement.shutdown().unwrap();
    }

    /// A handler clone is not permission to cross the close boundary. The production change that
    /// must make this fail is checking `Panes::Live` only before cloning the host and lease: a
    /// paused resize can then resume after shutdown, append `r` after cast `x`, mutate the master,
    /// and poison an otherwise eligible completed replay with a post-End retention error.
    #[test]
    fn a_resize_cloned_before_close_cannot_run_after_shutdown() {
        let w = Wired::new("handler-pane-stale-resize");
        let host = pane(&w, "root", "sleep 30");
        let conn = ConnId(1_108);
        let (out, _captured) = crate::serve::capture(conn);
        assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);

        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        *lock(&w.fx.handle.pane_delivery_hook) = Some(Box::new(move || {
            reached_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the stale delivery was not released");
        }));
        let handle = Arc::clone(&w.fx.handle);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let delivery = std::thread::spawn(move || {
            handle.deliver_input(
                conn,
                &marion_core::proto::Input::NodeResize {
                    agent_id: id("root"),
                    cols: 140,
                    rows: 50,
                },
            );
            done_tx.send(()).expect("the assertion side is alive");
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("delivery did not reach the post-clone seam");

        w.fx.handle.closing_pane(&id("root"), &host);
        host.shutdown().expect("legacy shutdown succeeds");
        let charge_before = host
            .completed_replay_charge()
            .expect("the drained replay is eligible");
        w.fx.handle
            .completed_pane(&id("root"), &host, charge_before);
        release_tx.send(()).expect("the delivery thread is alive");
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the stale delivery did not return");
        delivery.join().unwrap();

        assert_eq!(
            host.master().size().unwrap(),
            crate::pty::WinSize::new(80, 24)
        );
        assert_eq!(
            host.completed_replay_charge().unwrap(),
            charge_before,
            "late control cannot invalidate cached completion eligibility"
        );
        let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
        let codes = cast
            .lines()
            .skip(1)
            .map(|line| {
                serde_json::from_str::<(f64, String, String)>(line)
                    .unwrap()
                    .1
            })
            .collect::<Vec<_>>();
        assert_eq!(codes.last().map(String::as_str), Some("x"), "{codes:?}");
        assert!(!codes.iter().any(|code| code == "r"), "{codes:?}");
    }

    /// Keystrokes obey the same close boundary as resize. The production change that must make
    /// this fail is omitting the host gate from `write_input`: a handler clone paused before that
    /// call can resume after shutdown and append an `i` record after the cast's terminal `x`.
    #[test]
    fn input_cloned_before_close_cannot_append_after_shutdown() {
        let w = Wired::new("handler-pane-stale-input");
        let host = pane(&w, "root", "sleep 30");
        let conn = ConnId(1_109);
        let (out, _captured) = crate::serve::capture(conn);
        assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);

        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        *lock(&w.fx.handle.pane_delivery_hook) = Some(Box::new(move || {
            reached_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the stale delivery was not released");
        }));
        let handle = Arc::clone(&w.fx.handle);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let delivery = std::thread::spawn(move || {
            handle.deliver_input(
                conn,
                &marion_core::proto::Input::NodePtyWrite {
                    agent_id: id("root"),
                    bytes: "late-input".into(),
                },
            );
            done_tx.send(()).expect("the assertion side is alive");
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("delivery did not reach the post-clone seam");

        w.fx.handle.closing_pane(&id("root"), &host);
        host.shutdown().expect("legacy shutdown succeeds");
        let charge_before = host
            .completed_replay_charge()
            .expect("the drained replay is eligible");
        w.fx.handle
            .completed_pane(&id("root"), &host, charge_before);
        release_tx.send(()).expect("the delivery thread is alive");
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the stale delivery did not return");
        delivery.join().unwrap();

        assert_eq!(host.completed_replay_charge().unwrap(), charge_before);
        let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
        let codes = cast
            .lines()
            .skip(1)
            .map(|line| {
                serde_json::from_str::<(f64, String, String)>(line)
                    .unwrap()
                    .1
            })
            .collect::<Vec<_>>();
        assert_eq!(codes.last().map(String::as_str), Some("x"), "{codes:?}");
        assert!(!codes.iter().any(|code| code == "i"), "{codes:?}");
    }

    /// Sealing is a nonblocking registry transition; draining is shutdown's host-local barrier.
    /// The production change that must make this fail is waiting for admitted control while the
    /// global `Panes` lock is held, or writing cast `x` before that admitted mutation completes.
    #[test]
    fn closing_releases_the_registry_before_shutdown_drains_admitted_control() {
        let w = Wired::new("handler-pane-control-drain");
        let host = pane(&w, "root", "sleep 30");
        let conn = ConnId(1_110);
        let (out, _captured) = crate::serve::capture(conn);
        assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);

        let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        host.set_post_cast_input_hook(Box::new(move || {
            admitted_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the admitted control was not released");
        }));
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        host.set_input_delivery_wait_signal(waiting_tx);
        let handle = Arc::clone(&w.fx.handle);
        let (delivery_done_tx, delivery_done_rx) = std::sync::mpsc::sync_channel(1);
        let delivery = std::thread::spawn(move || {
            handle.deliver_input(
                conn,
                &marion_core::proto::Input::NodePtyWrite {
                    agent_id: id("root"),
                    bytes: "admitted".into(),
                },
            );
            delivery_done_tx
                .send(())
                .expect("the assertion side is alive");
        });
        admitted_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("input was not admitted at the host boundary");

        let closing_handle = Arc::clone(&w.fx.handle);
        let closing_host = Arc::clone(&host);
        let (closing_done_tx, closing_done_rx) = std::sync::mpsc::sync_channel(1);
        let closing = std::thread::spawn(move || {
            closing_handle.closing_pane(&id("root"), &closing_host);
            closing_done_tx
                .send(())
                .expect("the assertion side is alive");
        });

        let shutdown_host = Arc::clone(&host);
        let (shutdown_done_tx, shutdown_done_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            shutdown_done_tx
                .send(shutdown_host.shutdown())
                .expect("the assertion side is alive");
        });
        let registry = Arc::clone(&w.fx.handle);
        let (lookup_done_tx, lookup_done_rx) = std::sync::mpsc::sync_channel(1);
        let lookup = std::thread::spawn(move || {
            lookup_done_tx
                .send(registry.panes())
                .expect("the assertion side is alive");
        });

        waiting_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("shutdown did not observe the unresolved input delivery");
        closing_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("Closing waited for admitted control");
        lookup_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("Closing held the global pane registry while control was parked");
        host.fail_next_master_input(
            "injected delivery failure after shutdown observed the barrier",
        );
        release_tx.send(()).expect("the delivery thread is alive");

        delivery_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("admitted input did not finish");
        let shutdown_result = shutdown_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("shutdown did not finish after control drained");
        shutdown_result.expect("legacy shutdown succeeds");
        closing.join().unwrap();
        lookup.join().unwrap();
        delivery.join().unwrap();
        shutdown.join().unwrap();

        assert!(
            host.completed_replay_charge().is_err(),
            "a cast-recorded input that failed delivery remained cache-eligible"
        );
        let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
        let codes = cast
            .lines()
            .skip(1)
            .map(|line| {
                serde_json::from_str::<(f64, String, String)>(line)
                    .unwrap()
                    .1
            })
            .collect::<Vec<_>>();
        assert_eq!(codes.last().map(String::as_str), Some("x"), "{codes:?}");
        assert!(codes.iter().any(|code| code == "i"), "{codes:?}");
    }

    /// A cast-recorded input remains part of the completion decision until its master write has
    /// either succeeded or failed. A permit that ends at the cast record lets shutdown cache an
    /// eligible replay while the write is still parked; its later failure then arrives too late to
    /// retract the registry's Completed entry.
    #[test]
    fn unresolved_input_delivery_cannot_be_published_as_completed() {
        let w = Wired::new("handler-pane-input-delivery-completion");
        let host = pane(&w, "root", "sleep 30");
        let conn = ConnId(1_116);
        let (out, _captured) = crate::serve::capture(conn);
        assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);

        let (recorded_tx, recorded_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        host.set_post_cast_input_hook(Box::new(move || {
            recorded_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the parked input delivery was not released");
        }));

        let handle = Arc::clone(&w.fx.handle);
        let delivery = std::thread::spawn(move || {
            handle.deliver_input(
                conn,
                &marion_core::proto::Input::NodePtyWrite {
                    agent_id: id("root"),
                    bytes: "recorded-but-undelivered".into(),
                },
            );
        });
        recorded_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("input never reached the post-cast delivery seam");

        w.fx.handle.closing_pane(&id("root"), &host);
        let shutdown_host = Arc::clone(&host);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            shutdown_tx
                .send(shutdown_host.shutdown())
                .expect("the assertion side is alive");
        });
        let shutdown_result = shutdown_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("bounded shutdown waited indefinitely for an input write");
        shutdown_result.expect("legacy shutdown remains successful");
        let charge_before_release = host.completed_replay_charge();
        if let Ok(charge) = charge_before_release {
            w.fx.handle.completed_pane(&id("root"), &host, charge);
        } else {
            w.fx.handle.failed_pane(&id("root"), &host);
        }

        release_tx.send(()).expect("the delivery thread is alive");
        delivery.join().unwrap();
        shutdown.join().unwrap();

        assert!(
            charge_before_release.is_err(),
            "shutdown qualified replay while an input delivery outcome was unresolved"
        );
        assert_eq!(
            w.fx.handle.panes(),
            0,
            "an unresolved delivery was published into the completed-pane cache"
        );
        let (out, _rx) = crate::serve::capture(ConnId(1_117));
        assert!(
            host.begin_pane_replay(ConnId(1_117), out).is_none(),
            "the failed host remained available for internal replay"
        );
        let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
        let codes = cast
            .lines()
            .skip(1)
            .map(|line| {
                serde_json::from_str::<(f64, String, String)>(line)
                    .unwrap()
                    .1
            })
            .collect::<Vec<_>>();
        assert_eq!(codes.last().map(String::as_str), Some("x"), "{codes:?}");
        assert!(codes.iter().any(|code| code == "i"), "{codes:?}");
    }

    /// Legacy pane presence means a live, attachable terminal—not retained replay state. The
    /// production change that must make this fail is projecting every `Panes` key: Closing and
    /// Completed then advertise `pane: true` while legacy attach returns no pane or listener.
    #[test]
    fn only_live_panes_are_visible_on_legacy_projection_and_attach() {
        let w = Wired::new("handler-pane-live-projection");
        let host = pane(&w, "root", "sleep 30");
        say(&events_of(&w.fx, "root"), "root", &["ready"]);
        assert!(w.fx.handle.node_get(&id("root")).unwrap().node.pane);
        assert!(w.fx.handle.pane_ids().contains(&id("root")));

        w.fx.handle.closing_pane(&id("root"), &host);
        assert!(!w.fx.handle.node_get(&id("root")).unwrap().node.pane);
        assert!(!w.fx.handle.pane_ids().contains(&id("root")));
        let (closing_out, _closing_rx) = crate::serve::capture(ConnId(1_111));
        let closing =
            w.fx.handle
                .node_attach(&id("root"), false, &closing_out)
                .unwrap();
        assert!(!closing.node.pane);
        assert!(closing.pane.is_none());
        assert_eq!(host.listeners(), 0, "Closing installed a dead listener");

        let closing_v1_conn = ConnId(1_204);
        let (closing_v1_out, closing_v1_rx) = crate::serve::capture(closing_v1_conn);
        let closing_v1 =
            w.fx.handle
                .node_attach(&id("root"), true, &closing_v1_out)
                .expect("Closing remains explicitly replayable");
        let closing_v1_pane = closing_v1.pane.expect("Closing advertises only v1 replay");
        assert!(
            closing_v1.node.pane,
            "attach summary describes this exact v1 pane"
        );
        assert!(!closing_v1_pane.writable);
        assert_eq!(closing_v1_pane.held_by, None);
        assert!(
            closing_v1_pane.ended,
            "a Closing pane is read-only because it ended, not because a writer holds it"
        );
        let closing_descriptor = closing_v1_pane
            .pane_ready
            .expect("Closing advertises a response-first cursor");
        let before_ready = closing_v1_rx
            .try_iter()
            .map(|line| Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert!(before_ready.iter().all(|frame| {
            matches!(frame, Frame::Notification(note) if matches!(note.event, Event::NodeEvent { .. }))
        }));

        host.shutdown().expect("legacy shutdown succeeds");
        let charge = host.completed_replay_charge().expect("replay is eligible");
        w.fx.handle.completed_pane(&id("root"), &host, charge);
        w.fx.handle.input(
            closing_v1_conn,
            &marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id: id("root"),
                token: closing_descriptor.token,
                cut: closing_descriptor.cut,
            }),
        );
        let pending_frames = closing_v1_rx
            .try_iter()
            .map(|line| Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            pending_frames.first(),
            Some(Frame::Notification(note))
                if matches!(&note.event, Event::NodePaneFrame(frame)
                    if frame.seq == 0
                        && matches!(frame.frame, marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
        ));
        assert!(matches!(
            pending_frames.last(),
            Some(Frame::Notification(note))
                if matches!(&note.event, Event::NodePaneFrame(frame)
                    if matches!(frame.frame, marion_core::proto::PaneFrameKindV1::End {}))
        ));
        assert!(!w.fx.handle.node_get(&id("root")).unwrap().node.pane);
        assert!(!w.fx.handle.pane_ids().contains(&id("root")));
        let (completed_out, _completed_rx) = crate::serve::capture(ConnId(1_112));
        let completed =
            w.fx.handle
                .node_attach(&id("root"), false, &completed_out)
                .unwrap();
        assert!(!completed.node.pane);
        assert!(completed.pane.is_none());
        assert_eq!(host.listeners(), 0, "Completed installed a dead listener");

        let (completed_v1_out, _completed_v1_rx) = crate::serve::capture(ConnId(1_205));
        let completed_v1 =
            w.fx.handle
                .node_attach(&id("root"), true, &completed_v1_out)
                .expect("Completed remains explicitly replayable");
        let completed_v1_pane = completed_v1
            .pane
            .expect("Completed advertises only v1 replay");
        assert!(completed_v1.node.pane);
        assert!(!completed_v1_pane.writable);
        assert_eq!(completed_v1_pane.held_by, None);
        assert!(
            completed_v1_pane.ended,
            "a Completed pane is read-only because it ended"
        );
        assert!(completed_v1_pane.pane_ready.is_some());
        host.unlisten(closing_v1_conn);
        host.unlisten(ConnId(1_205));
    }

    /// A cursor activated while Live remains registered through the same host's Closing and
    /// Completed transitions, and receives the one retained terminal End.
    #[test]
    fn ready_pane_v1_subscription_survives_same_host_completion() {
        let w = Wired::new("handler-pane-v1-ready-completion");
        let host = pane(&w, "root", "sleep 30");
        say(&events_of(&w.fx, "root"), "root", &["before-ready"]);
        let conn = ConnId(1_206);
        let (out, captured) = crate::serve::capture(conn);
        let attached =
            w.fx.handle
                .node_attach(&id("root"), true, &out)
                .expect("Live v1 attach");
        let descriptor = attached
            .pane
            .unwrap()
            .pane_ready
            .expect("Live v1 descriptor");
        let before_ready = captured
            .try_iter()
            .map(|line| Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert!(before_ready.iter().all(|frame| {
            matches!(frame, Frame::Notification(note) if matches!(note.event, Event::NodeEvent { .. }))
        }));
        w.fx.handle.input(
            conn,
            &marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id: id("root"),
                token: descriptor.token,
                cut: descriptor.cut,
            }),
        );
        let initial = captured.try_recv().expect("initial geometry");
        assert!(matches!(
            Frame::from_line(std::str::from_utf8(&initial).unwrap()).unwrap(),
            Frame::Notification(note)
                if matches!(note.event, Event::NodePaneFrame(ref frame)
                    if frame.seq == 0
                        && matches!(frame.frame, marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
        ));
        assert!(captured.try_iter().next().is_none(), "replay reaches Ready");

        w.fx.handle.closing_pane(&id("root"), &host);
        host.shutdown().unwrap();
        let charge = host.completed_replay_charge().unwrap();
        w.fx.handle.completed_pane(&id("root"), &host, charge);

        let tail = captured
            .try_iter()
            .map(|line| Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            tail.last(),
            Some(Frame::Notification(note))
                if matches!(&note.event, Event::NodePaneFrame(frame)
                    if matches!(frame.frame, marion_core::proto::PaneFrameKindV1::End {}))
        ));
        assert_eq!(out.departed(), None);
        host.unlisten(conn);
    }

    #[test]
    fn completed_commit_preserves_final_legacy_tail_then_clears_listener() {
        let w = Wired::new("handler-pane-completed-legacy-tail");
        let host = pane(&w, "root", "sleep 0.05; printf 'final-tail'");
        let conn = ConnId(1_126);
        let (out, rx) = crate::serve::capture(conn);
        host.listen(out);
        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        let first = AtomicBool::new(true);
        host.set_legacy_delivery_hook(Box::new(move || {
            if first.swap(false, Ordering::SeqCst) {
                reached_tx.send(()).expect("the assertion side is alive");
                release_rx
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("the final legacy delivery was not released");
            }
        }));
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the final tail never reserved its legacy delivery");

        w.fx.handle.closing_pane(&id("root"), &host);
        assert_eq!(
            host.listeners(),
            1,
            "Closing cleared the listener before the reader drained its final tail"
        );
        release_tx.send(()).unwrap();
        assert!(until(|| host.bytes_read() >= b"final-tail".len() as u64));
        host.shutdown().unwrap();
        let mut tail = String::new();
        assert!(
            until(|| {
                tail.extend(rx.try_iter().filter_map(|line| {
                    let frame = Frame::from_line(std::str::from_utf8(&line).unwrap()).unwrap();
                    match frame {
                        Frame::Notification(note) => match note.event {
                            Event::NodePty { bytes, .. } => Some(bytes),
                            _ => None,
                        },
                        _ => None,
                    }
                }));
                tail == "final-tail"
            }),
            "shutdown joined before the final legacy tail was observable: {tail:?}"
        );

        let charge = host.completed_replay_charge().unwrap();
        w.fx.handle.completed_pane(&id("root"), &host, charge);
        assert_eq!(host.listeners(), 0, "Completed retained a legacy listener");

        // `emit_for_test` delivers synchronously; an empty queue immediately afterwards is the
        // causal refusal, with no scheduler delay standing in for correctness.
        host.emit_for_test("post-completion");
        assert!(
            rx.try_recv().is_err(),
            "a completed pane kept delivering legacy NodePty frames"
        );
    }

    /// The response's `node.pane` bit describes the exact attach committed by this call, not a
    /// registry snapshot from before event replay. The production change that must make this fail
    /// is summarizing first and selecting the pane later: Closing in that seam yields
    /// `node.pane=true` alongside `pane=None`.
    #[test]
    fn node_attach_summary_matches_the_post_replay_pane_selection() {
        let w = Wired::new("handler-pane-attach-selection");
        let host = pane(&w, "root", "sleep 30");
        say(&events_of(&w.fx, "root"), "root", &["before-selection"]);
        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        *lock(&w.fx.handle.pane_attach_selection_hook) = Some(Box::new(move || {
            reached_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("attach selection was not released");
        }));
        let conn = ConnId(1_117);
        let (out, captured) = crate::serve::capture(conn);
        let handle = Arc::clone(&w.fx.handle);
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let attaching = std::thread::spawn(move || {
            result_tx
                .send(handle.node_attach(&id("root"), false, &out))
                .expect("the assertion side is alive");
        });
        reached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("event replay did not reach final pane selection");
        w.fx.handle.closing_pane(&id("root"), &host);
        release_tx.send(()).expect("the attach worker is alive");
        let attached = result_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("attach did not finish")
            .expect("node attach succeeds");
        attaching.join().unwrap();

        assert_eq!(
            attached.node.pane,
            attached.pane.is_some(),
            "response summary disagrees with exact pane selection"
        );
        let frames = captured
            .try_iter()
            .map(|bytes| Frame::from_line(std::str::from_utf8(&bytes).unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            frames.first(),
            Some(Frame::Notification(note)) if matches!(note.event, Event::NodeEvent { .. })
        ));
        assert_eq!(host.listeners(), 0, "Closing gained a legacy listener");
        host.shutdown().unwrap();
    }

    /// A versioned attach may not return a descriptor for a host invalidated at the final
    /// selection seam. The parked event cursor and write lease are rolled back with the token.
    #[test]
    fn pane_v1_attach_refuses_a_replaced_generation_at_final_selection() {
        let w = Wired::new("handler-pane-v1-generation-race");
        let old = pane(&w, "root", "sleep 30");
        say(&events_of(&w.fx, "root"), "root", &["before-replacement"]);
        let replacement = unregistered_pane(&w, "root", "sleep 30");
        let handle = Arc::clone(&w.fx.handle);
        *lock(&w.fx.handle.pane_attach_selection_hook) = Some(Box::new(move || {
            handle.register_pane(&id("root"), Arc::clone(&replacement));
        }));
        let conn = ConnId(1_201);
        let (out, _captured) = crate::serve::capture(conn);

        let error =
            w.fx.handle
                .node_attach(&id("root"), true, &out)
                .expect_err("a dead Ready descriptor was returned");

        assert_eq!(error.kind(), Some(FailureKind::Conflict));
        assert_eq!(w.fx.handle.attachments(), 0);
        assert_eq!(old.writer(), None);
        assert_eq!(old.listeners(), 0);
        old.shutdown().unwrap();
        let current = lock(&w.fx.handle.panes)
            .hosts
            .get(&id("root"))
            .unwrap()
            .host()
            .clone();
        current.shutdown().unwrap();
    }

    /// A live pane whose write half another connection already holds is the *other* reason a v1
    /// attach is read-only, and it must not be spelled the same way: `ended` stays false, and
    /// `held_by` names the colleague. Without this pair a client cannot tell a node somebody else
    /// is typing into from a node that has finished.
    #[test]
    fn pane_v1_attach_separates_a_busy_writer_from_a_pane_that_ended() {
        let w = Wired::new("handler-pane-v1-busy-vs-ended");
        let host = pane(&w, "root", "sleep 30");
        say(&events_of(&w.fx, "root"), "root", &["running"]);

        let writer_conn = ConnId(1_301);
        let (writer_out, _writer_rx) = crate::serve::capture(writer_conn);
        let first =
            w.fx.handle
                .node_attach(&id("root"), true, &writer_out)
                .expect("the first attach takes the write half");
        let first_pane = first.pane.expect("a live pane answers v1");
        assert!(first_pane.writable);
        assert!(!first_pane.ended);

        let reader_conn = ConnId(1_302);
        let (reader_out, _reader_rx) = crate::serve::capture(reader_conn);
        let second =
            w.fx.handle
                .node_attach(&id("root"), true, &reader_out)
                .expect("a second attach is read-only, not refused");
        let second_pane = second.pane.expect("a live pane answers v1");
        assert!(!second_pane.writable);
        assert_eq!(
            second_pane.held_by,
            Some(writer_conn.0),
            "a busy write half names its holder"
        );
        assert!(
            !second_pane.ended,
            "the node is still running; only its keyboard is taken"
        );

        host.unlisten(writer_conn);
        host.unlisten(reader_conn);
        host.shutdown().unwrap();
    }

    /// Closing the same host at the final seam preserves its exact replay token but revokes the
    /// keyboard lease. The response must describe the lifecycle it actually committed.
    #[test]
    fn pane_v1_attach_finalizes_same_host_closing_as_read_only() {
        let w = Wired::new("handler-pane-v1-closing-race");
        let host = pane(&w, "root", "sleep 30");
        say(&events_of(&w.fx, "root"), "root", &["before-closing"]);
        let handle = Arc::clone(&w.fx.handle);
        let closing_host = Arc::clone(&host);
        *lock(&w.fx.handle.pane_attach_selection_hook) = Some(Box::new(move || {
            handle.closing_pane(&id("root"), &closing_host);
        }));
        let conn = ConnId(1_203);
        let (out, _captured) = crate::serve::capture(conn);

        let attached =
            w.fx.handle
                .node_attach(&id("root"), true, &out)
                .expect("same-host Closing keeps replay reachable");
        let pane = attached.pane.expect("Closing remains v1 replayable");
        let descriptor = pane.pane_ready.expect("the reserved descriptor survives");

        assert!(!pane.writable);
        assert_eq!(pane.held_by, None);
        assert!(
            pane.ended,
            "a pane that closed between reservation and commit ended; its writer lease was not \
             taken by anybody else, and a claimant told otherwise discards the replay it attached \
             for"
        );
        assert_eq!(host.writer(), None);
        assert!(host.pane_replay_reserved(conn, &descriptor.token, descriptor.cut));
        host.unlisten(conn);
        host.shutdown().unwrap();
    }

    /// Explicit v1 never downgrades after reservation failure. Even a durable NodeEvent prefix is
    /// untouched because reservation occurs before event delivery, cursor parking, or write lease.
    #[test]
    fn failed_pane_v1_reservation_has_no_attach_side_effects() {
        let w = Wired::new("handler-pane-v1-reservation-refusal");
        let host = pane(&w, "root", "sleep 30");
        say(&events_of(&w.fx, "root"), "root", &["must-not-deliver"]);
        host.invalidate_pane_streams();
        let conn = ConnId(1_207);
        let (out, captured) = crate::serve::capture(conn);

        let error =
            w.fx.handle
                .node_attach(&id("root"), true, &out)
                .expect_err("invalid replay silently downgraded");

        assert_eq!(error.kind(), Some(FailureKind::Internal));
        assert_eq!(w.fx.handle.attachments(), 0);
        assert_eq!(host.writer(), None);
        assert_eq!(host.listeners(), 0);
        assert!(captured.try_iter().next().is_none());
        host.shutdown().unwrap();
    }

    /// Same-id registration is a generation replacement, not a map overwrite. The production
    /// change that must make this fail is leaving old leases/listeners/replay valid, or invalidating
    /// the old host while `Panes` is locked: the stale lease can resize the new terminal and a
    /// blocking last-host drop can freeze every pane operation.
    #[test]
    fn replacing_a_pane_revokes_the_old_generation_outside_the_registry_lock() {
        let w = Wired::new("handler-pane-replacement");
        let old = pane(&w, "root", "printf 'old-prefix'; sleep 30");
        assert!(until(|| old.bytes_read() >= b"old-prefix".len() as u64));
        let conn = ConnId(1_113);
        let (out, _legacy_rx) = crate::serve::capture(conn);
        assert!(w.fx.handle.attach_pane(&id("root"), &out).unwrap().writable);
        let replay_conn = ConnId(1_114);
        let (replay_out, replay_rx) = crate::serve::capture(replay_conn);
        let descriptor = old
            .begin_pane_replay(replay_conn, replay_out.clone())
            .expect("old replay is reserved");

        let handle = Arc::downgrade(&w.fx.handle);
        let (invalidated_tx, invalidated_rx) = std::sync::mpsc::sync_channel(1);
        old.set_invalidation_hook(Box::new(move || {
            let handle = handle.upgrade().expect("the registry is alive");
            invalidated_tx
                .send(handle.panes.try_lock().is_ok())
                .expect("the assertion side is alive");
        }));
        let replacement = pane(&w, "root", "sleep 30");
        assert!(
            invalidated_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("replacement did not invalidate the old generation"),
            "old generation was invalidated while Panes was locked"
        );

        w.fx.handle.deliver_input(
            conn,
            &marion_core::proto::Input::NodeResize {
                agent_id: id("root"),
                cols: 140,
                rows: 50,
            },
        );
        assert_eq!(
            replacement.master().size().unwrap(),
            crate::pty::WinSize::new(80, 24),
            "the old lease resized the replacement"
        );
        assert_eq!(
            old.listeners(),
            0,
            "old legacy listener survived replacement"
        );
        assert_eq!(old.writer(), None, "old writer lease survived replacement");
        old.pane_ready(replay_conn, &descriptor.token, descriptor.cut);
        assert!(
            replay_rx.try_iter().next().is_none(),
            "old retained replay activated after replacement"
        );
        assert!(old.begin_pane_replay(replay_conn, replay_out).is_none());

        let attached = w.fx.handle.attach_pane(&id("root"), &out).unwrap();
        assert!(attached.writable, "replacement did not issue a fresh lease");
        assert_eq!(replacement.writer(), Some(conn));
        w.fx.handle.forget_pane(&id("root"));
        old.shutdown().unwrap();
        replacement.shutdown().unwrap();
    }

    /// Replacement has one strict legacy cut: no old NodePty delivery can emerge after the new
    /// Live generation is visible. The production change that must make this fail is publishing
    /// the map swap before synchronizing old listener delivery, or allowing same-Arc lifecycle
    /// regression from Closing/Completed back to Live.
    #[test]
    fn replacement_cuts_old_legacy_delivery_before_publishing_new_live_generation() {
        let w = Wired::new("handler-pane-replacement-cutoff");
        let old = pane(&w, "root", "sleep 30");
        let (old_out, old_rx) = crate::serve::capture(ConnId(1_115));
        old.listen(old_out);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        old.set_legacy_delivery_hook(Box::new(move || {
            entered_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("old delivery was not released");
        }));
        let emitting_old = Arc::clone(&old);
        let emit = std::thread::spawn(move || emitting_old.emit_for_test("old-after-cut"));
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("old delivery did not reach the cutoff seam");

        let replacement = unregistered_pane(&w, "root", "sleep 30");
        let registering = Arc::clone(&w.fx.handle);
        let registering_host = Arc::clone(&replacement);
        let (registered_tx, registered_rx) = std::sync::mpsc::sync_channel(1);
        let register = std::thread::spawn(move || {
            registering.register_pane(&id("root"), registering_host);
            registered_tx.send(()).expect("the assertion side is alive");
        });
        assert!(
            until(|| {
                matches!(
                    lock(&w.fx.handle.panes).hosts.get(&id("root")),
                    Some(PaneEntry::Replacing(current)) if Arc::ptr_eq(current, &replacement)
                )
            }),
            "replacement did not enter its non-live cutoff phase"
        );
        assert!(
            !lock(&w.fx.handle.panes).has_live(&id("root")),
            "replacement became Live before old delivery was cut"
        );
        release_tx.send(()).expect("the emitter is alive");
        emit.join().unwrap();
        registered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("replacement was not published after cutoff");
        register.join().unwrap();
        assert!(
            lock(&w.fx.handle.panes)
                .hosts
                .get(&id("root"))
                .is_some_and(|entry| Arc::ptr_eq(entry.host(), &replacement)),
            "replacement was not published"
        );
        let before_cut = old_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(
            before_cut.len(),
            1,
            "the one delivery reserved before cutoff completes before publication"
        );
        old.emit_for_test("old-definitely-after-cut");
        assert!(
            old_rx.try_iter().next().is_none(),
            "old NodePty was admitted after replacement visibility"
        );

        w.fx.handle.closing_pane(&id("root"), &replacement);
        w.fx.handle
            .register_pane(&id("root"), Arc::clone(&replacement));
        assert!(
            !lock(&w.fx.handle.panes).has_live(&id("root")),
            "same Arc resurrected Closing as Live"
        );

        let (forgotten_out, _forgotten_rx) = crate::serve::capture(ConnId(1_116));
        replacement.listen(forgotten_out);
        w.fx.handle.failed_pane(&id("root"), &replacement);
        assert_eq!(
            replacement.listeners(),
            0,
            "failed cleanup retained listeners"
        );
        old.shutdown().unwrap();
        replacement.shutdown().unwrap();
    }

    /// A replacement child may exit while publication is waiting for the old generation's
    /// admitted legacy delivery. Closing must wait for that publication transaction and then seal
    /// the new host; returning early from `Replacing(new)` would resurrect the dead child as Live.
    #[test]
    fn closing_a_replacement_waits_for_its_generation_to_publish() {
        let w = Wired::new("handler-pane-replacement-close-race");
        let old = pane(&w, "root", "sleep 30");
        let (old_out, _old_rx) = crate::serve::capture(ConnId(1_208));
        old.listen(old_out);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        old.set_legacy_delivery_hook(Box::new(move || {
            entered_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("old delivery was not released");
        }));
        let emitting = Arc::clone(&old);
        let emit = std::thread::spawn(move || emitting.emit_for_test("reserved"));
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("old delivery did not enter");

        let replacement = unregistered_pane(&w, "root", "sleep 30");
        let registering = Arc::clone(&w.fx.handle);
        let registering_host = Arc::clone(&replacement);
        let register = std::thread::spawn(move || {
            registering.register_pane(&id("root"), registering_host);
        });
        assert!(until(|| {
            matches!(
                lock(&w.fx.handle.panes).hosts.get(&id("root")),
                Some(PaneEntry::Replacing(current)) if Arc::ptr_eq(current, &replacement)
            )
        }));

        let closing = Arc::clone(&w.fx.handle);
        let closing_host = Arc::clone(&replacement);
        let (closed_tx, closed_rx) = std::sync::mpsc::sync_channel(1);
        let close = std::thread::spawn(move || {
            closing.closing_pane(&id("root"), &closing_host);
            closed_tx.send(()).expect("the assertion side is alive");
        });
        assert!(
            closed_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "Closing returned while the replacement transaction was still Replacing"
        );
        release_tx.send(()).unwrap();
        emit.join().unwrap();
        register.join().unwrap();
        closed_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("Closing did not follow publication");
        close.join().unwrap();

        assert!(matches!(
            lock(&w.fx.handle.panes).hosts.get(&id("root")),
            Some(PaneEntry::Closing(current)) if Arc::ptr_eq(current, &replacement)
        ));
        old.shutdown().unwrap();
        replacement.shutdown().unwrap();
    }

    /// Dark wire behavior cannot accidentally depend on the internal replay engine being enabled.
    /// Legacy live attach remains available and advertises no cursor even after replay invalidation.
    #[test]
    fn dark_pane_attach_does_not_consult_internal_replay_state() {
        let w = Wired::new("handler-pane-dark-internal-state");
        let host = pane(&w, "root", "sleep 30");
        host.invalidate_pane_streams();
        let conn = ConnId(104);
        let (out, rx) = crate::serve::capture(conn);
        let attached =
            w.fx.handle
                .node_attach(&id("root"), false, &out)
                .expect("legacy attach does not consult replay state");
        let pane = attached.pane.expect("the pty remains attachable");

        assert!(pane.pane_ready.is_none());
        assert_eq!(host.listeners(), 1);
        assert_eq!(host.writer(), Some(conn));
        assert!(
            rx.try_iter().next().is_none(),
            "dark attach cannot emit replay or pane frames"
        );
        host.unlisten(conn);
        host.shutdown().unwrap();
    }

    /// Legacy attach has always replayed the durable node stream before making live PTY bytes
    /// observable. Force a PTY emit at the listener-install seam so the queue order, not timing,
    /// proves that contract.
    #[test]
    fn legacy_attach_enqueues_replayed_node_events_before_concurrent_pty_output() {
        let w = Wired::new("handler-pane-legacy-order");
        say(&events_of(&w.fx, "root"), "root", &["event-first"]);
        let host = pane(&w, "root", "sleep 30");
        let conn = ConnId(106);
        let (out, captured) = crate::serve::capture(conn);
        let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        *lock(&w.fx.handle.pane_listener_hook) = Some(Box::new(move || {
            reached_tx
                .send(())
                .expect("the deterministic assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("the deterministic listener hook was not released");
        }));

        std::thread::scope(|scope| {
            let handle = Arc::clone(&w.fx.handle);
            let out = out.clone();
            let attaching = scope.spawn(move || handle.node_attach(&id("root"), false, &out));
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("attach reached the listener-install seam");
            host.emit_for_test("pty-second");
            release_tx.send(()).expect("the attach worker is alive");
            attaching.join().unwrap().unwrap();
        });
        let order = captured
            .try_iter()
            .map(|bytes| Frame::from_line(std::str::from_utf8(&bytes).unwrap()).unwrap())
            .filter_map(|frame| match frame {
                Frame::Notification(note) => Some(match note.event {
                    Event::NodeEvent { .. } => "event",
                    Event::NodePty { .. } => "pty",
                    _ => "other",
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        host.unlisten(conn);
        host.shutdown().unwrap();

        assert_eq!(order, ["event", "pty"]);
    }

    /// Explicit pane-v1 reserves a response-first replay and does not install the legacy listener.
    /// No pane frame may precede the response; the exact advertised Ready activates sequence zero.
    #[test]
    fn pane_stream_capability_reserves_response_first_replay() {
        let w = Wired::new("handler-pane-wire-active");
        let host = pane(&w, "root", "printf 'prefix'; sleep 30");
        assert!(
            until(|| host.bytes_read() >= b"prefix".len() as u64),
            "the retained prefix never arrived"
        );
        host.resize(crate::pty::WinSize::new(100, 40)).unwrap();
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());

        call(
            &mut c,
            Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
                agent_id: id("root"),
                pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
            }),
            1,
        );
        let mut before_response = Vec::new();
        let attached = loop {
            match next_frame(&mut r) {
                Frame::Response(response) => break attached_ok(response.outcome),
                Frame::Notification(note) => before_response.push(note.event),
                other => panic!("unexpected frame while attaching: {other:?}"),
            }
        };
        let pane = attached.pane.expect("the real pty is attachable");
        let descriptor = pane
            .pane_ready
            .expect("explicit v1 advertises the reserved replay");
        assert_eq!((pane.cols, pane.rows), (100, 40));
        assert_eq!(host.listeners(), 0, "opt-in installed a legacy listener");
        assert!(
            before_response
                .iter()
                .all(|event| !matches!(event, Event::NodePaneFrame(_))),
            "pane frames preceded the attach response"
        );
        let ready = Frame::Input(marion_core::proto::ClientNotification::new(
            marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id: id("root"),
                token: descriptor.token,
                cut: descriptor.cut,
            }),
        ));
        c.write_all(ready.to_line().as_bytes()).unwrap();
        c.flush().unwrap();
        let mut pane_frames = Vec::new();
        while pane_frames.len() < descriptor.cut as usize {
            let Frame::Notification(note) = next_frame(&mut r) else {
                panic!("exact Ready did not activate the replay")
            };
            let Event::NodePaneFrame(frame) = note.event else {
                panic!("Ready emitted a non-pane notification")
            };
            pane_frames.push(frame);
        }
        assert_eq!(
            pane_frames
                .iter()
                .map(|frame| frame.seq)
                .collect::<Vec<_>>(),
            (0..descriptor.cut).collect::<Vec<_>>()
        );
        assert!(matches!(
            pane_frames.first(),
            Some(frame)
                if matches!(frame.frame, marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 })
        ));
        let output_at = pane_frames.iter().position(|frame| {
            matches!(&frame.frame, marion_core::proto::PaneFrameKindV1::Output { bytes }
                if bytes.as_bytes() == b"prefix")
        });
        let resize_at = pane_frames.iter().position(|frame| {
            matches!(
                frame.frame,
                marion_core::proto::PaneFrameKindV1::Resize {
                    cols: 100,
                    rows: 40
                }
            )
        });
        assert!(
            matches!((output_at, resize_at), (Some(output), Some(resize)) if output < resize),
            "historical output/resize order was lost: {pane_frames:?}"
        );
    }

    #[test]
    fn pane_ready_routing_is_exact_and_preserves_the_valid_pending_slot() {
        let w = Wired::new("handler-pane-ready-exact");
        let host = pane(&w, "root", "printf 'prefix'; sleep 30");
        assert!(
            until(|| host.bytes_read() >= b"prefix".len() as u64),
            "the retained prefix never arrived"
        );
        let conn = ConnId(105);
        let (out, captured) = crate::serve::capture(conn);
        let descriptor = host
            .begin_pane_replay(conn, out)
            .expect("the internal replay seam remains testable");

        let wrong = [
            (
                conn,
                id("root"),
                marion_core::proto::PaneReadyTokenV1::new([0xa5; 32]),
                descriptor.cut,
            ),
            (
                conn,
                id("root"),
                descriptor.token.clone(),
                descriptor.cut + 1,
            ),
            (
                ConnId(106),
                id("root"),
                descriptor.token.clone(),
                descriptor.cut,
            ),
            (
                conn,
                id("not-root"),
                descriptor.token.clone(),
                descriptor.cut,
            ),
        ];
        for (ready_conn, agent_id, token, cut) in wrong {
            w.fx.handle.input(
                ready_conn,
                &marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                    agent_id,
                    token,
                    cut,
                }),
            );
            assert!(captured.try_iter().next().is_none());
            assert!(host.pane_replay_reserved(conn, &descriptor.token, descriptor.cut));
        }

        w.fx.handle.input(
            conn,
            &marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id: id("root"),
                token: descriptor.token.clone(),
                cut: descriptor.cut,
            }),
        );
        let observed = captured
            .try_iter()
            .next()
            .expect("exact Ready starts replay");
        host.unlisten(conn);
        host.shutdown().unwrap();

        let frame = Frame::from_line(std::str::from_utf8(&observed).unwrap()).unwrap();
        assert!(matches!(
            frame,
            Frame::Notification(note)
                if matches!(note.event, Event::NodePaneFrame(ref frame)
                    if frame.seq == 0
                        && matches!(frame.frame, marion_core::proto::PaneFrameKindV1::Resize { cols: 80, rows: 24 }))
        ));
    }

    /// Read `node/pty` notifications until `want` appears in the accumulated bytes.
    ///
    /// Returns `None` on timeout rather than hanging, so a test that proves a *negative* — a
    /// read-only client's keystrokes never landing — is a test that can fail rather than one that
    /// can only time out.
    fn pty_until(
        r: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
        want: &str,
        bound: std::time::Duration,
    ) -> Option<String> {
        use std::io::BufRead;
        let deadline = std::time::Instant::now() + bound;
        let mut seen = String::new();
        while std::time::Instant::now() < deadline {
            let mut line = String::new();
            // **The socket's own read timeout is not this bound**, and reading through
            // `next_frame` would make it so — its `unwrap` turns a quiet second into a panic. A
            // quiet second is exactly what the negative case (a read-only client's keystrokes
            // never landing) *is*, so it has to be an ordinary outcome here rather than a failure.
            match r.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => panic!("reading the attach socket: {e}"),
            }
            match Frame::from_line(&line).expect("well-formed") {
                Frame::Notification(n) => {
                    if let Event::NodePty { bytes, .. } = n.event {
                        seen.push_str(&bytes);
                        if seen.contains(want) {
                            return Some(seen);
                        }
                    }
                }
                other => panic!("unexpected frame: {other:?}"),
            }
        }
        None
    }

    fn alive(pid: i32) -> bool {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        // SAFETY: signal 0 performs the existence check and delivers nothing.
        unsafe { kill(pid, 0) == 0 }
    }

    /// **The whole inbound path in one test**: attach, be given the write half, type, and see the
    /// *child* answer.
    ///
    /// The echo the line discipline produces would not prove this — a mutation that wrote to the
    /// master and never reached the child would still echo. `got:ping` can only be written by the
    /// shell that read the keystroke, so this dies if `node/pty-write` stops reaching the pty.
    #[test]
    fn an_attached_client_is_given_the_write_half_and_its_keystrokes_reach_the_child() {
        let w = Wired::new("handler-pane-write");
        let _host = pane(&w, "root", "while read line; do echo \"got:$line\"; done");

        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (_, outcome) = attach(&mut c, &mut r, "root", 1);
        let got = attached_ok(outcome);

        let p = got
            .pane
            .expect("a node with a registered pty answers with a pane");
        assert!(p.writable, "the first attacher gets the write half");
        assert_eq!(p.held_by, None);
        assert!(!p.ended, "a live pane has not ended");
        assert_eq!(
            (p.cols, p.rows),
            (80, 24),
            "the size is the master's, read back from the kernel"
        );

        write_keys(&mut c, "root", "ping\r");
        assert!(
            pty_until(&mut r, "got:ping", std::time::Duration::from_secs(10)).is_some(),
            "the keystroke never reached the child"
        );
    }

    /// A v1 input notification has no response envelope. If its durability evidence is refused,
    /// keeping the socket open would tell the terminal that the bytes were accepted even though
    /// the master was deliberately never written. The exact negotiated connection must therefore
    /// close before any terminal `End`; ordinary node lifetime and the next writer remain intact.
    #[test]
    fn opaque_input_evidence_refusal_closes_only_that_pane_client_before_end() {
        use std::io::BufRead;

        let w = Wired::new("handler-pane-opaque-evidence-refusal");
        let received = w.dir.join("received.txt");
        let script = format!(
            "stty -echo; printf ready; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
            received.display()
        );
        let host = pane(&w, "root", &script);
        assert!(
            until(|| host.bytes_read() >= b"ready".len() as u64),
            "the child never entered its input loop"
        );
        let pid = host.child_pid().expect("the pane owns a live child");

        let mut first = w.dial();
        let mut first_reader = std::io::BufReader::new(first.try_clone().unwrap());
        call(
            &mut first,
            Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
                agent_id: id("root"),
                pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
            }),
            1,
        );
        let attached = loop {
            match next_frame(&mut first_reader) {
                Frame::Response(response) => break attached_ok(response.outcome),
                Frame::Notification(note) => assert!(
                    !matches!(note.event, Event::NodePaneFrame(_)),
                    "a pane frame preceded its attach response"
                ),
                other => panic!("unexpected attach frame: {other:?}"),
            }
        };
        let pane = attached.pane.expect("the live pane is attachable");
        assert!(pane.writable, "the v1 client must own the input lease");
        let descriptor = pane.pane_ready.expect("v1 replay was reserved");
        let ready = Frame::Input(marion_core::proto::ClientNotification::new(
            marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id: id("root"),
                token: descriptor.token,
                cut: descriptor.cut,
            }),
        ));
        first.write_all(ready.to_line().as_bytes()).unwrap();
        first.flush().unwrap();
        for expected in 0..descriptor.cut {
            let Frame::Notification(note) = next_frame(&mut first_reader) else {
                panic!("the advertised replay was not delivered")
            };
            let Event::NodePaneFrame(frame) = note.event else {
                panic!("the replay emitted a non-pane notification")
            };
            assert_eq!(frame.seq, expected);
            assert!(
                !matches!(frame.frame, marion_core::proto::PaneFrameKindV1::End {}),
                "a live pane ended during its retained prefix"
            );
        }

        host.fail_next_durable_append("injected opaque input evidence refusal");
        first_reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        write_opaque_keys(&mut first, "root", b"refused\r");
        loop {
            let mut line = String::new();
            match first_reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let frame = Frame::from_line(&line).expect("outbound remains framed");
                    assert!(
                        !matches!(
                            frame,
                            Frame::Notification(note)
                                if matches!(note.event, Event::NodePaneFrame(ref pane)
                                    if matches!(pane.frame, marion_core::proto::PaneFrameKindV1::End {}))
                        ),
                        "the refusal was forged into a successful terminal End"
                    );
                }
                Err(error) => panic!(
                    "the opaque notification failed without visibly closing its socket: {error}"
                ),
            }
        }
        assert!(
            until(|| w.fx.handle.attachments() == 0 && host.writer().is_none()),
            "gone did not release the failed connection's event cursor and write lease"
        );
        assert!(alive(pid), "input evidence failure killed the node");

        let mut second = w.dial();
        let mut second_reader = std::io::BufReader::new(second.try_clone().unwrap());
        let pane = attached_ok(attach(&mut second, &mut second_reader, "root", 2).1)
            .pane
            .expect("the node remains attachable");
        assert!(
            pane.writable,
            "the next client did not receive the released lease"
        );
        write_keys(&mut second, "root", "accepted\r");
        assert!(
            until(|| std::fs::read_to_string(&received)
                .is_ok_and(|contents| contents.contains("accepted\n"))),
            "the surviving node did not receive the next writer's input"
        );
        assert_eq!(
            std::fs::read_to_string(&received).unwrap(),
            "accepted\n",
            "bytes refused before durable evidence still reached the child"
        );
    }

    /// A pane `End` is a successful terminal-stream claim. It cannot overtake an opaque input
    /// notification admitted from the real v1 socket: if that write later fails, the exact client
    /// must depart visibly before an `End` can erase the failure as a clean finish.
    #[test]
    fn admitted_wire_opaque_input_failure_wins_over_terminal_end() {
        use std::io::BufRead;

        let w = Wired::new("handler-pane-opaque-close-race");
        let host = pane(&w, "root", "sleep 30");
        let mut client = w.dial();
        let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
        call(
            &mut client,
            Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
                agent_id: id("root"),
                pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
            }),
            1,
        );
        let attached = loop {
            match next_frame(&mut reader) {
                Frame::Response(response) => break attached_ok(response.outcome),
                Frame::Notification(note) => assert!(
                    !matches!(note.event, Event::NodePaneFrame(_)),
                    "a pane frame preceded its attach response"
                ),
                other => panic!("unexpected attach frame: {other:?}"),
            }
        };
        let pane = attached.pane.expect("a live pane is negotiated");
        assert!(pane.writable);
        let descriptor = pane.pane_ready.expect("v1 replay was reserved");
        let ready = Frame::Input(marion_core::proto::ClientNotification::new(
            marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id: id("root"),
                token: descriptor.token,
                cut: descriptor.cut,
            }),
        ));
        client.write_all(ready.to_line().as_bytes()).unwrap();
        client.flush().unwrap();
        for _ in 0..descriptor.cut {
            let Frame::Notification(note) = next_frame(&mut reader) else {
                panic!("the advertised replay was not delivered")
            };
            assert!(matches!(note.event, Event::NodePaneFrame(_)));
        }

        let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        host.set_control_hook(Box::new(move || {
            admitted_tx.send(()).expect("the assertion side is alive");
            release_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(Duration::from_secs(2))
                .expect("the admitted opaque write was not released");
        }));
        host.fail_next_master_input("injected close-race master delivery refusal");
        write_opaque_keys(&mut client, "root", b"late\r");
        admitted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("wire opaque input was not admitted at the host boundary");

        w.fx.handle.closing_pane(&id("root"), &host);
        let closing = {
            let host = Arc::clone(&host);
            std::thread::spawn(move || host.shutdown())
        };
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("the socket closed before the admitted write reported its failure"),
            Ok(_) => panic!("terminal output overtook the still-admitted opaque write: {line}"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("reading the negotiated pane socket: {error}"),
        }

        release_tx.send(()).expect("the socket delivery is alive");
        let _ = closing.join().unwrap();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let frame = Frame::from_line(&line).expect("outbound remains framed");
                    assert!(
                        !matches!(
                            frame,
                            Frame::Notification(note)
                                if matches!(note.event, Event::NodePaneFrame(ref pane)
                                    if matches!(pane.frame, marion_core::proto::PaneFrameKindV1::End {}))
                        ),
                        "a failed admitted input was followed by a successful End"
                    );
                }
                Err(error) => {
                    panic!("failed admitted input did not visibly close its exact socket: {error}")
                }
            }
        }
    }

    /// Grace expiry is a terminal outcome for an admitted delivery, not permission to publish a
    /// successful pane `End`. This uses a Pending v1 slot and blocks after evidence, when the
    /// control permit has already dropped: shutdown must pass `wait_drained`, expire the delivery
    /// grace, and close this exact socket before the master outcome is released.
    #[test]
    fn unresolved_pending_wire_input_is_visibly_failed_before_terminal_end() {
        use std::io::BufRead;

        let w = Wired::new("handler-pane-unresolved-input-grace");
        let host = pane(&w, "root", "sleep 30");
        let mut client = w.dial();
        let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
        call(
            &mut client,
            Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
                agent_id: id("root"),
                pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
            }),
            1,
        );
        let attached = loop {
            match next_frame(&mut reader) {
                Frame::Response(response) => break attached_ok(response.outcome),
                Frame::Notification(note) => assert!(
                    !matches!(note.event, Event::NodePaneFrame(_)),
                    "a pane frame preceded its attach response"
                ),
                other => panic!("unexpected attach frame: {other:?}"),
            }
        };
        let pane = attached.pane.expect("a live pane is negotiated");
        assert!(pane.writable);
        assert!(pane.pane_ready.is_some(), "the v1 slot remains Pending");

        let (admitted_tx, admitted_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        host.set_post_cast_input_hook(Box::new(move || {
            admitted_tx
                .send(())
                .expect("the assertion side remains alive");
            release_rx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv_timeout(Duration::from_secs(2))
                .expect("the unresolved master outcome was not released");
        }));
        write_opaque_keys(&mut client, "root", b"pending\r");
        admitted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Pending wire input never reached the post-evidence seam");

        w.fx.handle.closing_pane(&id("root"), &host);
        let closing = {
            let host = Arc::clone(&host);
            std::thread::spawn(move || host.shutdown())
        };
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let before_release = loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break Ok(()),
                Ok(_) => {
                    let frame = Frame::from_line(&line).expect("outbound remains framed");
                    if matches!(
                        frame,
                        Frame::Notification(note)
                            if matches!(note.event, Event::NodePaneFrame(ref pane)
                                if matches!(pane.frame, marion_core::proto::PaneFrameKindV1::End {}))
                    ) {
                        break Err("terminal End preceded the grace-expiry refusal".to_string());
                    }
                }
                Err(error) => {
                    break Err(format!(
                        "the unresolved exact socket stayed open past delivery grace: {error}"
                    ));
                }
            }
        };

        // Cleanup cannot cause the observation above: the socket outcome was read to completion
        // while the delivery hook still held the unresolved master write.
        release_tx
            .send(())
            .expect("the blocked delivery remains alive for cleanup");
        let _ = closing.join().unwrap();
        before_release.expect("grace expiry must visibly fail the exact socket before End");
    }

    /// The pane-v1 keyboard's happy path, over the real socket: a negotiated client's very first
    /// `node/pane-write`, sent the moment the Ready handshake completes, reaches the child's stdin.
    ///
    /// Every other opaque-input test in this module is a refusal. None of them asserted that an
    /// *admitted* write is delivered, so a path that admitted and then lost the bytes — or a
    /// client whose first keystroke raced the slot into an inadmissible phase — would have had no
    /// test to fail. The oracle is what the child read, not the `i` record: pane-v1 input is
    /// evidenced by length only (`PtyHost::write_opaque_input_admitted`), so `pty.cast` carries no
    /// `i` for it by design, and that is asserted too so nobody reaches for it as an oracle again.
    #[test]
    fn a_negotiated_clients_first_opaque_keystroke_reaches_the_child() {
        let w = Wired::new("handler-pane-opaque-first-key");
        let received = w.dir.join("first-key-received.txt");
        let script = format!(
            "stty -echo; printf ready; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
            received.display()
        );
        let host = pane(&w, "root", &script);
        assert!(
            until(|| host.bytes_read() >= b"ready".len() as u64),
            "the child never entered its input loop"
        );

        let mut client = w.dial();
        let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
        call(
            &mut client,
            Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
                agent_id: id("root"),
                pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
            }),
            1,
        );
        let attached = loop {
            match next_frame(&mut reader) {
                Frame::Response(response) => break attached_ok(response.outcome),
                Frame::Notification(note) => assert!(
                    !matches!(note.event, Event::NodePaneFrame(_)),
                    "a pane frame preceded its attach response"
                ),
                other => panic!("unexpected attach frame: {other:?}"),
            }
        };
        let pane = attached.pane.expect("the live pane is attachable");
        assert!(pane.writable, "the v1 client must own the input lease");
        let descriptor = pane.pane_ready.expect("v1 replay was reserved");
        let ready = Frame::Input(marion_core::proto::ClientNotification::new(
            marion_core::proto::Input::NodePaneReady(marion_core::proto::NodePaneReadyV1 {
                agent_id: id("root"),
                token: descriptor.token,
                cut: descriptor.cut,
            }),
        ));
        client.write_all(ready.to_line().as_bytes()).unwrap();
        client.flush().unwrap();
        // Typed immediately behind Ready, the way `marion attach` starts its keyboard: no replay
        // frame is waited for first, so this is the earliest a real client can type.
        write_opaque_keys(&mut client, "root", b"typed\r");

        assert!(
            until(|| std::fs::read_to_string(&received)
                .is_ok_and(|contents| contents.contains("typed\n"))),
            "the first admitted opaque keystroke never reached the child. Received: {:?}",
            std::fs::read_to_string(&received).unwrap_or_default()
        );
        let cast = std::fs::read_to_string(w.dir.join("root.cast")).unwrap();
        assert!(
            !cast.contains("\"i\""),
            "pane-v1 input is evidenced by length only; an `i` record means the opaque path \
             started persisting raw keyboard payloads:\n{cast}"
        );
        // The socket is still a live, framed connection: nothing about the delivery departed it.
        call(
            &mut client,
            Call::NodeGet(marion_core::proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            2,
        );
        loop {
            match next_frame(&mut reader) {
                Frame::Response(_) => break,
                Frame::Notification(note) => assert!(
                    matches!(note.event, Event::NodePaneFrame(_)),
                    "an unexpected notification followed the keystroke: {note:?}"
                ),
                other => panic!("the admitted keystroke closed the connection: {other:?}"),
            }
        }
    }

    /// `node/pane-write` is negotiated protocol, not an alternate spelling for legacy input. A
    /// legacy writer has a keyboard lease but no pane stream slot; forged opaque bytes must reach
    /// neither the master nor silence, so the exact legacy socket is visibly closed.
    #[test]
    fn legacy_attach_cannot_forge_opaque_pane_input() {
        use std::io::BufRead;

        let w = Wired::new("handler-pane-forged-opaque");
        let received = w.dir.join("forged-received.txt");
        let script = format!(
            "stty -echo; printf ready; while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done",
            received.display()
        );
        let host = pane(&w, "root", &script);
        assert!(until(|| host.bytes_read() >= b"ready".len() as u64));
        let mut client = w.dial();
        let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
        assert!(
            attached_ok(attach(&mut client, &mut reader, "root", 1).1)
                .pane
                .expect("the pane is live")
                .writable
        );

        write_opaque_keys(&mut client, "root", b"forged\r");
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut line = String::new();
        assert_eq!(
            reader
                .read_line(&mut line)
                .expect("the socket read is bounded"),
            0,
            "unnegotiated opaque input was ignored instead of visibly refused"
        );
        assert!(
            until(|| host.writer().is_none()),
            "gone did not release the forged sender's lease"
        );
        assert!(
            !received.exists() || std::fs::read(&received).unwrap().is_empty(),
            "unnegotiated opaque input reached the child"
        );
    }

    #[test]
    fn legacy_input_keeps_its_compatibility_delivery_on_durable_evidence_failure() {
        let w = Wired::new("handler-pane-legacy-evidence-failure");
        let host = pane(
            &w,
            "root",
            "stty -echo; while IFS= read -r line; do printf 'got:%s\\n' \"$line\"; done",
        );
        let mut client = w.dial();
        let mut reader = std::io::BufReader::new(client.try_clone().unwrap());
        let attached = attached_ok(attach(&mut client, &mut reader, "root", 1).1);
        assert!(attached.pane.expect("the pane is live").writable);

        host.fail_next_durable_append("injected legacy evidence failure");
        write_keys(&mut client, "root", "accepted\r");
        assert!(
            pty_until(&mut reader, "got:accepted", Duration::from_secs(10)).is_some(),
            "the legacy compatibility path stopped delivering after evidence failure"
        );
        call(
            &mut client,
            Call::NodeGet(marion_core::proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            2,
        );
        assert!(
            matches!(next_frame(&mut reader), Frame::Response(_)),
            "legacy evidence failure closed the connection"
        );
    }

    /// §5.3's one-writer rule, over the socket and **by name**.
    ///
    /// Two halves, and the second is the one that matters: the refusal is not merely reported, it
    /// is enforced. A supervisor that answered `writable: false` and then wrote the bytes anyway
    /// would pass a test that only read the response.
    #[test]
    fn a_second_attacher_is_refused_the_write_half_by_name_and_cannot_type() {
        let w = Wired::new("handler-pane-second");
        let _host = pane(&w, "root", "while read line; do echo \"got:$line\"; done");

        let mut first = w.dial();
        let mut fr = std::io::BufReader::new(first.try_clone().unwrap());
        let (_, outcome) = attach(&mut first, &mut fr, "root", 1);
        let a = attached_ok(outcome).pane.expect("a pane");
        assert!(a.writable);

        let mut second = w.dial();
        let mut sr = std::io::BufReader::new(second.try_clone().unwrap());
        let (_, outcome) = attach(&mut second, &mut sr, "root", 1);
        let b = attached_ok(outcome).pane.expect("a pane");
        assert!(
            !b.writable,
            "two writers on one pty interleave into nonsense"
        );
        assert!(
            b.held_by.is_some(),
            "a client told only `busy` cannot tell a colleague from a lease nobody released"
        );

        // The refusal is enforced, not merely announced.
        write_keys(&mut second, "root", "sneak\r");
        assert!(
            pty_until(&mut sr, "got:sneak", std::time::Duration::from_secs(2)).is_none(),
            "a read-only client typed into the node anyway"
        );
        // And the writer still works, so the block is about the lease and not about the socket.
        write_keys(&mut first, "root", "ping\r");
        assert!(
            pty_until(&mut fr, "got:ping", std::time::Duration::from_secs(10)).is_some(),
            "refusing the second client broke the first"
        );
    }

    /// **A resize reaches the pty and the child sees it as a `SIGWINCH`.**
    ///
    /// The `stty size` the trap prints is read by the shell *through its controlling terminal*, so
    /// it is the kernel's answer and not marion's: this dies if `node/resize` stops reaching
    /// `TIOCSWINSZ`, and it dies if the explicit `killpg(SIGWINCH)` stops being sent to a child
    /// that would otherwise never look.
    #[test]
    fn a_resize_reaches_the_pty_and_the_child_is_told() {
        let w = Wired::new("handler-pane-resize");
        let host = pane(
            &w,
            "root",
            // **`ready` is printed after the trap is installed**, and the test waits for it. A
            // signal delivered to a shell that has not reached its `trap` yet is simply lost, so
            // without this the test races the child's startup and fails under load — a flake that
            // says nothing about whether resize works.
            "stty -echo; trap 'stty size' WINCH; echo ready; while :; do sleep 0.05; done",
        );

        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (_, outcome) = attach(&mut c, &mut r, "root", 1);
        assert!(attached_ok(outcome).pane.expect("a pane").writable);
        assert!(
            pty_until(&mut r, "ready", std::time::Duration::from_secs(10)).is_some(),
            "the child never installed its WINCH trap"
        );

        send_resize(&mut c, "root", 140, 40);
        // `stty size` prints "rows cols".
        assert!(
            pty_until(&mut r, "40 140", std::time::Duration::from_secs(10)).is_some(),
            "the child was never told the new size"
        );
        let size = host.master().size().expect("TIOCGWINSZ");
        assert_eq!((size.cols, size.rows), (140, 40), "the master itself moved");
    }

    /// A read-only attacher must not resize either. The geometry is the *shared* master's, so a
    /// second client reflowing it would repaint the writer's pane from under them with nothing
    /// anywhere naming the cause — the same invisibility §5.3 gives for interleaved keystrokes.
    #[test]
    fn a_read_only_attacher_cannot_resize_the_node() {
        let w = Wired::new("handler-pane-resize-ro");
        let host = pane(&w, "root", "sleep 30");

        let mut first = w.dial();
        let mut fr = std::io::BufReader::new(first.try_clone().unwrap());
        attached_ok(attach(&mut first, &mut fr, "root", 1).1);

        let mut second = w.dial();
        let mut sr = std::io::BufReader::new(second.try_clone().unwrap());
        assert!(
            !attached_ok(attach(&mut second, &mut sr, "root", 1).1)
                .pane
                .expect("a pane")
                .writable
        );

        send_resize(&mut second, "root", 200, 60);
        // Nothing to wait *for*, so wait for the supervisor to have processed something later on
        // the same connection instead of sleeping on a hope.
        call(
            &mut second,
            Call::NodeGet(marion_core::proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            9,
        );
        assert!(matches!(next_frame(&mut sr), Frame::Response(_)));
        let size = host.master().size().expect("TIOCGWINSZ");
        assert_eq!(
            (size.cols, size.rows),
            (80, 24),
            "a read-only client reflowed the writer's pane"
        );
    }

    /// **Detach leaves the node running and hands the keyboard back.**
    ///
    /// This is M2's property at the surface M3 adds, and both halves are asserted because they
    /// fail independently: a supervisor that killed the node on departure would pass the lease
    /// half, and one that never released the lease would leave the node permanently read-only
    /// while the process ran on.
    #[test]
    fn a_client_departing_leaves_the_node_running_and_releases_its_keyboard() {
        let w = Wired::new("handler-pane-detach");
        let host = pane(&w, "root", "while read line; do echo \"got:$line\"; done");
        let pid = host.child_pid().expect("a child");

        let mut first = w.dial();
        let mut fr = std::io::BufReader::new(first.try_clone().unwrap());
        assert!(
            attached_ok(attach(&mut first, &mut fr, "root", 1).1)
                .pane
                .expect("a pane")
                .writable
        );

        // The client goes away without saying anything — §7.3.1's crash case, which is also what
        // `^] d` looks like from here once the attach process exits.
        drop(fr);
        drop(first);
        assert!(
            until(|| w.fx.handle.attachments() == 0),
            "the supervisor never noticed the departure"
        );

        assert!(alive(pid), "the node was killed by a client going away");
        let mut second = w.dial();
        let mut sr = std::io::BufReader::new(second.try_clone().unwrap());
        let p = attached_ok(attach(&mut second, &mut sr, "root", 1).1)
            .pane
            .expect("a pane");
        assert!(
            p.writable,
            "the departed client's lease was never released: {:?}",
            p.held_by
        );
        write_keys(&mut second, "root", "ping\r");
        assert!(
            pty_until(&mut sr, "got:ping", std::time::Duration::from_secs(10)).is_some(),
            "the node survived but nobody can type into it"
        );
        assert!(alive(pid), "the node is still the same process");
    }

    /// A node with no display plane answers `pane: None`, and that is a fact about the node rather
    /// than a failure of the attach — every other node in this file is one, and none of them
    /// regressed.
    #[test]
    fn a_node_with_no_pty_attaches_with_no_pane() {
        let w = Wired::new("handler-pane-none");
        let stream = events_of(&w.fx, "root");
        say(&stream, "root", &["one"]);
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let got = attached_ok(attach(&mut c, &mut r, "root", 1).1);
        assert_eq!(got.pane, None);
        assert_eq!(w.fx.handle.panes(), 0);
        // And a keystroke aimed at it is dropped rather than answered, crashing nothing.
        write_keys(&mut c, "root", "x");
        call(
            &mut c,
            Call::NodeGet(marion_core::proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            2,
        );
        assert!(matches!(next_frame(&mut r), Frame::Response(_)));
    }

    /// **The whole of §7.3.3, on one connection**: everything written while nobody was listening
    /// arrives as replay, the answer's read point is a statement about what has *already* been
    /// sent, and what the node says next arrives unsolicited on the same socket.
    ///
    /// The contiguity assertion is the seam. `events.rs` argues it is unreachable to get wrong
    /// because replay and subscribe are one cursor; this is that argument being spent — the
    /// ordinals across the two legs are `0..5` with nothing missing and nothing twice.
    #[test]
    fn node_attach_replays_the_detached_window_and_then_follows_the_same_cursor_live() {
        let w = Wired::new("handler-attach");
        let stream = events_of(&w.fx, "root");
        say(&stream, "root", &["before-1", "before-2", "before-3"]);

        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (replay, outcome) = attach(&mut c, &mut r, "root", 1);
        let got = attached_ok(outcome);

        assert_eq!(got.node.agent_id, id("root"));
        assert!(
            got.mode.is_live(),
            "a node that has not exited is a re-subscribe: {:?}",
            got.mode
        );
        assert_eq!(
            heard(&replay),
            vec![
                (0, "before-1".into()),
                (1, "before-2".into()),
                (2, "before-3".into())
            ],
            "the detached window is replayed in full, in order, exactly once"
        );
        assert_eq!(
            got.mode.replay_point().records,
            3,
            "the read point counts what the client has already been sent, not what it may expect"
        );

        // Written by a *second* writer, after the attach — which is the production case: the
        // supervisor is not the process driving this node.
        say(&stream, "root", &["after-1", "after-2", "after-3"]);
        let mut live = Vec::new();
        while live.len() < 3 {
            let Frame::Notification(n) = next_frame(&mut r) else {
                panic!("live events arrive as notifications on the connection the client has")
            };
            live.push(n.event);
        }
        assert_eq!(
            heard(&live),
            vec![
                (3, "after-1".into()),
                (4, "after-2".into()),
                (5, "after-3".into())
            ],
            "the subscribe leg continues the replay's own ordinals: no gap, no repeat, no join"
        );
    }

    /// **Two clients, two cursors.** A shared reader would make the second client's replay depend
    /// on when the first attached, which is the same collapse `events.rs` refuses between "nobody
    /// read this" and "there was nothing to read".
    #[test]
    fn a_second_client_attaching_later_gets_its_own_replay_from_the_beginning() {
        let w = Wired::new("handler-attach-two");
        let stream = events_of(&w.fx, "root");
        say(&stream, "root", &["one", "two"]);

        let mut a = w.dial();
        let mut ar = std::io::BufReader::new(a.try_clone().unwrap());
        let (first, outcome) = attach(&mut a, &mut ar, "root", 1);
        attached_ok(outcome);
        assert_eq!(heard(&first).len(), 2);

        say(&stream, "root", &["three"]);
        // A's live leg, drained so the two clients cannot be confused for one.
        let Frame::Notification(_) = next_frame(&mut ar) else {
            panic!("A hears the third event")
        };

        let mut b = w.dial();
        let mut br = std::io::BufReader::new(b.try_clone().unwrap());
        let (second, outcome) = attach(&mut b, &mut br, "root", 1);
        let got = attached_ok(outcome);
        assert_eq!(
            heard(&second),
            vec![(0, "one".into()), (1, "two".into()), (2, "three".into())],
            "B replays the whole file, not the tail A had not read"
        );
        assert_eq!(got.mode.replay_point().records, 3);
        assert!(until(|| w.fx.handle.attachments() == 2));
    }

    /// **A node marion never recorded is not a node that said nothing** — and the two are only
    /// distinguishable while something can still be written, which is why the answer turns on
    /// whether the node has exited.
    #[test]
    fn an_exited_node_with_no_stream_is_refused_as_unrecorded_rather_than_replayed_as_silent() {
        let w = Wired::new("handler-attach-unrecorded");
        append(
            &w.fx.path,
            &line(
                9,
                9_000,
                RecordKind::Exited(Exited {
                    agent_id: id("root"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "finished while nobody was attached".into(),
                    },
                }),
            ),
        );
        assert!(until(|| {
            w.fx.handle.live.refresh();
            w.fx.handle
                .live
                .read(|r| r.tree().get(&id("root")).unwrap().state.is_exited())
        }));

        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (notes, outcome) = attach(&mut c, &mut r, "root", 1);
        assert!(notes.is_empty(), "nothing was replayed");
        let e = refusal(outcome);
        assert_eq!(e.kind(), Some(FailureKind::NotFound));
        assert!(
            e.message.contains("not** an empty transcript"),
            "the refusal names which of the two facts it is: {e}"
        );
        assert_eq!(
            w.fx.handle.attachments(),
            0,
            "a refused attach leaves no cursor behind"
        );
    }

    /// The other side of the same split: a node that has **not** exited and has written nothing yet
    /// is followed, because its file appears on its first frame and the reader is already watching
    /// the name. Refusing here would make a client unable to attach to a node that is starting —
    /// the one an operator is most likely watching (§2's own argument for `tree/node-added`).
    #[test]
    fn a_live_node_that_has_not_spoken_yet_is_followed_and_its_first_frame_arrives() {
        let w = Wired::new("handler-attach-silent");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (notes, outcome) = attach(&mut c, &mut r, "root", 1);
        let got = attached_ok(outcome);
        assert!(notes.is_empty());
        assert!(got.mode.is_live());
        assert_eq!(got.mode.replay_point().records, 0);

        say(&events_of(&w.fx, "root"), "root", &["first-word"]);
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("the first frame of a node that had not spoken still reaches its client")
        };
        assert_eq!(heard(&[n.event]), vec![(0, "first-word".into())]);
    }

    /// A node the journal has no record of. The refusal carries the read point for the same reason
    /// `node/get`'s does: *"no such node"* and *"not yet"* are different answers.
    #[test]
    fn attaching_to_a_node_the_journal_never_recorded_is_a_refusal_that_says_how_much_was_read() {
        let w = Wired::new("handler-attach-missing");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (notes, outcome) = attach(&mut c, &mut r, "nobody", 1);
        assert!(notes.is_empty());
        let e = refusal(outcome);
        assert_eq!(e.kind(), Some(FailureKind::NotFound));
        assert!(e.is_refusal());
        assert!(e.message.contains("records read"), "{e}");
        assert_eq!(w.fx.handle.attachments(), 0);
    }

    /// **§7.2 meets §7.3.3.** An `Orphaned` node is one whose channel *no* supervisor holds: this
    /// one booted over a journal it did not write, and `restart.rs` marked the node because the
    /// record shows no decision about its fate. `ResubscribeFrom` asserts the opposite — that the
    /// supervisor has held the channel since `t=0` — so answering it here would promise live events
    /// that can never arrive. The orphan is a replayable record whose operator option is to bring it
    /// back, which is exactly what `ReplayResumable` says; that holds whether the orphan's last
    /// state was mid-turn or, per §7.2's *"the process may be gone"*, an exit the record never saw.
    #[test]
    fn an_orphaned_node_attaches_as_replay_resumable_and_never_as_a_live_channel() {
        let p = || ReplayPoint {
            records: 7,
            src_seq: None,
        };
        for state in [
            NodeState::Running,
            NodeState::Idle,
            NodeState::Exited(ExitStatus::Ok),
        ] {
            let mode = attach_mode(state, ReapState::Orphaned, p());
            assert!(
                matches!(mode, AttachMode::ReplayResumable(_)),
                "an orphan in state {state:?} answered {mode:?}"
            );
            assert!(!mode.is_live(), "no supervisor holds an orphan's channel");
            assert_eq!(mode.replay_point(), &p());
        }
    }

    /// §7.3.3's three answers, at the one place that chooses between them.
    #[test]
    fn the_attach_mode_is_derived_from_the_nodes_two_state_fields_and_nothing_else() {
        let p = || ReplayPoint {
            records: 4,
            src_seq: None,
        };
        assert!(matches!(
            attach_mode(NodeState::Running, ReapState::Live, p()),
            AttachMode::ResubscribeFrom(_)
        ));
        assert!(matches!(
            attach_mode(NodeState::Exited(ExitStatus::Ok), ReapState::Live, p()),
            AttachMode::ReplayOnly(_)
        ));
        // Reaped wins over exited: §7.3.2's disposition (c) reaps an *idle* node, and the operator's
        // option — bring it back — is what `ReplayResumable` exists to say.
        assert!(matches!(
            attach_mode(NodeState::Idle, ReapState::ReapedIdle, p()),
            AttachMode::ReplayResumable(_)
        ));
        assert!(matches!(
            attach_mode(
                NodeState::Exited(ExitStatus::Ok),
                ReapState::ReapedIdle,
                p()
            ),
            AttachMode::ReplayResumable(_)
        ));
        assert_eq!(
            attach_mode(NodeState::Running, ReapState::Live, p()).replay_point(),
            &p(),
            "the point is carried, never recomputed"
        );
    }

    /// **§7.3.1, on the attach path.** A client that leaves takes its cursor with it and nothing
    /// else: the node goes on writing, which is what makes the next client's attach a replay.
    #[test]
    fn a_departed_client_stops_being_followed_and_the_node_keeps_recording() {
        let w = Wired::new("handler-attach-gone");
        let stream = events_of(&w.fx, "root");
        say(&stream, "root", &["one"]);
        {
            let mut c = w.dial();
            let mut r = std::io::BufReader::new(c.try_clone().unwrap());
            let (replay, outcome) = attach(&mut c, &mut r, "root", 1);
            attached_ok(outcome);
            assert_eq!(heard(&replay).len(), 1);
            assert!(until(|| w.fx.handle.attachments() == 1));
        }
        assert!(
            until(|| w.fx.handle.attachments() == 0),
            "the cursor is dropped when its connection ends"
        );

        say(&stream, "root", &["two", "three"]);
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (replay, outcome) = attach(&mut c, &mut r, "root", 1);
        attached_ok(outcome);
        assert_eq!(
            heard(&replay),
            vec![(0, "one".into()), (1, "two".into()), (2, "three".into())],
            "everything written across the detached window is there, exactly once"
        );
    }

    /// An event marion **read and deliberately did not keep** is still delivered as the fact it is.
    /// The alternative — dropping it — would make a client's `agent_seq` run non-contiguous, which
    /// is the one signal it has for loss.
    #[test]
    fn a_withheld_payload_is_delivered_as_a_withholding_not_omitted_from_the_stream() {
        let w = Wired::new("handler-attach-withheld");
        let stream = events_of(&w.fx, "root");
        {
            let mut writer = EventWriter::open_path(&stream, &id("root")).unwrap();
            writer.record(Draft::observed(
                Payload::Raw("one".into()),
                Source::Protocol,
            ));
            writer.record(Draft::observed(
                Payload::Withheld {
                    key: "control_response".into(),
                    bytes: 30_000,
                    reason: "§5.2".into(),
                },
                Source::Protocol,
            ));
            writer.record(Draft::observed(
                Payload::Oversized {
                    was: PayloadKind::Vendor,
                    bytes: 1,
                },
                Source::Protocol,
            ));
            writer.sync().unwrap();
        }
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let (replay, outcome) = attach(&mut c, &mut r, "root", 1);
        attached_ok(outcome);
        let seqs: Vec<u64> = replay
            .iter()
            .map(|e| match e {
                Event::NodeEvent { agent_seq, .. } => *agent_seq,
                other => panic!("{}", other.method()),
            })
            .collect();
        assert_eq!(seqs, vec![0, 1, 2], "no ordinal is skipped");
        let Event::NodeEvent { payload, .. } = &replay[1] else {
            unreachable!()
        };
        assert!(payload.get("Withheld").is_some(), "{payload}");
        let Event::NodeEvent { payload, .. } = &replay[2] else {
            unreachable!()
        };
        assert!(payload.get("Oversized").is_some(), "{payload}");
    }

    /// **§11 item 28 step 4** — the supervisor owning a node's lifecycle: the table, the capability
    /// token, and `agent/spawn` answered rather than refused.
    ///
    /// Driven by a **test client** and no bridge, which is what makes this landable ahead of step 5.
    /// The design's point 4 says (a) and (b) are atomic on the wire — a handler with no client is dead
    /// code and a client with no handler cannot spawn — and names exactly this as what can split off.
    /// So the client here is `RegistryHandle::call` itself, reached the way `serve` reaches it.
    #[cfg(test)]
    mod owns_nodes {
        use super::*;
        use marion_core::paths::ProjectDir;
        use marion_core::proto::params::AgentSpawnParams;
        use marion_core::proto::{
            NativeEnvVarV1, NativeLaunchContextV1, NativeLaunchContextV2, OpaqueOsValueV1,
            SpawnCaller, TerminalGeometryV1,
        };
        use marion_testsupport::{fixture_repo, scratch};
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        /// A handle that **owns** what it spawns, over a real repo and a real project directory.
        struct Owning {
            handle: Arc<RegistryHandle>,
            project: ProjectDir,
            /// The tree these nodes live in. Held because it is now a per-node fact the fixture
            /// has to state when it claims one — see [`NodeHandle::repo`].
            repo: PathBuf,
            /// **Declared last so it is dropped last.** Rust drops fields in declaration order,
            /// and a scratch directory removed while a node's thread is still writing into it
            /// would turn a clean failure into an unrelated io error.
            _dir: marion_testsupport::Scratch,
        }

        fn owning(tag: &str, records: Vec<RecordKind>) -> Owning {
            let dir = scratch(tag);
            let repo = fixture_repo(&dir);
            let state = dir.join("state");
            // **Keyed the way production keys it** — `marion.rs` builds every one of the socket, the
            // `Launch` and the `ProjectDir` from a single `socket::project_root(&repo)`, which for a
            // repository is its git common dir and not its working tree. Keying on `repo` here gave
            // the fixture a supervisor whose project no client could name, which nothing noticed
            // until `spawn_root` began comparing the two (the sibling worktree fixture below always
            // keyed correctly, which is why it alone kept passing).
            let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
            std::fs::create_dir_all(project.path()).unwrap();
            let journal = project.journal();
            // Booted before the records are written, which is the production order — `tests::fx_with`
            // gives the argument, and it matters more here: a node already `Live` at boot is one this
            // supervisor never decided the fate of and `restart.rs` marks `Orphaned`.
            let live = Arc::new(crate::registry::LiveRegistry::follow(
                Registry::boot_path(&journal).unwrap(),
                std::time::Duration::from_millis(2),
            ));
            for (seq, kind) in records.into_iter().enumerate() {
                append(&journal, &line(seq as u64, 1_000 + seq as u64, kind));
            }
            live.refresh();
            let handle = RegistryHandle::owning(
                live,
                crate::run::Env {
                    project_dir: project.clone(),
                    state: state.clone(),
                    project_root: crate::socket::project_root(&repo),
                    bridge: std::path::PathBuf::from("/bin/marion-supervisor"),
                    // Answers nothing, which is what bounds the two tests below that really launch.
                    base_url: Some("http://127.0.0.1:8099/v1".into()),
                    auth: marion_harness::Auth::Canned,
                },
            );
            Owning {
                handle,
                project,
                repo,
                _dir: dir,
            }
        }

        fn params(caller: Option<SpawnCaller>, secs: u64) -> AgentSpawnParams {
            AgentSpawnParams {
                agent_type: "claude".into(),
                prompt: "do the task".into(),
                native_launch: None,
                caller,
                // Root-only, and every caller in this helper's `Some` half would be refused by
                // name for stating it — see `a_caller_that_states_no_change_record_is_refused`.
                no_change_record: None,
                pane: None,
                isolation: None,
                allow_concurrent_writes: None,
                // Every caller in this module is a `Some`, and a `Some` that states a repository
                // is refused by name — see `a_caller_that_states_its_own_repository_is_refused`.
                repo: None,
                acceptance_criteria: vec![],
                verification: vec![],
                writable_scope: vec!["src/**".into()],
                timeout_secs: Some(secs),
                model: None,
            }
        }

        fn spawn(
            fx: &Owning,
            p: AgentSpawnParams,
        ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
            let out = crate::serve::sink(ConnId(3));
            match fx.handle.call(ConnId(3), &Call::AgentSpawn(p), &out)? {
                MethodResult::AgentSpawn(r) => Ok(r),
                other => panic!("wrong result: {}", other.method().as_str()),
            }
        }

        fn journal_len(fx: &Owning) -> usize {
            std::fs::read_to_string(fx.project.journal())
                .map(|s| s.lines().count())
                .unwrap_or(0)
        }

        // ------------------------------------------------------------------------------------
        // F2: the caller states who it is and proves it, and every gated fact is derived.
        // ------------------------------------------------------------------------------------

        /// **T6's required regression: a socket spawn with a forged `SpawnCaller.agent_id` is
        /// refused.**
        ///
        /// This is the whole of F2. `serve_conn` performs no peer-credential check — `Handle::call`
        /// receives a `ConnId` and learns nothing about the process on the other end — so once
        /// `agent/spawn` is on the socket, *any* process that can `connect(2)` can name a node. The
        /// token is what separates naming from being.
        ///
        /// Both halves are asserted, because only together do they mean anything: the call is refused,
        /// **and nothing was journaled**. A refusal that arrived after the `SpawnIntent` was written
        /// would leave a node in the tree that no caller was ever entitled to create, and §5.7 would
        /// then hold the supervisor resident for it.
        #[test]
        fn a_socket_spawn_with_a_forged_caller_is_refused_and_journals_nothing() {
            let fx = owning("owns-forged", vec![intent("root", None, "claude", 0)]);
            let real = fx.handle.claim(
                &id("root"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            let before = journal_len(&fx);
            // **Derived from the real token, never a fixed digit.** Spelling this as
            // `format!("{}0", &real[..real.len() - 1])` made the case *conditional on the token's
            // own last character*: one token in sixteen already ends in `0`, and for those this row
            // handed the handler the genuine token, which was accepted — a `Spawning` result where
            // a refusal was asserted, and a suite failure that came and went with the entropy.
            let last_byte_changed = {
                let (head, tail) = real.split_at(real.len() - 1);
                format!("{head}{}", if tail == "0" { '1' } else { '0' })
            };
            assert_ne!(
                last_byte_changed, real,
                "a forgery must differ from the real token"
            );

            for (token, why) in [
                ("not-the-token".to_string(), "a guess"),
                (String::new(), "an empty token"),
                (format!("{real}x"), "the real token with a byte appended"),
                (real[..real.len() - 1].to_string(), "a truncated prefix"),
                (
                    last_byte_changed,
                    "the real token with its last byte changed",
                ),
            ] {
                let e = spawn(
                    &fx,
                    params(
                        Some(SpawnCaller {
                            agent_id: id("root"),
                            node_token: token,
                        }),
                        1,
                    ),
                )
                .expect_err(&format!("{why} must not be accepted"));
                assert_eq!(e.kind(), Some(FailureKind::Refused), "{why}: {e:?}");
                assert!(
                    e.message.contains("did not mint that node token"),
                    "{why}: the refusal must say what was not established: {}",
                    e.message
                );
            }
            assert_eq!(
                journal_len(&fx),
                before,
                "a refused spawn must journal nothing at all — not even an intent"
            );
            assert_eq!(
                fx.handle.owned_nodes(),
                1,
                "…and must not add a node to the table"
            );
        }

        /// A caller naming a node **no supervisor ever owned** is refused by the same sentence and the
        /// same path. Distinguishing "no such node" from "wrong token" in the answer would make node
        /// existence an oracle a caller could probe with ids alone.
        #[test]
        fn a_caller_naming_a_node_this_supervisor_does_not_own_is_refused_the_same_way() {
            let fx = owning("owns-unknown", vec![intent("root", None, "claude", 0)]);
            let e = spawn(
                &fx,
                params(
                    Some(SpawnCaller {
                        agent_id: id("nobody"),
                        node_token: "anything".into(),
                    }),
                    1,
                ),
            )
            .expect_err("an unowned node cannot spawn");
            assert_eq!(e.kind(), Some(FailureKind::Refused));
            assert!(
                e.message.contains("did not mint that node token"),
                "{}",
                e.message
            );
            // A node that *is* in the journal but was not claimed is the same case: the journal is not
            // the ownership record, the table is.
            let e = spawn(
                &fx,
                params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: "anything".into(),
                    }),
                    1,
                ),
            )
            .expect_err("a journalled node this supervisor never claimed cannot spawn either");
            assert!(
                e.message.contains("did not mint that node token"),
                "{}",
                e.message
            );
        }

        /// **§6.1 step 2's depth, read from the registry rather than from the call.** `SpawnCaller`
        /// carries no depth to forge, so the only way to be refused is for the supervisor to have gone
        /// and looked — and the only way to pass is to genuinely be shallow enough.
        ///
        /// The pair is the assertion: the *same* call shape is refused for a node the journal places at
        /// `max_depth` and admitted for one it places below it. A gate that read a constant, or that
        /// read anything the caller sent, could not tell the two apart.
        #[test]
        fn the_depth_gate_reads_the_callers_depth_from_the_registry() {
            let ty = agent_type::builtin("claude").unwrap();
            let deep = owning(
                "owns-depth-deep",
                vec![intent("deep", None, "claude", ty.max_depth)],
            );
            let token = deep.handle.claim(
                &id("deep"),
                Some(marion_core::contract::TaskId("t".into())),
                deep.repo.clone(),
            );
            let before = journal_len(&deep);
            let e = spawn(
                &deep,
                params(
                    Some(SpawnCaller {
                        agent_id: id("deep"),
                        node_token: token,
                    }),
                    1,
                ),
            )
            .expect_err("a caller at max_depth may not spawn");
            assert_eq!(e.kind(), Some(FailureKind::Refused));
            assert!(
                e.message.contains("max_depth"),
                "the refusal must name the bound: {}",
                e.message
            );
            assert_eq!(
                journal_len(&deep),
                before,
                "§6.1 step 2 runs before every side effect, so a gated spawn creates nothing"
            );
        }

        /// **`live_children` inverted: counted off the registry, not off a caller's table.**
        ///
        /// `background.rs` argued the bridge's table was authoritative *by construction* because
        /// `Spawned` was written after the whole run, so a journal read could not see a child between
        /// "thread started" and "process observed". Step 1 inverted that premise — `SpawnIntent` is
        /// journaled before every side effect — and this is the assertion that the count now comes from
        /// there. Nothing in the call says how many children the caller has; the journal does.
        ///
        /// The boundary is asserted from both sides, so a count that was simply always zero (which is
        /// what the constant `LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER` was, and never `>= 4`) fails the
        /// first half, and a count that was always saturated fails the second.
        #[test]
        fn the_concurrency_gate_counts_the_callers_children_in_the_journal() {
            let ty = agent_type::builtin("claude").unwrap();
            let max = ty.max_concurrent_children;
            let mut records = vec![intent("root", None, "claude", 0)];
            for i in 0..max {
                records.push(intent(&format!("kid{i}"), Some("root"), "claude", 1));
            }
            let fx = owning("owns-concurrency", records);
            let token = fx.handle.claim(
                &id("root"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            assert_eq!(
                fx.handle.live_children_of(&id("root")),
                max,
                "the journal places exactly {max} live children under this caller"
            );
            let before = journal_len(&fx);
            let e = spawn(
                &fx,
                params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: token,
                    }),
                    1,
                ),
            )
            .expect_err("a caller at max_concurrent_children may not spawn");
            assert!(
                e.message.contains("max_concurrent_children"),
                "the refusal must name the bound: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), before, "a gated spawn creates nothing");

            // The other side of the boundary, and the reason it is a *count* rather than a constant: a
            // child that has exited has released its slot.
            append(
                &fx.project.journal(),
                &line(
                    900,
                    9_000,
                    RecordKind::Exited(marion_core::journal::Exited {
                        agent_id: id("kid0"),
                        status: marion_core::contract::ExitStatus::Ok,
                        exit: ProcessExit {
                            code: Some(0),
                            signal: None,
                            description: "done".into(),
                        },
                    }),
                ),
            );
            fx.handle.live.refresh();
            assert_eq!(
                fx.handle.live_children_of(&id("root")),
                max - 1,
                "a terminal child no longer occupies a §3.1 concurrency slot"
            );
        }

        /// A root spawn that states a repository, which is the well-formed shape for the case.
        fn root_params(repo: &Path, secs: u64) -> AgentSpawnParams {
            AgentSpawnParams {
                native_launch: None,
                repo: Some(repo.to_path_buf()),
                ..params(None, secs)
            }
        }

        #[cfg(unix)]
        fn opaque(bytes: &[u8]) -> OpaqueOsValueV1 {
            use std::os::unix::ffi::OsStrExt;

            OpaqueOsValueV1::from_os_str(std::ffi::OsStr::from_bytes(bytes))
                .expect("Unix preserves native launch bytes")
        }

        #[cfg(unix)]
        fn native_context(repo: &Path) -> NativeLaunchContextV1 {
            NativeLaunchContextV1::new(
                opaque(b"/native/program-\xff"),
                vec![opaque(b"--opaque=\xfe")],
                OpaqueOsValueV1::from_os_str(repo.as_os_str())
                    .expect("the repository path is byte-exact on Unix"),
                vec![NativeEnvVarV1 {
                    name: opaque(b"NATIVE_MARKER"),
                    value: opaque(b"value-\xfd"),
                }],
                TerminalGeometryV1 {
                    cols: 137,
                    rows: 43,
                    xpixel: 9,
                    ypixel: 11,
                },
            )
        }

        #[cfg(unix)]
        fn native_context_v2(repo: &Path) -> NativeLaunchContextV2 {
            let context = native_context(repo);
            NativeLaunchContextV2::new(
                "atlas".into(),
                context.program,
                context.argv,
                context.cwd,
                context.env,
                context.geometry,
            )
        }

        /// The early pure boundary owns only child/native pairing; root binding happens after peer
        /// authentication and owns selector, platform, transport, and executable decisions.
        #[cfg(unix)]
        #[test]
        fn the_early_native_launch_boundary_owns_only_child_pairing() {
            let fx = owning("owns-native-boundary", vec![]);
            let caller = SpawnCaller {
                agent_id: id("caller"),
                node_token: "token".into(),
            };
            let context = NativeLaunchContext::V1(native_context(&fx.repo));

            assert_eq!(validate_native_launch_boundary(None, None), Ok(()));
            assert_eq!(validate_native_launch_boundary(Some(&caller), None), Ok(()));
            assert_eq!(
                validate_native_launch_boundary(Some(&caller), Some(&context)),
                Err(NativeLaunchGateError::ChildMisuse)
            );
            assert_eq!(
                validate_native_launch_boundary(None, Some(&context)),
                Ok(())
            );
        }

        /// Native process state belongs only to the root request that originated at the CLI.
        /// A child carrying it is a category error, before token lookup and before every write.
        #[cfg(unix)]
        #[test]
        fn a_child_request_carrying_native_launch_is_refused_before_every_side_effect() {
            let fx = owning("owns-native-child", vec![]);
            let before = journal_len(&fx);
            let mut p = params(
                Some(SpawnCaller {
                    agent_id: id("not-owned"),
                    node_token: "not-a-token".into(),
                }),
                1,
            );
            p.native_launch = Some(Box::new(NativeLaunchContext::V1(native_context(&fx.repo))));

            let e = spawn(&fx, p).expect_err("native launch state is root-only");

            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
            assert!(
                e.message
                    .contains("native launch context belongs only on a root"),
                "the refusal must name child misuse: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), before, "the refusal journals nothing");
            assert_eq!(fx.handle.owned_nodes(), 0, "the refusal claims no node");
        }

        /// V1 cannot name the facade that owns its program, so it stops before every launch effect.
        #[cfg(unix)]
        #[test]
        fn a_v1_root_native_launch_is_refused_because_its_selector_is_missing() {
            let fx = owning("owns-native-root", vec![]);
            let mut p = root_params(&fx.repo, 1);
            p.native_launch = Some(Box::new(NativeLaunchContext::V1(native_context(&fx.repo))));

            let e = spawn(&fx, p).expect_err("V1 cannot be bound without a selector");

            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
            assert!(
                e.message.contains("V1 carries no facade selector"),
                "the refusal must name the missing selector: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), 0, "the binding gate journals nothing");
            assert_eq!(
                fx.handle.owned_nodes(),
                0,
                "the binding gate claims no node"
            );
        }

        /// Ordinary root RPC cannot present the native-bootstrap authority required by V2.
        #[cfg(unix)]
        #[test]
        fn a_v2_root_native_launch_requires_the_native_bootstrap() {
            let fx = owning("owns-native-v2-root", vec![]);
            let mut p = root_params(&fx.repo, 1);
            p.native_launch = Some(Box::new(NativeLaunchContext::V2(native_context_v2(
                &fx.repo,
            ))));

            let e = spawn(&fx, p).expect_err("ordinary root RPC cannot authorize raw V2");

            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
            assert!(
                e.message
                    .contains("raw V2 native launch requires the native bootstrap"),
                "the refusal must name missing native-bootstrap authority: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), 0, "the binding gate journals nothing");
            assert_eq!(
                fx.handle.owned_nodes(),
                0,
                "the binding gate claims no node"
            );
        }

        /// A raw V2 frame cannot become bound even if its executable-shaped fields are valid.
        #[cfg(unix)]
        #[test]
        fn raw_v2_handler_validation_has_no_bound_success_arm() {
            let work = scratch("native-handler-bound-refusal");
            let bin = work.join("bin");
            std::fs::create_dir(&bin).expect("the fixture bin exists");
            let executable = bin.join("atlas-cli");
            std::fs::write(&executable, b"fixture executable\n")
                .expect("the executable is written");
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
                .expect("the executable is executable");
            let context = NativeLaunchContext::V2(NativeLaunchContextV2::new(
                "atlas".into(),
                OpaqueOsValueV1::from_os_str(executable.as_os_str()).unwrap(),
                vec![],
                OpaqueOsValueV1::from_os_str(work.as_os_str()).unwrap(),
                vec![NativeEnvVarV1 {
                    name: opaque(b"PATH"),
                    value: OpaqueOsValueV1::from_os_str(bin.as_os_str()).unwrap(),
                }],
                TerminalGeometryV1 {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                },
            ));

            let error = native_launch_refusal(&context);

            assert_eq!(error.kind(), Some(FailureKind::Refused));
            assert!(
                error
                    .message
                    .contains("raw V2 native launch requires the native bootstrap"),
                "raw V2 must stop before registry or managed-launch selection: {}",
                error.message
            );
        }

        /// **A root's `--timeout` reaches the spec that both enforces and journals it.**
        ///
        /// `marion run --timeout 300` states an `Option<u64>` on the wire and the supervisor
        /// resolves it (`root::blocked_bound_secs`). Resolved *into the spec* rather than beside
        /// it, because `root::prepare` is what writes the node's `SpawnIntent`: a bound held
        /// somewhere the intent cannot see is a bound no reader can report, which is how every node
        /// in `marion tree` came to read 900 s. A run that states nothing keeps the type's.
        #[test]
        fn a_root_spec_carries_the_bound_the_operator_asked_for() {
            let fx = owning("owns-root-bound", vec![]);
            let env = fx
                .handle
                .spawn_env
                .as_ref()
                .expect("an owning handle has a spawn environment");
            let ty = agent_type::builtin("claude").expect("the fixture type exists");

            let asked =
                root_spec_from_spawn(&root_params(&fx.repo, 300), fx.repo.clone(), env, &ty);
            assert_eq!(asked.bound_secs, 300);

            let mut p = root_params(&fx.repo, 300);
            p.timeout_secs = None;
            let silent = root_spec_from_spawn(&p, fx.repo.clone(), env, &ty);
            assert_eq!(
                silent.bound_secs,
                ty.timeout.0.as_secs(),
                "a run that names no bound gets §3.1's, which is the type's"
            );
            assert_ne!(
                asked.bound_secs, silent.bound_secs,
                "and the two are distinguishable, or the first assertion proves nothing"
            );
        }

        /// The production `RootSpec` constructor is the preparatory threading proof: byte-exact
        /// context survives that boundary even though the readiness gate keeps production from
        /// reaching it today.
        #[cfg(unix)]
        #[test]
        fn root_spec_construction_preserves_native_launch_byte_for_byte() {
            let fx = owning("owns-native-spec", vec![]);
            let context = native_context(&fx.repo);
            let expected = serde_json::to_vec(&context).expect("the context serializes");
            let mut p = root_params(&fx.repo, 1);
            p.native_launch = Some(Box::new(NativeLaunchContext::V1(context)));
            let env = fx
                .handle
                .spawn_env
                .as_ref()
                .expect("an owning handle has a spawn environment");
            let ty = agent_type::builtin(&p.agent_type).expect("the fixture type exists");

            let spec = root_spec_from_spawn(&p, fx.repo.clone(), env, &ty);
            let carried = spec
                .native_launch
                .as_ref()
                .expect("the root spec carries native context");
            let actual = serde_json::to_vec(carried).expect("the carried context serializes");

            assert!(
                actual == expected,
                "RootSpec must preserve every opaque native-context byte"
            );
        }

        /// **§11 item 28 step 6, at the handler: a client creating a root is served.**
        ///
        /// This test replaces `a_client_creating_a_root_over_the_socket_is_refused_naming_the_step_
        /// that_serves_it`, which asserted the opposite and was the honest pin while root creation
        /// was owed. Renamed rather than deleted, because the *name* is what a reader greps for and
        /// a stale one asserting a served path is refused would be a lie with a green tick next to
        /// it.
        ///
        /// What it can assert without launching a harness is that the frame **gets past every
        /// refusal that used to stop it** and is then judged on its own merits: the agent type is
        /// one no build has, which is the same lookup `root::prepare` performs, and nothing is
        /// journaled because nothing was minted. `client_run.rs` is where a root that really starts
        /// is measured, through `marion run` and a real provider.
        #[test]
        fn a_client_creating_a_root_reaches_the_launcher_rather_than_a_step_that_would_serve_it() {
            let fx = owning("owns-root", vec![]);
            let e = spawn(
                &fx,
                AgentSpawnParams {
                    native_launch: None,
                    agent_type: "no-such-agent-type".into(),
                    ..root_params(&fx.repo, 1)
                },
            )
            .expect_err("no build has that agent type");
            assert!(
                !e.message.contains("step 6"),
                "root creation is served; a refusal naming the step that would serve it is a \
                 revert: {}",
                e.message
            );
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
            assert!(
                e.message
                    .contains("the agent type is not a built-in and not a row"),
                "the frame reached the root launcher: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), 0, "and nothing was created");
            assert_eq!(fx.handle.owned_nodes(), 0, "…and no node was claimed");
        }

        /// **A node thread that panics after its claim still reaches a terminal outcome, and the
        /// supervisor can still exit.**
        ///
        /// Neither thread body had a `catch_unwind`. A panic after `SpawnObserver::identified`
        /// claimed the node and before `mark_finished` left `NodeHandle::outcome` `None` **for
        /// ever**, and `None` means *still running*: `running_nodes` counted it, so
        /// `idle_exit_eligible` refused, so `join_finished_nodes` never reaped the thread, and the
        /// supervisor could not exit for the rest of its life. A permanent phantom node, from one
        /// unwind.
        ///
        /// The panic is **real and injected in the window that matters** — inside `identified`,
        /// after the claim — rather than simulated by writing an outcome by hand, because what is
        /// being measured is the unwind path itself.
        ///
        /// **Bounded without a timeout.** Nothing here polls or sleeps: the thread sends
        /// `Progress::Finished` strictly after `mark_finished`, and `spawn_root` returns on
        /// receiving it, so the outcome is filed before this call returns. With the `catch_unwind`
        /// removed the thread dies instead, dropping its sender, and the receive fails as
        /// *disconnected* rather than waiting out `LAUNCH_BOUND` — so the mutation fails this test
        /// in milliseconds, on the assertion, which is what a mutation has to do to count.
        #[test]
        fn a_node_thread_that_panics_after_its_claim_still_reaches_a_terminal_outcome() {
            let fx = owning("owns-panic", vec![]);
            {
                let _armed = panic_after_claim::arm(&fx.repo);
                // A real agent type, so the frame reaches `prepare_watched` and the claim inside
                // it. The panic then fires before `prepare_watched`'s first side effect.
                spawn(&fx, root_params(&fx.repo, 1))
                    .expect_err("the node's thread panicked, so no process ever existed");
            }

            let claimed: Vec<AgentId> = lock(&fx.handle.nodes).keys().cloned().collect();
            assert_eq!(
                claimed.len(),
                1,
                "the premise: the panic came *after* the claim, so there is a node to strand"
            );
            let agent_id = &claimed[0];

            assert_eq!(
                fx.handle.running_nodes(),
                0,
                "**terminal.** A node whose thread panicked is not running, and an `outcome` left \
                 `None` here is the phantom: nothing in this process ever sets it afterwards"
            );
            let why = fx
                .handle
                .owned_failure(agent_id)
                .expect("a panicked node has a failure to report");
            assert!(
                why.contains("panicked"),
                "and the journal-facing sentence says what happened rather than inventing a \
                 clean exit: {why}"
            );

            assert!(
                fx.handle.idle_exit_eligible(),
                "**§5.7.** With no clients and no running node the supervisor is eligible to \
                 leave; this is the predicate the phantom held false for ever"
            );
            assert!(
                fx.handle.begin_idle_exit(),
                "and it really exits — `begin_idle_exit` joins the finished threads first, which \
                 is the step `join_finished_nodes` could never reach for an unreaped panic"
            );
        }

        /// **The same, on the child thread**, so the other `caught` call site is measured and not
        /// merely compiled.
        ///
        /// A child needs a caller holding a real token, and that caller is itself a claimed node
        /// this fixture never finishes — so the assertions here are about *this* node rather than
        /// about `running_nodes` or `idle_exit_eligible`, which the root test above owns.
        #[test]
        fn a_child_threads_panic_is_that_childs_own_terminal_outcome() {
            let fx = owning("owns-panic-child", vec![intent("root", None, "claude", 0)]);
            let token = fx.handle.claim(
                &id("root"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            {
                let _armed = panic_after_claim::arm(&fx.repo);
                spawn(
                    &fx,
                    params(
                        Some(SpawnCaller {
                            agent_id: id("root"),
                            node_token: token,
                        }),
                        1,
                    ),
                )
                .expect_err("the child's thread panicked, so no process ever existed");
            }

            let child = lock(&fx.handle.nodes)
                .keys()
                .find(|k| **k != id("root"))
                .cloned()
                .expect("the panic came after the claim, so the child is in the table");
            assert_eq!(
                fx.handle.owned_running(&child),
                Some(false),
                "**terminal.** Without the `catch_unwind` this stays `Some(true)` for the life of \
                 the supervisor, and §5.7 never lets it exit"
            );
            let why = fx
                .handle
                .owned_failure(&child)
                .expect("a panicked child has a failure to report");
            assert!(
                why.contains("panicked") && why.contains("no contract"),
                "and it is `SpawnError::Panicked`'s written sentence, which says there is no \
                 contract rather than leaving a caller to wait for one: {why}"
            );
        }

        /// The child half of the same mechanism, without a launch.
        ///
        /// The injection above exercises `caught` through the **root** arm, because a root needs no
        /// caller and so needs no token, no worktree and no harness. This pins the other
        /// vocabulary directly: §9 gives a root and a child different results, and `caught` takes
        /// the spelling from its caller precisely so neither gets the other's.
        #[test]
        fn a_panic_becomes_each_kind_of_nodes_own_way_of_saying_it_failed() {
            let child = caught("claude", spawn_panicked, || -> Result<(), _> {
                panic!("inside the child's thread")
            })
            .expect_err("a panic is not a success");
            assert!(
                matches!(child, crate::spawn::SpawnError::Panicked(ref t) if t == "claude"),
                "a child's panic is `SpawnError::Panicked` — the variant that was documented for \
                 exactly this and constructed nowhere until now: {child:?}"
            );
            assert!(
                child.to_string().contains("no contract"),
                "and it carries the written sentence, which points at the journal: {child}"
            );

            let root = caught("codex", root_panicked, || -> Result<(), _> {
                panic!("inside the root's thread")
            })
            .expect_err("a panic is not a success");
            assert!(
                root.contains("codex") && root.contains("panicked"),
                "a root has no `TaskContract` and so no `SpawnError`; its outcome is marion's own \
                 sentence: {root}"
            );

            assert_eq!(
                caught(
                    "claude",
                    spawn_panicked,
                    || Ok::<_, crate::spawn::SpawnError>(7)
                )
                .ok(),
                Some(7),
                "and a body that does not panic is passed through untouched"
            );
        }

        /// **The `no_change_record` half of the root-only pairing.**
        ///
        /// §9's change record exists because a root runs in the operator's own checkout. A child
        /// runs in a worktree marion made, so the field would be an accept-and-ignore there — and
        /// §11 item 23's whole rule is that a caller told nothing has been told their choice was
        /// honoured.
        #[test]
        fn a_caller_that_states_no_change_record_is_refused_by_name() {
            let fx = owning("owns-ncr", vec![intent("root", None, "claude", 0)]);
            let token = fx.handle.claim(
                &id("root"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            let before = journal_len(&fx);
            let e = spawn(
                &fx,
                AgentSpawnParams {
                    native_launch: None,
                    // `false` and not `true`: the refusal is for *stating* it, so a test that sent
                    // the interesting value would pass against a build that only refused `true`.
                    no_change_record: Some(false),
                    pane: None,
                    isolation: None,
                    allow_concurrent_writes: None,
                    ..params(
                        Some(SpawnCaller {
                            agent_id: id("root"),
                            node_token: token,
                        }),
                        1,
                    )
                },
            )
            .expect_err("a child has no change record to decline");
            assert_eq!(e.kind(), Some(FailureKind::Refused));
            assert!(
                e.message.contains("must not state `no_change_record`"),
                "the refusal must name the field: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), before, "and nothing was created");
        }

        /// **Open question 3, decided and pinned: root creation is authorized by filesystem
        /// permission on the socket, checked against peer credentials.**
        ///
        /// Asserted against the predicate rather than through a connection, and that is a stated
        /// limitation rather than a shortcut: making a real peer of another uid needs a second
        /// account or a setuid helper, neither of which a `cargo test` may assume. What the
        /// predicate *is* reached through in production is one line in `Handle::call`
        /// (`out.peer()`), and `Peer` is read there and nowhere else.
        ///
        /// The `Unknown` row is the load-bearing one: `getpeereid` can fail, and a check that could
        /// not be made must refuse rather than pass. A build that spelled this
        /// `matches!(peer, Peer::Uid(u) if u != own)` would accept every unreadable peer.
        #[test]
        fn root_creation_is_refused_to_a_peer_that_is_not_this_supervisors_own_user() {
            let own = crate::socket::own_uid();
            assert!(
                root_spawn_authorized(Peer::Uid(own)).is_ok(),
                "the supervisor's own user is who marion serves"
            );

            let e = root_spawn_authorized(Peer::Uid(own.wrapping_add(1)))
                .expect_err("another user's process may not start work here");
            assert_eq!(e.kind(), Some(FailureKind::Refused));
            assert!(
                e.message.contains("filesystem permission on the socket"),
                "the refusal must say what does authorize it: {}",
                e.message
            );

            let e = root_spawn_authorized(Peer::Unknown)
                .expect_err("a check that could not be made is not a check that passed");
            assert_eq!(e.kind(), Some(FailureKind::Refused));
            assert!(
                e.message
                    .contains("could not read this connection's peer credentials"),
                "{}",
                e.message
            );
        }

        /// Native root state is still attacker-controlled until the socket peer is authenticated.
        /// An unauthorized peer learns only that its credentials were refused: platform support
        /// and transport readiness are facts marion reveals after that boundary, never before it.
        #[cfg(unix)]
        #[test]
        fn an_unauthorized_root_native_request_gets_the_credential_refusal_first() {
            let fx = owning("owns-native-peer", vec![]);
            let mut p = root_params(&fx.repo, 1);
            p.native_launch = Some(Box::new(NativeLaunchContext::V1(native_context(&fx.repo))));

            let e = fx
                .handle
                .agent_spawn(&p, Peer::Unknown)
                .expect_err("an unreadable peer may not create a native root");

            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
            assert!(
                e.message
                    .contains("could not read this connection's peer credentials"),
                "credential refusal must precede platform/readiness disclosure: {}",
                e.message
            );
            assert!(
                !e.message.contains("native facade")
                    && !e.message.contains("byte-exact operating-system")
                    && !e.message.contains("V1 carries no facade selector"),
                "an unauthorized peer learned native support state: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), 0, "the refusal journals nothing");
            assert_eq!(fx.handle.owned_nodes(), 0, "the refusal claims no node");
        }

        /// **And it is not a token, deliberately** — the other half of open question 3.
        ///
        /// A caller that *does* name a node still has to prove it, and this asserts the two paths
        /// do not blur: the peer check never stands in for `resolve_caller`. Both calls below come
        /// from the same connection and the same uid; one is served, the other is refused for a
        /// reason that has nothing to do with the user.
        #[test]
        fn the_peer_check_does_not_stand_in_for_a_node_token() {
            let fx = owning(
                "owns-peer-not-token",
                vec![intent("root", None, "claude", 0)],
            );
            let e = spawn(
                &fx,
                params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: "not-the-token".into(),
                    }),
                    1,
                ),
            )
            .expect_err("§5.4 binds a capability to an AgentId, and this connection has none");
            assert!(
                e.message.contains("did not mint that node token"),
                "peer credentials answer which *user*; the token answers which *node*, and this \
                 refusal must be the second: {}",
                e.message
            );
        }

        /// **A caller does not get to say which tree its child branches from.**
        ///
        /// Same argument as `SpawnCaller` carrying no `agent_type` and no `depth`: every gated fact
        /// about a caller is derived from what the supervisor minted. A node that could name its own
        /// repository could branch its children off a tree its parent never entitled it to touch —
        /// and, since one supervisor serves a repository *and every linked worktree of it*, the
        /// trees within reach of a forged value are exactly the ones an operator is working in.
        ///
        /// Refused rather than ignored, for §11 item 23's rule: a caller that states a repository,
        /// receives no error and is quietly given a different one has been told nothing.
        #[test]
        fn a_caller_that_states_its_own_repository_is_refused_by_name() {
            let fx = owning("owns-stated-repo", vec![intent("root", None, "claude", 0)]);
            let token = fx.handle.claim(
                &id("root"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            let before = journal_len(&fx);
            let e = spawn(
                &fx,
                AgentSpawnParams {
                    native_launch: None,
                    // The *real* tree, not an implausible one: the refusal must not depend on the
                    // value being wrong. Stating it at all is the error.
                    repo: Some(fx.repo.clone()),
                    ..params(
                        Some(SpawnCaller {
                            agent_id: id("root"),
                            node_token: token,
                        }),
                        1,
                    )
                },
            )
            .expect_err("a caller may not state its own repository");
            assert_eq!(e.kind(), Some(FailureKind::Refused));
            assert!(
                e.message.contains("must not state a `repo`"),
                "the refusal must name the field and the pairing: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), before, "and nothing was created");
        }

        /// **The other half of the pairing: a root spawn must say which tree it is of.**
        ///
        /// There is no derivation available. §2 keys this supervisor on `git rev-parse
        /// --git-common-dir`, so `<state>/<project-hash>` names a repository and every linked
        /// worktree of it at once, and defaulting to the project root would branch a feature
        /// worktree's children off the main tree's HEAD.
        ///
        /// Asserted through the `Refused` **kind** as well as the sentence, because the neighbouring
        /// root refusal is `Unimplemented`: a build that answered "step 6" to a malformed frame
        /// would be hiding a client's mistake behind marion's own.
        #[test]
        fn a_root_spawn_that_states_no_repository_is_refused_by_name() {
            let fx = owning("owns-no-repo", vec![]);
            let e = spawn(&fx, params(None, 1))
                .expect_err("a root spawn with no repository cannot be served");
            assert_eq!(
                e.kind(),
                Some(FailureKind::Refused),
                "not `Unimplemented`: this is the client's frame being wrong, not marion's build \
                 being incomplete — {e:?}"
            );
            assert!(
                e.message.contains("only the client knows which tree"),
                "the refusal must say why nothing here could supply it: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), 0, "and nothing was created");
        }

        /// **The load-bearing one: two roots in two linked worktrees of one repository, whose
        /// children branch from different HEADs.**
        ///
        /// This is the whole reason the repository left `run::Env`. §2 keys a supervisor on `git
        /// rev-parse --git-common-dir`, so both trees below resolve to **one** project, one journal
        /// and one supervisor — that is asserted here rather than assumed, because if it were false
        /// the rest of the test would be measuring two supervisors and proving nothing. And
        /// `spawn::make_worktree` runs `git -C <repo> rev-parse HEAD`, so the tree each child
        /// branches from is a property of *its own* root, not of the supervisor they share.
        ///
        /// The assertion is on the branch `make_worktree` created, read back with `rev-parse` after
        /// the run: refs live in the common dir, so both branches are visible from either tree and
        /// the test cannot accidentally be asserting on "which repository has the ref". What
        /// separates a correct implementation from the one this design replaced is only **what
        /// commit** each branch points at.
        ///
        /// A supervisor-wide repository — of any spelling, including `launch.project_root` — makes
        /// both children branch from one commit and fails the inequality below. That failure is the
        /// bug this whole change exists to prevent, and it is silent in production: a real branch,
        /// off real commits, with a real worktree, and nothing anywhere reporting a problem.
        #[test]
        fn two_roots_in_different_worktrees_of_one_repository_branch_their_children_from_their_own_heads()
         {
            if !marion_testsupport::harness_available("claude") {
                return;
            }
            let dir = scratch("owns-two-worktrees");
            let main = fixture_repo(&dir);
            // Two linked worktrees, each carrying a commit the other does not have, so "branched
            // from the wrong tree" is visible as an oid and not merely as a path.
            let (side_a, head_a) = linked_worktree(&main, "a");
            let (side_b, head_b) = linked_worktree(&main, "b");
            assert_ne!(head_a, head_b, "the two trees must really differ");

            // **One supervisor, keyed the way production keys it.** Not `ProjectDir::new(&state,
            // &side_a)`: that is the mistake §2's rule exists to prevent, and it would give this
            // test two projects and no shared supervisor to prove anything about.
            let state = dir.join("state");
            let project = ProjectDir::new(&state, &crate::socket::project_root(&side_a));
            assert_eq!(
                project.path(),
                ProjectDir::new(&state, &crate::socket::project_root(&side_b)).path(),
                "§2 keys on the git common dir, so both worktrees are one project — one \
                 supervisor, one journal. If this fails the rest of the test proves nothing."
            );
            std::fs::create_dir_all(project.path()).unwrap();

            let live = Arc::new(crate::registry::LiveRegistry::follow(
                Registry::boot_path(&project.journal()).unwrap(),
                std::time::Duration::from_millis(2),
            ));
            for (seq, kind) in [
                intent("root-a", None, "claude", 0),
                intent("root-b", None, "claude", 0),
            ]
            .into_iter()
            .enumerate()
            {
                append(
                    &project.journal(),
                    &line(seq as u64, 1_000 + seq as u64, kind),
                );
            }
            live.refresh();
            let handle = RegistryHandle::owning(
                live,
                crate::run::Env {
                    project_dir: project.clone(),
                    state: state.clone(),
                    project_root: crate::socket::project_root(&main),
                    bridge: std::path::PathBuf::from("/bin/marion-supervisor"),
                    base_url: Some("http://127.0.0.1:8099/v1".into()),
                    auth: marion_harness::Auth::Canned,
                },
            );
            let fx = Owning {
                handle,
                project,
                // Never read by this test: each root states its own tree below. Present because the
                // fixture type is shared.
                repo: main.clone(),
                _dir: dir,
            };

            let child_of = |root: &str, tree: &Path| -> AgentId {
                let token = fx.handle.claim(
                    &id(root),
                    Some(marion_core::contract::TaskId(format!("t-{root}"))),
                    tree.to_path_buf(),
                );
                let child = spawn(
                    &fx,
                    params(
                        Some(SpawnCaller {
                            agent_id: id(root),
                            node_token: token,
                        }),
                        5,
                    ),
                )
                .unwrap_or_else(|e| panic!("{root}'s child must launch: {e:?}"))
                .agent_id;
                settle(&fx, &child);
                child
            };
            let child_a = child_of("root-a", &side_a);
            let child_b = child_of("root-b", &side_b);

            let base_of = |child: &AgentId| -> String {
                let task = fx
                    .handle
                    .owned_task_id(child)
                    .expect("the supervisor owns this node and knows its contract");
                // Read out of the **main** repository: refs are shared across every worktree of one
                // repository, so this cannot be reading "the tree it was made in".
                let out = std::process::Command::new("git")
                    .current_dir(&main)
                    .args(["rev-parse", &format!("marion/{}", task.0)])
                    .output()
                    .expect("git runs");
                assert!(
                    out.status.success(),
                    "`make_worktree` must have created marion/{}: {out:?}",
                    task.0
                );
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };

            assert_eq!(
                base_of(&child_a),
                head_a,
                "a child of the root in worktree `a` branches from **worktree a's** HEAD"
            );
            assert_eq!(
                base_of(&child_b),
                head_b,
                "and a child of the root in worktree `b` branches from worktree b's"
            );
            assert_ne!(
                base_of(&child_a),
                base_of(&child_b),
                "one supervisor, two trees, two bases. A supervisor-wide repository makes these \
                 equal — which is a child silently branched off a tree its root is not in."
            );
        }

        /// A linked worktree of `main` carrying one commit of its own, and its HEAD oid.
        fn linked_worktree(main: &Path, tag: &str) -> (PathBuf, String) {
            let path = main.parent().expect("the repo has a parent").join(tag);
            let git = |dir: &Path, args: &[&str]| {
                let out = std::process::Command::new("git")
                    .current_dir(dir)
                    .args(args)
                    .output()
                    .expect("git runs");
                assert!(out.status.success(), "git {args:?}: {out:?}");
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            git(
                main,
                &["worktree", "add", "-q", "-b", tag, &path.to_string_lossy()],
            );
            std::fs::write(path.join(format!("{tag}.txt")), format!("{tag}\n")).unwrap();
            git(&path, &["add", "-A"]);
            git(
                &path,
                &[
                    "-c",
                    "user.email=marion@example.invalid",
                    "-c",
                    "user.name=marion",
                    "commit",
                    "-qm",
                    tag,
                ],
            );
            let head = git(&path, &["rev-parse", "HEAD"]);
            (path, head)
        }

        /// A supervisor with no spawn environment refuses in its **own voice**, naming the
        /// constructor rather than failing further in on a directory. See
        /// `RegistryHandle::spawn_env`.
        ///
        /// `None` no longer means "production": stage 3 builds `owning`. It means this handle was
        /// built by `new`, which is a describing fixture, and the sentence says so.
        #[test]
        fn a_supervisor_that_cannot_spawn_refuses_by_naming_the_build_not_a_missing_directory() {
            let fx = fx("owns-no-env");
            let out = crate::serve::sink(ConnId(4));
            let e = fx
                .handle
                .call(
                    ConnId(4),
                    // Well-formed, so the pairing check ahead of it passes and this really is the
                    // refusal being asserted.
                    &Call::AgentSpawn(root_params(Path::new("/r"), 1)),
                    &out,
                )
                .expect_err("a handle built by `new` owns nothing");
            assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
            assert!(
                e.message.contains("RegistryHandle::new"),
                "the refusal names the constructor, which is the whole of what is missing: {}",
                e.message
            );
        }

        // ------------------------------------------------------------------------------------
        // The token itself.
        // ------------------------------------------------------------------------------------

        /// Two nodes never share a capability, and a token is long enough that guessing is not a
        /// strategy. 32 bytes of `/dev/urandom` as hex is 64 characters.
        #[test]
        fn every_node_gets_its_own_unguessable_token() {
            let fx = owning("owns-tokens", vec![]);
            let a = fx.handle.claim(
                &id("a"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            let b = fx.handle.claim(
                &id("b"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            assert_ne!(
                a, b,
                "a per-node token that is not per-node is a fleet token"
            );
            assert_eq!(a.len(), 64, "32 bytes as hex");
            assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        }

        /// The comparison reads the whole slice, so a caller cannot learn a token one byte at a time
        /// from the shape of a refusal. Asserted as *correctness over every boundary a short-circuit
        /// would get right anyway* — the timing property itself is not measurable in a unit test, so
        /// what is pinned here is the behaviour, and the loop is what a reader must not "simplify".
        #[test]
        fn a_token_comparison_is_total_over_the_slice() {
            assert!(tokens_match("abc", "abc"));
            assert!(!tokens_match("abc", "abd"), "a differing last byte");
            assert!(!tokens_match("abc", "bbc"), "a differing first byte");
            assert!(!tokens_match("abc", "abcd"), "a longer candidate");
            assert!(!tokens_match("abcd", "abc"), "a shorter candidate");
            // The empty stored token — what `mint_token` produces when `/dev/urandom` cannot be read.
            // It matches only the empty string, which `SpawnCaller` cannot carry: `node_token` has no
            // serde default, so an absent one fails deserialization rather than becoming this.
            assert!(!tokens_match("", "x"));
        }

        // ------------------------------------------------------------------------------------
        // §5.7's exit predicate, and §7.3.1's departure.
        // ------------------------------------------------------------------------------------

        // ------------------------------------------------------------------------------------
        // The two that really launch a process.
        // ------------------------------------------------------------------------------------

        /// How long this file will wait for a node's thread to finish before calling it a leak.
        /// Never a verdict: every assertion below is over an identity, a pid or a count.
        const SETTLE: std::time::Duration = std::time::Duration::from_secs(90);

        /// Wait for **this node's** thread to reach an outcome, so the scratch directory is not
        /// removed out from under it. Asserts rather than returns: a node that never settles is
        /// exactly the leak these tests exist to catch.
        ///
        /// Per node and not `running_nodes() == 0`, because the *caller* in these fixtures is a
        /// node this supervisor also owns — `claim`ed to give it a token — and nothing ever
        /// finishes it. Waiting on the whole table would wait for a node that is a fixture.
        fn settle(fx: &Owning, agent_id: &AgentId) {
            let deadline = std::time::Instant::now() + SETTLE;
            while fx.handle.owned_running(agent_id) == Some(true) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "a node's thread never produced an outcome within {}s",
                    SETTLE.as_secs()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        /// Spawn a real child and hand back the node's id, once the response has arrived.
        fn spawn_a_real_child(fx: &Owning, secs: u64) -> AgentId {
            let token = fx.handle.claim(
                &id("root"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            spawn(
                fx,
                params(
                    Some(SpawnCaller {
                        agent_id: id("root"),
                        node_token: token,
                    }),
                    secs,
                ),
            )
            .expect("the spawn is admitted and a process starts")
            .agent_id
        }

        /// An `Owning` fixture whose records are on disk **before** the registry boots — the
        /// inverse of [`owning`]'s order — so a node still `Live` at boot is one this supervisor
        /// never decided the fate of and the boot restart pass marks it `Orphaned` (§7.2). This is
        /// how a resume gets an orphan to relaunch.
        fn orphaning(tag: &str, records: Vec<RecordKind>) -> Owning {
            orphaning_with(tag, |_, _| records)
        }

        /// [`orphaning`] for records that have to **name the fixture's own directories** — a lost
        /// child's workspace is a path under the project, and the path is not knowable until the
        /// scratch repo exists. The closure is handed the repo and the project it was keyed to.
        fn orphaning_with(
            tag: &str,
            records: impl FnOnce(&std::path::Path, &ProjectDir) -> Vec<RecordKind>,
        ) -> Owning {
            let dir = scratch(tag);
            let repo = fixture_repo(&dir);
            let state = dir.join("state");
            let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
            std::fs::create_dir_all(project.path()).unwrap();
            let journal = project.journal();
            for (seq, kind) in records(&repo, &project).into_iter().enumerate() {
                append(&journal, &line(seq as u64, 1_000 + seq as u64, kind));
            }
            let live = Arc::new(crate::registry::LiveRegistry::follow(
                Registry::boot_path(&journal).unwrap(),
                std::time::Duration::from_millis(2),
            ));
            let handle = RegistryHandle::owning(
                live,
                crate::run::Env {
                    project_dir: project.clone(),
                    state: state.clone(),
                    project_root: crate::socket::project_root(&repo),
                    bridge: std::path::PathBuf::from("/bin/marion-supervisor"),
                    base_url: Some("http://127.0.0.1:8099/v1".into()),
                    auth: marion_harness::Auth::Canned,
                },
            );
            Owning {
                handle,
                project,
                repo,
                _dir: dir,
            }
        }

        /// The records of a lost claude root: its intent, a `Spawned` with **no pid** (nothing to
        /// signal on relaunch), and the session its stream named. Booted through [`orphaning`],
        /// this replays `Orphaned` with `harness_session` set — exactly what resume requires.
        fn lost_root(
            session: &str,
            pid: Option<i32>,
            start_id: Option<marion_core::node::StartId>,
        ) -> Vec<RecordKind> {
            vec![
                intent("root", None, "claude", 0),
                RecordKind::Spawned(marion_core::journal::Spawned {
                    agent_id: id("root"),
                    harness_version: "test".into(),
                    model: None,
                    pid,
                    start_id,
                }),
                RecordKind::SessionObserved(marion_core::journal::SessionObserved {
                    agent_id: id("root"),
                    harness: Harness::ClaudeCode,
                    session_id: session.into(),
                    pane: false,
                    // A root derives its cwd from the project this supervisor serves, so its
                    // launch names no workspace. The child fixture below is the one that does.
                    workspace: None,
                }),
            ]
        }

        /// The `TaskId` every lost-child fixture's contract is under.
        const CHILD_TASK: &str = "t-lost-child";

        /// The records of a **lost root and its lost child** — the shape a supervisor SIGKILL
        /// leaves behind. The child carries what a resume of it needs and a root's does not: the
        /// workspace it ran in, recorded on its `SessionObserved`.
        ///
        /// `workspace` is the caller's, so a test can name a tree that no longer exists (or none at
        /// all) without a second fixture. Neither node records a pid: `procid` is the root path's
        /// concern and is already measured there, and a fixture that recorded this process's pid
        /// would refuse for that reason instead of the one under test.
        fn lost_root_and_child(
            workspace: Option<marion_core::contract::Workspace>,
        ) -> Vec<RecordKind> {
            let spawned = |agent: &str| {
                RecordKind::Spawned(marion_core::journal::Spawned {
                    agent_id: id(agent),
                    harness_version: "test".into(),
                    model: None,
                    pid: None,
                    start_id: None,
                })
            };
            let session = |agent: &str, ws: Option<marion_core::contract::Workspace>| {
                RecordKind::SessionObserved(marion_core::journal::SessionObserved {
                    agent_id: id(agent),
                    harness: Harness::ClaudeCode,
                    session_id: format!("sess-{agent}"),
                    pane: false,
                    workspace: ws,
                })
            };
            vec![
                intent("root", None, "claude", 0),
                spawned("root"),
                session("root", None),
                RecordKind::SpawnIntent(SpawnIntent {
                    agent_id: id("child"),
                    parent_id: Some(id("root")),
                    agent_type: "claude".into(),
                    harness: Harness::ClaudeCode,
                    depth: 1,
                    task_id: Some(marion_core::contract::TaskId(CHILD_TASK.into())),
                    timeout_secs: None,
                }),
                spawned("child"),
                session("child", workspace),
            ]
        }

        /// The worktree a lost child ran in, **made on disk** so a resume of it finds the tree its
        /// session was created in still there.
        fn existing_child_worktree(project: &ProjectDir) -> marion_core::contract::Workspace {
            let path = project.agent(&id("child")).worktree();
            std::fs::create_dir_all(&path).unwrap();
            marion_core::contract::Workspace::Worktree {
                path,
                branch: format!("marion/{CHILD_TASK}"),
            }
        }

        fn resume(
            fx: &Owning,
            agent_id: AgentId,
            prompt: &str,
        ) -> Result<marion_core::proto::result::NodeResumeResult, RpcError> {
            let out = crate::serve::sink(ConnId(3));
            match fx.handle.call(
                ConnId(3),
                &Call::NodeResume(marion_core::proto::params::NodeResumeParams {
                    agent_id,
                    prompt: prompt.into(),
                }),
                &out,
            )? {
                MethodResult::NodeResume(r) => Ok(r),
                other => panic!("wrong result: {}", other.method().as_str()),
            }
        }

        /// **A resume relaunches an orphan into its own id, through `agent/spawn`'s launch path.**
        ///
        /// The orphan's process is gone (no pid), so the preflight proceeds straight to the
        /// launcher; `node/resume` answers with the **same** agent id and the next
        /// `spawn_generation`, and a second `Spawned` for that id lands on the one journal — replay
        /// then folds it as generation two. Nothing about the launch is a new node.
        #[test]
        fn resume_relaunches_an_orphan_into_its_own_node_id_through_agent_spawns_path() {
            if !marion_testsupport::harness_available("claude") {
                return;
            }
            let fx = orphaning("resume-relaunch", lost_root("sess-relaunch", None, None));
            // The orphan is what a resume is for: fate marked, a session to hand back.
            let before = fx
                .handle
                .call(
                    ConnId(9),
                    &Call::NodeGet(marion_core::proto::params::NodeGetParams {
                        agent_id: id("root"),
                    }),
                    &crate::serve::sink(ConnId(9)),
                )
                .expect("the orphan is on the tree");
            let MethodResult::NodeGet(before) = before else {
                panic!("node/get")
            };
            assert_eq!(before.node.reap_state, ReapState::Orphaned);

            let r = resume(&fx, id("root"), "carry on from here").expect("the orphan relaunches");
            assert_eq!(r.agent_id, id("root"), "the node keeps its own id");
            assert_eq!(r.spawn_generation, 2, "the second lifetime of one node");
            settle(&fx, &id("root"));

            let journalled = std::fs::read_to_string(fx.project.journal()).unwrap();
            let spawns = journalled
                .lines()
                .filter(|l| {
                    let v: serde_json::Value = serde_json::from_str(l).unwrap();
                    v["kind"]["Spawned"]["agent_id"] == serde_json::json!("root")
                })
                .count();
            assert_eq!(
                spawns, 2,
                "the relaunch wrote a second `Spawned` for the same id:\n{journalled}"
            );
            assert_eq!(
                marion_core::registry::replay(journalled.as_bytes())
                    .get(&id("root"))
                    .unwrap()
                    .spawn_generation,
                2,
                "replay folds the second spawn as generation two"
            );
        }

        /// **A resume relaunches a lost child into its own id, under its own parent.**
        ///
        /// The root path could rebuild a root's launch from the project the supervisor is keyed on.
        /// A child's could not, until its workspace was journaled: its cwd is a linked worktree
        /// marion made, and a relaunch anywhere else reaches the harness with a cwd the session was
        /// not created in. Now it is recorded, so the child goes back through the **same**
        /// `run::run_spawn` path a fresh child takes — its own `AgentId`, its recorded parent, its
        /// recorded depth, and `resume: Some(session)` — and lands in the tree it left.
        ///
        /// The contract the run writes is the oracle for *where*: §6.7 records the workspace, so a
        /// relaunch that had cut a second worktree would name a different path there.
        #[test]
        fn resume_relaunches_a_lost_child_into_its_own_node_id_under_its_parent() {
            if !marion_testsupport::harness_available("claude") {
                return;
            }
            let fx = orphaning_with("resume-child", |_, project| {
                lost_root_and_child(Some(existing_child_worktree(project)))
            });
            let expected = existing_child_worktree(&fx.project);

            let r = resume(&fx, id("child"), "carry on from here").expect("the child relaunches");
            assert_eq!(r.agent_id, id("child"), "the node keeps its own id");
            assert_eq!(r.spawn_generation, 2, "the second lifetime of one node");
            settle(&fx, &id("child"));

            let journalled = std::fs::read_to_string(fx.project.journal()).unwrap();
            let node = marion_core::registry::replay(journalled.as_bytes())
                .get(&id("child"))
                .cloned()
                .expect("the child still replays under its own id");
            assert_eq!(node.spawn_generation, 2, "replay folds the second spawn");
            assert_eq!(
                node.depth(),
                Some(1),
                "§7.5 makes the intent immutable, so the second life is at the same depth"
            );
            assert_eq!(
                node.intent.as_ref().and_then(|i| i.parent_id.clone()),
                Some(id("root")),
                "and under the same parent: the tree shows it where it was"
            );

            // **And it ran in the tree the journal recorded.** The fixture's worktree is a plain
            // directory rather than a real linked worktree, so `make_worktree` would have failed on
            // it and the resume would have returned an error instead of a node — a relaunch that
            // cut a second tree cannot reach this line. The directory is still the one the fixture
            // made, untouched by git. `select_workspace`'s own unit test asserts the value
            // directly; this asserts the launch took that path.
            assert!(
                expected.path().is_dir() && !expected.path().join(".git").exists(),
                "the recorded tree is still the one the first life used: {}",
                expected.path().display()
            );
        }

        /// **A resume refuses by name when the tree the session was created in is gone.** `marion
        /// run`'s cleanup and `worktree_reap` both remove a child's worktree; the session id
        /// outlives it on the journal, and handing it back from a directory the harness has never
        /// seen is how a "resume" silently becomes a fresh run under a resumed node's id.
        #[test]
        fn resume_refuses_a_child_whose_recorded_worktree_is_gone() {
            let fx = orphaning_with("resume-child-reaped", |_, project| {
                lost_root_and_child(Some(marion_core::contract::Workspace::Worktree {
                    path: project.agent(&id("child")).worktree(),
                    branch: format!("marion/{CHILD_TASK}"),
                }))
            });
            let before = journal_len(&fx);
            let e = resume(&fx, id("child"), "carry on").expect_err("a reaped tree blocks it");
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
            assert!(
                e.message.contains("no longer exists"),
                "the refusal names the missing tree: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), before, "nothing was launched");

            // And a child whose journal never named a workspace at all is refused for that, rather
            // than relaunched in whatever directory is at hand.
            let fx = orphaning_with("resume-child-unrecorded", |_, _| lost_root_and_child(None));
            let e = resume(&fx, id("child"), "carry on").expect_err("an unrecorded tree blocks it");
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
            assert!(
                e.message.contains("does not record"),
                "the refusal says the journal is silent: {}",
                e.message
            );
        }

        /// **A resume refuses a child whose parent is still Live.** Principle 8: marion holds a
        /// running node's channel, and a Live parent's own `spawn` owns this child — its outcome is
        /// owed to a call that is still waiting for it. Relaunching from outside would put a second
        /// process under one contract, which is the thing resume exists not to do. A parent whose
        /// fate is decided holds nothing, and that child resumes.
        #[test]
        fn resume_refuses_a_child_whose_parent_is_still_live() {
            // `owning` writes its records **after** the boot pass, so nothing is marked `Orphaned`:
            // the root is a node this supervisor holds, and the child is resumable only because its
            // own `Exited` is on the journal.
            let mut records = lost_root_and_child(None);
            records.push(RecordKind::Exited(marion_core::journal::Exited {
                agent_id: id("child"),
                status: marion_core::contract::ResultStatus::Failed,
                exit: ProcessExit {
                    code: Some(1),
                    signal: None,
                    description: "the child's first life ended".into(),
                },
            }));
            let fx = owning("resume-child-live-parent", records);
            let before = journal_len(&fx);
            let e = resume(&fx, id("child"), "carry on").expect_err("a live parent blocks it");
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e:?}");
            assert!(
                e.message.contains("still live"),
                "the refusal names the parent: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), before, "nothing was launched");
        }

        /// **A resume refuses when the orphan's own process cannot be proven gone.** A recorded pid
        /// that is still alive with no recorded identity is `procid::CannotTell`: marion will not
        /// start a second process that might run against a transcript a first is still writing, so
        /// it refuses and launches nothing.
        #[test]
        fn resume_refuses_when_the_orphans_process_cannot_be_identified() {
            // This test's own pid is alive, and the orphan recorded no start identity — so a probe
            // of it reads a live process marion cannot prove is or is not the node's.
            let fx = orphaning(
                "resume-cannot-tell",
                lost_root("sess-cannot", Some(std::process::id() as i32), None),
            );
            let before = journal_len(&fx);
            let e = resume(&fx, id("root"), "carry on")
                .expect_err("an unprovable process blocks the resume");
            assert_eq!(e.kind(), Some(FailureKind::Conflict), "{e:?}");
            assert!(
                e.message.contains("cannot prove"),
                "the refusal names why: {}",
                e.message
            );
            assert_eq!(
                journal_len(&fx),
                before,
                "nothing was signalled or launched, so nothing was journaled"
            );
        }

        /// **The response's `state` is a claim the journal already backs.**
        ///
        /// This is the whole reason `agent/spawn` returns at the `on_started` hook rather than
        /// before it or after the run. Before it, the answer would be a promise: a client told
        /// `Spawning` would have nothing on disk to read, and a supervisor that then died would
        /// leave a `SpawnIntent` meaning either "nothing was started" or "something is running and
        /// marion cannot name it" — §11 item 30's two indistinguishable shapes. After the run, the
        /// call would be a minutes-long synchronous JSON-RPC request, which is what
        /// `background.rs` records as taking a whole bridge down when it hangs.
        ///
        /// So the assertion is not that the pid is plausible but that **the record is already
        /// there when the caller has the answer**, with the same pid the table holds. The journal
        /// is read from the file rather than from the follower, because the follower is a poll and
        /// would let a record that had not been written yet appear a few milliseconds later.
        #[test]
        fn the_spawn_response_names_a_node_whose_spawned_record_is_already_on_disk() {
            if !marion_testsupport::harness_available("claude") {
                return;
            }
            let fx = owning("owns-launch", vec![intent("root", None, "claude", 0)]);
            let agent_id = spawn_a_real_child(&fx, 5);

            let journalled = std::fs::read_to_string(fx.project.journal()).unwrap();
            let spawned: Vec<serde_json::Value> = journalled
                .lines()
                .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                .filter(|v| v["kind"]["Spawned"]["agent_id"] == serde_json::json!(agent_id.0))
                .collect();
            assert_eq!(
                spawned.len(),
                1,
                "the caller has its answer, so the node's `Spawned` must already be on disk:\n{journalled}"
            );
            let journal_pid = spawned[0]["kind"]["Spawned"]["pid"]
                .as_i64()
                .map(|p| p as i32);
            assert!(
                journal_pid.is_some_and(|p| p > 0),
                "step 1 makes this a real signal target, not a `None`: {:?}",
                spawned[0]
            );
            assert_eq!(
                fx.handle.owned_pid(&agent_id),
                journal_pid,
                "the table and the journal must name one pid, or a `session/quit` KillTree and \
                 this supervisor's own handle would signal different processes"
            );
            assert!(
                fx.handle.owned_pgid(&agent_id).is_some_and(|g| g > 0),
                "§6.7 kills a per-node process group, so the group has to be recorded — and read \
                 with getpgid(2), never assumed equal to the pid"
            );
            assert!(
                fx.handle.owned_started_at(&agent_id).is_some(),
                "the instant the process came into existence"
            );
            assert!(
                fx.handle.owned_task_id(&agent_id).is_some(),
                "the contract this node runs under"
            );
            assert_eq!(fx.handle.owned_nodes(), 2, "the caller and its child");
            settle(&fx, &agent_id);
        }

        /// **The bound a caller asked for is the bound the tree reports.**
        ///
        /// `marion tree`'s detail pane prints `NodeSummary.timeout`, and until the intent recorded
        /// one there was nothing in the journal to print: the projection re-resolved §3.1's bound
        /// from the *agent type*, so every node on the screen read 900 s however short a clock the
        /// operator or the parent had actually put it under. A pane that reports a bound no node is
        /// running under is worse than one that reports none — it is the wrong number in the one
        /// place an operator looks to decide whether a run has time left.
        ///
        /// Over a **real** child, through `agent/spawn` and back out of `tree/subscribe`, because
        /// the two halves this pins are a write and a read on opposite sides of the journal.
        #[test]
        fn a_childs_requested_bound_is_what_the_tree_reports() {
            if !marion_testsupport::harness_available("claude") {
                return;
            }
            let fx = owning("owns-child-bound", vec![intent("root", None, "claude", 0)]);
            let agent_id = spawn_a_real_child(&fx, 120);

            let out = crate::serve::sink(ConnId(4));
            let MethodResult::TreeSubscribe(snap) = fx
                .handle
                .call(
                    ConnId(4),
                    &Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
                    &out,
                )
                .expect("the tree is readable")
            else {
                panic!("wrong result type")
            };
            let child = snap
                .nodes
                .iter()
                .find(|n| n.agent_id == agent_id)
                .expect("the spawned child is in the tree");
            assert_eq!(
                child.timeout,
                marion_core::encoding::Duration::from_secs(120),
                "`spawn`'s `timeout_secs` is the clock this node runs under, so it is the clock \
                 the tree must show — not its agent type's default"
            );
            assert_ne!(
                child.timeout,
                agent_type::builtin("claude").unwrap().timeout,
                "and the assertion above is not passing by coincidence with §3.1's default"
            );

            if let Some(pid) = fx.handle.owned_pid(&agent_id) {
                crate::kill::kill_process_tree_and_wait(pid);
            }
            settle(&fx, &agent_id);
        }

        /// **§7.3.1: `gone` touches nothing — and after this step that covers a *bridge*.**
        ///
        /// The comment on `Handle::gone` has always said so; this asserts it, because a comment is
        /// not a test and this is the step that makes the invariant load-bearing. While every node
        /// was owned by the bridge process the harness started, a SIGKILL of that bridge — which
        /// s16 measured as what a real Claude Code harness sends, uncatchable, ~450 ms after its
        /// SIGTERM — orphaned a live process at pid 1 with an unresolved `SpawnIntent` and no pid
        /// to recover it by. Once the supervisor owns the node, the same kill closes a socket.
        ///
        /// **Liveness by S15's three-valued `ps`, never `kill(pid, 0)`**, which reports a zombie as
        /// alive and would let a node that died a millisecond before the departure satisfy this.
        #[test]
        fn a_departing_client_does_not_touch_a_node_the_supervisor_owns() {
            if !marion_testsupport::harness_available("claude") {
                return;
            }
            let fx = owning("owns-departure", vec![intent("root", None, "claude", 0)]);
            let agent_id = spawn_a_real_child(&fx, 30);

            let before = (
                fx.handle.owned_pid(&agent_id),
                fx.handle.owned_pgid(&agent_id),
                fx.handle.owned_task_id(&agent_id),
                fx.handle.owned_started_at(&agent_id),
                fx.handle.owned_nodes(),
                fx.handle.running_nodes(),
            );
            let pid = before.0.expect("a process exists");
            assert_eq!(
                marion_testsupport::liveness(pid),
                marion_testsupport::Liveness::Alive,
                "the premise of this test is a live node; without it the assertions below are \
                 vacuous"
            );

            fx.handle
                .gone(ConnId(3), &ClientGone::SocketClosed, &Departure::Eof);

            assert_eq!(
                marion_testsupport::liveness(pid),
                marion_testsupport::Liveness::Alive,
                "a client's departure must not reach the node's process (§7.3.1)"
            );
            assert_eq!(
                (
                    fx.handle.owned_pid(&agent_id),
                    fx.handle.owned_pgid(&agent_id),
                    fx.handle.owned_task_id(&agent_id),
                    fx.handle.owned_started_at(&agent_id),
                    fx.handle.owned_nodes(),
                    fx.handle.running_nodes(),
                ),
                before,
                "…and must not reach the supervisor's record of it either"
            );

            // The node is still held, so §5.7 still refuses to exit — the other half of the same
            // invariant, and what stops a departure from becoming a fleet-wide shutdown.
            assert!(
                !fx.handle.idle_exit_eligible(),
                "a supervisor whose last client left still owns a running node"
            );

            // Ended deliberately rather than left to the 30 s bound, so the file leaves no
            // survivor and no scratch directory behind. This is cleanup, not an assertion.
            crate::kill::kill_process_tree_and_wait(pid);
            settle(&fx, &agent_id);
        }

        /// **The node table as §5.7's second guard.**
        ///
        /// The journal here says nothing at all — no node, no intent — so `resident_reason` is `None`
        /// and a journal-only predicate would let this supervisor exit. A node whose thread is running
        /// and whose journal write failed is exactly that shape, and exiting through it leaves the
        /// untracked live process §9's M2 criteria forbid.
        #[test]
        fn a_node_this_supervisor_still_runs_holds_it_even_when_the_journal_says_nothing() {
            let fx = owning("owns-exit-guard", vec![]);
            assert!(
                fx.handle.idle_exit_eligible(),
                "an empty supervisor with no clients may exit"
            );
            fx.handle.claim(
                &id("ghost"),
                Some(marion_core::contract::TaskId("t".into())),
                fx.repo.clone(),
            );
            assert!(
                !fx.handle.idle_exit_eligible(),
                "a node this process still owns and has no outcome for must hold the supervisor, \
                 however quiet the journal is"
            );
            fx.handle.mark_finished(
                &id("ghost"),
                NodeOutcome::Child(Box::new(Err(crate::spawn::SpawnError::UnknownAgentType(
                    "x".into(),
                )))),
            );
            assert!(
                fx.handle.idle_exit_eligible(),
                "…and must stop holding it once its thread has produced an outcome"
            );
        }
    }
}
