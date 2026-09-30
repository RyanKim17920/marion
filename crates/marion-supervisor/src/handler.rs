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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marion_core::contract::{AgentId, TaskContract, TaskId};
use marion_core::journal::{RecordKind, SupervisorExited};
use marion_core::proto::result::{NodeGetResult, TreeSubscribeResult};
use marion_core::proto::{Call, ClientGone, MethodResult, RpcError};
use marion_core::secret::Secret;

use crate::registry::LiveRegistry;
use crate::serve::{ConnId, Departure, Handle, Outbound, Peer};
use broadcast::{deliver, notify_claimer, notify_seed_for};
use launch::{check_project, decoy_token, mint_task_id, root_spawn_authorized, unminted_token};
use lifecycle::{
    QuitRuntime, SystemQuitRuntime, journal_failure_after_signal, journal_failure_after_signal_in,
    journal_failure_before_signal_in,
};
use panes::{Attachment, Panes};
use summary::{Told, collect, project};

pub use broadcast::read_point;
pub(crate) use launch::mint_token;
pub use summary::{Unprojectable, summarize};
pub(crate) use summary::{clock, summarize_spent};

#[cfg(test)]
use crate::registry::Registry;
#[cfg(test)]
use launch::{
    NativeLaunchGateError, NodeOwner, caught, native_launch_refusal, recorded_type, root_panicked,
    root_spec_from_spawn, spawn_failed_before_the_process_existed, spawn_panicked,
    validate_native_launch_boundary,
};
#[cfg(test)]
use marion_core::agent_type;
#[cfg(test)]
use marion_core::journal::{KillConfirmed, ReapConfirmed, ReapIntent};
#[cfg(test)]
use marion_core::node::{NodeState, ReapState};
#[cfg(test)]
use marion_core::proto::notify::Event;
#[cfg(test)]
use marion_core::proto::{
    AttachMode, FailureKind, NativeLaunchContext, ReplayPoint, ResidentReason,
};
#[cfg(test)]
use marion_core::registry::{Replay, ReplayedNode};
#[cfg(test)]
use panes::{PaneEntry, attach_mode};
#[cfg(test)]
use summary::Extra;

#[derive(Default)]
struct Shared {
    subs: Vec<Outbound>,
    /// Connections that asked to show desktop notices in their terminal (`notify/claim`), oldest
    /// first: only the first is sent them, and its departure hands them to the next.
    claimers: Vec<Outbound>,
    attached: Vec<Attachment>,
    clients: HashSet<ConnId>,
    /// Whom each connection proved it speaks for — see [`session`].
    principals: HashMap<ConnId, marion_core::proto::result::SessionPrincipal>,
    told: HashMap<AgentId, Told>,
    unprojectable: usize,
    /// The [`Registry::generation`] and [`crate::usage_tally::Tallies::version`] `collect` last ran
    /// against. See [`RegistryHandle::flush`].
    collected_at: Option<(u64, u64)>,
    /// When `collect` last ran: a figure that moved with no journal record is told at most every
    /// [`TOKEN_PUSH_DELAY`] after it. See [`RegistryHandle::flush`].
    collected_when: Option<std::time::Instant>,
    #[cfg(test)]
    collects: usize,
}

/// How soon after the last telling a token figure that moved **without a journal record** is told
/// to subscribers. A coalescing deadline, not a tick: the accept loop is woken by the first change
/// ([`crate::spending::Spending::notifying`]) and comes back at most this long after the last walk,
/// so a node streaming usage frames costs one tree walk a second rather than one per frame. A
/// journal change is told at once, as ever.
const TOKEN_PUSH_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

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
    token: Secret,
    /// **The tree this node lives in**, and therefore the tree its own children branch from —
    /// [`crate::run::SpawnRequest::repo`] for the child of this node's next `agent/spawn`.
    ///
    /// One supervisor serves `/r` and every linked worktree of `/r` (§2 keys on the git common
    /// dir), so the repository cannot be a field of the supervisor; it has to be remembered per
    /// node and inherited down the tree from whichever root stated it — until a child runs in a
    /// worktree of its own, whose entry then names that worktree (`workspace_chosen`), so the
    /// child's children start from the child's work and not from the root's HEAD.
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
    /// Who records how this node's process ended — see [`Ending`].
    ending: Ending,
    /// The node's thread has seen its process end ([`RegistryHandle::process_ended`]). What a
    /// cancel waits on before it moves up a level, and why it never signals a reaped pid.
    process_gone: bool,
    /// A `node/kill` asked for this node while it was being cancelled: the cancel stops waiting
    /// out its grace and kills what is left.
    escalated: bool,
}

/// **Who writes a node's terminal record**, decided once, under [`RegistryHandle::nodes`]'s lock.
///
/// Two writers can reach a node's end: its own thread, which observed the process exit and would
/// journal `Exited` with the status it derived, and an operator's kill (`node/kill`, KillTree),
/// which journals `KillConfirmed` — §6.7's `Exited(Cancelled)`. Both folding in means the later
/// one wins in replay, so a thread that wrote `Exited(Failed)` a moment after the confirmation
/// would rewrite a deliberate cancellation as a failure. The thread already has this answer for its
/// own timeout kill (`timed_out`); this is the same attribution for a kill marion made from outside
/// the thread, and whichever side gets here first settles it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Ending {
    /// Neither has happened yet.
    #[default]
    Running,
    /// A kill claimed the node first: its thread records the cancellation and no `Exited`.
    KillRequested,
    /// A graceful cancel claimed the node first, on behalf of `by`: its running turn is being
    /// ended by the row's abort. Its thread records the cancellation and no `Exited`, as for a
    /// kill; the cancel writes the `KillConfirmed` once the process is gone.
    CancelRequested(marion_core::journal::CancelBy),
    /// The thread saw its process end first and records the exit it observed.
    ProcessEnded,
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

/// `node/steer`: authority, the node's delivery strategy, and the inbox. See its module docs.
mod session;
mod steer;

/// `node/cancel`: freeze a subtree, abort it bottom-up, kill what outlives its grace.
mod cancel;

mod broadcast;
/// Token budgets: register each node's, and act on a line its spend crosses.
mod budget;
mod delivery;
mod launch;
mod lifecycle;
mod panes;
mod summary;
mod workflow;

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
    /// The socket a client dials to reach this supervisor, as the listener bound it. See
    /// [`Self::owning`] for why it is carried rather than derived from the journal.
    socket_path: PathBuf,
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
    /// Notified whenever a node's process ends or its thread finishes — what a cancel waits on
    /// between levels, beside [`Self::nodes`] whose lock it is used with.
    ended: std::sync::Condvar,
    /// **Turn delivery's queue, one inbox per node this supervisor owns** — beside [`Self::nodes`]
    /// and with the same lifetime: opened at [`Self::claim`], closed at [`Self::mark_finished`].
    /// `node/steer` enqueues here (`handler/steer.rs`); no delivery port is attached yet, so a
    /// queued message waits and is dropped, journaled, when its node ends.
    inboxes: Arc<crate::inbox::Inboxes>,
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
    /// The operator's capability, from `<state>/operator.key` (in memory only, for a handle that
    /// serves no state root); `Err` names why there is none, and then no connection can speak for
    /// the operator. See [`session`].
    operator_key: Result<Secret, String>,
    /// **The races this supervisor is driving**; empty, and silent, with none open.
    races: crate::race::Races,
    /// Every workflow run this supervisor is stepping ([`crate::workflow`]).
    workflows: crate::workflow::Workflows,
    /// **What this supervisor sent each child it started**, for `node/get` to show while the child
    /// runs: its contract file is written when it ends, and until then nothing on disk holds the
    /// prompt. In memory only, like [`NodeHandle`]; after the run the contract is the record.
    sent: Mutex<HashMap<AgentId, marion_core::proto::result::TaskSent>>,
    /// **What each running node has spent so far**, published by its event sink as each frame
    /// moves it ([`crate::spending`]) — no file read and no timer. An ended node's figure is the
    /// journal's, which the registry already holds.
    spending: Arc<crate::spending::Spending>,
    /// **Every running node's spend against its budget** ([`crate::budget`]), fed by
    /// [`Self::spending`] as figures move; each line crossed is acted on by [`Self::budget_crossed`]
    /// on the one enforcer thread an owning handle starts.
    budgets: Arc<crate::budget::BudgetBook>,
    /// **Desktop notifications** ([`crate::notify`]), observed on each flush. `None` — the
    /// default — where the operator has not turned them on, and for a handle that runs no nodes.
    /// Replaced by `notify/configure` while the supervisor runs; always taken before `shared`.
    notifier: Mutex<Option<crate::notify::Notifier>>,
    /// What [`Self::notifier`] is built from, so `notify/configure` can rebuild it. `None` for a
    /// handle that runs no nodes.
    notify_seed: Option<crate::notify::NotifySeed>,
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
    ///
    /// It serves no socket of its own, so the socket its guidance names is the journal-adjacent
    /// default rather than one a listener bound.
    pub fn new(live: Arc<LiveRegistry>) -> Arc<RegistryHandle> {
        let socket_path = Self::journal_adjacent_socket(&live);
        Self::build(live, socket_path, Arc::new(SystemQuitRuntime), None, None)
    }

    /// A handle that **owns the nodes it spawns**: §2's `agent/spawn`, answered rather than refused.
    ///
    /// The environment is passed in rather than derived because it cannot be derived — see
    /// [`Self::spawn_env`]. So is `socket_path`, the socket the server in front of this handle
    /// actually bound: §2 moves a socket whose path would exceed `sun_path` to a `/tmp` fallback
    /// while the journal stays under the state root, so the journal cannot say where clients dial.
    pub fn owning(
        live: Arc<LiveRegistry>,
        env: crate::run::Env,
        socket_path: PathBuf,
    ) -> Arc<RegistryHandle> {
        let seed = notify_seed_for(&env.project_root);
        let handle = Self::build(
            live,
            socket_path,
            Arc::new(SystemQuitRuntime),
            Some(env),
            Some(seed),
        );
        handle.redrive_open_races();
        handle.redrive_open_workflows();
        handle
    }

    /// [`Self::owning`] with the notifier given rather than read from the operator's config — the
    /// tests' seam for a `Record` backend.
    #[cfg(test)]
    fn owning_notified(
        live: Arc<LiveRegistry>,
        env: crate::run::Env,
        socket_path: PathBuf,
        seed: crate::notify::NotifySeed,
    ) -> Arc<RegistryHandle> {
        let handle = Self::build(
            live,
            socket_path,
            Arc::new(SystemQuitRuntime),
            Some(env),
            Some(seed),
        );
        handle.redrive_open_races();
        handle.redrive_open_workflows();
        handle
    }

    fn journal_adjacent_socket(live: &LiveRegistry) -> PathBuf {
        live.read(|r| {
            r.path()
                .parent()
                .map(|p| p.join("supervisor.sock"))
                .unwrap_or_else(|| PathBuf::from("supervisor.sock"))
        })
    }

    fn build(
        live: Arc<LiveRegistry>,
        socket_path: PathBuf,
        runtime: Arc<dyn QuitRuntime>,
        spawn_env: Option<crate::run::Env>,
        notify_seed: Option<crate::notify::NotifySeed>,
    ) -> Arc<RegistryHandle> {
        let notifier = notify_seed
            .as_ref()
            .and_then(|s| s.notifier(s.config.enabled));
        // `new_cyclic` rather than a `Mutex<Option<Weak<_>>>` filled in afterwards: a node's thread
        // outlives the call that started it and has to hold the handle it reports to, so the
        // reference is a property of the value and not a step a construction site could forget.
        // The inbox's records go through the supervisor's one journal handle, as every other
        // record this handle writes does ([`Self::journal_append`]).
        let journal = live.read(|r| r.path().to_path_buf());
        let inboxes = Arc::new(crate::inbox::Inboxes::new(Box::new(move |kind| {
            crate::journal::append_at(&journal, kind)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })));
        let budgets = Arc::new(crate::budget::BudgetBook::default());
        // Only a handle that runs nodes enforces: a describing handle has nothing to cancel.
        let spending = crate::spending::Spending::notifying(live.changes());
        let (spending, crossed) = if spawn_env.is_some() {
            let (tx, rx) = std::sync::mpsc::channel();
            (
                Arc::new(spending.enforcing(Arc::clone(&budgets), tx)),
                Some(rx),
            )
        } else {
            (Arc::new(spending), None)
        };
        let operator_key = match &spawn_env {
            Some(env) => crate::operator_key::ensure(&env.state).map_err(|e| {
                format!(
                    "{} could not be read or created: {e}",
                    crate::operator_key::path(&env.state).display()
                )
            }),
            // A key that exists nowhere but in this process: no client can present it, and a test
            // in this crate can read it back.
            None => Ok(mint_token()),
        };
        Arc::new_cyclic(|me: &std::sync::Weak<RegistryHandle>| {
            if let Some(rx) = crossed {
                Self::enforce_budgets(me.clone(), rx);
            }
            RegistryHandle {
                me: me.clone(),
                live,
                socket_path,
                inboxes,
                shared: Mutex::new(Shared::default()),
                runtime,
                nodes: Mutex::new(HashMap::new()),
                ended: std::sync::Condvar::new(),
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
                operator_key,
                races: crate::race::Races::default(),
                workflows: crate::workflow::Workflows::default(),
                sent: Mutex::new(HashMap::new()),
                spending,
                budgets,
                notifier: Mutex::new(notifier),
                notify_seed,
                quit: Mutex::new(()),
                quit_waived_grace: AtomicBool::new(false),
                stopped_reported: AtomicBool::new(false),
                exiting: AtomicBool::new(false),
            }
        })
    }

    #[cfg(test)]
    fn with_runtime(live: Arc<LiveRegistry>, runtime: Arc<dyn QuitRuntime>) -> Arc<RegistryHandle> {
        let socket_path = Self::journal_adjacent_socket(&live);
        Self::build(live, socket_path, runtime, None, None)
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
        // Taken before `shared`, as `notify_claim` takes them.
        let notifier = lock(&self.notifier);
        let mut g = lock(&self.shared);
        // **Before the subscriber check**: a notice is for the operator, who may have no client
        // open at all. The notifier returns at once when the journal has not moved.
        if let Some(n) = notifier.as_ref() {
            let shown = self.live.read(|r| n.observe(r.tree(), r.generation()));
            if !shown.is_empty() && !n.deliver(shown.clone()) {
                notify_claimer(&mut g, n.ring(), &shown);
            }
        }
        // **Nobody to tell, or nothing new to tell them: no walk.** `collect` visits every node the
        // journal has ever recorded, and the accept loop flushes on every pass, so at 100k records
        // an idle supervisor spent half a core re-deriving an empty diff. With no subscriber the
        // diff has no audience, and `subscribe` runs `collect` itself before its snapshot, so the
        // told-set is caught up the moment one arrives. With an unchanged generation the tree is
        // the one already told — the pane set only shapes a *new* node's summary, and a new node
        // is a new generation. A running node's token figure moves without a journal record, so
        // the live spending map's own change counter is the other half of "unchanged".
        //
        // A figure that moved with no journal record is told at most every [`TOKEN_PUSH_DELAY`]:
        // within it the walk is deferred, and [`Self::next_deadline`] brings the loop back when it
        // ends. The spending map is marked announced before its version is read, so a change
        // landing during the walk wakes the loop again.
        let events = self.live.read(|r| {
            if g.subs.is_empty() {
                return Vec::new();
            }
            let generation = r.generation();
            if g.collected_at == Some((generation, self.spending.version())) {
                return Vec::new();
            }
            let figures_only = g.collected_at.is_some_and(|(seen, _)| seen == generation);
            if figures_only
                && g.collected_when
                    .is_some_and(|at| at.elapsed() < TOKEN_PUSH_DELAY)
            {
                return Vec::new();
            }
            self.spending.announced();
            g.collected_at = Some((generation, self.spending.version()));
            g.collected_when = Some(std::time::Instant::now());
            let events = collect(r, &mut g, &panes, &self.spending);
            // Told first, then forgotten: an ended node's row is the journal's from here on.
            self.spending.forget_ended(r.tree());
            events
        });
        deliver(&mut g, &events);
        events.len()
    }

    /// **When the accept loop must next tick this handle** if nothing wakes it — the serve loop's
    /// [`Handle::next_deadline`].
    ///
    /// Journal changes, connections, node completions, a followed node's `events.jsonl`
    /// ([`AttachWatch`]) and a moved token figure ([`crate::spending::Spending::notifying`]) wake
    /// the loop through [`LiveRegistry::changes`], so what is left is the work that is still
    /// *time*-driven: a figure deferred by [`TOKEN_PUSH_DELAY`] is due when it ends, and a
    /// Completed pane expires at its TTL or is evicted at once when the cache is over budget.
    /// `None` means nothing is due. An attachment whose watch thread could not be started is
    /// re-read at [`crate::wake::DEGRADED_RECHECK`].
    pub fn next_deadline(&self) -> Option<std::time::Instant> {
        let figure_due = {
            let g = lock(&self.shared);
            if g.attached.iter().any(|a| a.watch.is_none()) {
                return Some(std::time::Instant::now() + crate::wake::DEGRADED_RECHECK);
            }
            let untold = g
                .collected_at
                .is_some_and(|(_, told)| told != self.spending.version());
            (!g.subs.is_empty() && untold).then(|| {
                g.collected_when
                    .map_or_else(std::time::Instant::now, |at| at + TOKEN_PUSH_DELAY)
            })
        };
        let panes = lock(&self.panes);
        if panes.completed_count > 0
            && (panes.completed_count > panes.completed_limit()
                || panes.completed_bytes > Panes::MAX_COMPLETED_BYTES)
        {
            return Some(std::time::Instant::now());
        }
        match (figure_due, panes.next_completed_expiry) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// §2's `tree/subscribe`: the snapshot, and the point live notifications begin from.
    /// `notify/claim`: queue this connection to show desktop notices in its terminal, where
    /// notices are on and reach a terminal at all.
    fn notify_claim(&self, out: &Outbound) -> marion_core::proto::result::NotifyClaimResult {
        let terminal = lock(&self.notifier)
            .as_ref()
            .is_some_and(|n| *n.backend() == crate::notify::Backend::Terminal);
        let mut g = lock(&self.shared);
        if terminal && !g.claimers.iter().any(|c| c.conn() == out.conn()) {
            g.claimers.push(out.clone());
        }
        marion_core::proto::result::NotifyClaimResult {
            terminal,
            head: terminal && g.claimers.first().is_some_and(|c| c.conn() == out.conn()),
        }
    }

    /// `notify/configure`: turn this supervisor's desktop notices on or off now — `marion notify
    /// on|off` has already written the file a later start reads. The operator's own call: a peer
    /// running as another user is refused.
    fn notify_configure(
        &self,
        p: &marion_core::proto::params::NotifyConfigureParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::NotifyConfigureResult, RpcError> {
        match peer {
            Peer::Uid(uid) if uid == crate::socket::own_uid() => {}
            _ => {
                return Err(RpcError::refused(
                    "enabled",
                    "only a client running as this supervisor's own user may turn its \
                     notifications on or off.",
                    "§2",
                ));
            }
        }
        let Some(seed) = &self.notify_seed else {
            return Err(RpcError::unimplemented(
                "notify/configure",
                "this handle runs no nodes, so it has no notifications to turn on or off.",
                "§2",
            ));
        };
        let next = seed.notifier(p.enabled);
        let enabled = next.is_some();
        *lock(&self.notifier) = next;
        Ok(marion_core::proto::result::NotifyConfigureResult { enabled })
    }

    fn subscribe(&self, out: &Outbound) -> TreeSubscribeResult {
        let panes = self.pane_ids();
        let mut g = lock(&self.shared);
        // **One read, three uses.** See the module doc: catching up existing subscribers, building
        // this one's snapshot, and recording what it has been told all happen against the same view,
        // under one lock, so there is no instant at which a notification could slip between the
        // snapshot and the subscription.
        let (events, nodes, read_point) = self.live.read(|r| {
            self.spending.announced();
            g.collected_at = Some((r.generation(), self.spending.version()));
            g.collected_when = Some(std::time::Instant::now());
            let events = collect(r, &mut g, &panes, &self.spending);
            let nodes = project(r.tree(), &mut g, &panes, &self.spending);
            (events, nodes, r.read_point())
        });
        deliver(&mut g, &events);
        g.subs.push(out.clone());
        TreeSubscribeResult { nodes, read_point }
    }

    fn node_get(
        &self,
        id: &AgentId,
        cursor: Option<marion_core::proto::params::ActivityCursor>,
    ) -> Result<NodeGetResult, RpcError> {
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
                .map(|summary| (summary, crate::node_detail::inputs(node), r.project()))
                .map_err(|e| e.as_error(id)),
        })
        .map(|(node, inputs, project)| {
            // The detail's file reads happen here, after the registry lock is released: a long
            // stream or a slow disk must never hold up the tree for everyone else.
            let mut detail: marion_core::proto::result::NodeDetail = match (inputs, project) {
                (Some(i), Some(p)) => {
                    crate::node_detail::read(&p, id, &i, cursor, self.spending.get(id).as_ref())
                }
                _ => Default::default(),
            };
            let mut node = node;
            node.tokens = detail.usage.map(|u| u.total());
            if detail.task.is_none() {
                detail.task = lock(&self.sent).get(id).cloned();
            }
            NodeGetResult { node, detail }
        })
    }

    // ----------------------------------------------------------------------------------------
    // §2's `agent/spawn`, and the node table it fills.
    // ----------------------------------------------------------------------------------------

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
    /// **The token check alone**: the caller's repository if this supervisor minted `c`'s token
    /// for the node it names, `None` otherwise. Shared by every call a node makes as itself, so
    /// `agent/spawn` and `node/steer` cannot come to check a claim two ways.
    fn authenticate(&self, c: &marion_core::proto::SpawnCaller) -> Option<PathBuf> {
        let nodes = lock(&self.nodes);
        // **A node this supervisor does not own is still compared**, against a decoy of the
        // same shape, so "no such node" and "wrong token" take the same path and cost the same.
        // Returning early on the absent case would turn the *existence* of a node into an
        // oracle a caller could probe with ids alone.
        let (stored, repo) = match nodes.get(&c.agent_id) {
            Some(n) => (n.token.clone(), Some(n.repo.clone())),
            None => (decoy_token().clone(), None),
        };
        if (stored == c.node_token) & repo.is_some() {
            repo
        } else {
            None
        }
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
}

impl Handle for RegistryHandle {
    fn connected(&self, conn: ConnId) {
        lock(&self.shared).clients.insert(conn);
    }

    fn claimed_by_native(&self, conn: ConnId, agent: &AgentId) {
        lock(&self.shared).principals.insert(
            conn,
            marion_core::proto::result::SessionPrincipal::Node(agent.clone()),
        );
    }

    fn call(&self, conn: ConnId, call: &Call, out: &Outbound) -> Result<MethodResult, RpcError> {
        if let Call::SessionHello(p) = call {
            return self
                .session_hello(conn, p, out.peer_pid())
                .map(MethodResult::SessionHello);
        }
        self.authorize(conn, call)?;
        match call {
            Call::NodeGet(p) => self
                .node_get(&p.agent_id, p.activity)
                .map(MethodResult::NodeGet),
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
            Call::NotifyClaim(_) => Ok(MethodResult::NotifyClaim(self.notify_claim(out))),
            Call::NotifyConfigure(p) => self
                .notify_configure(p, out.peer())
                .map(MethodResult::NotifyConfigure),
            Call::WorkflowRun(p) => self
                .workflow_run(p, out.peer())
                .map(MethodResult::WorkflowRun),
            Call::WorkflowCancel(p) => self
                .workflow_cancel(p, out.peer())
                .map(MethodResult::WorkflowCancel),
            Call::SessionQuit(p) => self
                .session_quit(&p.disposition)
                .map(MethodResult::SessionQuit),
            Call::NodeResume(p) => self
                .node_resume(p, out.peer())
                .map(MethodResult::NodeResume),
            Call::NodeSteer(p) => self.node_steer(p, out.peer()).map(MethodResult::NodeSteer),
            Call::NodeCollected(p) => self.node_collected(p).map(MethodResult::NodeCollected),
            Call::NodeKill(p) => self.node_kill(p, out.peer()).map(MethodResult::NodeKill),
            Call::NodeCancel(p) => self
                .node_cancel(p, out.peer())
                .map(MethodResult::NodeCancel),
            Call::NodePrompt(_) => Err(RpcError::unimplemented(
                "node/prompt",
                "`node/prompt` is not built. A message for a node's next turn is `node/steer`, which \
                 queues it for the node's next turn boundary whatever the node is doing; a \
                 separate prompt verb for an idle node lands with the delivery lanes that take \
                 turns (§6.3).",
                "§2, §6.3",
            )),
            // Everything else is specified and not built. `Unimplemented` and not `Unsupported`,
            // per `error.rs`: the gap is marion's, not the harness's, and the operator's next move
            // is to check the milestone rather than the node.
            other => Err(RpcError::unimplemented(
                other.method().as_str(),
                format!(
                    "`{}` is specified (§2) and not built. This supervisor answers `node/get`, \
                     `tree/subscribe`, `node/attach`, `agent/spawn`, `session/quit`, \
                     `node/resume`, `node/steer`, `node/collected`, `node/cancel` and `node/kill`; \
                     the remaining methods land with the milestone that needs them.",
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
        // The next claimer in line, if any, is the head now.
        g.claimers.retain(|c| c.conn() != conn);
        // A node stream this connection was following. Dropping the cursor is the whole of it:
        // §7.3.1's invariant is about nodes, and a reader is not one. The node goes on running and
        // goes on writing its `events.jsonl`, which is what makes the *next* client's attach a
        // replay rather than a hole.
        g.attached.retain(|a| a.conn != conn);
        g.clients.remove(&conn);
        g.principals.remove(&conn);
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

    /// §2's notifications, driven by the accept loop's passes — see [`Handle::tick`] for why
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

    fn next_deadline(&self) -> Option<std::time::Instant> {
        RegistryHandle::next_deadline(self)
    }

    fn changes(&self) -> Option<Arc<crate::wake::Signal>> {
        Some(self.live.changes())
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

/// A poisoned lock is taken, not unwrapped — `registry.rs`'s rule and §5.7's requirement.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests;
