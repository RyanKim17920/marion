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
use marion_core::proto::result::{NodeGetResult, SessionQuitResult, TreeSubscribeResult};
use marion_core::proto::{
    Call, ClientGone, DetachGuidance, FailureKind, KilledNode, MethodResult, NativeLaunchContext,
    QuitDisposition, QuitOutcome, ReplayPoint, ResidentReason, RpcError, SpawnCaller,
    SupervisorDisposition,
};
use marion_core::registry::ReplayedNode;
use marion_core::secret::Secret;

use crate::native_binding::{NativeBindingError, refuse_untrusted_native_launch};
use crate::registry::LiveRegistry;
use crate::serve::{ConnId, Departure, Handle, Outbound, Peer};
use panes::{Attachment, PaneEntry, Panes};
use summary::{Told, collect, project};

pub use summary::{Unprojectable, summarize};
pub(crate) use summary::{clock, summarize_spent};

#[cfg(test)]
use crate::registry::Registry;
#[cfg(test)]
use marion_core::proto::AttachMode;
#[cfg(test)]
use marion_core::registry::Replay;
#[cfg(test)]
use panes::attach_mode;
#[cfg(test)]
use summary::Extra;

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
pub(crate) fn mint_token() -> Secret {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)) {
        Ok(()) => Secret::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        // **A token nothing can present, rather than one anything can guess.** The empty string
        // never matches: `Secret`'s equality compares lengths first, and every real `SpawnCaller` must
        // carry a non-empty `node_token` to deserialize at all. So the node runs and cannot spawn,
        // which is the safe direction for a machine whose entropy source is unreadable.
        Err(_) => Secret::new(String::new()),
    }
}

/// **A fixed decoy of a real token's shape**, so a `SpawnCaller` naming a node this supervisor does
/// not own takes the same comparison path as one naming a node it does. Minted once per process and
/// never written anywhere, so it matches nothing.
fn decoy_token() -> &'static Secret {
    static DECOY: std::sync::OnceLock<Secret> = std::sync::OnceLock::new();
    DECOY.get_or_init(mint_token)
}

/// What a node's own thread tells the call that started it. Three moments, in this order.
enum Progress {
    /// The node has an identity and its `SpawnIntent` is durable.
    Identified(AgentId),
    /// A process exists and `Spawned { pid: Some(_) }` is journaled.
    Started,
    /// `run_spawn` refused before the node had an identity, in its own words — a containment
    /// refusal says how to allow it, and a generic sentence would lose that.
    Refused(String),
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
    /// The parent this child's end is owed to as a message (a background `spawn`,
    /// `AgentSpawnParams::notify_parent`), or `None`.
    announce_to: Option<AgentId>,
    /// The parent's inbox recorded the debt at claim ([`crate::inbox::Inboxes::owe`]), so the end
    /// must settle it — and only then, or it would settle another child's.
    owes: std::sync::atomic::AtomicBool,
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
    /// A worktree child's own children branch from its worktree, so its entry's tree becomes that
    /// worktree. A `shared-cwd` child runs in the tree it inherited, which its entry already names.
    fn workspace_chosen(&self, agent_id: &AgentId, workspace: &marion_core::contract::Workspace) {
        if let marion_core::contract::Workspace::Worktree { path, .. } = workspace
            && let Some(node) = lock(&self.handle.nodes).get_mut(agent_id)
        {
            node.repo = path.clone();
        }
    }

    fn identified(&self, agent_id: &AgentId) -> Option<Secret> {
        let token = self
            .handle
            .claim(agent_id, self.task_id.clone(), self.repo.clone());
        *lock(&self.identified) = Some(agent_id.clone());
        // Held open from here: the parent's driver must not end it while this child runs.
        if let Some(parent) = &self.announce_to {
            self.owes.store(
                self.handle.inboxes.owe(parent),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
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

    fn spending(&self) -> Option<Arc<crate::spending::Spending>> {
        Some(self.handle.spending.clone())
    }

    fn started(&self, agent_id: &AgentId, pid: i32) {
        self.handle.mark_started(agent_id, pid);
        let _ = self.tx.send(Progress::Started);
    }

    /// The node's own inbox, opened at [`RegistryHandle::claim`] a moment before this is asked.
    fn turn_source(&self, agent_id: &AgentId) -> Option<Arc<dyn crate::inbox::TurnSource>> {
        Some(Arc::new(crate::inbox::BoundInbox::new(
            Arc::clone(&self.handle.inboxes),
            agent_id.clone(),
        )))
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

    /// [`RegistryHandle::process_ended`]: this supervisor is the one a `node/kill` reaches.
    fn process_ended(&self, agent_id: &AgentId) -> bool {
        self.handle.process_ended(agent_id)
    }

    fn ended_by(&self, agent_id: &AgentId) -> Option<marion_core::journal::CancelBy> {
        self.handle.ended_by(agent_id)
    }
}

/// **The uid half of what authorizes an operator-only call** (a root, a resume, an operator's
/// steer or kill).
///
/// *Who the operator is* is decided first and elsewhere: the connection's `session/hello` presented
/// the operator's key ([`session`]). Open question 3 once answered "filesystem permission and
/// deliberately not a token", on the argument that a key at rest is readable by every same-uid
/// process that can reach the socket. The 2026-09-29 audit found that answer made every node's own
/// shell the operator; a key marion never hands a node, refused besides from any process descending
/// from a node, excludes exactly the processes the uid cannot. This check stays behind it.
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
/// What [`RegistryHandle::reviewed`] read about the node a review names.
struct Reviewed {
    review: crate::review::Target,
    contract: marion_core::contract::TaskContract,
    intent: SpawnIntent,
    model: Option<String>,
}

/// **§2's one-socket-one-project rule for a spawn that names its own `repo`** — a root, or an
/// operator's review: the tree named must key to the project this supervisor serves.
fn check_project(env: &crate::run::Env, repo: &Path) -> Result<(), RpcError> {
    let named = marion_core::paths::ProjectDir::new(&env.state, &crate::socket::project_root(repo));
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
    Ok(())
}

/// A fresh contract id. Minted by marion rather than by the caller, for §9's reason: the id names a
/// run marion performed, and a caller-chosen one would let two runs share a contract file.
fn mint_task_id() -> Result<TaskId, RpcError> {
    crate::run::entropy()
        .map(|e| marion_core::contract::new_task_id(crate::run::unix_millis(), e))
        .map_err(|e| {
            RpcError::internal(format!(
                "marion could not mint a task id for this spawn, so nothing was started: {e}"
            ))
        })
}

/// A child's launch request from a spawn's terms, for one agent type and model: a plain spawn's
/// own, or one seat's of a race.
fn child_request(
    p: &marion_core::proto::params::AgentSpawnParams,
    repo: PathBuf,
    agent_type: String,
    model: Option<String>,
    race: Option<marion_core::race::RaceSeat>,
    budget: Option<marion_core::budget::Budget>,
    timeout_secs: u64,
) -> crate::run::SpawnRequest {
    crate::run::SpawnRequest {
        budget,
        review: None,
        agent_type,
        prompt: p.prompt.clone(),
        repo,
        acceptance_criteria: p.acceptance_criteria.clone(),
        verification: p.verification.clone(),
        race,
        writable_scope: p.writable_scope.clone(),
        // Resolved by the caller from `p.timeout_secs` and the clock above it; `effective_timeout`
        // clamps it exactly as it clamps a stated one.
        timeout_secs,
        model,
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
        profile: None,
        read_only: false,
        workflow: None,
    }
}

/// The task as the child will receive it, resolved the way `run_spawn_watched` resolves it,
/// before the request moves into the launch.
fn sent_task(req: &crate::run::SpawnRequest) -> Option<marion_core::proto::result::TaskSent> {
    let prompt = crate::run::agent_types(&req.repo)
        .ok()
        .and_then(|t| t.resolve(&req.agent_type))
        .map(|t| crate::run::child_prompt(&t, &req.prompt))?;
    Some(crate::node_detail::task_of(
        &prompt,
        req.acceptance_criteria.clone(),
        req.verification.clone(),
    ))
}

/// §5.4's refusal of a node token this supervisor holds no binding for — at `session/hello` and at
/// every call that re-proves a `caller`.
fn unminted_token(agent: &AgentId) -> RpcError {
    RpcError::refused(
        &agent.0,
        "this supervisor did not mint that node token, so it cannot tell the caller it claims to \
         be from any other process that can reach this socket. §5.4 binds a capability token to an \
         `AgentId`; the supervisor holds the binding and there is no round trip that could \
         establish one for a node it does not own. A node whose supervisor has restarted is in \
         this case and it is not a mistake the caller made — its parent is `Orphaned` (§7.2) and \
         needs an operator, not a retry.",
        "§5.4, §6.1",
    )
}

/// **A spawn with no `caller` that asks for a worktree**: the operator asking for a contracted node
/// rather than starting a root in their checkout. A race with no caller is one too, seat by seat.
fn operator_contracted(p: &marion_core::proto::params::AgentSpawnParams) -> bool {
    p.caller.is_none() && p.isolation == Some(Isolation::Worktree)
}

/// What a contracted node the operator asked for cannot carry: a root's own fields. It runs in a
/// worktree under a contract, so there is no checkout to snapshot, no terminal for an operator to
/// drive (a TUI takes no turn and would only time out), and no native session to adopt.
fn check_operator_contracted(
    p: &marion_core::proto::params::AgentSpawnParams,
) -> Result<(), RpcError> {
    let root_only = [
        ("no_change_record", p.no_change_record.is_some()),
        ("pane", p.pane.is_some()),
        ("native_launch", p.native_launch.is_some()),
        (
            "allow_concurrent_writes",
            p.allow_concurrent_writes.is_some(),
        ),
    ];
    match root_only.into_iter().find(|(_, stated)| *stated) {
        Some((field, _)) => Err(RpcError::refused(
            field,
            format!(
                "a spawn with no `caller` and a worktree is a contracted node the operator asked \
                 for: it runs in its own worktree under a contract, and `{field}` belongs to a \
                 root in the operator's checkout. Refused rather than dropped (§11 item 23)."
            ),
            "§6.6, §9, §11 item 23",
        )),
        None => Ok(()),
    }
}

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
                 it means the permissions are not what marion set."
            ),
            "§2, §5.4",
        )),
        Peer::Unknown => Err(RpcError::refused(
            "caller",
            "marion could not read this connection's peer credentials, so it cannot establish that \
             the caller is this supervisor's own user — and a check that could not be made is not \
             a check that passed.",
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
///
/// With `project`, the node's own recorded table ([`crate::types_snapshot`]) where it has one, so a
/// resumed child is the type its tree started with, not whatever its parent's worktree now says.
fn recorded_type(
    project: Option<&marion_core::paths::ProjectDir>,
    repo: &Path,
    intent: &SpawnIntent,
) -> Result<marion_core::agent_type::AgentType, RpcError> {
    let resolved = match project {
        Some(project) => {
            crate::types_snapshot::for_caller(project, &intent.agent_id, repo, &intent.agent_type)
                .and_then(|s| s.resolve(&intent.agent_type))
                .map_err(|e| RpcError::refused("agent_type", e.to_string(), "§3.1"))?
        }
        None => tree_types(repo)?.resolve(&intent.agent_type),
    };
    let ty = resolved.ok_or_else(|| {
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
    /// Kill whatever is left in a group whose leader ended on its own. A no-op by default, for a
    /// runtime that signals nothing.
    fn sweep_group(&self, _pgid: i32) {}
}

struct SystemQuitRuntime;

impl QuitRuntime for SystemQuitRuntime {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
        crate::kill::kill_process_tree_and_wait(pid)
    }

    fn sweep_group(&self, pgid: i32) {
        crate::kill::sweep_group(pgid);
    }
}

/// `node/steer`: authority, the node's delivery strategy, and the inbox. See its module docs.
mod session;
mod steer;

/// `node/cancel`: freeze a subtree, abort it bottom-up, kill what outlives its grace.
mod cancel;

/// Token budgets: register each node's, and act on a line its spend crosses.
mod budget;
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
/// How long a harness's session listing may take before the resume it serves is refused.
const SESSION_LISTING_BOUND: std::time::Duration = std::time::Duration::from_secs(20);

/// **The workspace a child ran in, from the directory its harness says its session was created in**,
/// for a child whose stream never named a session and so never journaled one. A directory that is
/// the worktree marion cuts for this node is that worktree, on the branch marion cuts for its task;
/// any other is the caller's own directory it shared. `None` for a child with no task.
fn listed_workspace(
    node: &ReplayedNode,
    env: &crate::run::Env,
    dir: &Path,
) -> Option<marion_core::contract::Workspace> {
    use marion_core::contract::Workspace;
    let task = node.intent.as_ref()?.task_id.as_ref()?;
    let worktree = env.project_dir.agent(&node.agent_id).worktree();
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    Some(if same(dir, &worktree) {
        // The branch it has checked out, whichever way it was named; a tree too far gone to say
        // is on the name its task's branch had before names were readable.
        let branch = crate::spawn::checked_out_branch(crate::run::tree_of(env, &worktree))
            .unwrap_or_else(|| crate::run::legacy_worktree_branch(task));
        Workspace::Worktree {
            path: worktree,
            branch,
        }
    } else {
        Workspace::SharedCwd {
            path: dir.to_path_buf(),
        }
    })
}

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
        os_sandbox: env.os_sandbox,
        wider_children: p.wider_children.unwrap_or(false),
        // A root has no ancestor to narrow it: its type's budget and the tree limit it was run with.
        budget: marion_core::budget::resolve(agent_type.budget, p.budget_tokens, None),
        agent_type: p.agent_type.clone(),
        prompt: p.prompt.clone(),
        native_launch: p.native_launch.as_deref().cloned(),
        repo,
        state: env.state.clone(),
        base_url: env.base_url.clone(),
        bridge: env.bridge.clone(),
        // §3.1's precedence, the same one a child's spawn gets: the stated model, else the agent
        // type's own `model` key — dropped on a live run when it names marion's canned plumbing
        // (`AgentType::default_model`).
        model: p
            .model
            .clone()
            .or_else(|| agent_type.default_model(env.auth == marion_harness::Auth::Canned)),
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
        // The operator's choice of login for this root, where the client stated one.
        profile: p.profile.clone(),
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
    ) -> Secret {
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
                ending: Ending::Running,
                process_gone: false,
                escalated: false,
            },
        );
        self.inboxes.open(agent_id);
        token
    }

    /// **A kill's half of [`Ending`]**: claim the node's end before signalling it. `false` means
    /// the node's own thread already saw its process end and is recording that exit, so there is
    /// nothing left to signal. A node this supervisor does not own has no thread to race and is
    /// always claimable — its end is the kill's to record.
    fn claim_kill(&self, agent_id: &AgentId) -> bool {
        let mut nodes = lock(&self.nodes);
        let Some(node) = nodes.get_mut(agent_id) else {
            return true;
        };
        if node.ending == Ending::ProcessEnded || !node.running() {
            return false;
        }
        // A kill of a node already cancelling is the cancel's escalation: the cancel keeps its
        // attribution, and the kill signals now rather than after the grace.
        if !matches!(node.ending, Ending::CancelRequested(_)) {
            node.ending = Ending::KillRequested;
        }
        true
    }

    /// **A cancel's half of [`Ending`]**, the graceful sibling of [`Self::claim_kill`]: claim the
    /// node's end for `by` before anything is written to it. `false` for a node whose thread
    /// already saw its process end, one a kill or an earlier cancel already claimed, and one this
    /// supervisor does not own — which has no driver to abort, and is the kill's.
    fn claim_cancel(&self, agent_id: &AgentId, by: &marion_core::journal::CancelBy) -> bool {
        let mut nodes = lock(&self.nodes);
        match nodes.get_mut(agent_id) {
            Some(node) if node.running() && node.ending == Ending::Running => {
                node.ending = Ending::CancelRequested(by.clone());
                true
            }
            _ => false,
        }
    }

    /// A journal failure after [`Self::claim_cancel`]: hand the node's end back, so its thread
    /// records its own exit rather than wait for a cancel that will never confirm it.
    fn unclaim_cancel(&self, agent_id: &AgentId) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id)
            && matches!(node.ending, Ending::CancelRequested(_))
        {
            node.ending = Ending::Running;
        }
    }

    /// The node's thread has seen its process end.
    fn process_gone(&self, agent_id: &AgentId) -> bool {
        lock(&self.nodes)
            .get(agent_id)
            .is_some_and(|n| n.process_gone)
    }

    /// **`node/kill` on a node being cancelled**: tell the cancel to stop waiting out the grace.
    fn escalate(&self, agent_id: &AgentId) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.escalated = true;
        }
        self.ended.notify_all();
    }

    /// Wait until every node in `ids` has finished its thread, or `deadline`.
    fn wait_finished(&self, ids: &[AgentId], deadline: std::time::Instant) {
        let mut nodes = lock(&self.nodes);
        loop {
            let pending = ids
                .iter()
                .any(|id| nodes.get(id).is_some_and(NodeHandle::running));
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            if !pending || wait.is_zero() {
                return;
            }
            nodes = self
                .ended
                .wait_timeout(nodes, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Whether the node is being cancelled — what `agent/spawn` and `node/steer` refuse a
    /// cancelling node for.
    pub(crate) fn cancelling(&self, agent_id: &AgentId) -> bool {
        lock(&self.nodes)
            .get(agent_id)
            .is_some_and(|n| matches!(n.ending, Ending::CancelRequested(_)))
    }

    /// **A node being cancelled spawns nothing** — no child and no race seat. Its subtree was
    /// frozen under [`Self::spawn_decision`], which the caller holds, and a child started now would
    /// outlive the cancel that was meant to end it.
    fn refuse_if_cancelling(&self, caller_id: &SpawnCaller) -> Result<(), RpcError> {
        if self.cancelling(&caller_id.agent_id) {
            return Err(RpcError::refused(
                "caller",
                "the calling node is being cancelled, so it may not spawn: its turn is being ended \
                 and everything below it with it.",
                "§6.7",
            ));
        }
        Ok(())
    }

    /// **Who ended the node, if marion did**, settling nothing: see
    /// [`crate::run::SpawnObserver::ended_by`]. A kill is always the operator's — only an operator
    /// may call `node/kill` or confirm a KillTree.
    pub(crate) fn ended_by(&self, agent_id: &AgentId) -> Option<marion_core::journal::CancelBy> {
        match &lock(&self.nodes).get(agent_id)?.ending {
            Ending::KillRequested => Some(marion_core::journal::CancelBy::Operator),
            Ending::CancelRequested(by) => Some(by.clone()),
            Ending::Running | Ending::ProcessEnded => None,
        }
    }

    /// Wait until every node in `ids` this supervisor owns has seen its process end, or
    /// `deadline`, or any node in `watch` was escalated — on [`Self::ended`], never on a timer.
    /// Answers the ones still running.
    fn wait_processes_gone(
        &self,
        ids: &[AgentId],
        deadline: std::time::Instant,
        watch: &[AgentId],
    ) -> Vec<AgentId> {
        let mut nodes = lock(&self.nodes);
        loop {
            let left: Vec<AgentId> = ids
                .iter()
                .filter(|id| {
                    nodes
                        .get(*id)
                        .is_some_and(|n| !n.process_gone && n.running())
                })
                .cloned()
                .collect();
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            let escalated = watch
                .iter()
                .any(|id| nodes.get(id).is_some_and(|n| n.escalated));
            if left.is_empty() || wait.is_zero() || escalated {
                return left;
            }
            nodes = self
                .ended
                .wait_timeout(nodes, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// **A node thread's half of [`Ending`]**, asked the instant its process has ended and before
    /// anything terminal is written: `true` means marion killed it on request, so the thread
    /// records the cancellation and leaves the terminal record to the kill's `KillConfirmed`.
    /// `false` settles the other way — the exit is the thread's to record, and a kill arriving now
    /// is refused rather than signalling a reaped pid.
    pub(crate) fn process_ended(&self, agent_id: &AgentId) -> bool {
        let mut nodes = lock(&self.nodes);
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        node.process_gone = true;
        self.ended.notify_all();
        match node.ending {
            Ending::KillRequested | Ending::CancelRequested(_) => true,
            Ending::Running | Ending::ProcessEnded => {
                node.ending = Ending::ProcessEnded;
                false
            }
        }
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
        self.register_budget(agent_id);
    }

    /// `run_spawn` returned. **The entry is kept, not removed**: it is what tells a later caller
    /// "that node finished" from "no such node", and §5.7's exit predicate reads liveness off it
    /// rather than membership.
    fn mark_finished(&self, agent_id: &AgentId, outcome: NodeOutcome) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.outcome = Some(outcome);
        }
        self.ended.notify_all();
        // §5.7's second guard just moved; nothing else would wake the accept loop to re-read it.
        self.live.changes().notify();
        // Sealed with the node, and every message still waiting is dropped by name: no later turn
        // of this process will take it.
        self.inboxes.close(
            agent_id,
            "the node ended before its next turn took the message",
        );
        // A requester that ends abandons the races it asked for.
        for race_id in self.races.requested_by(agent_id) {
            self.drive_race(&race_id);
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

    fn resolve_caller(
        &self,
        c: &marion_core::proto::SpawnCaller,
    ) -> Result<crate::run::Caller, RpcError> {
        // **The token is checked before anything else is said about the node**, so a caller who
        // guesses an `AgentId` learns nothing from the shape of the refusal beyond "no".
        // The caller's repository rides out with the answer: it is the tree whose
        // `.marion/agents.toml` defines the caller's own type, read below, outside the registry lock.
        let repo = self.authenticate(c);
        let Some(repo) = repo else {
            return Err(unminted_token(&c.agent_id));
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
        let agent_type = match self.spawn_env.as_ref() {
            // The table the caller's tree started with, bounded by its root's `max_depth`: never
            // the caller's worktree, which it can edit (see `crate::types_snapshot`).
            Some(env) => {
                crate::types_snapshot::for_caller(&env.project_dir, &c.agent_id, &repo, &type_name)
                    .and_then(|s| s.resolve(&type_name))
                    .map_err(|e| RpcError::refused("agent_type", e.to_string(), "§3.1"))?
            }
            None => tree_types(&repo)?.resolve(&type_name),
        }
        .ok_or_else(|| Unprojectable::UnknownAgentType(type_name.clone()).as_error(&c.agent_id))?;
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
            && !operator_contracted(p)
            && p.candidates.is_empty()
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
        if p.caller.is_none() && p.allow_concurrent_writes.is_some() && !operator_contracted(p) {
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
    /// **A root has no contract to check**: `verification` or `writable_scope` on a spawn with no
    /// `caller`.
    ///
    /// Both are checks a contract carries: a child's verification runs over its worktree and its
    /// scope judges that worktree's diff. §9 gives a root no contract, so a root stating them would
    /// run in the operator's checkout with nothing checked while the caller believed it had asked
    /// for checks — the silent drop a live `marion mcp` session hit. Refused by name.
    ///
    /// `acceptance_criteria` is not refused: nothing checks it on a child either (it is the
    /// caller's statement of done, which the child reports against), and `spawn`'s schema requires
    /// it on every call, so refusing it would refuse every well-formed top-level spawn.
    fn check_root_contract_fields(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        // A child's, and a contracted node's the operator asked for: both run under a contract.
        if p.caller.is_some() || operator_contracted(p) || !p.candidates.is_empty() {
            return Ok(());
        }
        let stated = [
            ("verification", p.verification.is_empty()),
            ("writable_scope", p.writable_scope.is_empty()),
        ];
        let Some((field, _)) = stated.into_iter().find(|(_, empty)| !empty) else {
            return Ok(());
        };
        Err(RpcError::refused(
            field,
            format!(
                "a spawn without a `caller` creates a **root**, and §9 gives a root no contract: it \
                 runs in the operator's own checkout, and nothing would check `{field}` against \
                 it. Refused rather than dropped, because a caller told nothing would believe its \
                 checks ran (§11 item 23). State it on a child spawn, whose worktree and contract \
                 are what it is checked against."
            ),
            "§6.7, §9, §11 item 23",
        ))
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
    /// **The opt-in to wider children is the operator's, for a whole tree**: a spawn with a
    /// `caller` — a node asking for its child — must not state `wider_children`, or a node could
    /// lift its own limits by asking.
    fn check_child_wider(p: &marion_core::proto::params::AgentSpawnParams) -> Result<(), RpcError> {
        if p.caller.is_some() && p.wider_children.is_some() {
            return Err(RpcError::refused(
                "wider_children",
                "only the operator can let an agent start agents with more authority than its own, \
                 for a whole run: `marion run --allow-wider-children`, or `[delegation] \
                 allow_wider_children = true` in ~/.config/marion/config.toml. An agent cannot \
                 ask for it.",
                "delegation",
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
        Self::check_root_contract_fields(p)?;
        Self::check_child_no_change_record(p)?;
        Self::check_child_pane(p)?;
        Self::check_child_wider(p)?;
        crate::profiles::check_child_profile(p)?;
        crate::trust::check_child_spawn(p)?;
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
        let racing = !p.candidates.is_empty() || p.race.is_some();
        if racing && p.review_of.is_some() {
            return Err(RpcError::refused(
                "candidates",
                "a review reads one finished node and a race runs one task on several; name \
                 either `review_of` or `candidates`, not both.",
                "race",
            ));
        }
        if let Some(target) = &p.review_of {
            return self.spawn_review(me, env, p, target, peer);
        }
        let Some(caller_id) = p.caller.as_ref() else {
            // **The operator asking for a contracted node** — a race's seats, or one node in a
            // worktree: the child path, with the operator as its requester. Authorized as a root
            // is, by the connection, and for the tree the call names.
            if racing || operator_contracted(p) {
                root_spawn_authorized(peer)?;
                check_operator_contracted(p)?;
                let repo = p.repo.clone().ok_or_else(|| {
                    RpcError::internal("a spawn with no caller reached the launcher with no repo")
                })?;
                check_project(&env, &repo)?;
                return if racing {
                    self.spawn_race(me, env, p, None, repo, None)
                } else {
                    self.spawn_contracted(me, env, p, repo)
                };
            }
            // **A client creating a root** — §11 item 28 step 6. Answered on its own path: a root
            // is not a contracted node with no parent but the operator's own session, and
            // `root::prepare` owns everything that makes it one — its agent-dir layout,
            // `ROOT_DEPTH`, §9's change record over the operator's checkout, no contract.
            return self.spawn_root(me, env, p, peer);
        };
        if racing {
            let repo = self.caller_repo(caller_id)?;
            return self.spawn_race(me, env, p, Some(caller_id), repo, None);
        }

        // **Held from here to the child's durable intent, and no further.** See
        // [`Self::spawn_decision`]: the registry is a follower, so two callers that both evaluated
        // the gate before either wrote its intent would both pass a bound only one fits under.
        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        let caller = self.resolve_caller(caller_id)?;
        self.refuse_if_cancelling(caller_id)?;
        let repo = self.caller_repo(caller_id)?;
        // §6.1 step 2, before every side effect — the same pure function `run_spawn` calls, run
        // here as well so the refusal arrives in the frame that asked for it rather than as a node
        // that was never going to start. Two call sites of one function, never two rules.
        agent_type::check_spawn_gates(&caller.agent_type, caller.depth, caller.live_children)
            .map_err(gate_refusal)?;

        let task_id = mint_task_id()?;
        let budget = self.child_budget(
            &repo,
            &p.agent_type,
            p.budget_tokens,
            Some(&caller_id.agent_id),
        );
        // Never past what is left of the clock above it: a child cannot outlive its parent.
        let wall = self.child_wall_secs(
            p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS),
            &caller_id.agent_id,
        )?;
        let req = child_request(
            p,
            repo.clone(),
            p.agent_type.clone(),
            p.model.clone(),
            None,
            budget,
            wall,
        );
        // A background spawn's end is owed to its caller as a message (turn delivery).
        let announce_to = p.notify_parent.then(|| caller_id.agent_id.clone());
        self.start_child(
            me,
            env,
            req,
            task_id,
            caller.into(),
            repo,
            decision,
            announce_to,
        )
    }

    /// **A contracted node the operator asked for directly**: the child path — a worktree on a
    /// `marion/` branch, verification, a contract, usage on the record — with the operator as its
    /// requester ([`crate::run::Requester::Operator`]) and a top-level place in the tree. No caller
    /// gates it or bounds its clock or budget; its type's own ceilings and the operator's config
    /// do. Its end is announced to nobody: the client that asked reads the contract.
    fn spawn_contracted(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        p: &marion_core::proto::params::AgentSpawnParams,
        repo: PathBuf,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        // Held to the intent for the same reason a child's spawn holds it: a quit or a race's
        // count must see this node once it is decided.
        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        let task_id = mint_task_id()?;
        let budget = self.child_budget(&repo, &p.agent_type, p.budget_tokens, None);
        let req = child_request(
            p,
            repo.clone(),
            p.agent_type.clone(),
            p.model.clone(),
            None,
            budget,
            p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS),
        );
        let requester = crate::run::Requester::Operator {
            allow_wider_children: p.wider_children == Some(true),
        };
        self.start_child(me, env, req, task_id, requester, repo, decision, None)
    }

    /// The end every `agent/spawn` of a child shares, ordinary or review: launch it through
    /// [`Self::launch_child`], remember the task it was sent, and answer with its contract's id.
    #[allow(clippy::too_many_arguments)]
    fn start_child(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        req: crate::run::SpawnRequest,
        task_id: TaskId,
        requester: crate::run::Requester,
        repo: PathBuf,
        decision: std::sync::MutexGuard<'_, ()>,
        announce_to: Option<AgentId>,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        // Kept out of the thread's move, because the answer names it: the composing client reads
        // `contracts/<task_id>.json` and cannot mint this id itself (see `AgentSpawnResult`).
        let answered_task_id = task_id.clone();
        let sent = sent_task(&req);
        let (agent_id, state) = self.launch_child(
            me,
            env,
            req,
            task_id,
            requester,
            repo,
            decision,
            announce_to,
        )?;
        self.remember_sent(&agent_id, sent);
        Ok(marion_core::proto::result::AgentSpawnResult {
            state,
            agent_id,
            // §9's contract, named before it exists: this call answers at `Spawned`, and the id is
            // what lets the caller find the file when the run ends.
            task_id: Some(answered_task_id),
            note: None,
            race: None,
        })
    }

    /// **The child's tree is its caller's tree**, read off the entry `resolve_caller` has just
    /// proved this caller owns. Not derived from `<project-hash>` — that is the git common dir
    /// shared by every linked worktree of this repository, so deriving it would branch a
    /// feature-worktree node's children off the main tree's HEAD. See [`NodeHandle::repo`].
    fn caller_repo(
        &self,
        caller_id: &marion_core::proto::SpawnCaller,
    ) -> Result<PathBuf, RpcError> {
        lock(&self.nodes)
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
            })
    }

    /// What `node/get` shows for a child this supervisor started, filed once the node has an id.
    fn remember_sent(
        &self,
        agent_id: &AgentId,
        sent: Option<marion_core::proto::result::TaskSent>,
    ) {
        if let Some(sent) = sent {
            lock(&self.sent).insert(agent_id.clone(), sent);
        }
    }

    /// **A race: one spawn, one seat per candidate**, each an ordinary child of the caller under
    /// the same verification, tagged with the race and its seat. Everything that can refuse the
    /// race — its shape, an unknown agent type, the caller's child bound for every seat at once —
    /// refuses before anything is written; a seat whose launch then fails is reported as not
    /// started and the rest run, and only a race with no seat started at all is refused.
    ///
    /// The spawn decision is held across every seat's intent, so another spawn cannot take the
    /// room the gate counted for a later seat. The race is decided later, from the seats' own
    /// ends ([`Self::drive_race`]); this answers once the seats exist.
    fn spawn_race(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        p: &marion_core::proto::params::AgentSpawnParams,
        caller_id: Option<&marion_core::proto::SpawnCaller>,
        repo: PathBuf,
        step: Option<&crate::run::StepLaunch>,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        use marion_core::proto::result::{RaceStarted, SeatStarted};
        use marion_core::race::{self, RacePolicy, RaceRole, RaceSeat};
        let race_refused =
            |subject: &str, e: race::RaceError| RpcError::refused(subject, e.to_string(), "race");
        if !p.agent_type.is_empty() {
            return Err(RpcError::refused(
                "agent_type",
                "a race names its seats in `candidates`, each `agent_type[:model]`; name either \
                 one `agent_type` or the candidates, not both.",
                "race",
            ));
        }
        if p.model.is_some() {
            return Err(RpcError::refused(
                "model",
                "each seat of a race names its own model, as `agent_type:model` in `candidates`.",
                "race",
            ));
        }
        if p.isolation == Some(Isolation::SharedCwd) {
            return Err(RpcError::refused(
                "isolation",
                "every seat of a race runs in its own worktree, so the seats cannot write over \
                 one another's work; `shared-cwd` would put them all in one directory.",
                "race, §6.6",
            ));
        }
        let candidates =
            race::parse_candidates(&p.candidates).map_err(|e| race_refused("candidates", e))?;
        race::check_race(p.verification.len()).map_err(|e| race_refused("verification", e))?;

        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        // A node's race seats its children; the operator's seats contracted nodes of its own.
        let caller = match caller_id {
            Some(c) => {
                let caller = self.resolve_caller(c)?;
                self.refuse_if_cancelling(c)?;
                Some(caller)
            }
            None => None,
        };
        let asked = p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS);
        // Every seat's clock, never past what is left of the caller's: refused before any seat.
        let wall = match caller_id {
            Some(c) => self.child_wall_secs(asked, &c.agent_id)?,
            None => asked,
        };
        let types = crate::run::agent_types(&repo)
            .map_err(|e| RpcError::refused("agent_type", e.to_string(), "§3.1"))?;
        let policy = RacePolicy::try_from(
            p.race
                .clone()
                .unwrap_or_default()
                .over(types.race().clone()),
        )
        .map_err(|e| race_refused("race", e))?;
        if let Some(c) = candidates
            .iter()
            .find(|c| types.resolve(&c.agent_type).is_none())
        {
            return Err(RpcError::refused(
                "candidates",
                format!(
                    "no agent type is named {:?}; known types are {}.",
                    c.agent_type,
                    types.names().join(", ")
                ),
                "§3.1",
            ));
        }
        // Every seat is one more child of the caller, counted now: the gate passes only if the
        // last seat would still fit. The operator's seats have no caller to gate them.
        if let Some(caller) = &caller {
            let extra = u32::try_from(candidates.len() - 1).unwrap_or(u32::MAX);
            agent_type::check_spawn_gates(
                &caller.agent_type,
                caller.depth,
                caller.live_children.saturating_add(extra),
            )
            .map_err(gate_refusal)?;
        }
        let requester = match &caller {
            Some(c) => crate::run::Requester::from(c.clone()),
            None => crate::run::Requester::Operator {
                allow_wider_children: p.wider_children == Some(true),
            },
        };

        let race_id = crate::run::entropy()
            .map(|e| race::new_race_id(crate::run::unix_millis(), e))
            .map_err(|e| {
                RpcError::internal(format!(
                    "marion could not mint a race id, so nothing was started: {e}"
                ))
            })?;
        self.races.open(&race_id, &requester.id());
        // Before the first seat's intent, whose fsync makes this durable too.
        if let Err(e) =
            self.journal_append(RecordKind::RaceOpened(marion_core::journal::RaceOpened {
                race_id: race_id.clone(),
                parent_id: requester.parent_id(),
                policy,
                seats: candidates.clone(),
            }))
        {
            self.races.close(&race_id);
            return Err(RpcError::internal(format!(
                "marion could not journal the race, so no seat was started: {e}"
            )));
        }

        let mut seats = Vec::with_capacity(candidates.len());
        let mut first: Option<(AgentId, NodeState, TaskId)> = None;
        for (i, candidate) in candidates.iter().enumerate() {
            let seat = u8::try_from(i + 1).unwrap_or(u8::MAX);
            let mut started = SeatStarted {
                seat,
                candidate: candidate.label(),
                agent_id: None,
                task_id: None,
                refused: None,
            };
            let launched = mint_task_id().and_then(|task_id| {
                let mut req = child_request(
                    p,
                    repo.clone(),
                    candidate.agent_type.clone(),
                    candidate.model.clone(),
                    Some(RaceSeat {
                        race_id: race_id.clone(),
                        role: RaceRole::Candidate(seat),
                    }),
                    self.child_budget(
                        &repo,
                        &candidate.agent_type,
                        p.budget_tokens,
                        caller_id.map(|c| &c.agent_id),
                    ),
                    wall,
                );
                // A workflow's race: each seat carries the step, as its own part.
                req.workflow = step.map(|s| crate::run::StepLaunch {
                    seat: marion_core::workflow::WorkflowSeat {
                        part: seat - 1,
                        ..s.seat.clone()
                    },
                    base: s.base.clone(),
                });
                let sent = sent_task(&req);
                let (agent_id, state) = self.launch_child(
                    me.clone(),
                    env.clone(),
                    req,
                    task_id.clone(),
                    requester.clone(),
                    repo.clone(),
                    &decision,
                    None,
                )?;
                self.remember_sent(&agent_id, sent);
                Ok((agent_id, state, task_id))
            });
            match launched {
                Ok((agent_id, state, task_id)) => {
                    started.agent_id = Some(agent_id.clone());
                    started.task_id = Some(task_id.clone());
                    first.get_or_insert((agent_id, state, task_id));
                }
                Err(e) => started.refused = Some(e.to_string()),
            }
            seats.push(started);
        }
        drop(decision);
        // A node that backgrounds its race is owed the decision as a message, and its inbox is held
        // open for it as for a backgrounded child's end; the seats announce nothing one by one.
        // Registered before the race can be decided, and only for a race that has a seat running.
        if let Some(c) = caller_id.filter(|_| p.notify_parent && first.is_some()) {
            let owed = self.inboxes.owe(&c.agent_id);
            self.races.announce_to(&race_id, c.agent_id.clone(), owed);
        }
        self.races.launched(&race_id);
        // A seat may already have ended; and a race with no seat started is decided here.
        self.drive_race(&race_id);
        let Some((agent_id, state, task_id)) = first else {
            let why = seats
                .iter()
                .filter_map(|s| {
                    s.refused
                        .as_deref()
                        .map(|r| format!("{}: {r}", s.candidate))
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(RpcError::refused(
                "candidates",
                format!("no seat of the race could be started — {why}"),
                "race",
            ));
        };
        Ok(marion_core::proto::result::AgentSpawnResult {
            agent_id,
            state,
            task_id: Some(task_id),
            // A seat is never a reviewer, so nothing about its write guard is owed.
            note: None,
            race: Some(RaceStarted { race_id, seats }),
        })
    }

    /// **Step a race and do what the step says** — the one driver, called wherever a race can
    /// change: a seat's thread after its end is filed, the spawn once every seat is started, the
    /// requester's own end, and a restart's boot. Reads the state afresh from the journal and the
    /// seats' contracts every time, so calling it twice, or after a restart, is always safe.
    pub(crate) fn drive_race(&self, race_id: &marion_core::race::RaceId) {
        use marion_core::race::Step;
        let crate::race::Drive::Go { requester } = self.races.begin_drive(race_id) else {
            return;
        };
        let Some(env) = self.spawn_env.as_ref() else {
            self.races.end_drive(race_id);
            return;
        };
        self.live.refresh();
        // The race and its seats' nodes, copied out so the contract files are read after the
        // registry lock is released.
        let Some((race, nodes)) = self.live.read(|r| {
            let race = r.tree().race(race_id)?.clone();
            let nodes: Vec<_> = race
                .seats
                .iter()
                .filter_map(|(_, id)| r.tree().get(id).cloned())
                .collect();
            Some((race, nodes))
        }) else {
            self.races.end_drive(race_id);
            return;
        };
        // The operator is not a node, and does not end: only a node's race can be abandoned.
        let requester_running = requester.0 == crate::run::OPERATOR_REQUESTER
            || self.owned_running(&requester) == Some(true);
        let Some((policy, state)) = crate::race::gather(
            &nodes,
            &env.project_dir,
            &race,
            &requester,
            requester_running,
        ) else {
            self.races.end_drive(race_id);
            return;
        };
        match marion_core::race::step(&policy, &state) {
            Step::Wait => self.races.end_drive(race_id),
            Step::Stop(seats) => {
                let ids: Vec<AgentId> = race
                    .seats
                    .iter()
                    .filter(|(seat, _)| seats.contains(seat))
                    .map(|(_, id)| id.clone())
                    .collect();
                let why = if state.abandoned {
                    StopReason::RaceAbandoned
                } else {
                    StopReason::RaceDecided
                };
                let told = self.races.to_stop(race_id, ids);
                // Released before stopping: a stopped seat's own thread drives the race again
                // once its end is filed, and that drive is what decides it.
                self.races.end_drive(race_id);
                for id in told {
                    if !self.stop_node(&id, why) {
                        // Not stoppable yet (still spawning): a later drive tries again.
                        self.races.forget_stop(race_id, &id);
                    }
                }
            }
            Step::Decided(result) => self.decide_race(&env.project_dir, &env.project_root, result),
        }
    }

    /// Write a decided race: its scoreboard file first, then the journal record that says it is
    /// decided, so a reader that sees the record finds the file.
    fn decide_race(
        &self,
        project: &marion_core::paths::ProjectDir,
        repo: &std::path::Path,
        mut result: marion_core::race::RaceResult,
    ) {
        // Losers' branches go first, so the scoreboard records what is actually gone.
        for seat in result.prunable() {
            if let Some(row) = result.seats.iter_mut().find(|r| r.seat == seat) {
                row.pruned = crate::race::prune_branch(repo, project, row);
            }
        }
        if let Err(e) = crate::race::write_result(project, &result) {
            // The race stays open and a later drive (another seat's end, the next boot) retries.
            eprintln!(
                "marion: could not write race {}'s result, left open: {e}",
                result.race_id.0
            );
            self.races.end_drive(&result.race_id);
            return;
        }
        if let Err(e) = self.journal_now(RecordKind::RaceDecided(crate::race::decided_record(
            &result,
        ))) {
            eprintln!(
                "marion: could not journal race {}'s decision: {e}",
                result.race_id.0
            );
        }
        if let Some((parent, owed)) = self.races.take_announcement(&result.race_id) {
            self.announce_race_end(&parent, &result, owed);
        }
        self.races.close(&result.race_id);
        // A workflow's race step is decided by its race: step the run on.
        let run = self.live.read(|r| {
            result
                .seats
                .iter()
                .filter_map(|row| row.agent_id.as_ref())
                .find_map(|id| r.tree().get(id)?.intent.as_ref()?.workflow.clone())
        });
        if let Some(seat) = run {
            self.drive_workflow(&seat.wf_id);
        }
    }

    /// **The one seam through which marion stops a node it decided to stop itself** — a race's
    /// loser once a first passer has won, a seat whose requester is gone. Today that is §6.7's kill,
    /// through [`Self::kill_node`] exactly as `node/kill` does it and recorded as cancelled; a
    /// graceful cancel can replace this body without touching its callers. `false` when the node
    /// cannot be stopped yet (no recorded pid), or its own thread already saw it end.
    fn stop_node(&self, agent_id: &AgentId, why: StopReason) -> bool {
        let _decision = lock(&self.quit);
        self.live.refresh();
        let Some(node) = self.live.read(|r| r.tree().get(agent_id).cloned()) else {
            return false;
        };
        if node.state.is_exited() {
            return true;
        }
        let retire = node.reap_state == ReapState::ReapedIdle;
        if !retire && (node.pid.is_none() || !self.claim_kill(agent_id)) {
            return false;
        }
        if let Err(e) = self.kill_node(&node, KillBy::Stop(why)) {
            eprintln!("marion: could not stop `{}`: {}", agent_id.0, e.message);
            return false;
        }
        self.live.refresh();
        true
    }

    /// **A restart re-drives every race its predecessor left open.** Decided now if every seat has
    /// ended; a seat marion lost counts as cancelled. Its requester is gone with the predecessor, so
    /// the result is the record's, never delivered.
    fn redrive_open_races(&self) {
        let open: Vec<_> = self.live.read(|r| {
            r.tree()
                .races()
                .iter()
                .filter(|race| race.decided.is_none())
                .filter_map(|race| {
                    // A race with no parent is the operator's.
                    let requester = race
                        .opened
                        .as_ref()?
                        .parent_id
                        .clone()
                        .unwrap_or_else(|| AgentId(crate::run::OPERATOR_REQUESTER.into()));
                    Some((race.race_id.clone(), requester))
                })
                .collect()
        });
        for (race_id, requester) in open {
            self.races.open(&race_id, &requester);
            self.races.launched(&race_id);
            self.drive_race(&race_id);
        }
    }

    /// **The node a review names, read and checked**: known, ended, a child with a contract that
    /// changed something. Every refusal is [`crate::review::refusal`]'s plain sentence. Shared by a
    /// fresh review and by the resume of a reviewer, so a resumed reviewer is as read-only and as
    /// grounded as the one it continues.
    fn reviewed(&self, env: &crate::run::Env, target: &AgentId) -> Result<Reviewed, RpcError> {
        let refuse = |why: String| RpcError::refused(&target.0, why, "review");
        let (intent, state, model) = self
            .live
            .read(|r| {
                r.tree()
                    .get(target)
                    .map(|n| (n.intent.clone(), n.state, n.model.clone()))
            })
            .ok_or_else(|| {
                refuse(crate::review::refusal(
                    target,
                    "no node with that id is in this project's journal",
                ))
            })?;
        if !state.is_exited() {
            return Err(refuse(crate::review::refusal(
                target,
                "it has not ended yet; wait for it to finish, then ask again",
            )));
        }
        let intent = intent.ok_or_else(|| {
            refuse(crate::review::refusal(
                target,
                "its journal has no spawn record, so marion cannot tell what it was asked to do",
            ))
        })?;
        let task_id = intent.task_id.clone().ok_or_else(|| {
            refuse(crate::review::refusal(
                target,
                "it is a root, which has no task contract to review",
            ))
        })?;
        let path = env.project_dir.agent(target).contract(&task_id);
        let contract = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| {
                serde_json::from_slice::<marion_core::contract::TaskContract>(&b)
                    .map_err(|e| e.to_string())
            })
            .map_err(|e| {
                refuse(crate::review::refusal(
                    target,
                    &format!("its contract at {} could not be read ({e})", path.display()),
                ))
            })?;
        let review = crate::review::target(target, &contract).map_err(refuse)?;
        Ok(Reviewed {
            review,
            contract,
            intent,
            model,
        })
    }

    /// **`agent/spawn` with `review_of`: a read-only reviewer of an ended node** ([`crate::review`]).
    ///
    /// One spawn path for both client forms: `marion review` sends this with no `caller` (the
    /// operator, authorized as a root spawn is, naming the `repo`), and a parent model sends it
    /// through its bridge's `spawn` with its own token. Either way the reviewer is placed **under
    /// the node it reviews** — the reviewed node stands as the caller for §6.1's gates, the tree
    /// and the contract's `requester` — so the tree shows the review where the work is.
    ///
    /// Refused, in plain words and before anything is written, when the node is unknown, is a
    /// root, has not ended, or changed nothing.
    fn spawn_review(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        p: &marion_core::proto::params::AgentSpawnParams,
        target: &AgentId,
        peer: Peer,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        let repo = match p.caller.as_ref() {
            Some(c) => self.authenticate(c).ok_or_else(|| {
                RpcError::refused(
                    &c.agent_id.0,
                    "this supervisor did not mint that node token, so it will not start a review \
                     on its word.",
                    "§5.4",
                )
            })?,
            None => {
                root_spawn_authorized(peer)?;
                let repo = p.repo.clone().ok_or_else(|| {
                    RpcError::internal("a review with no caller reached the launcher with no repo")
                })?;
                check_project(&env, &repo)?;
                repo
            }
        };
        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        let Reviewed {
            review,
            contract,
            intent,
            model,
        } = self.reviewed(&env, target)?;
        let agent_type = tree_types(&repo)?
            .resolve(&intent.agent_type)
            .ok_or_else(|| {
                Unprojectable::UnknownAgentType(intent.agent_type.clone()).as_error(target)
            })?;
        let caller = crate::run::Caller {
            agent_id: target.0.clone(),
            agent_type,
            depth: intent.depth,
            live_children: self.live_children_of(target),
        };
        agent_type::check_spawn_gates(&caller.agent_type, caller.depth, caller.live_children)
            .map_err(gate_refusal)?;
        let task_id = mint_task_id()?;
        // No type named: a reviewer from another model family than the work's.
        let reviewer = match p.agent_type.trim() {
            "" => crate::review::default_reviewer(intent.harness, model.as_deref()).to_string(),
            named => named.to_string(),
        };
        // A reviewer on a row that cannot refuse a write is allowed when asked for by name, and
        // said so in the answer; its contract says it again (`run::record_review`).
        let note = tree_types(&repo)?
            .resolve(&reviewer)
            .and_then(|t| crate::review::unguarded(t.harness))
            .map(str::to_string);
        let req = crate::run::SpawnRequest {
            race: None,
            budget: self.child_budget(&repo, &reviewer, None, Some(target)),
            agent_type: reviewer,
            prompt: crate::review::prompt(&review, &contract),
            repo: repo.clone(),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec![],
            timeout_secs: self
                .child_wall_secs(p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS), target)?,
            model: p.model.clone(),
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
            review: Some(review),
            read_only: false,
            workflow: None,
        };
        // A parent that asked for its child's review in the background is owed its end.
        let announce_to = p
            .caller
            .as_ref()
            .filter(|_| p.notify_parent)
            .map(|c| c.agent_id.clone());
        self.start_child(
            me,
            env,
            req,
            task_id,
            caller.into(),
            repo,
            decision,
            announce_to,
        )
        .map(|r| marion_core::proto::result::AgentSpawnResult { note, ..r })
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
    ///
    /// `decision` is the guard itself, or a borrow of it for a race: dropping a borrow releases
    /// nothing, so the guard spans every seat's intent and the caller drops it after the last.
    #[allow(clippy::too_many_arguments)]
    fn launch_child<Decision>(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        req: crate::run::SpawnRequest,
        task_id: TaskId,
        requester: crate::run::Requester,
        repo: PathBuf,
        decision: Decision,
        announce_to: Option<AgentId>,
    ) -> Result<(AgentId, NodeState), RpcError> {
        let (tx, progress) = std::sync::mpsc::channel();
        let observer = NodeOwner {
            handle: me.clone(),
            task_id: Some(task_id.clone()),
            // The child inherits its caller's tree and hands the same one to *its* children.
            repo,
            tx,
            identified: Mutex::new(None),
            announce_to,
            owes: Default::default(),
        };
        // **Everything the thread needs is owned**, for `Background::start`'s reason: this work
        // outlives the JSON-RPC frame that asked for it, so it cannot borrow from this stack frame.
        let owner = me.clone();
        let join = std::thread::spawn(move || {
            let agent_type = req.agent_type.clone();
            // **A panic here must still resolve the node.** See [`caught`]: without this the
            // outcome stays `None` for ever and the supervisor can never exit.
            let outcome = caught(&agent_type, spawn_panicked, || {
                crate::run::run_spawn_watched(&env, &req, &task_id, &requester, &observer)
            });
            // The agent id is known only if `identified` fired. A spawn refused above it — an
            // unknown agent type, a scope outside the ceiling — never minted a node, so there is
            // nothing to file the outcome under and nothing holding the supervisor open.
            if let Some(agent_id) = observer.identified_id() {
                // The parent's owed announcement, before the outcome moves into the table.
                // Owed where the parent's inbox was open when this child started; where it was not,
                // the parent had ended and the end goes to the nearest live ancestor.
                if let Some(parent) = &observer.announce_to {
                    owner.announce_child_end(
                        parent,
                        steer::ChildEnd {
                            child: &agent_id,
                            agent_type: &agent_type,
                            task_id: &task_id,
                            outcome: &outcome,
                        },
                        observer.owes.load(std::sync::atomic::Ordering::SeqCst),
                    );
                }
                owner.mark_finished(&agent_id, NodeOutcome::Child(Box::new(outcome)));
                // A seat's end may decide its race; its contract is on disk by now.
                if let Some(seat) = &req.race {
                    owner.drive_race(&seat.race_id);
                }
                // And a workflow step's end may decide its step and start the next.
                if let Some(seat) = &req.workflow {
                    workflow::after_step_node(&owner, seat);
                }
            } else if let Err(e) = &outcome {
                let _ = observer.tx.send(Progress::Refused(e.to_string()));
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
            // `run_spawn` refused before it minted an id — an unknown agent type, a writable scope
            // outside that type's ceiling, a less-contained child. There is no node; the caller is
            // told the refusal in `run_spawn`'s own words.
            Ok(Progress::Refused(why)) => {
                return Err(RpcError::refused("agent_type", why, "§6.1"));
            }
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
        check_project(&env, &repo)?;
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
            note: None,
            race: None,
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
                announce_to: None,
                owes: Default::default(),
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
                // The same attribution the child path asks through the trait: did a `node/kill`
                // (or KillTree) end this root, so its terminal record is the kill's?
                let ended_by_kill = || observer.process_ended(&node.agent_id);
                // And the ask between a failed attempt and a key rotation's next, which settles
                // nothing: a kill asked for then ends the root rather than its next credential.
                let kill_requested = || observer.ended_by(&node.agent_id).is_some();
                crate::root::launch_owned(
                    &node,
                    bound,
                    node.boot.budget(),
                    // No live sink here: this runs inside `marion-supervisor`, and the client
                    // watches through `node/attach` over the socket rather than through a pipe
                    // this process would have to own. `events.jsonl` is what both legs read.
                    None,
                    Some(&started),
                    // §9's M3: where a pane goes if this run asked for one. The same observer,
                    // because it is the same ownership — this supervisor answers `node/attach`,
                    // so it is the only process for which a registered pane means anything.
                    Some(&observer),
                    Some(&ended_by_kill),
                    Some(&kill_requested),
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
    /// 3. its stream named a session — or, where its row lists sessions by title, the harness's own
    ///    listing names one under the node's title ([`Self::session_by_title`]) — else there is
    ///    nothing to resume and the refusal says so;
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
        // **A session the stream never named** is looked for under the title marion gave it: a node
        // whose supervisor died during its first request may have a session its harness names only
        // once a response streams. Where the harness says so, that listing also says where the
        // session was created — the tree a child's resume must run in, which only a named session
        // would otherwise have journaled.
        let mut node = node;
        let session = match node.harness_session.clone() {
            Some(session) => session,
            None => {
                let found = self.session_by_title(&node, &env).ok_or_else(|| {
                    RpcError::refused(
                        "agent_id",
                        format!(
                            "`{}` named no harness session — its stream never carried one, or it \
                             ran on a harness marion reads no session from, and no listing of its \
                             harness's sessions names one under its title — so marion cannot hand \
                             a resume back to the harness. Refused by name rather than started \
                             fresh under a resumed session's id, which would misdescribe the run.",
                            p.agent_id.0
                        ),
                        "§8",
                    )
                })?;
                if node.launch_workspace.is_none() && depth > 0 {
                    node.launch_workspace = found
                        .directory
                        .as_deref()
                        .and_then(|dir| listed_workspace(&node, &env, dir));
                }
                found.id
            }
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

    /// **The session a node's stream never named, found under the title marion launched it with**
    /// — the row's [`marion_harness::grammar::TitleLookup`]: the harness's own read-only listing,
    /// run with the node's own environment (where the harness keeps its session store) and the
    /// row's update switch, from the project's tree, bounded. `None` where the row lists no
    /// sessions by title, the listing could not run, or it names none — or more than one — by this
    /// node's title.
    fn session_by_title(
        &self,
        node: &ReplayedNode,
        env: &crate::run::Env,
    ) -> Option<marion_harness::grammar::TitledSession> {
        let harness = node.intent.as_ref()?.harness;
        let lookup = marion_harness::adapter::adapter_for(harness)
            .ok()?
            .session_lookup()?;
        let spec = marion_harness::adapter::harness_spec(harness);
        let (vars, args) = marion_harness::probe::ProbeSwitch::bare(spec).ok()?;
        let cwd = resumable_root_cwd(&env.project_root);
        let fields = marion_harness::spec::Fields {
            cwd: cwd.clone(),
            config_dir: env.project_dir.agent(&node.agent_id).config_dir(),
            auth: env.auth,
            ..Default::default()
        };
        let set: Vec<(String, String)> = marion_harness::spec::render_env(spec.env, &fields)
            .into_iter()
            .chain(vars)
            .collect();
        let filter = marion_harness::env_filter::InheritFilter {
            login: spec.login_env,
            auth: env.auth,
            passthrough: vec![],
        };
        let mut listing = std::process::Command::new(spec.program?);
        listing
            .args(args)
            .args(lookup.argv)
            .envs(set.iter().cloned())
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null());
        for key in marion_harness::invocation::removals_for(Some(&filter), &set) {
            listing.env_remove(key);
        }
        let out = crate::run::run_bounded(&mut listing, SESSION_LISTING_BOUND).ok()?;
        if out.timed_out || out.code != Some(0) {
            return None;
        }
        marion_harness::grammar::session_by_title(
            lookup,
            &String::from_utf8_lossy(&out.stdout),
            &marion_harness::grammar::session_title(&node.agent_id),
        )
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
            .map(|i| recorded_type(Some(&env.project_dir), &repo, i))
            .transpose()?
            .ok_or_else(spawn_refused_before_the_node_existed)?;
        let spec = crate::root::RootSpec {
            os_sandbox: env.os_sandbox,
            wider_children: false,
            // A second life runs under the budget its first was spawned with.
            budget: node.intent.as_ref().and_then(|i| i.budget),
            agent_type: node.agent_type().unwrap_or_default().to_string(),
            prompt: prompt.to_string(),
            native_launch: None,
            repo: repo.clone(),
            state: env.state.clone(),
            base_url: env.base_url.clone(),
            bridge: env.bridge.clone(),
            // An endpoint root re-resolves its journaled provider (and re-reads its key).
            model: crate::endpoint::resume_model(
                node.model.as_deref(),
                node.provider.as_deref(),
                &agent_type,
            ),
            no_change_record: false,
            auth: env.auth,
            // **From the recorded value, never inferred** (`plan-restart-resume.md` step 6): the
            // shape the node's session was observed in. A pane names no session, so a resumable
            // node is always headless today; the field keeps that a checked fact.
            pane: node.harness_pane,
            resume: Some(crate::root::RootResume {
                agent_id: node.agent_id.clone(),
                session,
                usage: node.usage,
            }),
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
            // The account the session was recorded under, never a re-resolved one.
            profile: node.profile.clone(),
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
    /// **Its `verification` comes back from the intent**, where `run_spawn` journals the lines the
    /// spawn asked for, so the second life re-runs them and its contract records their outcome as
    /// evidence. An intent written before the field replays as no lines, and that child resumes
    /// with none, as every child did before.
    ///
    /// **Two limits, stated rather than hidden.** A child's `writable_scope` and its
    /// `acceptance_criteria` are not journaled — they live on the contract, which an orphaned child
    /// never got far enough to write — so the second life runs with the empty scope, which is its
    /// agent type's ceiling (`run::requested_scope`, still checked against that ceiling), and an
    /// empty criteria list. Narrowing them again would need a second record, not a guess here.
    /// A resumed child's parent, read back and checked: on this journal, and no longer waiting on
    /// the child — so the child's outcome is owed to nobody still holding a `spawn`.
    fn decided_parent(
        &self,
        node: &marion_core::registry::ReplayedNode,
        parent_id: &AgentId,
    ) -> Result<marion_core::registry::ReplayedNode, RpcError> {
        let parent = self
            .live
            .read(|r| r.tree().get(parent_id).cloned())
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
        Ok(parent)
    }

    fn relaunch_child(
        &self,
        me: Arc<RegistryHandle>,
        node: &marion_core::registry::ReplayedNode,
        env: &crate::run::Env,
        prompt: &str,
        repo: PathBuf,
        session: String,
    ) -> Result<(AgentId, NodeState), RpcError> {
        // **§7.5's immutable parent link, read back.** The intent is the only record that names it:
        // a node's child names its caller, and a contracted node the operator asked for directly
        // names none and carries a contract — the operator is its requester again.
        let parent_id = node.intent.as_ref().and_then(|i| i.parent_id.clone());
        let operators = node
            .intent
            .as_ref()
            .is_some_and(|i| i.parent_id.is_none() && i.task_id.is_some());
        let parent_id = match parent_id {
            Some(p) => Some(p),
            None if operators => None,
            None => {
                return Err(RpcError::refused(
                    "agent_id",
                    format!(
                        "`{}` records a depth below the root and neither a parent nor a contract, \
                         so marion cannot say whose node it is or what gates its relaunch. Refused \
                         by name.",
                        node.agent_id.0
                    ),
                    "§7.5",
                ));
            }
        };
        let parent = match &parent_id {
            Some(parent_id) => Some(self.decided_parent(node, parent_id)?),
            None => None,
        };
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
            .as_ref()
            .map(|parent| {
                parent
                    .intent
                    .as_ref()
                    .map(|i| recorded_type(Some(&env.project_dir), &repo, i))
                    .transpose()?
                    .ok_or_else(spawn_refused_before_the_node_existed)
            })
            .transpose()?;
        let agent_type = node
            .intent
            .as_ref()
            .map(|i| recorded_type(Some(&env.project_dir), &repo, i))
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
        let requester = match (parent_id, parent, parent_type) {
            (Some(parent_id), Some(parent), Some(parent_type)) => {
                crate::run::Requester::from(crate::run::Caller {
                    live_children: self.live_children_of(&parent_id),
                    agent_id: parent_id.0,
                    agent_type: parent_type,
                    depth: parent.depth().unwrap_or(0),
                })
            }
            // Its own tree's table is in its agent directory, read back by the resume.
            _ => crate::run::Requester::Operator {
                allow_wider_children: false,
            },
        };
        // A reviewer's second life is a reviewer's: read-only, against the same reviewed work.
        let review = node
            .intent
            .as_ref()
            .and_then(|i| i.review_of.as_ref())
            .map(|target| self.reviewed(env, target).map(|r| r.review))
            .transpose()?;
        let req = crate::run::SpawnRequest {
            // A second life runs under the budget its first was spawned with.
            budget: node.intent.as_ref().and_then(|i| i.budget),
            review,
            agent_type: agent_type.name.clone(),
            prompt: prompt.to_string(),
            repo: repo.clone(),
            // Not journaled; see this function's doc for why it is empty rather than invented.
            acceptance_criteria: vec![],
            // Journaled on the intent, so the second life re-runs what its spawn asked for.
            verification: node
                .intent
                .as_ref()
                .map(|i| i.verification.clone())
                .unwrap_or_default(),
            writable_scope: vec![],
            timeout_secs: agent_type.timeout.0.as_secs(),
            // An endpoint child re-resolves its journaled provider (and re-reads its key).
            model: crate::endpoint::resume_model(
                node.model.as_deref(),
                node.provider.as_deref(),
                &agent_type,
            ),
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
                usage: node.usage,
            }),
            // The account the session was recorded under, never a re-resolved one.
            profile: node.profile.clone(),
            // A resumed seat keeps its seat.
            race: node.intent.as_ref().and_then(|i| i.race.clone()),
            read_only: false,
            // A resumed step node keeps its step; its recorded tree is where it resumes.
            workflow: node
                .intent
                .as_ref()
                .and_then(|i| i.workflow.clone())
                .map(|seat| crate::run::StepLaunch { seat, base: None }),
        };
        // A resumed child answers the operator's `node/resume`, not a parent's `spawn`.
        self.launch_child(
            me,
            env.clone(),
            req,
            task_id,
            requester,
            repo,
            decision,
            None,
        )
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
        let socket = &self.socket_path;
        DetachGuidance {
            reattach: format!(
                "`marion ls` shows them again (a client: tree/subscribe on {}). A node named in \
                 gate_exposed is refused any permission it asks for, since nobody can approve it; \
                 it sees the refusal as an error result.",
                socket.display()
            ),
            stop_fleet: format!(
                "`marion cancel <id>` stops one (a client: session/quit with KillTree on {}, \
                 confirmed against a fresh tree/subscribe).",
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
            // A node whose own thread already saw its process end is still ended here, exactly as
            // before `node/kill` existed: the operator confirmed this exact set, and KillTree has
            // no refusal for a node that happens to be finishing. The claim only tells an owned
            // node's thread that marion ended it, when marion got there first.
            self.claim_kill(&node.agent_id);
            self.kill_node(node, KillBy::QuitKillTree)?;
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

    /// **§6.7's kill of one node**: the intent made durable, the per-node process tree signalled
    /// and observed dead, the confirmation made durable — and nothing else. Shared by
    /// `session/quit`'s KillTree, which runs it per confirmed node, and by `node/kill`; each caller
    /// owns its own preflight, and neither the supervisor's exit nor any other node is this
    /// function's business.
    ///
    /// A `ReapedIdle` node is retired rather than signalled: §7.2 already ended its process, and
    /// its recorded pid may name something else by now. The confirmation then records no signal,
    /// because marion sent none.
    ///
    /// The caller has checked that a node to be signalled has a recorded pid.
    fn kill_node(
        &self,
        node: &marion_core::registry::ReplayedNode,
        by: KillBy,
    ) -> Result<(), RpcError> {
        self.journal_append(RecordKind::KillIntent(KillIntent {
            agent_id: node.agent_id.clone(),
            was: node.state,
        }))
        .map_err(|e| journal_failure_before_signal_in(by.verb(), e))?;
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
                    by.signalled().into()
                } else {
                    by.retired().into()
                },
            },
        }))
        .map_err(|e| journal_failure_after_signal_in(by.verb(), e))
    }

    /// **§2's `node/kill` — end one node**, which §6.7 classifies `Exited(Cancelled)`.
    ///
    /// The per-node half of §7.3.2's disposition (a), through the same [`Self::kill_node`], and
    /// deliberately *only* that half: no confirmed-set comparison (the caller named one node), and
    /// no change to the supervisor's disposition — §5.7 goes on deciding that from whatever else is
    /// still running.
    ///
    /// The refusals come before any side effect, in the order that decides them:
    ///
    /// 1. the caller is this supervisor's own user — the check every operator call makes
    ///    ([`root_spawn_authorized`]); a node cannot kill through this verb;
    /// 2. the node is on the journal (`NotFound` otherwise), named by its whole id as `node/steer`
    ///    names it;
    /// 3. it has not already ended (`Refused`, naming its state) — its old pid is never signalled;
    /// 4. a node to be signalled has a recorded pid (`Conflict`: still spawning, retry);
    /// 5. **for a node this supervisor owns, its own thread has not already seen its process end**
    ///    (`Conflict`). This is the thread race, settled under one lock by [`Self::claim_kill`]
    ///    and [`Self::process_ended`]: whichever reaches the node's end first decides who writes
    ///    its terminal record. A kill that wins tells the thread, which then records its outcome
    ///    as the cancellation and writes no `Exited` over the `KillConfirmed`; a thread that wins
    ///    is already recording an exit it observed, and a kill would aim at a reaped pid.
    ///
    /// Held under the same lock as `session/quit`, so a KillTree and a `node/kill` cannot both
    /// journal an intent for one node.
    fn node_kill(
        &self,
        p: &marion_core::proto::params::NodeKillParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::NodeKillResult, RpcError> {
        root_spawn_authorized(peer)?;
        let _decision = lock(&self.quit);
        self.live.refresh();
        let node = self
            .live
            .read(|r| r.tree().get(&p.agent_id).cloned())
            .ok_or_else(|| {
                RpcError::not_found(
                    &p.agent_id.0,
                    format!(
                        "this project's journal records no node `{}`, so there is nothing to end. \
                         Nothing was signalled.",
                        p.agent_id.0
                    ),
                    "§2, §6.7",
                )
            })?;
        if node.state.is_exited() {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` has already ended ({:?}); there is no process to end, and a terminal \
                     node's recorded PID is never signalled. Nothing was signalled.",
                    p.agent_id.0, node.state
                ),
                "§6.7",
            ));
        }
        if node.reap_state != ReapState::ReapedIdle && node.pid.is_none() {
            return Err(RpcError::conflict(
                &p.agent_id.0,
                "the node has no recorded PID yet, so marion cannot prove a signal reaches it. \
                 Nothing was signalled; retry after its spawn resolves.",
                "§6.7",
            ));
        }
        // **A kill of a node being cancelled is the cancel's escalation**: the cancel kills what is
        // left now rather than after its grace, and keeps the attribution and the confirmation.
        if self.cancelling(&p.agent_id) {
            self.escalate(&p.agent_id);
            drop(_decision);
            self.wait_processes_gone(
                std::slice::from_ref(&p.agent_id),
                std::time::Instant::now() + cancel::ESCALATE_WAIT,
                &[],
            );
            self.live.refresh();
            return Ok(marion_core::proto::result::NodeKillResult {
                state: self.spawned_state(&p.agent_id),
            });
        }
        // A `ReapedIdle` node is retired, not signalled, so there is no process end to race for:
        // §7.2's reap already ended it, and its thread's own reading of that is long settled.
        if node.reap_state != ReapState::ReapedIdle && !self.claim_kill(&p.agent_id) {
            return Err(RpcError::conflict(
                &p.agent_id.0,
                "the node's process has already ended on its own and its thread is recording how; \
                 its terminal state lands on the journal in a moment. Nothing was signalled.",
                "§6.7",
            ));
        }
        self.kill_node(&node, KillBy::NodeKill)?;
        self.live.refresh();
        Ok(marion_core::proto::result::NodeKillResult {
            state: self.spawned_state(&p.agent_id),
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
                    // ReapedIdle writes no Completion. Roots have no waiting spawn by construction;
                    // a top-level contracted node may, from the operator's own client.
                    && n.is_root()
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

fn journal_failure_before_signal(error: crate::journal::JournalError) -> RpcError {
    journal_failure_before_signal_in("session/quit", error)
}

fn journal_failure_after_signal(error: crate::journal::JournalError) -> RpcError {
    journal_failure_after_signal_in("session/quit", error)
}

fn journal_failure_before_signal_in(verb: &str, error: crate::journal::JournalError) -> RpcError {
    RpcError::internal(format!(
        "{verb} could not durably journal its intent, so it refused before signalling the node: \
         {error}"
    ))
}

fn journal_failure_after_signal_in(verb: &str, error: crate::journal::JournalError) -> RpcError {
    RpcError::internal(format!(
        "{verb} changed a process but could not durably journal its confirmation; its intent \
         remains for restart recovery and the supervisor will not exit: {error}"
    ))
}

/// Which operator act a [`RegistryHandle::kill_node`] carries out — what its journal records say
/// marion did, and which verb its failures name. §6.7 wants the description to record marion as
/// the sender; the act is what tells a reader why.
#[derive(Clone, Copy)]
enum KillBy {
    /// §7.3.2's disposition (a), one confirmed node at a time.
    QuitKillTree,
    /// §2's `node/kill`, for the one node it names.
    NodeKill,
    /// marion's own decision, through [`RegistryHandle::stop_node`].
    Stop(StopReason),
}

/// Why marion stops a node nobody asked it to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    /// A `first` race has its winner; the seats still running have lost.
    RaceDecided,
    /// The race's requester ended, so no seat's result will be read.
    RaceAbandoned,
}

impl KillBy {
    fn verb(self) -> &'static str {
        match self {
            KillBy::QuitKillTree => "session/quit",
            KillBy::NodeKill => "node/kill",
            KillBy::Stop(_) => "race",
        }
    }

    fn signalled(self) -> &'static str {
        match self {
            KillBy::QuitKillTree => "marion sent SIGKILL for confirmed session/quit KillTree",
            KillBy::NodeKill => "marion sent SIGKILL for the operator's node/kill",
            KillBy::Stop(StopReason::RaceDecided) => {
                "marion sent SIGKILL: its race was decided by an earlier seat"
            }
            KillBy::Stop(StopReason::RaceAbandoned) => {
                "marion sent SIGKILL: the node that asked for its race has ended"
            }
        }
    }

    fn retired(self) -> &'static str {
        match self {
            KillBy::QuitKillTree => {
                "confirmed session/quit retired an already ReapedIdle node; no process existed to \
                 signal"
            }
            KillBy::NodeKill => {
                "node/kill retired an already ReapedIdle node; no process existed to signal"
            }
            KillBy::Stop(_) => {
                "a race retired an already ReapedIdle seat; no process existed to signal"
            }
        }
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

/// Send to every subscriber, dropping the ones that have gone.
///
/// [`Outbound::send`] never blocks, so this cannot be slowed by a client — see `serve.rs`: a full
/// queue is a verdict about that client, and §5.7 is what makes it the right one.
/// What the operator's `notify.toml` and `MARION_NOTIFY` ask for, titled with the project's
/// directory name: the notifier's seed, whether notices start on or off.
fn notify_seed_for(project_root: &std::path::Path) -> crate::notify::NotifySeed {
    let config = crate::notify::NotifyConfig::load(
        crate::credentials::config_dir().ok().as_deref(),
        std::env::var(crate::notify::NOTIFY_ENV).ok().as_deref(),
    )
    .unwrap_or_else(|e| {
        eprintln!("marion: notifications are off: {e}");
        crate::notify::NotifyConfig::default()
    });
    let backend =
        crate::notify::Backend::resolve(std::env::var(crate::notify::BACKEND_ENV).ok().as_deref());
    let shown = match project_root.file_name().and_then(|f| f.to_str()) {
        Some(".git") => project_root.parent().unwrap_or(project_root),
        _ => project_root,
    };
    let project = shown
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("project")
        .to_string();
    crate::notify::NotifySeed {
        config,
        backend,
        project,
    }
}

/// Send `shown` to the first claimer still connected; a claimer whose send fails is gone, and the
/// next is tried.
fn notify_claimer(
    g: &mut Shared,
    ring: crate::notify::TerminalRing,
    shown: &[crate::notify::Shown],
) {
    while let Some(head) = g.claimers.first() {
        let sent = shown.iter().all(|s| {
            head.send(&marion_core::proto::Frame::Notification(
                marion_core::proto::Notification::new(Event::NotifyNotice {
                    title: s.title.clone(),
                    body: s.body.clone(),
                    ring: ring.word().to_string(),
                }),
            ))
        });
        if sent {
            return;
        }
        g.claimers.remove(0);
    }
}

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

/// A poisoned lock is taken, not unwrapped — `registry.rs`'s rule and §5.7's requirement.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The read point the registry is currently serving, for a caller that wants it without a client.
pub fn read_point(live: &LiveRegistry) -> ReplayPoint {
    live.read(|r| r.read_point())
}

#[cfg(test)]
mod tests;
