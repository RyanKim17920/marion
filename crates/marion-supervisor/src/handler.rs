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
//! [`marion_proto::result::TreeSubscribeResult`] says why the two travel together: *"a client that
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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marion_core::agent_type;
use marion_core::contract::{AgentId, ProcessExit, TaskContract, TaskId};
use marion_core::journal::{
    KillConfirmed, KillIntent, ReapConfirmed, ReapIntent, RecordKind, SupervisorExited,
};
use marion_core::node::{NodeState, ReapState};
use marion_core::registry::{Replay, ReplayedNode};
use marion_proto::notify::Event;
use marion_proto::result::{
    NodeAttachResult, NodeGetResult, SessionQuitResult, TreeSubscribeResult,
};
use marion_proto::{
    AttachMode, Call, ClientGone, DetachGuidance, FailureKind, KilledNode, MethodResult,
    NodeSummary, QuitDisposition, QuitOutcome, ReplayPoint, ResidentReason, RpcError,
    SupervisorDisposition,
};

use crate::registry::{LiveRegistry, Registry};
use crate::serve::{ConnId, Departure, Handle, Outbound};

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
/// Pure: it reads the replayed node and this build's agent-type registry, and touches nothing else.
/// That is what makes every arm above testable without a socket, a journal or a thread.
pub fn summarize(node: &ReplayedNode) -> Result<NodeSummary, Unprojectable> {
    let intent = node.intent.as_ref().ok_or(Unprojectable::NoIntent)?;
    let ty = agent_type::builtin(&intent.agent_type)
        .ok_or_else(|| Unprojectable::UnknownAgentType(intent.agent_type.clone()))?;
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
        depth,
        state: node.state,
        reap_state: node.reap_state,
        timeout: ty.timeout,
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
    task_id: TaskId,
    /// §5.4's per-node capability, minted here and written into exactly one other place: the MCP
    /// declaration this node's own bridge reads. See [`RegistryHandle::claim`].
    token: String,
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
    /// What `run_spawn` returned. `None` means **still running**, and that is the reading §5.7's
    /// exit predicate takes as a second guard beside the journal's.
    outcome: Option<Result<TaskContract, crate::spawn::SpawnError>>,
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
/// The same 900 the bridge's `spawn` tool has always defaulted to (`main::handle_tool_call`), kept
/// as one constant rather than a second literal so the two surfaces cannot drift into promising
/// different bounds for the same absent field. `run::effective_timeout` clamps it exactly as it
/// clamps a stated one.
const DEFAULT_SPAWN_TIMEOUT_SECS: u64 = 900;

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
    task_id: TaskId,
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

impl crate::run::SpawnObserver for NodeOwner {
    fn identified(&self, agent_id: &AgentId) -> Option<String> {
        let token = self.handle.claim(agent_id, self.task_id.clone());
        *lock(&self.identified) = Some(agent_id.clone());
        // Ignored: a receiver dropped before this fires means the call that started the spawn has
        // already given up on it, and the node goes on running either way. Panicking here would
        // unwind through `AbortOnDrop` and journal `SpawnAborted` over a node that is fine.
        let _ = self.tx.send(Progress::Identified(agent_id.clone()));
        (!token.is_empty()).then_some(token)
    }

    fn started(&self, agent_id: &AgentId, pid: i32) {
        self.handle.mark_started(agent_id, pid);
        let _ = self.tx.send(Progress::Started);
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
        "marion refused this spawn before it minted a node: the agent type is not one this build \
         has, or the requested writable scope is outside that type's ceiling. Nothing was \
         journaled and nothing was started, so there is no node to look up. (§3.1, §5.4)",
        "§3.1",
    )
}

fn spawn_failed_before_the_process_existed(agent_id: &AgentId) -> RpcError {
    RpcError::of(
        FailureKind::Internal,
        Some(&agent_id.0),
        format!(
            "node `{}` was journaled and its launch failed before any process existed — a \
             worktree, a configuration document, or the harness `--version` probe. Its \
             `SpawnIntent` is resolved by a `SpawnAborted` beside it, which after §11 item 28 step \
             1 is evidence that **no process exists**, not merely consistent with it (§7.2). A \
             worktree may be left behind; a process is not.",
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

trait QuitRuntime: Send + Sync {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool;
}

struct SystemQuitRuntime;

impl QuitRuntime for SystemQuitRuntime {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
        crate::run::kill_process_tree_and_wait(pid)
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
    /// **What `agent/spawn` runs a node in**, or `None` for a supervisor that cannot spawn.
    ///
    /// `None` is not a degenerate case, it is production today: `detach.rs`'s stage 3 builds this
    /// handle from a journal path, and the *repo* — which `run::Env` needs and a `<project-hash>`
    /// cannot be inverted back into — reaches that process as `Launch::project_root` and goes no
    /// further. Wiring it through is §11 item 28 step 5's one-line change there, and until it lands
    /// a socket `agent/spawn` is refused with a sentence that says exactly this rather than failing
    /// somewhere further in. See [`RegistryHandle::owning`].
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
        let mut g = lock(&self.shared);
        let events = self.live.read(|r| collect(r, &mut g));
        deliver(&mut g, &events);
        events.len()
    }

    /// §2's `tree/subscribe`: the snapshot, and the point live notifications begin from.
    fn subscribe(&self, out: &Outbound) -> TreeSubscribeResult {
        let mut g = lock(&self.shared);
        // **One read, three uses.** See the module doc: catching up existing subscribers, building
        // this one's snapshot, and recording what it has been told all happen against the same view,
        // under one lock, so there is no instant at which a notification could slip between the
        // snapshot and the subscription.
        let (events, nodes, read_point) = self.live.read(|r| {
            let events = collect(r, &mut g);
            let nodes = project(r.tree(), &mut g);
            (events, nodes, r.read_point())
        });
        deliver(&mut g, &events);
        g.subs.push(out.clone());
        TreeSubscribeResult { nodes, read_point }
    }

    fn node_get(&self, id: &AgentId) -> Result<NodeGetResult, RpcError> {
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
            Some(node) => summarize(node)
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
    /// [`marion_proto::result::NodeAttachResult`] has no field for them and deliberately so: a
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
    fn node_attach(&self, id: &AgentId, out: &Outbound) -> Result<NodeAttachResult, RpcError> {
        let (summary, state, reap_state, project) = self.live.read(|r| {
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
            let summary = summarize(node).map_err(|e| e.as_error(id))?;
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
        // Sent before the reader is parked, so nothing appended between the two can be delivered
        // ahead of the replay it comes after.
        let live = deliver_events(out, id, &replayed);
        if live {
            lock(&self.shared).attached.push(Attachment {
                conn: out.conn(),
                agent_id: id.clone(),
                reader,
                out: out.clone(),
            });
        }
        Ok(NodeAttachResult {
            node: summary,
            mode,
        })
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
    /// `pub(crate)` rather than private because it is also the whole of what a test needs to put a
    /// node in this table — and a test that reached in through a back door would be asserting
    /// against a binding production does not make.
    pub(crate) fn claim(&self, agent_id: &AgentId, task_id: TaskId) -> String {
        let token = mint_token();
        lock(&self.nodes).insert(
            agent_id.clone(),
            NodeHandle {
                task_id,
                token: token.clone(),
                pid: None,
                pgid: None,
                started_at: None,
                join: None,
                outcome: None,
            },
        );
        token
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
    fn mark_finished(
        &self,
        agent_id: &AgentId,
        outcome: Result<TaskContract, crate::spawn::SpawnError>,
    ) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.outcome = Some(outcome);
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
    pub fn owned_task_id(&self, agent_id: &AgentId) -> Option<TaskId> {
        lock(&self.nodes).get(agent_id).map(|n| n.task_id.clone())
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
            // panic already reached the caller as `SpawnError::Panicked` through the outcome.
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
        c: &marion_proto::SpawnCaller,
    ) -> Result<crate::run::Caller, RpcError> {
        // **The token is checked before anything else is said about the node**, so a caller who
        // guesses an `AgentId` learns nothing from the shape of the refusal beyond "no".
        let known = {
            let nodes = lock(&self.nodes);
            // **A node this supervisor does not own is still compared**, against a decoy of the
            // same shape, so "no such node" and "wrong token" take the same path and cost the same.
            // Returning early on the absent case would turn the *existence* of a node into an
            // oracle a caller could probe with ids alone.
            let (stored, owned) = match nodes.get(&c.agent_id) {
                Some(n) => (n.token.clone(), true),
                None => (decoy_token().to_string(), false),
            };
            tokens_match(&stored, &c.node_token) & owned
        };
        if !known {
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
        }
        let (agent_type, depth) = self.live.read(|r| {
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
            let ty = agent_type::builtin(&intent.agent_type).ok_or_else(|| {
                Unprojectable::UnknownAgentType(intent.agent_type.clone()).as_error(&c.agent_id)
            })?;
            Ok::<_, RpcError>((ty, intent.depth))
        })?;
        Ok(crate::run::Caller {
            agent_id: c.agent_id.0.clone(),
            agent_type,
            depth,
            live_children: self.live_children_of(&c.agent_id),
        })
    }

    /// §2's `agent/spawn` — see [`agent_spawn`](Self::agent_spawn)'s doc for the whole shape.
    fn agent_spawn(
        &self,
        p: &marion_proto::params::AgentSpawnParams,
    ) -> Result<marion_proto::result::AgentSpawnResult, RpcError> {
        let me = self.me.upgrade().ok_or_else(|| {
            RpcError::internal(
                "this supervisor is being dropped and will not start a node it could not then own",
            )
        })?;
        let Some(env) = self.spawn_env.clone() else {
            return Err(RpcError::unimplemented(
                "agent/spawn",
                "this supervisor was booted without a spawn environment, so it can describe nodes \
                 and cannot start one. `run::Env` needs the repository path, and a supervisor \
                 recovers only `<state>/<project-hash>` from its journal — a hash it cannot invert. \
                 The path reaches this process as `Launch::project_root` and stops there; §11 item \
                 28 step 5 is what carries it in. Refused here rather than further in, where the \
                 failure would be about a directory instead of about a build.",
                "§2",
            ));
        };
        let Some(caller_id) = p.caller.as_ref() else {
            return Err(RpcError::unimplemented(
                "agent/spawn",
                "a client creating a **root** is §11 item 28 step 6, and this build does not \
                 serve it. `root::prepare` owns a root's launch — its own agent-dir layout, its \
                 `ROOT_DEPTH`, its change record and the `parent_id: None` that makes it a root — \
                 and `run_spawn` would give it a parent it does not have. A spawn with a `caller` \
                 is served; one without is refused rather than served as a child of nobody.",
                "§2",
            ));
        };

        // **Held from here to the child's durable intent, and no further.** See
        // [`Self::spawn_decision`]: the registry is a follower, so two callers that both evaluated
        // the gate before either wrote its intent would both pass a bound only one fits under.
        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        let caller = self.resolve_caller(caller_id)?;
        // §6.1 step 2, before every side effect — the same pure function `run_spawn` calls, run
        // here as well so the refusal arrives in the frame that asked for it rather than as a node
        // that was never going to start. Two call sites of one function, never two rules.
        agent_type::check_spawn_gates(&caller.agent_type, caller.depth, caller.live_children)
            .map_err(gate_refusal)?;

        // Minted here rather than by the caller, for §9's reason: the contract id names a run
        // marion performed, and a caller-chosen one would let two runs share a contract file.
        let task_id = crate::run::entropy()
            .map(|e| marion_core::ids::new_task_id(crate::run::unix_millis(), e))
            .map_err(|e| {
                RpcError::internal(format!(
                    "marion could not mint a task id for this spawn, so nothing was started: {e}"
                ))
            })?;
        let req = crate::run::SpawnRequest {
            agent_type: p.agent_type.clone(),
            prompt: p.prompt.clone(),
            acceptance_criteria: p.acceptance_criteria.clone(),
            writable_scope: p.writable_scope.clone(),
            // **Resolved here, not defaulted in the params.** `params.rs` argues why the wire
            // carries `Option`; this is the one place that turns absence into a number, and
            // `effective_timeout` clamps it exactly as it clamps a stated one.
            timeout_secs: p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS),
            model: p.model.clone(),
        };

        let (tx, progress) = std::sync::mpsc::channel();
        let observer = NodeOwner {
            handle: me.clone(),
            task_id: task_id.clone(),
            tx,
            identified: Mutex::new(None),
        };
        // **Everything the thread needs is owned**, for `Background::start`'s reason: this work
        // outlives the JSON-RPC frame that asked for it, so it cannot borrow from this stack frame.
        let owner = me.clone();
        let join = std::thread::spawn(move || {
            let outcome = crate::run::run_spawn_watched(&env, &req, &task_id, &caller, &observer);
            // The agent id is known only if `identified` fired. A spawn refused above it — an
            // unknown agent type, a scope outside the ceiling — never minted a node, so there is
            // nothing to file the outcome under and nothing holding the supervisor open.
            if let Some(agent_id) = observer.identified_id() {
                owner.mark_finished(&agent_id, outcome);
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
            // would not compile, a `--version` probe that expired.
            Ok(_) => return Err(spawn_failed_before_the_process_existed(&agent_id)),
            Err(_) => return Err(launch_bound_expired(Some(&agent_id))),
        }
        self.live.refresh();
        Ok(marion_proto::result::AgentSpawnResult {
            // **Read back off the registry, not asserted.** The state this returns is the state the
            // journal says, which is the point of answering at `Spawned` rather than before it: a
            // client that renders `Spawning` here is rendering a record it could have read itself.
            state: self
                .live
                .read(|r| r.tree().get(&agent_id).map(|n| n.state))
                .unwrap_or(NodeState::Spawning),
            agent_id,
        })
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

    fn journal(&self) -> Result<crate::journal::Journal, RpcError> {
        crate::journal::Journal::open_path(&self.journal_path(), crate::journal::writer_id())
            .map_err(|e| {
                RpcError::internal(format!(
                    "session/quit could not open the journal, so it refused before changing any \
                     node: {e}"
                ))
            })
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
    /// leaves a record marion cannot mistake for a spawn that never happened. It is still
    /// aspirational for a **root**, whose record is written after the run with `pid: None`
    /// (`root.rs`'s `spawned_record`) — item 28's step 6.
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

        let mut journal = self.journal()?;
        let mut killed = Vec::with_capacity(targets.len());
        for node in targets {
            journal
                .append(RecordKind::KillIntent(KillIntent {
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
            journal
                .append(RecordKind::KillConfirmed(KillConfirmed {
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
        let mut journal = self.journal()?;
        for node in reaping {
            journal
                .append(RecordKind::ReapIntent(ReapIntent {
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
            journal
                .append(RecordKind::ReapConfirmed(ReapConfirmed {
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
/// `ReapedIdle` is tested **before** the exit, because §7.3.2's disposition (c) reaps a node that
/// is idle rather than one that is finished, and a reaped node whose journal also shows an exit is
/// still the one the operator can bring back. Collapsing it into `ReplayOnly` would tell a client
/// its only option is to read, when the node is resumable.
fn attach_mode(state: NodeState, reap_state: ReapState, point: ReplayPoint) -> AttachMode {
    if reap_state == ReapState::ReapedIdle {
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
                .node_attach(&p.agent_id, out)
                .map(MethodResult::NodeAttach),
            // **Not keyed by the connection**, and that is §7.3.1 restated on the way *in*
            // rather than defended on the way out: a node this supervisor owns is not a resource
            // of the client that asked for it, so nothing here records which connection called.
            // That is what makes `gone` able to touch nothing — there is no per-connection node
            // list for it to reap, structurally, rather than by a rule someone must remember.
            Call::AgentSpawn(p) => self.agent_spawn(p).map(MethodResult::AgentSpawn),
            Call::SessionQuit(p) => self
                .session_quit(&p.disposition)
                .map(MethodResult::SessionQuit),
            // Everything else is specified and not built. `Unimplemented` and not `Unsupported`,
            // per `error.rs`: the gap is marion's, not the harness's, and the operator's next move
            // is to check the milestone rather than the node.
            other => Err(RpcError::unimplemented(
                other.method().as_str(),
                format!(
                    "`{}` is specified (§2) and not built. This supervisor answers `node/get`, \
                     `tree/subscribe`, `node/attach`, `agent/spawn` and `session/quit`; the \
                     remaining ten methods land with the milestone that needs them.",
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
    }

    /// §2's notifications, driven by the accept loop's heartbeat — see [`Handle::tick`] for why
    /// that loop and not a fourth thread.
    ///
    /// Both halves, in this order. `flush` pushes what the *journal* said (a node appeared, a node
    /// changed state); `pump_attached` pushes what a *node* said. Journal first because a client
    /// that learns of an event on a node it has not been told exists has to hold it, and §2 puts
    /// `tree/node-added` before anything else about a node for exactly that reason.
    fn tick(&self) {
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
        let result = self.journal().and_then(|mut journal| {
            journal
                .append(RecordKind::SupervisorExited(SupervisorExited {}))
                .map(|_| ())
                .map_err(journal_failure_after_signal)
        });
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
fn project(tree: &Replay, g: &mut Shared) -> Vec<NodeSummary> {
    let mut out = Vec::new();
    let mut lost = 0usize;
    for n in tree.nodes() {
        match summarize(n) {
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
fn collect(r: &Registry, g: &mut Shared) -> Vec<Event> {
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
                if let Ok(node) = summarize(n) {
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
            s.send(&marion_proto::Frame::Notification(
                marion_proto::Notification::new(e.clone()),
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
    use marion_core::contract::{ExitStatus, ProcessExit};
    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::journal::{
        Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, StateChanged, WriterId, encode,
    };
    use marion_core::node::BlockReason;
    use marion_proto::Frame;
    use marion_testsupport::scratch;
    use std::io::Write;
    use std::path::Path;

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
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
        })
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
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
    fn a_summary_resolves_its_bound_from_the_agent_type_and_names_nothing_it_was_not_told() {
        let n = node_of(
            &[line(0, 1, intent("child", Some("root"), "codex-impl", 1))],
            "child",
        );
        let s = summarize(&n).expect("a fully described node projects");
        assert_eq!(s.agent_id, id("child"));
        assert_eq!(s.parent_id, Some(id("root")));
        assert_eq!(s.agent_type, "codex-impl");
        assert_eq!(s.harness, Harness::Codex);
        assert_eq!(s.depth, 1);
        assert_eq!(s.state, NodeState::Spawning);
        assert_eq!(
            s.timeout,
            agent_type::builtin("codex-impl").unwrap().timeout,
            "§3.1 makes the agent type the source of the bound and §9 re-resolves it from there; \
             a journalled copy would be a second source of truth"
        );
        assert_eq!(
            s.timeout,
            marion_core::encoding::Duration::from_secs(agent_type::DEFAULT_TIMEOUT_SECS),
            "and that source really is §3.1's 900 s default, not a value invented here"
        );
        assert_eq!(
            s.name, None,
            "nothing sets `Node.name` yet, so `None` is what the journal says rather than a \
             placeholder for what marion does not know"
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
                }),
            )],
            "no-intent",
        );
        assert_eq!(summarize(&orphan), Err(Unprojectable::NoIntent));
        let e = Unprojectable::NoIntent.as_error(&id("no-intent"));
        assert_eq!(e.kind(), Some(FailureKind::Internal));
        assert!(e.message.contains("SpawnIntent"), "{e}");

        // (2) an agent type this build does not have — so §3.1's bound has no source.
        let unknown = node_of(&[line(0, 1, intent("a", None, "codex-turbo", 0))], "a");
        assert_eq!(
            summarize(&unknown),
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
        assert_eq!(summarize(&deep), Err(Unprojectable::DepthOutOfRange(300)));
        assert!(
            summarize(&node_of(
                &[line(0, 1, intent("a", None, "codex-impl", 255))],
                "a"
            ))
            .is_ok(),
            "255 fits, so the boundary is the type's and not an arbitrary cap"
        );
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
        disposition: marion_proto::QuitDisposition,
    ) -> Result<marion_proto::result::SessionQuitResult, RpcError> {
        let out = crate::serve::sink(ConnId(9));
        match fx.handle.call(
            ConnId(9),
            &Call::SessionQuit(marion_proto::params::SessionQuitParams { disposition }),
            &out,
        )? {
            MethodResult::SessionQuit(r) => Ok(r),
            other => panic!("wrong result: {}", other.method().as_str()),
        }
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
        let f = Frame::Request(marion_proto::Request::new(
            marion_proto::RequestId::Number(id),
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

    fn until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        cond()
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
            Call::NodeGet(marion_proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::NodeGet(got) =
            marion_proto::Method::NodeGet.decode_result(&body).unwrap()
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
            Call::NodeGet(marion_proto::params::NodeGetParams {
                agent_id: id("nobody"),
            }),
            2,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Error(e) = resp.outcome else {
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
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
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
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
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
                Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
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
                &Call::SessionQuit(marion_proto::params::SessionQuitParams {
                    disposition: marion_proto::QuitDisposition::DetachAll,
                }),
                &out,
            )
            .expect("detach is implemented, not refused");
        let MethodResult::SessionQuit(result) = result else {
            panic!("wrong result type")
        };
        let marion_proto::QuitOutcome::Detached {
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
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::SpawnOutstanding
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
                &Call::NodeGet(marion_proto::params::NodeGetParams {
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
            marion_proto::QuitDisposition::KillTree { confirmed: vec![] },
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

        let disposition = marion_proto::QuitDisposition::KillTree {
            confirmed: vec![id("child"), id("root")],
        };
        fx.handle.connected(ConnId(9));
        let result = quit(&fx, disposition.clone()).expect("the exact set was confirmed");
        let marion_proto::QuitOutcome::Killed { nodes, supervisor } = result.outcome else {
            panic!("kill returned another disposition's outcome")
        };
        assert_eq!(
            nodes,
            [
                marion_proto::KilledNode {
                    agent_id: id("root"),
                    was: NodeState::Running,
                },
                marion_proto::KilledNode {
                    agent_id: id("child"),
                    was: NodeState::Blocked(BlockReason::Permission),
                },
            ]
        );
        assert_eq!(supervisor, marion_proto::SupervisorDisposition::Exiting);
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

        let result = quit(&fx, marion_proto::QuitDisposition::KillTree { confirmed });
        // Reap before asserting, unconditionally: a failure here must not also leak the victim.
        let outcome = result.map(|r| r.outcome);
        let after = liveness(pid);
        let _ = victim.kill();
        let _ = victim.wait();

        let marion_proto::QuitOutcome::Killed { nodes, .. } =
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

        let result = quit(&fx, marion_proto::QuitDisposition::ReapIdleDetachBusy)
            .expect("the default disposition is implemented");
        let marion_proto::QuitOutcome::ReapedAndDetached {
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
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::BlockedNode
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
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&aborted, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Exiting,
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
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&outstanding, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::SpawnOutstanding
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
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::SpawnOutstanding
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
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Exiting,
            "the control: with the journal readable, nothing here holds it"
        );

        append(&fx.path, b"this is a complete line and not a record\n");
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::RegistryStopped
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
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::NonTerminalNode
            )
        );
        fx.handle.gone(
            ConnId(9),
            &ClientGone::Quit(marion_proto::QuitDisposition::DetachAll),
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
        let outcome = quit(&fx, marion_proto::QuitDisposition::ReapIdleDetachBusy)
            .unwrap()
            .outcome;
        let marion_proto::QuitOutcome::ReapedAndDetached { supervisor, .. } = outcome else {
            panic!("wrong disposition outcome")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::NonTerminalNode
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
        let outcome = quit(&fx, marion_proto::QuitDisposition::DetachAll)
            .unwrap()
            .outcome;
        let marion_proto::QuitOutcome::Detached { supervisor, .. } = outcome else {
            panic!("wrong disposition outcome")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::UnconfirmedReapIntent
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
            Some(marion_proto::ResidentReason::NonTerminalNode),
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
            marion_proto::QuitDisposition::KillTree {
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
            marion_proto::QuitDisposition::KillTree {
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
            marion_proto::QuitDisposition::KillTree {
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
            marion_proto::QuitDisposition::KillTree {
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

        let marion_proto::QuitOutcome::ReapedAndDetached {
            reaped, detached, ..
        } = quit(&fx, marion_proto::QuitDisposition::ReapIdleDetachBusy)
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

        let marion_proto::QuitOutcome::ReapedAndDetached {
            reaped, detached, ..
        } = quit(&fx, marion_proto::QuitDisposition::ReapIdleDetachBusy)
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

        let marion_proto::QuitOutcome::Detached {
            detached,
            gate_exposed,
            ..
        } = quit(&fx, marion_proto::QuitDisposition::DetachAll)
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
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .expect("detach is implemented")
                .outcome
        else {
            panic!("detach returned another disposition's outcome")
        };
        assert_eq!(supervisor, marion_proto::SupervisorDisposition::Exiting);
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
            Call::NodeCancel(marion_proto::params::NodeCancelParams {
                agent_id: id("root"),
            }),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Error(e) = resp.outcome else {
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
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
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
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );

        // The late subscriber's snapshot already has the child…
        let Frame::Response(resp) = next_frame(&mut lr) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
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
    ) -> (Vec<Event>, marion_proto::Outcome) {
        call(
            c,
            Call::NodeAttach(marion_proto::params::NodeAttachParams {
                agent_id: id(agent),
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

    fn attached_ok(outcome: marion_proto::Outcome) -> marion_proto::result::NodeAttachResult {
        let marion_proto::Outcome::Result(body) = outcome else {
            panic!("node/attach was refused: {outcome:?}")
        };
        let MethodResult::NodeAttach(r) = marion_proto::Method::NodeAttach
            .decode_result(&body)
            .expect("the result decodes")
        else {
            panic!("wrong result type")
        };
        r
    }

    fn refusal(outcome: marion_proto::Outcome) -> RpcError {
        match outcome {
            marion_proto::Outcome::Error(e) => e,
            other => panic!("expected a refusal, got {other:?}"),
        }
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
        use marion_proto::SpawnCaller;
        use marion_proto::params::AgentSpawnParams;
        use marion_testsupport::{fixture_repo, scratch};

        /// A handle that **owns** what it spawns, over a real repo and a real project directory.
        struct Owning {
            handle: Arc<RegistryHandle>,
            project: ProjectDir,
            /// **Declared last so it is dropped last.** Rust drops fields in declaration order,
            /// and a scratch directory removed while a node's thread is still writing into it
            /// would turn a clean failure into an unrelated io error.
            _dir: marion_testsupport::Scratch,
        }

        fn owning(tag: &str, records: Vec<RecordKind>) -> Owning {
            let dir = scratch(tag);
            let repo = fixture_repo(&dir);
            let state = dir.join("state");
            let project = ProjectDir::new(&state, &repo);
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
                    repo,
                    project_dir: project.clone(),
                    bridge: std::path::PathBuf::from("/bin/marion-supervisor"),
                    // Answers nothing, which is what bounds the two tests below that really launch.
                    base_url: Some("http://127.0.0.1:8099/v1".into()),
                    auth: marion_harness::Auth::Canned,
                },
            );
            Owning {
                handle,
                project,
                _dir: dir,
            }
        }

        fn params(caller: Option<SpawnCaller>, secs: u64) -> AgentSpawnParams {
            AgentSpawnParams {
                agent_type: "claude".into(),
                prompt: "do the task".into(),
                caller,
                acceptance_criteria: vec![],
                writable_scope: vec!["src/**".into()],
                timeout_secs: Some(secs),
                model: None,
            }
        }

        fn spawn(
            fx: &Owning,
            p: AgentSpawnParams,
        ) -> Result<marion_proto::result::AgentSpawnResult, RpcError> {
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
            let real = fx
                .handle
                .claim(&id("root"), marion_core::contract::TaskId("t".into()));
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
            let token = deep
                .handle
                .claim(&id("deep"), marion_core::contract::TaskId("t".into()));
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
            let token = fx
                .handle
                .claim(&id("root"), marion_core::contract::TaskId("t".into()));
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

        /// A client creating a **root** is step 6, and this build says so rather than serving it as a
        /// child of nobody. `run_spawn` writes `parent_id: Some(caller)` unconditionally, so serving
        /// this would put a node in the tree whose parent is a fabrication.
        #[test]
        fn a_client_creating_a_root_over_the_socket_is_refused_naming_the_step_that_serves_it() {
            let fx = owning("owns-root", vec![]);
            let e = spawn(&fx, params(None, 1)).expect_err("a root spawn is not served yet");
            assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
            assert!(
                e.message.contains("step 6"),
                "the refusal must name what would serve it: {}",
                e.message
            );
            assert_eq!(journal_len(&fx), 0, "and nothing was created");
        }

        /// A supervisor with no spawn environment refuses in its **own voice**, naming the build rather
        /// than failing further in on a directory. See `RegistryHandle::spawn_env`.
        #[test]
        fn a_supervisor_that_cannot_spawn_refuses_by_naming_the_build_not_a_missing_directory() {
            let fx = fx("owns-no-env");
            let out = crate::serve::sink(ConnId(4));
            let e = fx
                .handle
                .call(ConnId(4), &Call::AgentSpawn(params(None, 1)), &out)
                .expect_err("a handle built by `new` owns nothing");
            assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
            assert!(e.message.contains("spawn environment"), "{}", e.message);
        }

        // ------------------------------------------------------------------------------------
        // The token itself.
        // ------------------------------------------------------------------------------------

        /// Two nodes never share a capability, and a token is long enough that guessing is not a
        /// strategy. 32 bytes of `/dev/urandom` as hex is 64 characters.
        #[test]
        fn every_node_gets_its_own_unguessable_token() {
            let fx = owning("owns-tokens", vec![]);
            let a = fx
                .handle
                .claim(&id("a"), marion_core::contract::TaskId("t".into()));
            let b = fx
                .handle
                .claim(&id("b"), marion_core::contract::TaskId("t".into()));
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
            let token = fx
                .handle
                .claim(&id("root"), marion_core::contract::TaskId("t".into()));
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
            crate::run::kill_process_tree_and_wait(pid);
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
            fx.handle
                .claim(&id("ghost"), marion_core::contract::TaskId("t".into()));
            assert!(
                !fx.handle.idle_exit_eligible(),
                "a node this process still owns and has no outcome for must hold the supervisor, \
                 however quiet the journal is"
            );
            fx.handle.mark_finished(
                &id("ghost"),
                Err(crate::spawn::SpawnError::UnknownAgentType("x".into())),
            );
            assert!(
                fx.handle.idle_exit_eligible(),
                "…and must stop holding it once its thread has produced an outcome"
            );
        }
    }
}
