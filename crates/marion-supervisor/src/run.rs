//! Executing one `spawn`: worktree → child → persisted contract → capped return (design §6.1).
//!
//! **The bound is finite by construction, and does not depend on the kill sweep being complete.**
//! `run_bounded` returns within `timeout + DRAIN_GRACE` no matter what survives: the deadline is
//! enforced by `kill_process_tree`, and the *output drain* is enforced separately by its own bound
//! (see [`Drain`]). That separation is deliberate. A drain thread blocks until every holder of the
//! pipe's write end closes it, and an escaped descendant inherits that write end — so joining the
//! drains unconditionally would make marion's liveness rest on the sweep's completeness, which has
//! a known residual race (a child forking into a fresh group between the `ps` snapshot and the
//! first signal). When the drain bound expires the capture is short, and — like every other
//! shortening in this system (design §6.7) — the shortening is *recorded*, in
//! `ProcessExit.description`, never silent.

use std::io::Read;
use std::ops::ControlFlow;
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command as SysCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use marion_core::agent_type::{AgentType, AgentTypes, check_spawn_gates};
use marion_core::cap::cap_for_return;
use marion_core::contract::*;
use marion_core::encoding::{Duration, Millis, SystemTime};
use marion_core::journal::{
    ContractPersisted, Exited, RecordKind, SpawnAborted, SpawnIntent, Spawned,
};
use marion_core::paths::{AgentDir, ProjectDir};
use marion_core::scope::check_spawn_scope;
use marion_core::secret::Secret;
use marion_harness::{
    Auth, ChildExit, Extras, Invocation, LaunchSpec, McpDeclaration, SpawnCtx, adapter_for_type,
};

pub(crate) use crate::clock::{entropy, unix_millis};
use crate::duplex::{self, DuplexSpec, LaunchPath, launch_path};
use crate::kill::{DRAIN_GRACE, kill_process_tree};
use crate::spawn::{ChildOutcome, SpawnError, build_contract, changed_paths, diff_text};

/// How long a **child**'s harness may take to have marion's tool list before the run is refused
/// (§6.1 step 8). The same 30 s `marion run` gives a root, for the same reason: the measured
/// connect is ~70 ms, and the alternative to waiting is a run that ends in plain text with no error
/// anywhere. It is additionally capped at the child's own `timeout_secs` in [`duplex_child`] — a
/// child may not spend longer waiting to be ready than it is allowed to live.
const CHILD_MCP_READY_TIMEOUT: StdDuration = StdDuration::from_secs(30);

/// **The longest wall clock marion will hold for one child**, and the reason there has to be one.
///
/// `timeout_secs` is caller-controlled and the caller is a *language model* — on the background path
/// it is a child agent, which §3.1 item 2 says is data and never authority. It arrives as a bare
/// `u64` off the JSON-RPC frame, and every value in that type is syntactically legal. Without a cap
/// the value reached `Instant::now() + Duration::from_secs(n)` in [`run_bounded`], which **panics**
/// on overflow — and it panicked *after* `Command::spawn` had already started an OS process, so the
/// unwind left a live child behind (dropping `std::process::Child` kills nothing), plus its
/// worktree, branch and agent directory. On the synchronous path the panic was on the bridge's own
/// thread and took the whole MCP server, and every *other* live child of that node, with it.
///
/// **24 hours, and the number is a policy rather than a measurement.** It is chosen to be far
/// beyond any plausible agent run — s16 measured the harness that hosts the bridge tearing it down
/// in under a second at its own exit, so nothing marion can hold is bounded by this in practice —
/// and far below the point where a deadline stops being representable. What matters is not the
/// particular number but that it is finite and stated: any finite cap makes the arithmetic below
/// total, and an unstated one would be rediscovered as a panic.
///
/// **Clamped, not refused, and it is recorded.** §6.7's rule throughout this file is that the
/// contract records *the compiled value, never the asked-for one* — `harness`, `model` and
/// `allowed_tools` are all sourced that way. The wall clock joins them: `TaskContract`'s `timeout`
/// is built from [`effective_timeout`]'s output, so a caller that asked for more can read exactly
/// what it got. Refusing instead would fail a spawn over a number marion is perfectly able to
/// honour a defensible version of, and would put the refusal in the one place — the result slot —
/// this whole design keeps free of surprises.
pub const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// **The most a spawn's `verification` lines may cost on the node's `SpawnIntent`**, measured as
/// the journal encodes them (JSON, escapes included).
///
/// The lines are journaled so a resumed child re-runs them, and a record over
/// [`marion_core::journal::MAX_RECORD_BYTES`] is refused by the codec and dropped — for an intent,
/// that loses the node's identity record, not merely its verification. Half the record cap leaves
/// the intent's other fields and the record envelope room to spare, and 8 KiB of shell is far
/// beyond a list of check commands; a longer check belongs in a script the child's tree carries.
/// Refused, not truncated: a truncated line would run a different command.
pub const MAX_VERIFICATION_BYTES: usize = 8 * 1024;
const _: () = assert!(MAX_VERIFICATION_BYTES * 2 <= marion_core::journal::MAX_RECORD_BYTES);

/// Refuses `verification` whose encoded size exceeds [`MAX_VERIFICATION_BYTES`].
///
/// The encoded size rather than the raw one, because the record carries the escaped bytes: a NUL
/// is one byte raw and six on disk, so a raw-length guard would pass an intent the journal drops.
pub(crate) fn check_verification_size(verification: &[String]) -> Result<(), SpawnError> {
    if verification.is_empty() {
        return Ok(());
    }
    let bytes = serde_json::to_vec(verification)?.len();
    if bytes > MAX_VERIFICATION_BYTES {
        return Err(SpawnError::VerificationTooLarge {
            bytes,
            cap: MAX_VERIFICATION_BYTES,
        });
    }
    Ok(())
}

/// The wall clock a request of `secs` actually gets: **its own, or [`MAX_TIMEOUT_SECS`]**.
///
/// One function rather than a `min` at each use, so the bound the child runs under and the bound
/// its contract records cannot diverge — two spellings of one clamp are two chances to disagree
/// about how long a node was allowed to live, which is the same class of defect as two derivations
/// of one status.
pub fn effective_timeout(secs: u64) -> StdDuration {
    StdDuration::from_secs(secs.min(MAX_TIMEOUT_SECS))
}

/// How long `program --version` may take before marion records the version as unknown.
///
/// The probe is a second process marion starts on the spawn path, and until it was bounded it was
/// the one step with **no deadline at all**: the child's `timeout_secs` bounds the harness
/// invocation and nothing else, so a harness that answered its run and then hung on `--version`
/// left `run_spawn` unable to return — and, on the background path, left `wait` blocked on a thread
/// that would never finish, which stalls every later frame the bridge would have read.
///
/// Generous against the measurement (a real `--version` answers in milliseconds) and short against
/// the thing it protects: a node's whole run must not be lost to a version string, which is why
/// expiry degrades to `"unknown"` rather than failing the spawn.
const HARNESS_VERSION_TIMEOUT: StdDuration = StdDuration::from_secs(5);

/// `Clone` because a **backgrounded** spawn runs on a thread that outlives the JSON-RPC frame the
/// request arrived on ([`crate::background`]). Borrowing was fine while every spawn was served
/// inside `handle_tool_call`'s own stack frame; a `'static` thread body cannot borrow from it.
#[derive(Clone)]
pub struct SpawnRequest {
    pub agent_type: String,
    pub prompt: String,
    /// **The tree this child branches from** — the argument [`crate::spawn::make_worktree`] takes,
    /// and the one thing about a spawn that is *not* a property of the supervisor serving it.
    ///
    /// It used to live on [`Env`], which was wrong the moment a supervisor stopped being one per
    /// directory. §2 keys a supervisor on `git rev-parse --git-common-dir`, so `/r` and **every
    /// linked worktree of `/r`** reach the same supervisor over the same journal; but
    /// `make_worktree` runs `git -C <repo> rev-parse HEAD`, so a single supervisor-wide `repo`
    /// would branch a feature-worktree root's children off the *other* tree's HEAD — silently,
    /// with a real worktree and a real branch and nothing anywhere reporting a problem.
    ///
    /// So it rides the request, beside the prompt and the scope: one value per spawn, resolved by
    /// whoever knows which tree the spawn is *of*. On the socket that is the caller's own node
    /// entry (`handler::NodeHandle::repo`); in the bridge it is `MARION_REPO`; in `marion run` it
    /// is `--repo`. Putting it here rather than in a sixth argument also means every existing
    /// carrier of a spawn — `background::Background::start`'s owned clone in particular — carries
    /// it already, so there is no second channel that could disagree with this one.
    pub repo: PathBuf,
    pub acceptance_criteria: Vec<String>,
    /// §5.4's `verification`: shell lines, each run by `sh -c` in the child's workspace at its
    /// terminal transition (see [`run_verification`]). Any non-zero exit fails the contract.
    pub verification: Vec<String>,
    pub writable_scope: Vec<String>,
    pub timeout_secs: u64,
    /// The model to run the child on, in marion's request vocabulary. **Optional, with the agent
    /// type's own `model` key as the default** (§3.1) — see [`resolve_model`].
    pub model: Option<String>,
    /// §5.4's `isolation`: **which of §6.6's two workspaces this child gets** (see
    /// [`marion_core::contract::Isolation`]).
    ///
    /// Resolved, not optional. The wire carries an `Option` because absence and a stated value are
    /// different requests, but by the time a spawn is a `SpawnRequest` the question is settled — the
    /// same discipline `timeout_secs` follows one field up, and for the same reason: two call sites
    /// that each turn absence into a value are two places for the default to disagree. `run_spawn`
    /// reads this field and nothing else to decide where the child runs.
    pub isolation: Isolation,
    /// §5.4/§6.6's escape hatch: may this child share a cwd with a live write-capable sibling?
    ///
    /// Only ever `true` under [`Isolation::SharedCwd`] — the request edge refuses the combination
    /// with `worktree`, where there is no sibling to share with. It suppresses the §6.6 occupancy
    /// claim and nothing else; it does not widen scope, and a child that takes it is still judged
    /// against `writable_scope`.
    pub allow_concurrent_writes: bool,
    /// **A resume, or a fresh spawn** — [`crate::root::RootSpec::resume`]'s counterpart on the
    /// child path, and `None` on every `agent/spawn`.
    ///
    /// `Some` makes this launch the **second life of a node that already exists** rather than a new
    /// one: `run_spawn_watched` reuses the recorded id instead of minting one (so the second
    /// `Spawned` lands on the node every earlier record names, which replay folds as generation
    /// two), [`select_workspace`] reuses the recorded workspace instead of cutting a new worktree,
    /// and the session reaches the harness through its row's measured resume flag.
    ///
    /// One field carrying all three, because they are one decision. Splitting it into three
    /// `Option`s would admit the combinations that are not launches at all — an id without a
    /// session, a session without the tree it was created in — and each of those relaunches
    /// *something*, quietly, in the wrong place or under the wrong name.
    pub resume: Option<ChildResume>,
    /// **The profile this launch runs on**, overriding the agent type's `profile` and
    /// `profiles.toml`'s `[default]` — a spawn's own choice, or, on a resume, the profile the
    /// node's session was recorded under. `None` resolves as `profiles::resolve` says.
    pub profile: Option<String>,
    /// **A review, or ordinary work** — `Some` makes this child a reviewer of an ended node
    /// ([`crate::review`]): journaled as its intent's `review_of`, cut at the reviewed work,
    /// launched read-only under an empty writable scope, and its report read into findings.
    /// `None` on every other spawn.
    pub review: Option<crate::review::Target>,
}

/// What a child's second life is reconstructed from — all of it read off the journal, none of it
/// from a caller. See [`SpawnRequest::resume`].
#[derive(Clone)]
pub struct ChildResume {
    /// The node's own id, reused so the relaunch is a second lifetime and not a second node.
    pub agent_id: AgentId,
    /// The harness's own name for the conversation, handed back verbatim.
    pub session: String,
    /// The tree the session was created in, from the node's `SessionObserved`. A harness resumes a
    /// session only from the cwd that created it, and the caller has already proved this one is
    /// still on disk.
    pub workspace: Workspace,
    /// What the node's earlier runs recorded spending (`ReplayedNode::usage`) — the total a row
    /// whose counters run over the session subtracts (`UsageMeter::resumed_from`).
    pub usage: Option<TokenUsage>,
}

/// The node whose `spawn` this is — §6.1 step 2's gates read the **caller's** agent type, never
/// the child's just-resolved one.
///
/// One struct rather than three loose arguments because the three travel together and are wrong
/// apart: `agent_id` stamps `TaskContract.requester` (§9), `agent_type` carries the `max_depth` /
/// `max_concurrent_children` the gates read (§3.1), and `depth` is the position in the tree the
/// child's is derived from. Passing the *child's* type here would make the concurrency gate
/// vacuous — the child has no children yet — which is the mistake
/// [`marion_core::agent_type::check_spawn_gates`]'s signature already exists to prevent.
///
/// A root builds one of these from what `marion run` minted; a child's bridge rebuilds it from the
/// per-server MCP `env` block marion wrote (`marion_harness::AGENT_ID_ENV` and friends), because
/// marion is not the bridge's parent and that declaration is the only channel there is.
#[derive(Debug, Clone)]
pub struct Caller {
    /// `TaskContract.requester` (§9): the caller's own `AgentId`.
    pub agent_id: String,
    pub agent_type: AgentType,
    /// The caller's depth, **root = 0**. The child lands at `depth + 1`.
    pub depth: u32,
    /// **How many children of this caller are live and unreaped right now** — §6.1 step 2's
    /// concurrency gate reads exactly this, and it is a field rather than a constant since
    /// backgrounding landed.
    ///
    /// It was `LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER: u32 = 0`, a constant whose own doc comment
    /// said *"this constant is the one place to revisit when backgrounding lands"*. The reasoning
    /// that justified the zero was true and is now false: `spawn` ran the child to completion
    /// before returning, and the bridge answered one JSON-RPC line at a time, so a caller could
    /// not have a second live child. A backgrounded `spawn` returns while its child runs, so it
    /// can — and 0 is never `>= 4`, which means leaving the constant would have left
    /// `max_concurrent_children` inert on the very change that made it bind.
    ///
    /// It belongs on `Caller` because it is a fact *about the caller*, exactly as `depth` and
    /// `agent_type` are, and §6.1 step 2 reads all three off the same node. Living here also means
    /// [`Caller::root`] answers it once for every test and call site that does not background.
    ///
    /// **The count has two sources, and which one is right depends on who owns the node.** The
    /// argument that used to sit here — that the bridge's own [`crate::background::Background`] is
    /// the complete set by construction, and that a journal read cannot see a child started but
    /// not yet journaled `Spawned` — was true and its second half is now false. §11 item 28 step 1
    /// journals `SpawnIntent` before every side effect, so a child is in the journal from the first
    /// instant it exists. `background.rs`'s module docs carry the whole inversion.
    ///
    /// A caller reached through a **bridge** still fills this from that bridge's table, because
    /// that process follows no journal. A caller reached through the **supervisor's**
    /// `agent/spawn` fills it from the registry (`handler::RegistryHandle::live_children_of`),
    /// which is exact, survives a restart, and can see a sibling this caller's own bridge never
    /// started.
    pub live_children: u32,
}

impl Caller {
    /// The root of a tree: depth 0, which §6.1 step 2 says the gates are simply inapplicable to
    /// (it has no parent to have been gated by). Its own `spawn` is gated like anyone else's.
    pub fn root(agent_id: impl Into<String>, agent_type: AgentType) -> Self {
        Self {
            agent_id: agent_id.into(),
            agent_type,
            // The same constant `root::prepare` writes into the root's own declaration, not a
            // second literal beside it: two spellings of "the root is 0" could disagree.
            depth: crate::depth::ROOT_DEPTH,
            // A caller that is not going through a bridge has no background table to count, and a
            // `marion run` root calling this has not spawned anything yet. Zero is the measurement,
            // not the old constant's assumption: the bridge overwrites it from
            // `Background::live_children` on every `spawn` it serves.
            live_children: 0,
        }
    }
}

/// Where `LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER` went, since §11 item 23, `MILESTONES.md` and
/// `spawn::SpawnError` all cite that name and a reader grepping it should land somewhere.
///
/// It was a `u32 = 0` whose own doc said *"this constant is the one place to revisit when
/// backgrounding lands"*. Backgrounding landed; the count is now [`Caller::live_children`], read
/// from the bridge's [`crate::background::Background`] table, and that field carries the reasoning.
///
/// `Clone` here is part of the same change: a backgrounded child's thread owns its environment
/// outright rather than borrowing the bridge's stack frame.
///
/// **Everything here is a property of the supervisor, not of a spawn.** That is now a checkable
/// claim rather than a habit: `repo` used to sit at the top of this struct and it was the one field
/// that failed the test — see [`SpawnRequest::repo`] for what a supervisor-wide repository does to
/// a linked worktree's children. What is left is derivable by a detached stage 3 from what §5.7
/// already tells it (`project_dir` from `(state, project root)`, `bridge` from
/// `current_exe`) or is carried to it explicitly on argv (`base_url`, `auth`).
#[derive(Clone)]
pub struct Env {
    pub project_dir: ProjectDir,
    /// `<state>` of §4.3 — the directory [`Self::project_dir`] is a `<project-hash>` inside.
    ///
    /// **Carried rather than recovered from `project_dir.path().parent()`.** That derivation is
    /// true by construction today and is exactly the kind of fact a later `ProjectDir` constructor
    /// could quietly falsify, and what depends on it is not internal: it is `MARION_STATE_DIR` in every
    /// node's MCP declaration (`marion_harness::SpawnCtx::state_dir`), which is how that node's own
    /// bridge finds this project again. A supervisor is told `<state>` on its argv (`detach::Launch`)
    /// and a bridge reads it from its declaration, so both already have it; this is where the two
    /// meet.
    pub state: PathBuf,
    /// **The project root this supervisor serves** — §2's key, the git common dir. Carried so a
    /// `node/resume` can reconstruct a lost **root**'s launch, whose cwd and worktree base is
    /// exactly this directory. It is **not** a spawn input: a child's tree is its own
    /// [`SpawnRequest::repo`] resolved from the caller's node entry (a linked worktree branches its
    /// children off its own HEAD, not this one), so nothing on the `agent/spawn` path reads it.
    pub project_root: PathBuf,
    pub bridge: PathBuf,
    /// The canned provider's base URL, `None` where marion overrides no endpoint (`Auth::Inherited`)
    /// and each harness resolves its own.
    pub base_url: Option<String>,
    /// Whether children present a credential marion minted or the operator's own login.
    pub auth: Auth,
}

pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    /// A pipe was still open when the drain bound expired, so the captured bytes are a prefix of
    /// what the child wrote. Recorded, never silent (design §6.7).
    pub capture_truncated: bool,
}

/// The credential a spawned child presents to marion's own endpoint.
///
/// Not a secret and not checked by anything: the endpoint is marion's canned provider or its proxy,
/// which authenticates nothing. It exists because a harness can refuse to *start* over an absent
/// credential — see the `api_key` field in [`run_spawn`]'s `LaunchSpec` — and because codex's
/// generated config names a variable that has to hold something (`env_key =
/// "MARION_PROVIDER_KEY"`, compiled from this field by codex's row).
pub const PLACEHOLDER_API_KEY: &str = "dummy";

/// A pipe drain that can be stopped while the pipe is still open.
///
/// The thread never blocks in `read`: it waits in `poll(2)` on the pipe **and on its stop flag's
/// descriptor**, with no timeout, and reads only bytes it knows are there. So an idle drain costs
/// no wakeups, a stop request is honoured the moment it is made, and the thread *exits* — it is not
/// detached and left wedged. That matters because `spawn` is called repeatedly by a long-lived
/// supervisor: one leaked thread (and one leaked fd, and its buffer) per timed-out spawn would be
/// its own unbounded leak, traded for the hang it fixed.
pub(crate) struct Drain {
    handle: thread::JoinHandle<(Vec<u8>, bool)>,
    stop: Arc<crate::wake::Flag>,
    /// Closed by the thread as it returns, so [`Self::finish`] waits for it on a channel with the
    /// deadline as its only timeout.
    done: std::sync::mpsc::Receiver<()>,
}

impl Drain {
    pub(crate) fn start<R: Read + AsRawFd + Send + 'static>(pipe: R) -> Self {
        Self::start_with_lines(pipe, None, None)
    }

    /// [`Self::start`], and every **complete line** forwarded on `lines` as it lands — the live
    /// seam the `LaunchOnly` path otherwise lacks. The drain thread only forwards; whoever holds the
    /// receiver reads it on its own thread, so nothing a caller does with a line has to be `Send`.
    /// Bytes after the last newline are forwarded at EOF, so a stream whose final frame has no
    /// trailing newline is not read one frame short. The whole capture is still returned by
    /// [`Self::finish`]: the lines are a copy, not a diversion.
    ///
    /// `wake`, when given, is rung after every forward (and at the end), so a caller waiting in its
    /// own `poll` for lines learns of them at once.
    pub(crate) fn start_with_lines<R: Read + AsRawFd + Send + 'static>(
        mut pipe: R,
        lines: Option<std::sync::mpsc::Sender<String>>,
        wake: Option<Arc<crate::wake::Pipe>>,
    ) -> Self {
        let stop = Arc::new(crate::wake::Flag::new());
        let flag = Arc::clone(&stop);
        let (done_tx, done) = std::sync::mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            // Dropped on every return, which is what `finish` waits for.
            let _done = done_tx;
            let ring = || {
                if let Some(wake) = &wake {
                    wake.wake();
                }
            };
            // SAFETY: `pipe` owns the descriptor for the whole of this thread.
            let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(pipe.as_raw_fd()) };
            let mut bytes = Vec::new();
            let mut buf = [0u8; 8192];
            // The start of the first byte not yet forwarded as part of a line.
            let mut forwarded = 0usize;
            loop {
                if flag.load(Ordering::SeqCst) {
                    // Abandoned with the pipe still open: what we have is a prefix.
                    return (bytes, false);
                }
                // Readable, hung up, errored, or the stop was raised. An interrupted wait reports
                // nothing ready and comes round again.
                if !crate::wake::wait_until(&[Some(fd), flag.fd()], None)[0] {
                    continue;
                }
                // Readable, hung up, or errored. Only `read` can tell the three apart, and with a
                // single reader it cannot block now.
                match pipe.read(&mut buf) {
                    Ok(0) => {
                        // EOF: every write end is closed.
                        forward_lines(lines.as_ref(), &bytes, &mut forwarded, true);
                        ring();
                        return (bytes, true);
                    }
                    Ok(n) => {
                        bytes.extend_from_slice(&buf[..n]);
                        if forward_lines(lines.as_ref(), &bytes, &mut forwarded, false) {
                            ring();
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return (bytes, false),
                }
            }
        });
        Self { handle, stop, done }
    }

    /// Collect the drained bytes, waiting no later than `deadline` for a natural EOF.
    ///
    /// Returns `(bytes, complete)`; `complete` is false exactly when the pipe was still open at the
    /// deadline, i.e. when the capture is a prefix. The wait is one receive on the thread's
    /// completion channel, bounded by what is left of the deadline; a stop raised past it wakes the
    /// thread's `poll`, so the join is immediate.
    pub(crate) fn finish(self, deadline: Instant) -> (Vec<u8>, bool) {
        let _ = self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()));
        self.stop.store(true, Ordering::SeqCst);
        self.handle.join().unwrap_or((Vec::new(), false))
    }
}

/// Forward every complete line in `bytes[*forwarded..]` on `lines`, advancing `forwarded` past
/// them. At EOF the bytes after the last newline are forwarded too, so a stream whose final frame
/// has no trailing newline is not read one frame short. A `None` sender forwards nothing. Whether
/// anything was forwarded.
fn forward_lines(
    lines: Option<&std::sync::mpsc::Sender<String>>,
    bytes: &[u8],
    forwarded: &mut usize,
    at_eof: bool,
) -> bool {
    let Some(tx) = lines else { return false };
    let from = *forwarded;
    while let Some(nl) = bytes[*forwarded..].iter().position(|b| *b == b'\n') {
        let line = &bytes[*forwarded..*forwarded + nl];
        let _ = tx.send(String::from_utf8_lossy(line).into_owned());
        *forwarded += nl + 1;
    }
    if at_eof && *forwarded < bytes.len() {
        let _ = tx.send(String::from_utf8_lossy(&bytes[*forwarded..]).into_owned());
        *forwarded = bytes.len();
    }
    *forwarded > from
}

/// Run `command` to completion or to `timeout`, whichever comes first, killing its whole
/// descendant tree on expiry. Public because the end-to-end test bounds a real `marion run` with
/// it: a test that leaked a `claude`, a `codex` and their tool-call grandchildren would be the
/// very failure S7 exists for, and this is the one implementation in the workspace that has been
/// measured to leave no survivors.
pub fn run_bounded(
    command: &mut SysCommand,
    timeout: StdDuration,
) -> Result<CommandOutput, SpawnError> {
    run_bounded_with(command, timeout, kill_process_tree, None, None)
}

/// §6.7's default bound for one verification command: killed on expiry with `timed_out: true`.
pub const VERIFICATION_TIMEOUT: StdDuration = StdDuration::from_secs(300);

/// §5.4's `verification` lines as the [`Command`]s the contract records: each line is one
/// `sh -c <line>` in `cwd`, under [`VERIFICATION_TIMEOUT`], in the parent's order.
///
/// Built before anything runs, so the contract carries what was *asked for* even where nothing
/// ran (a timed-out child gets no verification, see `run_spawn`) — "asked for and never run" and
/// "never asked for" are the two answers the old hardcoded `verification: vec![]` could not tell
/// apart.
pub fn verification_commands(lines: &[String], cwd: &Path) -> Vec<Command> {
    lines
        .iter()
        .map(|line| Command {
            program: "sh".into(),
            args: vec!["-c".into(), line.clone()],
            cwd: cwd.to_path_buf(),
            timeout: Duration::from_secs(VERIFICATION_TIMEOUT.as_secs()),
        })
        .collect()
}

/// Run `commands` one after another, in order, each through [`run_bounded`] so an expired one is
/// killed with its whole process group. Sequential on purpose: `cargo build` before `cargo test`
/// is the ordinary shape, and a later line must see what an earlier one wrote.
///
/// Every stream is recorded `Capped::whole`: the persisted contract is the uncapped record
/// (`m1_hop.rs` asserts it) and `cap_for_return` alone shortens the copy handed back. A command
/// that could not be started at all is an outcome too — `exit_code: None`, the error in `stderr` —
/// never a gap in the evidence, which would read as "one fewer command was asked for".
pub fn run_verification(commands: &[Command]) -> Vec<CommandOutcome> {
    commands
        .iter()
        .map(|command| {
            let started = Instant::now();
            let mut sys = SysCommand::new(&command.program);
            sys.args(&command.args).current_dir(&command.cwd);
            let (exit_code, stdout, stderr, timed_out) =
                match run_bounded(&mut sys, command.timeout.0) {
                    Ok(out) => (
                        out.code,
                        String::from_utf8_lossy(&out.stdout).into_owned(),
                        String::from_utf8_lossy(&out.stderr).into_owned(),
                        out.timed_out,
                    ),
                    Err(e) => (None, String::new(), e.to_string(), false),
                };
            CommandOutcome {
                command: command.clone(),
                exit_code,
                stdout: Capped::whole(stdout),
                stderr: Capped::whole(stderr),
                duration: Millis(started.elapsed()),
                timed_out,
            }
        })
        .collect()
}

/// §5.4's evidence for one child: [`run_verification`] over `commands`, or nothing at all for a
/// child marion cut short.
///
/// **The condition is `timed_out`, and only that.** It is the one field both child drivers set
/// exactly when marion ended the run for its own reasons: `run_bounded` sets it on the deadline
/// kill of a pty or headless child, and `acp_child::finish` sets it from `marion_cut_it_short`,
/// the caller's attributed statement that the turn did not finish. Such a workspace is whatever
/// the kill left, and evidence gathered over it would be judged against work that never finished.
///
/// `signal` is deliberately *not* consulted. This used to skip on `signal.is_some()` as well, and
/// that silently disabled verification for every ACP child: an ACP agent is a long-lived stdio
/// server that marion itself shuts down after the turn — stdin EOF, then SIGINT, then the group
/// kill — so its exit carries a signal on every ordinary run (`AcpRun::exit` documents that the
/// status describes marion's shutdown, not the turn). A live opencode child finished `Ok` with
/// `evidence: []` beside three verification lines. For a pty child a signal that is not marion's
/// timeout is something else's kill; the commands still run over what the child left, and their
/// outcomes are recorded so the contract judges the work rather than guessing at it.
pub fn verification_evidence(outcome: &ChildOutcome, commands: &[Command]) -> Vec<CommandOutcome> {
    if outcome.timed_out {
        vec![]
    } else {
        run_verification(commands)
    }
}

/// [`run_bounded`], plus the pid of the process it started, handed over at the instant it exists,
/// and each stdout line as it lands.
///
/// The counterpart of [`crate::duplex::DuplexSpec::on_started`], and it exists for exactly the same
/// reason: §6.1 step 7's confirmation belongs to the caller, but only this function knows the pid
/// and only this function knows when there is one. A separate entry point rather than a fourth
/// parameter on [`run_bounded`] — that signature is public, has callers outside `run_spawn`, and
/// widening it would make every one of them state an absence they have nothing to say about.
///
/// `on_line` is the `LaunchOnly` path's one live seam, and it exists for a node that never reaches
/// its capture: the harness names its session in its first frame, and a node whose supervisor is
/// lost mid-run is exactly the node a resume needs that name for. Called on the caller's thread,
/// between polls of the child, so the capture returned afterwards is still whole.
/// A live stdout line's hook: `Break` ends the run now (marion kills the tree).
pub(crate) type LineHook<'a> = &'a dyn Fn(&str) -> ControlFlow<()>;

pub(crate) fn run_bounded_watched(
    command: &mut SysCommand,
    timeout: StdDuration,
    on_started: &dyn Fn(i32),
    on_line: Option<LineHook<'_>>,
) -> Result<CommandOutput, SpawnError> {
    run_bounded_with(
        command,
        timeout,
        kill_process_tree,
        Some(on_started),
        on_line,
    )
}

/// `run_bounded` with the expiry kill injected, so tests can run the path where the sweep *fails*
/// to reach an escapee — the case whose liveness must not depend on the sweep.
fn run_bounded_with(
    command: &mut SysCommand,
    timeout: StdDuration,
    kill_tree: fn(i32),
    on_started: Option<&dyn Fn(i32)>,
    on_line: Option<LineHook<'_>>,
) -> Result<CommandOutput, SpawnError> {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE.spawn(command)?;
    // **Before the pipes are drained, let alone before the process is waited on.** Everything below
    // this line can block for the child's whole lifetime, so a hook called anywhere else would be
    // reporting the existence of a process the caller had already finished with — see
    // [`run_bounded_watched`].
    if let Some(started) = on_started {
        started(child.id() as i32);
    }
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    // The line channel exists only when someone listens; a drain nobody reads would otherwise
    // buffer the whole stream twice.
    let (lines_tx, lines_rx) = match on_line {
        Some(_) => {
            let (tx, rx) = std::sync::mpsc::channel();
            (Some(tx), Some(rx))
        }
        None => (None, None),
    };
    // Rung by the stdout drain after it forwards lines, so the wait below wakes for them. Only
    // when someone listens; `None` inside if no descriptor could be had (lines are then looked at
    // on the degraded re-check).
    let lines_wake = on_line.map(|_| crate::wake::Pipe::new().ok().map(Arc::new));
    let stdout_drain = Drain::start_with_lines(stdout, lines_tx, lines_wake.clone().flatten());
    let stderr_drain = Drain::start(stderr);
    // `Break` from the hook: the caller read a line that ends the run now. Every line is still
    // delivered, so the live view misses nothing said before the kill.
    let deliver_lines = || {
        let mut stop = false;
        if let (Some(hook), Some(rx)) = (on_line, &lines_rx) {
            for line in rx.try_iter() {
                stop |= hook(&line).is_break();
            }
        }
        stop
    };

    // **`checked_add`, because the child is already running by this line.** `Instant + Duration`
    // panics on overflow, and every escape from this function from here on abandons the process
    // started four lines above — `Child`'s `Drop` does not kill it. `run_spawn` clamps its caller's
    // number to [`MAX_TIMEOUT_SECS`] before it ever gets here, so this is unreachable through the
    // bridge; it is kept because `run_bounded` is `pub`, has callers that are not `run_spawn` (the
    // end-to-end test bounds a real `marion run` with it), and a public function whose liveness
    // depends on an invariant enforced by one of its callers is a defect waiting for the second one.
    //
    // The fallback is the cap rather than `Instant::now()`: saturating to *now* would kill a
    // healthy child instantly, turning an unrepresentable request into the most aggressive possible
    // answer, and saturating to "never" would leave the process unbounded — the failure this whole
    // function exists to prevent. The cap is the only choice that is both finite and honest.
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(|| Instant::now() + StdDuration::from_secs(MAX_TIMEOUT_SECS));
    // Event-driven: the wait between looks ends on the child's exit, a forwarded line, or the
    // deadline — never on a timer.
    let exit = crate::wake::ProcExit::new(child.id() as i32);
    let mut exited = None;
    let (status, timed_out) = loop {
        if let Some(Some(wake)) = &lines_wake {
            wake.drain();
        }
        if deliver_lines() {
            kill_tree(child.id() as i32);
            break (child.wait()?, false);
        }
        if let Some(status) = exited {
            break (status, false);
        }
        if Instant::now() >= deadline {
            kill_tree(child.id() as i32);
            break (child.wait()?, true);
        }
        let lines = lines_wake
            .as_ref()
            .map(|wake| wake.as_ref().map(|w| w.fd()));
        exited = crate::wake::step_child(&mut child, &exit, lines.as_slice(), Some(deadline))?;
    };
    // The child is reaped; anything still holding a write end is an escapee. One deadline for both
    // drains, so the total wait is `DRAIN_GRACE`, not twice it.
    let drain_deadline = Instant::now() + DRAIN_GRACE;
    let (stdout, stdout_complete) = stdout_drain.finish(drain_deadline);
    let (stderr, stderr_complete) = stderr_drain.finish(drain_deadline);
    // Whatever landed between the last poll and the drain's end, including a final unterminated
    // line: the live view sees every line the capture does.
    let _ = deliver_lines();
    Ok(CommandOutput {
        stdout,
        stderr,
        code: status.code(),
        signal: status.signal(),
        timed_out,
        capture_truncated: !(stdout_complete && stderr_complete),
    })
}

/// Append the drain-bound truncation to a `ProcessExit.description`, keeping what
/// `build_contract` already derived. §6.7's rule for caps — shortening is always *recorded* — read
/// onto the one shortening that happens outside the cap machinery.
fn note_truncated_capture(description: &str) -> String {
    format!(
        "{description}; output capture truncated: a pipe was still held open {} s after the child \
         was reaped, so stdout/stderr are a prefix",
        DRAIN_GRACE.as_secs()
    )
}

/// **The node is over: its contract to disk, then the journal's terminal record.**
///
/// One function because the two are one rule, and a rule spread over two statements in the middle of
/// a five-hundred-line launcher is a rule that gets reordered by someone fixing something else — which
/// is how it came to be the wrong way round. Named, it can also be tested, which the inline version
/// could not be: the difference between the right order and the wrong one is purely temporal, so the
/// only way to see it is from inside the window (see [`at_contract_write`]).
///
/// **`ended_by_kill` leaves the terminal record to the kill.** A node marion ended on an operator's
/// kill already has one coming — the killer's `KillConfirmed`, §6.7's `Exited(Cancelled)` — and an
/// `Exited` written here as well would be a second terminal record for one end, folding in over the
/// confirmation in whichever order the two happened to land. See [`SpawnObserver::process_ended`].
fn persist_contract_then_record_exit(
    project_dir: &ProjectDir,
    agent_dir: &AgentDir,
    agent_id: &AgentId,
    contract: &TaskContract,
    ended_by_kill: bool,
) -> Result<TaskContract, SpawnError> {
    let persisted = persist_then_cap(agent_dir, contract);
    if let Some(completion) = contract.completion.as_ref().filter(|_| !ended_by_kill) {
        crate::journal::record(
            project_dir,
            RecordKind::Exited(Exited {
                agent_id: agent_id.clone(),
                status: completion.status,
                exit: completion.exit.clone(),
            }),
        );
    }
    persisted
}

fn persist_then_cap(agent: &AgentDir, contract: &TaskContract) -> Result<TaskContract, SpawnError> {
    // **The one instant the ordering rule is about.** See [`at_contract_write`]: the test that
    // guards `Exited`-after-the-contract reads the journal from here, because the window is far
    // too narrow to sample from outside.
    #[cfg(test)]
    at_contract_write::fire();
    let mut body = serde_json::to_vec_pretty(contract)?;
    body.push(b'\n');
    crate::private_fs::write_atomic(&agent.contract(&contract.task_id), &body)?;
    Ok(cap_for_return(contract.clone()))
}

/// **The instant the contract is about to be written**, so the ordering around it can be *observed*
/// rather than raced for.
///
/// The rule [`persist_contract_then_record_exit`] enforces is temporal: the journal's terminal
/// record must not exist yet at this point. In production the window is one `create` plus one
/// `write`, far too narrow to sample from outside — a test that polled would pass against the
/// broken order most of the time, and a test that usually passes against the defect is worse than
/// no test. So the seam offers the one instant that matters and the test looks at the journal from
/// inside it.
///
/// **Thread-local, not process-wide**, for the reason `handler.rs`'s injected panic gives and one
/// more: the lib tests share one process and run in parallel, so a hook that outlived its test
/// would fire inside an unrelated spawn — and a hook installed in a `static` fires inside an
/// unrelated spawn *while its own test is still running*. That second case is not hypothetical. A
/// global hook was invoked by every other test's contract write, on that test's thread, against
/// *this* test's journal; once this test's own terminal record was on disk any such foreign firing
/// latched the observation `true` and failed the assertion, so the rule's guard flaked under load
/// precisely because it was watching the whole process instead of its own call. The observer
/// belongs to the thread that performs the write it is about. It clears on drop, including on an
/// unwind.
#[cfg(test)]
pub(crate) mod at_contract_write {
    use std::cell::RefCell;

    type Hook = Box<dyn Fn() + 'static>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub(crate) fn fire() {
        HOOK.with(|h| {
            if let Some(f) = h.borrow().as_ref() {
                f();
            }
        });
    }

    pub(crate) struct Installed(());

    impl Drop for Installed {
        fn drop(&mut self) {
            HOOK.with(|h| *h.borrow_mut() = None);
        }
    }

    pub(crate) fn install(f: impl Fn() + 'static) -> Installed {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
        Installed(())
    }
}

/// Which model a spawn runs on: **the request, else the agent type's default, else none**.
///
/// Optional rather than required, and this is the one decision here with a real alternative. Making
/// it required would have forced every caller — including the two harnesses that have always run
/// without one — to name a model, changing `codex exec`'s measured argv for nothing and giving
/// `spawn`'s schema a mandatory field two of its four harnesses cannot use. Optional-with-a-default
/// keeps codex and claude-code byte-identical (both resolve to `None`, exactly what they passed
/// before) while making gemini and opencode launchable without the caller having to know which
/// harness needs what.
///
/// It follows §3.1's precedent for tools rather than inventing one. Tool names are *"marion's
/// vocabulary, and the mapping is part of the adapter contract"*; so is this. The name here is what
/// the request or the type asked for, in marion's terms; the **adapter** maps it to that harness's
/// own spelling — a bare id for gemini's `-m`, a `provider/model` pair that opencode's argv and its
/// generated provider block must both repeat, `--model` for Claude Code, and *nothing at all* for
/// codex, whose `exec` surface takes no model argument. And, as with `allowed_tools`, the contract
/// records **the compiled value, not the asked-for one**: `TaskContract.child.model` is read off
/// the compiled `Invocation`, so a codex contract records `None` however loudly a caller asked.
///
/// On a live run the type's default is [`AgentType::default_model`]'s: one that names marion's
/// canned plumbing is dropped, so the harness uses the operator's own default model.
///
/// [`AgentType::default_model`]: marion_core::agent_type::AgentType::default_model
fn resolve_model(
    req: &SpawnRequest,
    agent_type: &marion_core::agent_type::AgentType,
    auth: Auth,
) -> Option<String> {
    req.model
        .clone()
        .or_else(|| agent_type.default_model(auth == Auth::Canned))
}

/// The `request_id` a node's `initialize` goes out with — **one derivation, three readers**.
///
/// The driver waits on it (`DuplexSpec::init_id`), and `events::EventSink` needs the *same* string
/// to recognise the one `control_response` §5.2 forbids journaling verbatim. Two spellings of this
/// would mean the sink withholding a frame the driver never sent, or — worse, and silently — not
/// withholding the one it did.
pub(crate) fn init_request_id(agent_id: &AgentId) -> String {
    format!("marion-init-{}", agent_id.0)
}

/// Ask a harness what version it is, **under a deadline**, degrading to `"unknown"` rather than
/// hanging.
///
/// Driven through [`run_bounded`] rather than `Command::output` for the one property `output` does
/// not have: `output` blocks until the process closes its pipes, with no bound and no way out. A
/// harness that hangs here — or that forks something holding the write end — stalled `run_spawn`
/// forever, after the child had already run and been reaped. `run_bounded` gives it
/// [`HARNESS_VERSION_TIMEOUT`] and kills its whole process group on expiry, which is the same
/// treatment the child itself gets and for the same reason.
///
/// A timed-out, failed or refused probe is `"unknown"`, never an error: the version is a field in
/// an audit record, and losing a whole node's contract because a version string did not arrive
/// would trade a large truth for a small one.
///
/// **Read once per binary, not once per spawn** ([`VersionCache`]): a fan-out of twenty children
/// of one harness used to fork twenty `--version` probes of the same unchanged file.
pub(crate) fn harness_version(program: &str, harness: marion_core::harness::Harness) -> String {
    let identity = BinaryIdentity::of(program);
    if let Some(version) = identity
        .as_ref()
        .and_then(|id| VERSION_CACHE.get(id, harness))
    {
        return version;
    }
    let version = probe_version(program, harness);
    if let (Some(id), Some(v)) = (identity, &version) {
        VERSION_CACHE.put(id, harness, v.clone());
    }
    version.unwrap_or_else(|| "unknown".into())
}

/// One bounded `--version` probe, uncached: the version line, or `None` for anything else.
fn probe_version(program: &str, harness: marion_core::harness::Harness) -> Option<String> {
    let mut probe = version_probe(program, harness).ok()?;
    run_bounded(&mut probe.command, HARNESS_VERSION_TIMEOUT)
        .ok()
        .filter(|o| !o.timed_out && o.code == Some(0))
        .and_then(|o| version_line(&String::from_utf8_lossy(&o.stdout)))
}

/// `<program> --version` as every marion probe runs it: under the harness row's no-self-update
/// switch, whatever its shape ([`marion_harness::probe`]) — or not at all, where the row's binary
/// may update itself and no switch is known. Without it copilot 1.0.83 downloads a newer build and
/// answers with *that* version, and gemini 0.53.0 updated the operator's install. The doctor and
/// every spawn's audit probe build their command here; keep the returned probe alive until the
/// child exits, since it owns a document switch's settings file.
pub(crate) fn version_probe(
    program: impl AsRef<std::ffi::OsStr>,
    harness: marion_core::harness::Harness,
) -> Result<marion_harness::probe::VersionProbe, marion_harness::probe::ProbeError> {
    marion_harness::probe::version_probe(marion_harness::adapter::harness_spec(harness), program)
}

/// Which file a program names, and which *contents* of it: a binary replaced on disk is a
/// different identity, so its version is read again.
///
/// The canonical path (symlinks resolved, so an installer's `current` link moving to a new release
/// is a new path) plus the file's inode, size, mtime and ctime. mtime alone is not enough: npm
/// extracts every package file with the same fixed mtime, so two releases' same-sized entry
/// scripts would collide; a replaced file has a new inode, and any write moves ctime, which no
/// installer can set back.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BinaryIdentity {
    path: std::path::PathBuf,
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl BinaryIdentity {
    /// Resolved the way the probe will resolve it — through this process's `PATH` for a bare
    /// name. `None` where the program cannot be found; such a probe runs uncached.
    fn of(program: &str) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let path = crate::doctor::which(program)?;
        let m = std::fs::metadata(&path).ok()?;
        Some(Self {
            path,
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        })
    }
}

/// The process-wide cache behind [`harness_version`]: one entry per `(path, harness)`, holding
/// the identity it was read at. Keyed by path and harness rather than by the whole identity so a
/// replaced binary **overwrites** its entry — the map is bounded by the distinct harness programs
/// this process has run, never by how often they changed. The harness is in the key because the
/// probe itself is the row's: the same file probed as two harnesses runs two different commands.
///
/// Only a version actually read is stored. A timeout or a failed exit is re-probed next time, so
/// a binary that was slow once under load is not recorded as `"unknown"` for the process's life.
/// The lock is never held across a probe; two concurrent first spawns may both probe, and agree.
static VERSION_CACHE: VersionCache = VersionCache(std::sync::LazyLock::new(Default::default));

type VersionKey = (std::path::PathBuf, marion_core::harness::Harness);

struct VersionCache(
    std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<VersionKey, (BinaryIdentity, String)>>,
    >,
);

impl VersionCache {
    fn get(&self, id: &BinaryIdentity, harness: marion_core::harness::Harness) -> Option<String> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&(id.path.clone(), harness))
            .filter(|(seen, _)| seen == id)
            .map(|(_, version)| version.clone())
    }

    fn put(&self, id: BinaryIdentity, harness: marion_core::harness::Harness, version: String) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.insert((id.path.clone(), harness), (id, version));
    }
}

/// The line of a `--version` output that names the version: the first non-blank one, trimmed.
///
/// Generic across every harness rather than a per-harness branch — the version is the first thing
/// each of them prints, and anything after it is advice (`copilot` follows
/// `GitHub Copilot CLI 1.0.83.` with `Run 'copilot update' to check for updates.`). Before this the
/// contract's `child.version` carried the whole output, so a copilot child's audit record named
/// its version in two lines. `None` where nothing was printed at all.
fn version_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// What one child run produced, in the two terms `build_contract` needs: what the harness wrote,
/// and what marion observed of the process. One shape for both launch paths, so nothing downstream
/// has to know which one ran.
struct ChildRun {
    stdout: String,
    stderr: String,
    exit: ChildExit,
    capture_truncated: bool,
    /// `tool_name` of each permission marion denied on this child's behalf. Always empty on the
    /// `LaunchOnly` path, which has no control plane for an ask to arrive on; populated on the
    /// duplex path, where Claude Code really does ask (see [`duplex_child`]). Carried out here
    /// because `run_spawn` journals them — before this field existed `DuplexOutcome`'s copy was
    /// dropped on the floor and a child's denial appeared in no contract and no journal record.
    denied_permissions: Vec<String>,
    /// marion ended the run itself on a frame that said it had failed already — a refused
    /// credential the harness was retrying ([`marion_harness::HarnessAdapter::auth_refusal`]) —
    /// with the harness's words. `None` for a run that ended on its own or on its bound.
    stopped: Option<String>,
}

/// **A run marion ended on a refused credential failed, and says so**: where the stream made no
/// failure claim of its own, marion's reason is the claim, so the contract reads `Failed` in the
/// harness's words rather than a bare signal.
fn note_stopped(outcome: &mut ChildOutcome, run: &ChildRun) {
    if outcome.failure.is_none() {
        outcome.failure = run.stopped.clone().map(stopped_words);
    }
}

/// What a run marion ended on a refused credential says of itself.
pub(crate) fn stopped_words(why: String) -> String {
    format!("marion ended the run: the harness was retrying a refused credential ({why})")
}

/// **Every credential an endpoint node held, removed from what it wrote**: the provider's key, and on
/// a translated route the gateway's bearer the harness was handed instead.
fn redact_run(
    run: &mut ChildRun,
    endpoint: Option<&crate::endpoint::Endpoint>,
    gateway: Option<&crate::gateway::Gateway>,
) {
    let held = endpoint
        .and_then(|e| e.key.as_ref())
        .into_iter()
        .chain(gateway.map(|g| g.bearer()));
    for key in held {
        run.stdout = crate::endpoint::redact(&run.stdout, key.expose());
        run.stderr = crate::endpoint::redact(&run.stderr, key.expose());
    }
}

/// What a failed attempt relaunches on, where it relaunches at all.
pub(crate) enum Next {
    /// The endpoint's next stated API key.
    Credential(crate::endpoint::Endpoint),
    /// The next profile the agent type listed, by index into the node's profiles.
    Profile(usize),
}

/// **The node's billing**: an endpoint node spends an API key, and every other node the operator's
/// own login — a subscription, whether or not it runs on a profile.
pub(crate) fn billing(endpoint: Option<&crate::endpoint::Endpoint>) -> marion_harness::Billing {
    match endpoint {
        Some(_) => marion_harness::Billing::ApiKey,
        None => marion_harness::Billing::Subscription,
    }
}

/// **What a finished attempt said about why it failed** — the one classifier, read through the
/// row ([`marion_harness::HarnessAdapter::failure_cause`]): its stderr, its stream's own failure
/// claim and the provider errors the row's grammar reads off its stream, with the node's billing.
/// A run killed on its wall clock while the harness retried is read the same way: the retries
/// said why (claude's `api_retry` carries the status on every attempt).
fn attempt_cause(
    adapter: &(dyn marion_harness::HarnessAdapter + Send + Sync),
    run: &ChildRun,
    stream_failure: Option<&str>,
    endpoint: Option<&crate::endpoint::Endpoint>,
) -> Option<marion_core::contract::FailureCause> {
    let said = format!("{}\n{}", run.stderr, stream_failure.unwrap_or_default());
    adapter.failure_cause(&said, &run.stdout, billing(endpoint))
}

/// **A child attempt, as the relaunch policy reads it**: the cause it failed on, from the one
/// classifier — or `None` for any run that must stand as it is.
///
/// A child is a relaunch candidate when every one of these holds: the process ended on its own,
/// with wall clock left to spend; it failed (a nonzero exit or a stream that says so) with no
/// report and nothing changed in its worktree — the evidence available that no turn of it
/// succeeded. A root's evidence is read by `root`'s own gatherer; both then go to [`relaunch_on`].
#[allow(clippy::too_many_arguments)]
fn next_attempt(
    endpoint: Option<&crate::endpoint::Endpoint>,
    profiles: &crate::profiles::Launch,
    at: usize,
    run: &ChildRun,
    adapter: &(dyn marion_harness::HarnessAdapter + Send + Sync),
    wt: &Path,
    base: Option<&Oid>,
    remaining: StdDuration,
) -> Option<(Next, marion_core::contract::FailureCause)> {
    if run.exit.timed_out || remaining.is_zero() {
        return None;
    }
    let stream = adapter.parse_stream(&run.stdout, run.exit);
    // A run marion stopped on a refused credential failed, whatever exit its driver reports (a
    // typed driver's turn exit is the session's, code 0).
    let failed = run.exit.code != Some(0) || stream.failure.is_some() || run.stopped.is_some();
    if !failed || stream.narrative.is_some() {
        return None;
    }
    if base.is_some_and(|b| changed_paths(wt, b).map_or(true, |c| !c.is_empty())) {
        return None;
    }
    let cause = attempt_cause(adapter, run, stream.failure.as_deref(), endpoint)?;
    Some((relaunch_on(endpoint, profiles, at, &cause)?, cause))
}

/// **The one relaunch policy**, for a child and a root alike: given an attempt that failed before
/// any turn of it succeeded, and the cause the one classifier read, what it relaunches on — `None`
/// where it must stand as it ended.
///
/// * an **endpoint** node rotates to its next stated API key on a rate limit, a refused key or an
///   outage — each credential tried once;
/// * a node on the operator's **own login** fails over to the next profile its type listed on an
///   auth failure only (a root lists one profile, so never);
/// * a **usage limit** relaunches nothing, on either: it is reported and never worked around.
pub(crate) fn relaunch_on(
    endpoint: Option<&crate::endpoint::Endpoint>,
    profiles: &crate::profiles::Launch,
    at: usize,
    cause: &marion_core::contract::FailureCause,
) -> Option<Next> {
    use marion_core::contract::FailureCause;
    match (endpoint, cause) {
        (
            Some(ep),
            FailureCause::RateLimit { .. }
            | FailureCause::Auth { .. }
            | FailureCause::Outage { .. },
        ) if !ep.fallbacks.is_empty() => {
            // A store that cannot be read now is no reason to lose the failed run's own record:
            // the run stands as it ended, and the failure it recorded says why.
            Some(Next::Credential(
                crate::endpoint::next_for_launch(ep).ok().flatten()?,
            ))
        }
        (None, FailureCause::Auth { .. }) => Some(Next::Profile(profiles.next_after(at)?)),
        _ => None,
    }
}

/// **The readiness marker a relaunch must not inherit.** The failed attempt's bridge touched it; left
/// in place, it would let the prompt reach the next attempt before that attempt's own bridge has
/// the tool list. Found on the profile failover (`ccf06a3`); the credential rotation relaunches the
/// same way and needs the same.
pub(crate) fn clear_ready_marker(marker: Option<&Path>) {
    if let Some(m) = marker {
        let _ = std::fs::remove_file(m);
    }
}

/// The `LaunchOnly` child, **unchanged**: the prompt is already in argv, so there is nothing to
/// withhold and nothing to steer. codex, gemini and opencode all declare
/// `launch_only_with_protocol_events()` and all take this path; §6.1 step 8 asserts their MCP
/// readiness *post hoc* from their own streams instead.
///
/// A line the row reads as a refused credential ([`marion_harness::HarnessAdapter::auth_refusal`])
/// ends the run at once: no retry heals one, and codex, copilot and the rest retry a 401 before
/// giving up (S37). Rate limits and outages are left to the harness's backoff.
fn launch_only_child(
    inv: &Invocation,
    tmpdir: &Path,
    bound: StdDuration,
    on_started: &dyn Fn(i32),
    session: &crate::session_watch::SessionWatch<'_>,
    events: Option<&crate::events::EventSink>,
    adapter: &(dyn marion_harness::HarnessAdapter + Send + Sync),
) -> Result<ChildRun, SpawnError> {
    let mut cmd = inv.command(tmpdir);
    // **Recorded as it lands**, so a running child's `events.jsonl` already says what it has done
    // — `status`'s peek reads it — and a child killed on its wall clock has recorded everything it
    // said before the kill. The capture this returns is still whole, and is not recorded again:
    // every child path records its lines live, so nothing is read back after the fact.
    // An endpoint node's key is scrubbed by the sink itself (`EventSink::scrub_key`), on this
    // live seam as on every other, before a line is kept.
    let stopped: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let on_line = |line: &str| {
        if let Some(es) = events {
            es.record_line(line);
        }
        session.observe_line(line);
        let refusal = serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()
            .and_then(|frame| adapter.auth_refusal(&frame));
        match refusal {
            Some(why) => {
                stopped.borrow_mut().get_or_insert(why);
                ControlFlow::Break(())
            }
            None => ControlFlow::Continue(()),
        }
    };
    let output = run_bounded_watched(&mut cmd, bound, on_started, Some(&on_line));
    // The process is over, however it ended: what it spent is settled, and a continuation's frames
    // are the next generation's.
    if let Some(es) = events {
        es.end_generation();
    }
    let output = output?;
    Ok(ChildRun {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit: ChildExit {
            code: output.code,
            signal: output.signal,
            timed_out: output.timed_out,
        },
        capture_truncated: output.capture_truncated,
        // No control plane, so no ask can reach marion at all: the prompt is already in argv and
        // there is no channel afterwards. An absence by construction, not an empty measurement.
        denied_permissions: vec![],
        stopped: stopped.into_inner(),
    })
}

/// The **duplex child**: §6.1 step 8's gate, then the prompt as a `user` frame.
///
/// Driven by [`crate::duplex`], which is the same code `marion run` drives a duplex *root* with —
/// deliberately, because the gate is normative for any launcher driving Claude Code headlessly and
/// a second implementation of it here would be exactly the drift §9 warns about. What a child adds
/// is what a child *is*: a contract, and a wall clock its contract records (`timeout_secs`) — the
/// same kind of bound a root runs under.
///
/// **An ask is reachable here, and it is denied at once.** There is one compile path for a Claude
/// Code node — `ClaudeCodeAdapter::compile` over `claude_code::SPEC` — and it emits
/// `--permission-prompt-tool stdio` **unconditionally**, guarded by its own test
/// (`permission_prompt_tool_is_set_or_can_use_tool_never_fires`). A duplex child is compiled with
/// `--tools ""` plus exactly one allowlisted verb (`report`), so **every other tool call it makes
/// asks**. marion has nobody to ask, so the driver denies the ask the moment it arrives and the
/// node proceeds, on a child exactly as on a root (§9): holding it could only spend the task's own
/// wall clock and turn a run that should have been `Ok` into `TimedOut`. The denial leaves a
/// `PermissionDenied` record.
/// What a child's duplex launch needs beyond its compiled [`Invocation`] — the node, rather than the
/// program.
///
/// A struct because these travel together and always have: they are one node's identity, its
/// turn and its bounds, and every one of them is read straight into a [`DuplexSpec`] field. Passing
/// them positionally was already at the edge of readable and went over it when recording landed.
struct ChildDuplex<'a> {
    agent_id: &'a AgentId,
    /// The node's own `TMPDIR` ([`crate::node_tmp`]).
    tmpdir: &'a Path,
    ready_file: &'a Path,
    prompt: &'a str,
    bound: StdDuration,
    depth: u32,
    /// Where this node's stream is recorded, or `None` if nothing is recording it (§7.3.3).
    events: Option<&'a crate::events::EventSink>,
    /// §6.1 step 7's confirmation, run at the instant the child's process exists. Not `Option`
    /// here the way it is on [`DuplexSpec`]: a child marion journals an intent for is a child
    /// marion must journal a confirmation for, so this path has nothing to say about an absence.
    on_started: &'a dyn Fn(i32),
    /// The watch for the frame that names this node's harness session.
    session: &'a crate::session_watch::SessionWatch<'a>,
    /// The node's inbox, for its turns after the first ([`DuplexSpec::turns`]).
    turns: Option<crate::inbox::TurnFeed>,
    /// The frames that end the run now ([`DuplexSpec::stop_on`]).
    stop_on: &'a dyn Fn(&serde_json::Value) -> Option<String>,
    /// What the child's pipes speak ([`DuplexSpec::dialect`]).
    dialect: duplex::Dialect,
}

fn duplex_child(inv: &Invocation, child: ChildDuplex<'_>) -> Result<ChildRun, SpawnError> {
    let ChildDuplex {
        agent_id,
        tmpdir,
        ready_file,
        prompt,
        bound,
        depth,
        events,
        on_started,
        session,
        turns,
        stop_on,
        dialect,
    } = child;
    let mut cmd = inv.command(tmpdir);
    // **A sink that writes to a file, never to stdout** — which is what makes this path's long-held
    // `sink: None` safe to lift. This runs inside `marion-supervisor`, whose stdout *is* the stdio
    // MCP stream the root harness parses, so the rule was never "no sink"; it was "nothing that
    // writes to marion's stdout". `EventSink` writes to `<agent-dir>/events.jsonl` and nowhere else,
    // and `a_child_run_writes_not_one_byte_of_the_nodes_stream_to_marions_own_stdout` is asserted
    // against a run that *has* one, so the invariant is proved rather than preserved by absence.
    //
    // Live rather than recovered from `out.stdout` afterwards, because this path genuinely has a
    // live seam and §4.1's `observed_live` should be true only when it is. It also means a run
    // killed on its wall clock has already recorded everything it said before the kill.
    // The session watch rides the same sink: a `SessionObserved` is a journal record, not a byte
    // of the node's stream, so the stdout invariant above is untouched by it.
    let record = |ev: duplex::StreamEvent<'_>| {
        if let Some(es) = events {
            es.record(ev);
        }
        session.observe_event(ev);
    };
    let sink = Some(&record as duplex::StreamSink<'_>);
    let out = duplex::run_duplex(
        &mut cmd,
        &DuplexSpec {
            ready_file,
            prompt,
            init_id: init_request_id(agent_id),
            mcp_ready_timeout: CHILD_MCP_READY_TIMEOUT.min(bound),
            // The child's own depth, not the caller's — the same value its bridge is told, so the
            // driver and the bridge answer the same question about the same node.
            depth,
            wall_clock: Some(bound),
            sink,
            on_started: Some(on_started),
            turns,
            stop_on: Some(stop_on),
            dialect,
        },
    )?;
    Ok(ChildRun {
        stdout: out.stdout,
        stderr: out.stderr,
        exit: ChildExit {
            code: out.exit_code,
            signal: out.signal,
            timed_out: out.timed_out,
        },
        // The duplex reader consumes the pipe inline and stops at the terminal frame, so there is
        // no abandoned drain and nothing to record as short.
        capture_truncated: false,
        denied_permissions: out.denied_permissions,
        stopped: out.stopped,
    })
}

/// Resolves a written [`SpawnIntent`] as an **abort** unless the spawn path reaches its own
/// terminal record.
///
/// A guard rather than a line before each `return`, because `run_spawn` leaves through a dozen `?`
/// operators — the worktree, the config writes, `compile`, the child's own driver — and any one of
/// them that left the intent unresolved would produce precisely the node §7.2 forbids: one that
/// *looks* like a node marion lost, when in fact marion decided its fate and simply never said so.
/// `Drop` catches every one of those exits, plus a panic, which no explicit call site can.
///
/// The reason is generic by default because the error is gone by the time `Drop` runs — a `?` has
/// already moved it into the caller's `Err`. That is the honest trade: the journal records *that*
/// marion abandoned the spawn (which is what replay needs, and what keeps the node out of
/// `unresolved()`), and the error itself reaches the caller, who is the one who can act on it.
///
/// **Except where the refusal is marion's own sentence**, which is worth more than "marion left".
/// An adapter that refuses to compile a launch — codex asked for a `read` tool it has none of —
/// says which tool on which harness, and a `?` routed through [`Self::filed`] keeps that sentence
/// for the record while still returning the error untouched. The root path already journals its
/// refusals this way (`root.rs` writes `e.to_string()` into its `SpawnAborted`); this is the child
/// path catching up at the one site whose sentence a reader most needs.
struct AbortOnDrop<'a> {
    project: &'a ProjectDir,
    agent_id: AgentId,
    armed: bool,
    /// The refusal's own words, when a `?` passed through [`Self::filed`]; `None` is the generic
    /// reason above.
    reason: Option<String>,
}

impl AbortOnDrop<'_> {
    /// Pass a fallible step's result through, remembering its error's sentence for the abort
    /// record. The error itself is returned exactly as it was: this files a copy, not a diversion.
    fn filed<T>(&mut self, result: Result<T, SpawnError>) -> Result<T, SpawnError> {
        if let Err(e) = &result {
            self.reason = Some(e.to_string());
        }
        result
    }
}

impl Drop for AbortOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let reason = self.reason.take().unwrap_or_else(|| {
                "marion left the spawn path before the child reached a terminal record; the error \
                 was returned to the caller (§7.2: a node marion decided the fate of is never one \
                 marion lost)"
                    .into()
            });
            crate::journal::record(
                self.project,
                RecordKind::SpawnAborted(SpawnAborted {
                    agent_id: self.agent_id.clone(),
                    reason,
                }),
            );
        }
    }
}

/// **Whoever owns this node's lifecycle, watching the spawn from outside it.**
///
/// `run_spawn` is a blocking call that returns a finished node. That was the whole story while
/// every caller *was* the owner — `marion run` holds the root for its entire turn, and the bridge
/// holds a backgrounded child on a thread of its own. §11 item 28 moves ownership into the
/// supervisor, and a supervisor that learned a node's identity from the **return value** would
/// learn it after the node had run: too late to answer `agent/spawn`, too late to mint the node a
/// capability, and too late to hold a handle on it.
///
/// So two facts are pushed out at the instant they become true, and each hook is placed *before*
/// the thing it is about becomes irreversible:
///
/// * [`Self::identified`] — the node has an identity and nothing else. Its `SpawnIntent` is
///   fsynced and no side effect has been taken: no worktree, no config document, no process. The
///   token it returns is written into the declaration a few lines later, so minting it here is
///   what makes it reach the one file the node's own bridge reads.
/// * [`Self::started`] — a process exists and this is its pid, at the same instant step 1's
///   `Spawned { pid: Some(_) }` is journaled. An owner that answers its caller when this fires is
///   making a claim the journal already backs, rather than one it is about to.
///
/// **Not a channel, and not a return value.** A channel would let the spawn proceed past a point
/// the owner has not yet recorded; the declaration is written between the two hooks, so
/// `identified` has to be answered *synchronously* or the token is decided after the file that
/// carries it. `&dyn` for [`crate::duplex::DuplexSpec::on_started`]'s reason.
///
/// Neither hook may block or panic. Both run on the spawning thread inside the node's own critical
/// path — a hook that blocks delays a real process's launch, and one that panics unwinds through
/// [`AbortOnDrop`], journaling `SpawnAborted` for a node that was fine.
pub trait SpawnObserver: Sync {
    /// The node's identity, the instant it has one and nothing more. Returns §5.4's per-node
    /// capability token to write into its declaration, or `None` for an owner that mints none.
    fn identified(&self, agent_id: &AgentId) -> Option<Secret>;
    /// A process exists. `pid` is a signal target, not an identity — see the `Spawned` writer below
    /// and `restart.rs` for why those are different claims.
    fn started(&self, agent_id: &AgentId, pid: i32);
    /// **The node's inbox, for its driver** — the queue `node/steer` and a background child's end
    /// land in, bound to this node, or `None` for an owner that keeps none. A typed-turn driver
    /// delivers from it at the node's turn boundaries (and mid-turn where the row folds); with
    /// `None` it takes the one turn it was launched with, as before.
    fn turn_source(
        &self,
        _agent_id: &AgentId,
    ) -> Option<std::sync::Arc<dyn crate::inbox::TurnSource>> {
        None
    }
    /// **§7.6's subtree scan, answered by whoever holds the tree.** The live descendants of
    /// `agent_id` — the whole subtree, in the gating sense `descendant_gate::live_descendants`
    /// states — or `None` for an owner that holds no registry and so cannot see the tree at all.
    ///
    /// `None` and `Some(vec![])` are different answers on purpose: the second is a check that ran
    /// and found nothing, the first is a check that could not run. Defaulted to `None` so an owner
    /// that cannot look never reports a clean bill of health by failing to.
    ///
    /// Unlike the two hooks above this **may block the spawning thread**: the gate's hold polls it
    /// until the descendants are terminal or the node's bound expires, which is §7.6 step 3.
    fn live_descendants(&self, _agent_id: &AgentId) -> Option<Vec<AgentId>> {
        None
    }
    /// **Asked once, the instant the node's process has ended and before anything terminal is
    /// written: did marion end it on an operator's kill?** `true` is the same kind of fact as the
    /// driver's own `timed_out` — marion's attributed act, known to marion rather than read off the
    /// exit — except that the kill came from outside this thread (`node/kill`, or `session/quit`'s
    /// KillTree). The node then ends `Cancelled` (§6.7), and its terminal record is the killer's
    /// `KillConfirmed`, never an `Exited` of this thread's.
    ///
    /// Asking also settles the race the other way: once this has answered `false`, the owner
    /// refuses a kill of the node rather than signalling a process already reaped. Defaulted to
    /// `false` for an owner that cannot be asked to kill anything.
    fn process_ended(&self, _agent_id: &AgentId) -> bool {
        false
    }
    /// Whether an operator's kill of the node has been asked for, **without** settling anything:
    /// the check between one launch attempt and the next (a key rotation), where
    /// [`Self::process_ended`]'s answer would make the next attempt unkillable.
    fn kill_requested(&self, _agent_id: &AgentId) -> bool {
        false
    }
    /// **Where a node's sink publishes its live spend** ([`crate::spending`]), or `None` for an
    /// owner that shows no live figures — its node's figure still reaches the contract and the
    /// journal when it ends.
    fn spending(&self) -> Option<std::sync::Arc<crate::spending::Spending>> {
        None
    }
    /// **The tree the node runs in, the instant it is chosen** — the tree the node's own children
    /// will branch from. A worktree child's children branch from its worktree (and from the work in
    /// it, `spawn::make_worktree`), not from the tree its parent named. Defaulted to nothing for an
    /// owner whose node has no children to hand it to.
    fn workspace_chosen(&self, _agent_id: &AgentId, _workspace: &Workspace) {}
}

/// The observer for a caller that owns the node **by holding this call** — which is every caller
/// but the supervisor's `agent/spawn`.
///
/// `marion run` blocks on the root for its whole turn and the bridge holds a backgrounded child on
/// a thread it owns, so both already know everything these hooks announce. Neither mints a token:
/// §5.4's capability is bound to a node whose lifecycle the *supervisor* owns, and a token minted
/// by a process that is about to exit would be a credential with nothing behind it.
pub struct Unwatched;

impl SpawnObserver for Unwatched {
    fn identified(&self, _: &AgentId) -> Option<Secret> {
        None
    }
    fn started(&self, _: &AgentId, _: i32) {}
}

/// [`run_spawn_watched`] for a caller that owns the node by holding this call.
///
/// Kept as the name with the plain signature — rather than making every caller pass [`Unwatched`] —
/// for the reason [`run_bounded`] keeps its own next to [`run_bounded_watched`]: it is `pub`, it
/// has callers outside this crate's spawn path, and a widened signature would make five integration
/// files re-state a parameter none of them has an opinion about.
pub fn run_spawn(
    env: &Env,
    req: &SpawnRequest,
    task_id: &TaskId,
    caller: &Caller,
) -> Result<TaskContract, SpawnError> {
    run_spawn_watched(env, req, task_id, caller, &Unwatched)
}

/// Where a working tree declares its own agent types, relative to the tree's root.
pub const AGENT_TYPES_FILE: &str = ".marion/agents.toml";

/// The agent types a working tree can spawn: the built-ins plus [`AGENT_TYPES_FILE`]'s rows.
///
/// Keyed on the **tree**, not on [`Env::project_root`] — that is the git common dir, which a
/// linked worktree shares with the main repository, and the file is a tracked file of the tree
/// the node runs in. No file is the built-in table; any other failure to read it, or to parse it,
/// is [`SpawnError::AgentTypesFile`] naming the path, so a mistyped row refuses every spawn
/// against that tree rather than silently spawning the built-ins.
pub fn agent_types(tree: &Path) -> Result<AgentTypes, SpawnError> {
    match read_agent_types(tree)? {
        Some((path, text)) => agent_types_text(path, &text),
        None => Ok(AgentTypes::builtins_only()),
    }
}

/// The type a **launch** runs: `name` resolved through the tree's table, and refused unless every
/// command the row names has the operator's consent ([`crate::trust::require`]).
///
/// The one seam both launch paths — `run_spawn_watched` and `root::prepare_watched` — resolve
/// through, so a repository's `acp:<command>` row cannot reach a process by any other route. The
/// digest is of the very text parsed here, never of a second read.
pub fn launch_type(tree: &Path, name: &str) -> Result<AgentType, SpawnError> {
    let file = read_agent_types(tree)?;
    let types = match &file {
        Some((path, text)) => agent_types_text(path.clone(), text)?,
        None => AgentTypes::builtins_only(),
    };
    let ty = types
        .resolve(name)
        .ok_or_else(|| SpawnError::UnknownAgentType(name.to_string()))?;
    if let Some((path, text)) = &file
        && types.user().iter().any(|u| u.name == ty.name)
    {
        crate::trust::require(path, text, &ty)?;
    }
    Ok(ty)
}

/// [`AGENT_TYPES_FILE`]'s path and text, or `None` where the tree has none.
fn read_agent_types(tree: &Path) -> Result<Option<(PathBuf, String)>, SpawnError> {
    let path = tree.join(AGENT_TYPES_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some((path, text))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SpawnError::AgentTypesFile {
            path,
            error: e.to_string(),
        }),
    }
}

/// [`agent_types`]' checks over `text` as the contents of `path`, without reading it: so a writer
/// can hold a new file to the very rules a spawn will, before it replaces the old one.
pub fn agent_types_text(path: PathBuf, text: &str) -> Result<AgentTypes, SpawnError> {
    let types = AgentTypes::parse(text).map_err(|e| SpawnError::AgentTypesFile {
        path: path.clone(),
        error: e.to_string(),
    })?;
    check_providers(&types, crate::credentials::user_registry)
        .map_err(|error| SpawnError::AgentTypesFile { path, error })?;
    Ok(types)
}

/// Every row's `provider` is one the user's registry knows. `registry` is called only when some
/// row names a provider, so a tree that uses none never depends on the user's `providers.toml`.
fn check_providers(
    types: &AgentTypes,
    registry: impl FnOnce() -> Result<marion_core::provider::Registry, String>,
) -> Result<(), String> {
    let named: Vec<(&str, &str)> = types
        .user()
        .iter()
        .filter_map(|t| t.provider.as_deref().map(|p| (t.name.as_str(), p)))
        .collect();
    if named.is_empty() {
        return Ok(());
    }
    let registry = registry()?;
    for (name, provider) in named {
        if registry.get(provider).is_none() {
            return Err(format!(
                "agent type {name:?} names provider `{provider}`, which is neither built in nor \
                 in your providers.toml; `marion login --list` shows the ones marion knows"
            ));
        }
    }
    Ok(())
}

/// The prompt a node of `ty` is given for `prompt`: the type's `prompt_prefix` in front of it,
/// once, or the prompt untouched — byte for byte — for a type that states none.
///
/// **One newline between the two, unless the author already put whitespace there.** A prefix is
/// a standing instruction and the prompt is a task; joined byte-to-byte they read as one run-on
/// sentence (`"…do not edit.DEMO-1: review …"`), which is what a node was handed before this
/// rule. A prefix that ends in whitespace — an author's own `"\n\n"`, a trailing space — is
/// joined exactly as written, so the separator is the author's whenever they chose one and
/// marion's only when they chose none.
///
/// Applied exactly once, at the top of `run_spawn_watched` and of `root::prepare_watched`, right
/// after the type resolves; every later reader — the launch, the frame, the contract — sees the
/// prefixed text, so the audit record names the prompt the node actually saw.
pub fn prefixed_prompt(ty: &AgentType, prompt: &str) -> String {
    match &ty.prompt_prefix {
        Some(prefix) if prefix.is_empty() || prefix.ends_with(char::is_whitespace) => {
            format!("{prefix}{prompt}")
        }
        Some(prefix) => format!("{prefix}\n{prompt}"),
        None => prompt.to_string(),
    }
}

/// The prompt a **child** of `ty` is given: [`prefixed_prompt`], then marion's
/// [`crate::bridge::REPORT_INSTRUCTION`] after a blank line.
///
/// A child and not a root: §9 gives a root no contract and no `report`, so `root::prepare_watched`
/// keeps [`prefixed_prompt`] alone. Applied once, where the child's type resolves, so the launch,
/// the frame and the contract all read the text the node actually saw.
pub fn child_prompt(ty: &AgentType, prompt: &str) -> String {
    format!(
        "{}\n\n{}",
        prefixed_prompt(ty, prompt),
        crate::bridge::REPORT_INSTRUCTION
    )
}

/// §6.1's spawn, with the node's owner told about it as it happens. See [`SpawnObserver`].
pub fn run_spawn_watched(
    env: &Env,
    req: &SpawnRequest,
    task_id: &TaskId,
    caller: &Caller,
    observer: &dyn SpawnObserver,
) -> Result<TaskContract, SpawnError> {
    // The tree's own table, read now: the file is the operator's and may have changed since the
    // last spawn, and a type it no longer defines is refused here, before the intent is journaled.
    let agent_type = launch_type(&req.repo, &req.agent_type)?;
    // The type's standing instruction and marion's report instruction, once, here — and `req` is
    // the child's full request from this line on, so the four places that read its prompt read
    // one value.
    let req = &SpawnRequest {
        prompt: child_prompt(&agent_type, &req.prompt),
        ..req.clone()
    };
    // **§6.1 step 2, and it runs before every side effect there is** — before the worktree, before
    // `config_files`, before `compile`, before any process. That ordering is the whole point: the
    // things this refuses are a real git worktree, a real branch, a real agent-dir and a real OS
    // process tree, and a gate evaluated after any of them would be cleaning up rather than
    // preventing.
    //
    // It reads the **caller's** type (§6.1 step 2 says so in as many words), not `agent_type`
    // above: the child has no children yet, so the concurrency bound of *its* type would be
    // vacuous, and its `max_depth` is a bound on its own descendants rather than on its existence.
    //
    // Until this call existed `check_spawn_gates` had no production caller at all, nothing computed
    // a depth, and `max_depth` was an inert number: a child could spawn a grandchild, and that
    // grandchild another, without bound. Only Claude Code reads `allowed_tools`, so on the other
    // three harnesses the ungated `spawn` was simply *served* — real processes, real worktrees, no
    // error anywhere.
    //
    // The third argument was the constant `LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER = 0` until
    // backgrounding landed, which made the concurrency half of this gate unreachable while the
    // depth half was live. It is now the caller's real count (see [`Caller::live_children`]), so a
    // parent that backgrounds more children than its type allows is refused here — before the
    // worktree, before the process — rather than served.
    check_spawn_gates(&caller.agent_type, caller.depth, caller.live_children)?;
    let requested = requested_scope(req);
    check_spawn_scope(&agent_type.scope_ceiling, &requested)?;
    // The verification lines ride on the intent below, so a set too large to journal is refused
    // here, with the other refusals and before the node has an identity at all.
    check_verification_size(&req.verification)?;
    // **Which of the operator's own logins this node runs on**, refused here — before the node
    // exists — when a named profile is unknown, belongs to another harness, or has lost its
    // directory. Empty under canned auth and wherever nothing names a profile.
    let profiles = crate::profiles::Launch::resolve(
        env.auth,
        &env.state,
        agent_type.harness,
        req.profile.as_deref(),
        req.resume.is_some(),
        &agent_type.profiles,
    )?;
    // Endpoint mode, where the child's type or model names a provider: refused by name here, with
    // the other refusals and before the node has an identity. The wires are the row's; an adapter
    // that cannot be built at all is refused below, in its own words, as it always was.
    let endpoint = crate::endpoint::resolve_for_launch(
        req.model.as_deref(),
        &agent_type,
        &adapter_for_type(agent_type.harness, agent_type.acp_agent.as_deref())
            .map(|a| a.endpoint_wires())
            .unwrap_or_default(),
    )?;

    let spawned_at = SystemTime(std::time::SystemTime::now());
    // **A resume reuses the node's own id; a fresh spawn mints one** — `root::prepare_watched`'s
    // rule, on the child path and for the same reason: the second `Spawned` has to land on the node
    // every earlier record names, which is the whole of what replay folds as generation two.
    let agent_id = match &req.resume {
        Some(r) => r.agent_id.clone(),
        None => new_agent_id(unix_millis(), entropy()?),
    };
    // §6.1 step 7, first half: *"journal the spawn intent, start the process, journal
    // confirmation."* **Written at the first instant the node has an identity at all**, and
    // deliberately before the worktree, the config files and the process — everything below this
    // line is a side effect marion would otherwise have taken on behalf of a node the journal has
    // never heard of. A node that is spawned and never recorded is exactly the untracked live
    // process §9's M2 criteria exist to forbid.
    //
    // The gates (§6.1 step 2) and the scope check run **above** it, unchanged: a spawn that is
    // refused creates no node, and recording an intent for one would put a node in the tree that
    // never existed.
    //
    // **The caller's number, clamped once, here.** Everything downstream — the child's wall clock,
    // the MCP readiness cap, the contract's recorded `timeout` — reads this one value, so the bound
    // the node ran under and the bound its audit record claims are the same by construction. See
    // [`MAX_TIMEOUT_SECS`] for why an unclamped `u64` was a live process left behind. It is
    // resolved *above* the intent rather than at its first use because the intent now records it:
    // one clamp, and the journal's copy is the compiled value §6.7 records everywhere else.
    let bound = effective_timeout(req.timeout_secs);
    //
    // **A barrier, not a best-effort record**: `append`, and the spawn is refused if it fails. The
    // intent is the only record naming the node before anything exists, so a child launched after
    // its intent was lost is invisible to a restart — the untracked live process this ordering
    // exists to prevent.
    crate::journal::append(
        &env.project_dir,
        RecordKind::SpawnIntent(SpawnIntent {
            review_of: req.review.as_ref().map(|t| t.agent_id.clone()),
            agent_id: agent_id.clone(),
            // §3.1's bound for *this* child, from the one clamp above — so `marion tree` shows the
            // clock the node is running under rather than its agent type's default.
            timeout_secs: Some(bound.as_secs()),
            // The lines a restart re-runs when it resumes this child, sized by the check above.
            verification: req.verification.clone(),
            // §7.5: immutable, written once. The caller is the parent by construction.
            parent_id: Some(AgentId(caller.agent_id.clone())),
            // The canonical name off the resolved type, not the alias `req.agent_type` used —
            // the same value `SpawnCtx` carries to the child's own bridge.
            agent_type: agent_type.name.clone(),
            harness: agent_type.harness,
            // §3.1's depth, one level below the caller's — the same derivation `SpawnCtx` uses, so
            // the journal and the child's declaration can never disagree about where it sits.
            depth: caller.depth + 1,
            // A child runs under a contract; §9's `None` is for a root.
            task_id: Some(task_id.clone()),
        }),
    )
    .map_err(|source| SpawnError::SpawnIntentBarrier {
        agent_id: agent_id.clone(),
        source,
    })?;
    // **§11 item 28 step 4's first hook, and its position is the argument.** The intent is on disk,
    // so a supervisor that crashes after this line has a record naming the node; and nothing below
    // has happened yet, so the token this returns is decided before the document that carries it is
    // written. Between the two there is exactly one durable fact and no side effect — which is the
    // only window in which "the node exists and nothing about it is irreversible" is true.
    let node_token = observer.identified(&agent_id);
    // Every path out of this function from here on resolves that intent — including the `?`
    // returns below, which is what this guard is for. See [`AbortOnDrop`].
    let mut resolution = AbortOnDrop {
        project: &env.project_dir,
        agent_id: agent_id.clone(),
        armed: true,
        reason: None,
    };
    let agent_dir = env.project_dir.agent(&agent_id);
    let ch = agent_dir.config_dir();
    crate::private_fs::create_dir_all(&ch)?;
    // Everything that can be refused is refused before this line — see [`select_workspace`] for
    // why a worktree is the first irreversible thing a spawn does.
    let (workspace, base, cwd_claim, mut prelaunch) =
        select_workspace(req, &agent_type, &env.project_dir, task_id, &agent_id)?;
    // The tree this child's own children branch from is the one it runs in.
    observer.workspace_chosen(&agent_id, &workspace);
    // Held for the child's whole run. Named rather than `_`, because `let _ = ..` drops immediately
    // and would release §6.6's claim before the child it is protecting had started — the guard would
    // still compile, still be tested by a single-spawn test, and protect nothing.
    let _cwd_claim = cwd_claim;
    let wt = workspace.path().clone();

    // §6.1 step 5, through the seam, **dispatched on the agent type's harness**. This was a
    // constant until now, which meant a `claude` agent type wrote a Codex config, exec'd `codex`,
    // and still recorded `"claude-code"` in the contract below — §6.7's audit record asserting
    // something that never happened.
    //
    // A harness marion can *name* but not yet *run* stops here, as a typed
    // `HarnessError::Unimplemented` naming it. There is deliberately no fallback: silently running
    // some other harness is the failure mode §12's correction rows keep recording, and a loud
    // refusal is always the cheaper one to diagnose.
    // **`adapter_for_type`, not `adapter_for`**, because on the fifth harness a name is not a
    // program: `acp` is a protocol, one adapter serves many agents, and they spell marion's verbs
    // three different ways (S21/S22). The second half of the selection is the agent type's
    // `acp_agent`, and this is the seam where it becomes behaviour. `adapter_for` would hand back
    // the protocol row, which refuses to compile anything at all — an ACP type that resolved,
    // dispatched, and then failed at `compile`, which is the shape of the dispatch bug above.
    let adapter = adapter_for_type(agent_type.harness, agent_type.acp_agent.as_deref())?;
    // §3.4, the same derivation `marion run` uses for a root: **branch on the surfaces, never on a
    // harness name.** Until this branch existed `run_spawn` drove every child as `LaunchOnly` —
    // correct for the three harnesses that declare it, and the reason a `claude` child took turn
    // one with `"tools":[]` and exited 0 having called nothing.
    let path = launch_path(&adapter.surfaces())
        .ok_or(SpawnError::UnsupportedChildSurface(agent_type.harness))?;
    let ready_file = child_ready_file(path, &agent_dir);
    let ctx = SpawnCtx {
        agent_id: agent_id.clone(),
        // The **canonical** name, read off the resolved type rather than off `req.agent_type`: the
        // alias `codex` and the name `codex-impl` are one definition (`agent_type::builtin`), and
        // writing the alias would make the child's bridge re-resolve a spelling marion had already
        // resolved once.
        agent_type: agent_type.name.clone(),
        // §3.1's depth, one level below the caller's. This is what reaches the child's own bridge
        // through the per-server `env` block, and it is what makes the gate above evaluable at all
        // when *this* child spawns in turn.
        depth: caller.depth + 1,
        // `Some` only on the duplex path. On a `LaunchOnly` child the prompt rides argv, so there
        // is no frame to withhold and no marker to wait on (§6.1 step 8); its MCP readiness is
        // asserted post hoc from its JSONL stream.
        ready_file: ready_file.clone(),
        // §5.4's capability, from whoever owns this node — `None` under [`Unwatched`], which is
        // every caller that owns the node by holding this call. It reaches the child's bridge
        // through the same per-server `env` block `agent_id` and `depth` ride, because that block
        // is the only channel marion has to a process the *harness* starts.
        node_token: node_token.clone(),
        repo: req.repo.clone(),
        // §4.3's `<state>` **root**, which is `<state>/<project-hash>`'s parent — not the project
        // dir itself. This used to be the project dir behind a `TODO(phase-3)` that called itself
        // harmless because codex's `config_toml` emitted no per-server `env` and so read neither
        // this nor `agent_id`. That is no longer true (`CodexAdapter::config_files`), and it was
        // never harmless on the three harnesses that *did* pass it through: a node that spawned in
        // turn would have handed its own bridge `MARION_STATE_DIR=<state>/<hash>`, which
        // `spawn_env` re-hashes into `<state>/<hash>/<hash>` — a second, invisible state tree for
        // the same project. `ProjectDir` is `<state>/<hash>` by construction, so the root is its
        // parent; the fallback is the dir itself, which is what it already was.
        state_dir: env
            .project_dir
            .path()
            .parent()
            .unwrap_or_else(|| env.project_dir.path())
            .to_path_buf(),
        bridge: env.bridge.clone(),
        bridge_args: vec!["mcp".into()],
    };
    // **§7.3.3's replay leg, for the node it needs most.** A child spawned, run and terminated
    // entirely inside a detached window is the case re-attach cannot answer from anything else: it
    // has no live channel to re-subscribe to, and the journal records that it existed and how it
    // ended but not one word of what it *said*. This is where those words get written, and the
    // bridge's process is the only process that ever has them (`registry.rs` makes the same point
    // about a child's lifecycle records).
    //
    // `Option`, because a viewer may never fail a run: a node that cannot open its event file still
    // runs, and `EventReader::ever_written` is what later tells "nobody recorded this" from "it said
    // nothing" rather than presenting the first as the second.
    let events = crate::events::EventSink::open(
        &agent_dir,
        &agent_id,
        adapter.harness(),
        init_request_id(&agent_id),
    )
    .map(|es| es.publishing_to(&agent_id, observer.spending()));
    // The opening bookend, before the process exists — the stream begins where marion started
    // watching, not where the node first spoke, so a node that says nothing at all is still
    // distinguishable from one that was never recorded.
    if let Some(es) = &events {
        es.lifecycle(marion_core::event::Lifecycle::Opened);
        es.resumed_from(req.resume.as_ref().and_then(|r| r.usage));
    }
    // From here every way out of this function records what the stream said the node spent — the
    // contract path below does it before the terminal records, and every early return does it as
    // it leaves. See [`SpendOnEveryEnd`].
    let mut spend = SpendOnEveryEnd {
        project: &env.project_dir,
        agent_id: &agent_id,
        events: events.as_ref(),
        recorded: false,
    };
    // The node's harness session, journaled the moment its stream names one — the handle a later
    // `node/resume` hands back. Beside `events` because both read the same live frames, and for
    // the same reason: a node killed mid-run never reaches a capture.
    // **And where it ran**, from the workspace `select_workspace` chose a few lines above and from
    // nothing else. A child's cwd is a linked worktree marion made or the caller's own directory,
    // and neither is recoverable from anything else on the journal — so a child resume can only
    // relaunch into the tree its session was created in if this launch writes it down.
    let session = crate::session_watch::SessionWatch::new(
        &env.project_dir,
        &agent_id,
        adapter.harness(),
        false,
    )
    .in_workspace(Some(workspace.clone()))
    .with_profiles(&profiles);
    // The child's inbox, for the typed paths' turns after the first (duplex and ACP): a steer, or
    // the end of a child it backgrounded. A child is always headless (a pane belongs to a root).
    let turns = observer.turn_source(&agent_id).map(|source| {
        crate::inbox::TurnFeed::new(
            source,
            adapter.turn_delivery(marion_harness::spec::NodeShape::Headless),
        )
    });
    // **The same inbox, on the lane whose next turn is a relaunch** (`crate::continuation`): a
    // `LaunchOnly` node whose row measured a resume that continues its session. Attached before
    // the first launch, so a message queued while it runs is waiting at its stop. The typed lanes
    // take their turns inside their own drivers, from `turns`.
    let continuation = match (
        path,
        adapter.turn_delivery(marion_harness::spec::NodeShape::Headless),
    ) {
        (LaunchPath::LaunchOnly, marion_harness::spec::TurnDelivery::Continuation { .. }) => turns
            .as_ref()
            .map(|feed| crate::continuation::Turns::attach(Arc::clone(&feed.source))),
        _ => None,
    };
    // **§6.1 step 7's confirmation, moved to the instant it becomes true.**
    //
    // It used to be written here-ish in the source but *after* the whole run: `spawn` is
    // synchronous, so the first instant marion had observed a process was also the instant it had
    // observed the exit, and the record could only honestly carry `pid: None`. That absence was
    // load-bearing in the wrong direction — `session/quit`'s `KillTree` preflight refused whenever
    // there was anything to kill, and §11 item 30's two runaway shapes, a live reparented process
    // and a process already dead, were indistinguishable on disk.
    //
    // Handed to the launch path as a hook because only the launch path knows the pid and when
    // there is one. `Spawned` is in §4.3's barrier set, so the append fsyncs before the driver
    // writes a byte to the child's stdin.
    //
    // **The window this creates, named rather than apologised for.** Between `command.spawn()` and
    // this append a process exists and the journal does not say so — one `write(2)` plus one
    // fsync wide. It cannot be made smaller: the pid does not exist before the spawn, so there is
    // nothing earlier to record. Today's equivalent window was the child's *entire lifetime*, and
    // what it left behind was a `SpawnIntent` that could mean either "no process was ever started"
    // or "a process is running and marion cannot name it". After this, `SpawnIntent` alone means
    // **no process exists**, full stop — a worktree leak, never a process leak.
    //
    // **And that sentence is a claim about the append, not about the spawn**, which is why this one
    // record goes through the fallible `journal::append` rather than `journal::record`. If the
    // barrier does not land — a full disk, a revoked state directory, a short write, a record the
    // 16 KiB cap refuses — then a process exists that replay cannot name, and `procid::audit`, whose
    // scope is `node.pid.is_some()`, cannot see it at all. Answering `agent/spawn` successfully
    // there would be marion reporting a node its own authoritative record says does not exist, and
    // it would recreate §11 item 30's untracked live process under §9 criterion 3's nose.
    //
    // **So the process is unwound instead of the record being wished into existence.** The only two
    // states marion can honestly leave behind are *recorded and live* or *not live*; `command.spawn`
    // cannot be taken back, so the second is reached by killing the tree here and letting the driver
    // below reap it. The owner is **not** told — `observer.started` is the claim that must stay
    // backed — and `run_spawn_watched` returns [`SpawnError::UnaccountableNode`] instead of a
    // contract. The node then replays as `SpawnIntent` with no pid, which is exactly what it now
    // means: no process exists.
    let unaccountable: std::cell::Cell<Option<crate::journal::JournalError>> =
        std::cell::Cell::new(None);
    // Asked of each process with what that process carries: its attempt's version, model and
    // endpoint on the first generation, and the same on every continuation generation after.
    let announce = |pid: i32,
                    version: &str,
                    model: Option<&str>,
                    endpoint: Option<&crate::endpoint::Endpoint>| {
        let appended = crate::journal::confirm_spawned(
            &env.project_dir,
            child_spawned_record(&agent_id, version, model, pid, endpoint),
        );
        match appended {
            // **After the append, never before.** The owner's whole reason for wanting this instant
            // is that the response it sends is a claim the journal already backs; telling it first
            // would let it answer "the node exists" a `write(2)` before the only record that says
            // so.
            Ok(_) => observer.started(&agent_id, pid),
            Err(e) => {
                // `kill_process_tree` rather than a bare `kill(pid)`: the child is a group leader
                // and may already have started tool-call descendants of its own, and those are as
                // unaccountable as it is. Not `_and_wait` — the driver below is already about to
                // wait on this exact child, and a second five-second poll here would only delay it.
                kill_process_tree(pid);
                unaccountable.set(Some(e));
            }
        }
    };
    // Each attempt's driver takes its own handle on the one inbox: a typed driver consumes the
    // feed it is given, and a rotated attempt is a fresh process with the same inbox behind it.
    let feed = || {
        turns.as_ref().map(|f| crate::inbox::TurnFeed {
            source: Arc::clone(&f.source),
            mid_turn: f.mid_turn,
        })
    };
    // **One attempt loop, two failovers, one policy** (`next_attempt` below the launch): an
    // endpoint node rotates to its next API key, and a node on the operator's own login fails over
    // to the next profile its type listed — each only for a finished process that reported
    // nothing, and every attempt on what is left of the node's one wall clock.
    //
    // **API-key rotation, before the first successful turn.** An endpoint node whose provider
    // refused its key (401/403), rate-limited it (429) or failed or could not be reached (5xx, a
    // connection error) — with no report and nothing changed in its worktree, so no turn of it had
    // succeeded — is relaunched fresh on the next credential in the stated order, and the move is
    // recorded on its contract. Only a finished process is rotated, so never mid-turn; each
    // credential is tried once; and every attempt shares the node's one wall clock.
    let mut endpoint = endpoint;
    // The attempt's gateway, where its route is translated: replaced with each attempt (a rotated
    // key is the next gateway's), and held to the end of this function — the node's whole life,
    // continuations included — so it stops when the node does.
    let mut gateway: Option<crate::gateway::Gateway> = None;
    let mut failovers: Vec<marion_core::contract::CredentialFailover> = Vec::new();
    let mut probed_version: Option<String> = None;
    // The node's own `TMPDIR`, for every attempt below and removed when this function returns —
    // after the last attempt's process has been reaped. See [`crate::node_tmp`].
    let node_tmp = crate::node_tmp::NodeTmp::create(&agent_dir)?;
    let launched_at = Instant::now();
    // The profile this attempt runs on: an index into `profiles`, 0 for the first.
    let mut at = 0;
    let (launch, inv, mut run, version) = loop {
        let attempt_bound = bound.saturating_sub(launched_at.elapsed());
        // The live stream is scrubbed of every key this node has been launched on, as it arrives.
        if let (Some(es), Some(key)) = (&events, endpoint.as_ref().and_then(|e| e.key.as_ref())) {
            es.scrub_key(key.expose());
        }
        drop(gateway.take());
        gateway = match &endpoint {
            Some(ep) => crate::endpoint::open(ep)?,
            None => None,
        };
        // And of the gateway's bearer, which is what the harness itself holds on that route.
        if let (Some(es), Some(gw)) = (&events, &gateway) {
            es.scrub_key(gw.bearer().expose());
        }
        let mut launch = child_launch_spec(
            env,
            req,
            &agent_type,
            adapter.as_ref(),
            path,
            caller.depth + 1,
            &wt,
            &ch,
        );
        if let Some(ep) = &endpoint {
            crate::endpoint::apply(&mut launch, ep, gateway.as_ref());
        }
        launch.extra.profile_dir = profiles.dir(at);
        // The adapter's refusal — a tool this harness has none of, a pane it cannot draw — is the
        // one sentence on this path a reader of the journal needs verbatim, so it is filed for the
        // abort record on the way out rather than replaced by the guard's generic reason.
        let inv = resolution.filed(declare_and_compile(adapter.as_ref(), &launch, &ctx))?;
        profiles.used(at);
        // **§6.1 step 3's position, and it is a move rather than a new call.** The version used to be
        // asked for after the child had been run and reaped, which was the only place it *could* be
        // asked while `Spawned` was written there too. Step 7's confirmation now goes out at the
        // instant the process exists, and it carries this field, so the probe has to precede the
        // launch — which is where §6.1 step 3 put it in the first place.
        //
        // The cost is stated rather than hidden: [`HARNESS_VERSION_TIMEOUT`] now sits on the
        // pre-launch critical path, so a harness that hangs answering `--version` delays the child by
        // that bound instead of delaying its caller by that bound after the child is done. It is the
        // same total, spent before rather than after, and it is bounded for the same reason.
        //
        // Asked **once**, not once per reader. The `Spawned` record below and `contract.child.version`
        // further down both need it, and each used to run its own `--version` — two processes, two
        // deadlines to hang on, and two chances for the journal and the contract to disagree about the
        // version of a single node's harness.
        let version = probed_version
            .get_or_insert_with(|| harness_version(&inv.program, agent_type.harness))
            .clone();
        let announce_started =
            |pid: i32| announce(pid, &version, inv.model.as_deref(), endpoint.as_ref());
        // **A contracted child does not get a pane, and the refusal is the design rather than a
        // gap.** A child is defined by §9's `TaskContract`: it is spawned to do a task and to
        // `report`, under the wall clock its contract records. A pane node is a TUI, which takes
        // no turn at all until a human presses return — so a contracted child in a pane is a task
        // that can only ever time out, and the contract would record that as the child's failure.
        // A pane belongs to a **root**: a node an operator started and is watching. Refused here,
        // still before the seam below, so the worktree it never used is taken back.
        if path == LaunchPath::Terminal {
            return Err(SpawnError::UnsupportedChildSurface(agent_type.harness));
        }
        // **The first-execution seam.** Every prelaunch refusal is behind this line; the child
        // process starts in the match below and may write the only copy of its work into the
        // worktree before a contract is durable, so from here the success tail decides the reap.
        prelaunch.disarm();
        let run = match path {
            LaunchPath::Terminal => unreachable!("a terminal child is refused above"),
            LaunchPath::LaunchOnly => launch_only_child(
                &inv,
                node_tmp.path(),
                attempt_bound,
                &announce_started,
                &session,
                events.as_ref(),
                adapter.as_ref(),
            ),
            // **The fifth harness, as a child.** §9's M5 clause 1 asks for ACP agents running *as
            // children through the single ACP adapter*, and until this arm existed the only thing that
            // had ever driven one was `marion doctor --adapter` — a probe, which has no worktree, no
            // contract, no journal and no bridge, so it could not answer the clause however green it
            // was. That is the sixth "fully tested in isolation and unreachable from any binary" of
            // the day, and this arm is the fix.
            //
            // The declaration is asked of the adapter here rather than rebuilt in the driver, so the
            // frame marion sends and the frame `McpRoute::Session` verified are the same object.
            //
            // **Recorded as it lands**, as the other two paths are: every line the agent writes
            // reaches the child's `events.jsonl` while the turn runs (the sink scrubs an endpoint
            // key), and the session watch reads the `sessionId` its `session/new` answered with —
            // the id a resume's `session/load` hands back.
            LaunchPath::Acp => crate::acp_child::run_acp_child(crate::acp_child::AcpChildSpec {
                inv: &inv,
                tmpdir: node_tmp.path(),
                session_declaration: adapter.session_declaration(&launch, &ctx)?,
                prompt: &req.prompt,
                bound: attempt_bound,
                on_started: &announce_started,
                on_line: Some(&|line: &str| {
                    if let Some(es) = events.as_ref() {
                        es.record_line(line);
                    }
                    session.observe_line(line);
                }),
                turns: feed(),
            })
            .map(|r| ChildRun {
                stdout: r.stdout,
                stderr: r.stderr,
                exit: crate::acp_child::turn_exit(r.exit),
                capture_truncated: r.capture_truncated,
                // ACP has a permission surface (`session/request_permission`) and marion answers it
                // permissively in the driver, so nothing is denied on this path yet. An empty vector
                // here is therefore "marion refused nothing", which is true, and **not** the
                // `LaunchOnly` arm's "no ask could reach marion at all". When a policy lands, this is
                // the field it fills; §11 item 24's two axes are why the distinction is written down
                // rather than left to look identical.
                denied_permissions: vec![],
                // ACP's reader is code, and no refused-credential frame was measured on it.
                stopped: None,
            })
            .map_err(SpawnError::from),
            // **codex over app-server**: the same driver as ACP under a thread vocabulary. The
            // opening request is the adapter's (`thread/start`, or `thread/resume` of the node's
            // session), and the gate is marion's MCP server reporting ready — where the launch
            // declares it. Recorded as it lands, exactly as the ACP arm records.
            LaunchPath::AppServer => crate::app_server::run_app_server(app_server_spec(
                adapter.as_ref(),
                &inv,
                node_tmp.path(),
                &launch,
                &ctx,
                &req.prompt,
                attempt_bound,
                &announce_started,
                &|line: &str| {
                    if let Some(es) = events.as_ref() {
                        es.record_line(line);
                    }
                    session.observe_line(line);
                },
                feed(),
                &|frame| adapter.auth_refusal(frame),
            )?)
            .map(|r| ChildRun {
                stdout: r.stdout,
                stderr: r.stderr,
                exit: crate::rpc::turn_exit(r.exit),
                capture_truncated: r.capture_truncated,
                // The row's answers decline every approval; a decline is the model's to read, not a
                // permission marion withheld from an operator, so nothing is recorded here.
                denied_permissions: vec![],
                stopped: r.stopped,
            })
            .map_err(SpawnError::from),
            LaunchPath::Duplex => duplex_child(
                &inv,
                ChildDuplex {
                    agent_id: &agent_id,
                    tmpdir: node_tmp.path(),
                    ready_file: ready_file
                        .as_deref()
                        .expect("the duplex path always mints a marker"),
                    prompt: &req.prompt,
                    bound: attempt_bound,
                    depth: ctx.depth,
                    events: events.as_ref(),
                    on_started: &announce_started,
                    session: &session,
                    turns: feed(),
                    stop_on: &|frame| adapter.auth_refusal(frame),
                    dialect: duplex::Dialect::of(adapter.spec()),
                },
            ),
        };
        // **Checked before the launch's own `?`, and that ordering is the whole point.** The kill above
        // makes the driver return *something* — a signalled exit on one path, a `DuplexError` on the
        // other — and either of those, reported as itself, would name the symptom and bury the cause.
        // Reading the cell first means the caller is told the one thing that is true about this node:
        // marion could not record it, so marion does not have it.
        if let Some(why) = unaccountable.take() {
            return Err(SpawnError::UnaccountableNode {
                agent_id: agent_id.clone(),
                // The error's *shape*, never a copy of the record that would not fit. This string is
                // journaled inside a `SpawnAborted`, and pasting a 16 KiB record into the explanation
                // of why a 16 KiB record was refused would fail the same cap twice.
                why: why.to_string(),
            });
        }
        // An attempt the operator killed is the node's end, never a reason to rotate to the next
        // key: asked without settling the race, which `process_ended` does once, after the loop.
        let killed = observer.kill_requested(&agent_id);
        // A driver that failed because the kill landed before its first turn (the node died before
        // `initialize`, say) is a node marion *did* decide the fate of: the kill's `KillConfirmed`
        // is its terminal record, so the abort guard must not add a `SpawnAborted` beside it. The
        // error still goes back to the caller as the driver reported it.
        if run.is_err() && killed {
            resolution.armed = false;
        }
        let mut run = run?;
        // An endpoint node's key never outlives the process in what it wrote: a harness that echoes
        // its credential in an error would otherwise put it in the contract and the event log.
        redact_run(&mut run, endpoint.as_ref(), gateway.as_ref());
        // A kill never becomes a failover: an attempt the operator ended is the node's end.
        if killed {
            break (launch, inv, run, version);
        }
        match next_attempt(
            endpoint.as_ref(),
            &profiles,
            at,
            &run,
            adapter.as_ref(),
            &wt,
            base.as_ref(),
            bound.saturating_sub(launched_at.elapsed()),
        ) {
            Some((Next::Credential(next), cause)) => {
                failovers.push(marion_core::contract::CredentialFailover {
                    from: endpoint
                        .as_ref()
                        .map(|e| e.credential.to_string())
                        .unwrap_or_default(),
                    to: next.credential.to_string(),
                    cause,
                });
                endpoint = Some(next);
            }
            // Journaled before the relaunch, as the profile it names.
            Some((Next::Profile(next), cause)) => {
                profiles.record_failover(at, next, &cause, &env.project_dir, &agent_id);
                at = next;
                session.restart(at);
            }
            None => break (launch, inv, run, version),
        }
        clear_ready_marker(ready_file.as_deref());
    };
    let announce_started =
        |pid: i32| announce(pid, &version, inv.model.as_deref(), endpoint.as_ref());
    // **The process has ended; nothing terminal is written yet.** The one instant at which "did
    // marion end this on a kill?" can be asked and answered for good — see
    // [`SpawnObserver::process_ended`].
    let ended_by_kill = observer.process_ended(&agent_id);
    // **Every permission marion refused on this child's behalf**, through the same emitter the root
    // uses (`journal::record_permission_denials`), which is also where the argument for the journal
    // being the *only* destination lives. Until this call existed `duplex_child` discarded
    // `DuplexOutcome.denied_permissions` and a child's denial appeared in no contract and no
    // journal record at all — while §5.2's ask is genuinely reachable on a duplex child, since
    // `claude_code::SPEC` carries `--permission-prompt-tool stdio` unconditionally.
    //
    // Written after `Spawned` and before `Exited`, which is the order the events happened in: the
    // ask can only arrive from a process that exists, and only before it has finished.
    crate::journal::record_permission_denials(
        &env.project_dir,
        &agent_id,
        &run.denied_permissions,
        "denied at once: marion has nobody to ask for a permission, so it denies every ask no rule \
         decides (§9)",
    );
    // §6.1 step 9, through the same seam as step 5. This was codex-JSONL-specific until now, so a
    // gemini or opencode child's report was unreadable and its contract said `Unreported` about a
    // run that had reported — the §12 silent-failure shape, one layer down from the dispatch bug.
    let mut parsed = adapter.parse_stream(&run.stdout, run.exit);
    // A resume the harness answered with a fresh session (agy's unknown `--conversation`, exit 0)
    // is a run with none of the resumed node's history: the more specific claim, so it wins.
    if let Some(why) =
        adapter.resume_refusal(&run.stdout, req.resume.as_ref().map(|r| r.session.as_str()))
    {
        parsed.failure = Some(why);
    }
    let row = marion_harness::adapter::harness_spec(adapter.harness());
    let mut outcome = ChildOutcome::from_stream(parsed, run.exit, row.quiet_stderr(&run.stderr));
    note_stopped(&mut outcome, &run);
    // **§7.6's descendant gate, at the only moment it can run**: the process has stopped and
    // nothing terminal is written yet — no `Exited`, no contract, no closing bookend. A voluntary,
    // unreported stop with a live descendant is *held* here, on the remainder of `bound`, and the
    // verdict is applied to the contract below once `build_contract` has assembled it.
    //
    // A node marion killed did not *stop*: it was ended, and holding it for its descendants — or
    // continuing it into another generation — would hold a cancellation the operator asked for.
    // It is exempt the way a timeout is.
    // **And every stop is a turn boundary** (`crate::continuation::boundary`): on the
    // continuation lane a message waiting now, or arriving during the hold, relaunches this same
    // node under its observed session as its next generation, on what is left of the one `bound`.
    // The gate runs again at that generation's stop, over the outcome folded so far, and only the
    // last stop's verdict reaches the contract. Every other lane passes no inbox and gates once.
    let deadline =
        Instant::now() + bound.saturating_sub(spawned_at.0.elapsed().unwrap_or_default());
    let mut generation = 1u32;
    let gated = if ended_by_kill {
        crate::descendant_gate::Gated::default()
    } else {
        loop {
            let mut gate =
            |o: &ChildOutcome,
             wait: &mut crate::descendant_gate::Pause<'_, crate::inbox::Message>| {
                crate::descendant_gate::gate_or_woken(
                    observer,
                    &agent_id,
                    &env.project_dir,
                    o,
                    spawned_at.0,
                    bound,
                    wait,
                )
            };
            let turn = crate::continuation::boundary(
                continuation.as_ref(),
                &outcome,
                session.session().as_deref(),
                deadline,
                &mut gate,
            );
            let (message, resume) = match turn {
                crate::continuation::Turn::Last(gated) => break gated,
                crate::continuation::Turn::Next { message, session } => (message, session),
            };
            let Some(turns) = continuation.as_ref() else {
                unreachable!("a next turn is only ever taken from an inbox")
            };
            generation += 1;
            let via = format!("continuation:gen{generation}");
            // The same launch, resumed: `node/resume`'s spelling (the row's resume grammar), with the
            // message as the prompt, compiled and declared exactly as the first generation was.
            let next_launch = LaunchSpec {
                resume: Some(resume),
                prompt: crate::inbox::render(&message),
                ..launch.clone()
            };
            let next_inv = match declare_and_compile(adapter.as_ref(), &next_launch, &ctx) {
                Ok(inv) => inv,
                Err(e) => {
                    turns.dropped(
                        &message.id,
                        &format!("the continuation that would carry it could not be compiled: {e}"),
                    );
                    continue;
                }
            };
            // Delivered at the instant the process that carries it exists and is journaled — the
            // generation's own `Spawned`, through the first generation's confirmation.
            let carried = std::cell::Cell::new(false);
            let announce_generation = |pid: i32| {
                announce_started(pid);
                let failed = unaccountable.take();
                if failed.is_none() {
                    turns.delivered(&message.id, &via);
                    carried.set(true);
                }
                unaccountable.set(failed);
            };
            let left = deadline.saturating_duration_since(Instant::now());
            let next = launch_only_child(
                &next_inv,
                node_tmp.path(),
                left,
                &announce_generation,
                &session,
                events.as_ref(),
                adapter.as_ref(),
            );
            if let Some(why) = unaccountable.take() {
                turns.dropped(
                    &message.id,
                    "the continuation's process could not be journaled",
                );
                return Err(SpawnError::UnaccountableNode {
                    agent_id: agent_id.clone(),
                    why: why.to_string(),
                });
            }
            let mut next = match next {
                Ok(next) => next,
                // A process that never started carried nothing; one that did was already delivered to,
                // and its failure is the node's outcome of the turn it did not finish.
                Err(e) if !carried.get() => {
                    turns.dropped(
                        &message.id,
                        &format!("the continuation that would carry it did not start: {e}"),
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };
            // Redacted like the first generation's capture, and for the same reason.
            redact_run(&mut next, endpoint.as_ref(), gateway.as_ref());
            let mut later = ChildOutcome::from_stream(
                adapter.parse_stream(&next.stdout, next.exit),
                next.exit,
                row.quiet_stderr(&next.stderr),
            );
            note_stopped(&mut later, &next);
            outcome = crate::continuation::fold(outcome, later);
            run = ChildRun {
                capture_truncated: run.capture_truncated || next.capture_truncated,
                ..next
            };
        }
    };

    // **§6.7's honest degradation, and the `Option` is the whole of it.**
    //
    // Both routes need a `base_commit` to diff against, so with no commit there is no route — which
    // is precisely the case §6.7 reserves `scope_enforced: false` for: the flag records whether the
    // check *ran*, not whether it passed. `None` propagates that into `build_contract`, which is the
    // only thing that can turn it into the flag.
    //
    // `changed_paths(..)` used to be `.unwrap_or_default()`, which collapsed *failed* into *empty*
    // and left the flag `true` — a clean bill of health issued by a check that never ran. Failure
    // and absence now travel the same honest path, because a git call that errored told marion
    // nothing about what changed either.
    //
    // No `changed_paths` is fabricated for the no-git case, and there is deliberately no filesystem
    // fallback (mtimes, a directory walk): git is §6.7's one authority for this field, so a second
    // source would be a different measurement wearing the same field name.
    let changed = base.as_ref().and_then(|b| changed_paths(&wt, b).ok());
    let diff = base
        .as_ref()
        .and_then(|b| diff_text(&wt, b).ok())
        .filter(|d| !d.is_empty());
    // **The child's work onto its own branch, over the same sealed tree the diff describes** —
    // before verification can write into it, and before the reap below removes it. Without this
    // the reap deleted uncommitted work and the diff text was the only copy left.
    // A reviewer's change is a violation to record, never work to land.
    let landed = crate::spawn::Landed::land(
        &workspace,
        base.as_ref().filter(|_| req.review.is_none()),
        changed.as_deref(),
        &crate::spawn::commit_message(
            &req.agent_type,
            &task_id.0,
            outcome.narrative.as_deref(),
            &req.prompt,
        ),
    );
    // **After `changed_paths` and the diff, never before.** Verification writes into the worktree
    // — a `cargo test` leaves a `target/`, a formatter rewrites files — and §6.7's diff is the
    // child's work, so the measurement is taken first and the commands run over the sealed
    // result. The request itself is always recorded (`verification_commands`), so the contract
    // says what was asked even where `verification_evidence` decides nothing ran.
    let verification = verification_commands(&req.verification, &wt);
    // Nothing runs over a killed node's workspace, for `verification_evidence`'s own reason about a
    // timed-out one: it is whatever the kill left.
    let evidence = if ended_by_kill {
        vec![]
    } else {
        verification_evidence(&outcome, &verification)
    };
    let mut contract = build_contract(
        task_id.clone(),
        AgentId(caller.agent_id.clone()),
        // Read off the **adapter**, not off `agent_type`: the contract is §6.7's audit record, so
        // the harness it names must be the one that actually produced the work, never the one that
        // was asked for. Sourcing it here makes that an invariant the code enforces rather than one
        // a reader has to check.
        adapter.harness(),
        RepoIdentity {
            git_common_dir: crate::socket::git_common_dir(&req.repo),
            head_branch: None,
        },
        base,
        workspace,
        &req.prompt,
        &req.acceptance_criteria,
        &agent_type.scope_ceiling,
        &requested,
        // **`bound`, not `req.timeout_secs`** — §6.7's compiled-value rule applied to the wall
        // clock. The contract is the audit record of what marion did, so it must name the clock the
        // node actually ran under; recording the asked-for number would let a clamped request
        // (see [`MAX_TIMEOUT_SECS`]) produce a contract asserting a bound that was never enforced.
        Duration::from_secs(bound.as_secs()),
        spawned_at,
        &outcome,
        changed,
        diff,
        verification,
        evidence,
    );
    note_capture_truncated(&mut contract, run.capture_truncated);
    // The run's cause, and on a usage limit the notice the parent reads — nothing more.
    if contract
        .completion
        .as_ref()
        .is_some_and(|c| c.status != marion_core::contract::ExitStatus::Ok)
    {
        let stream = adapter.parse_stream(&run.stdout, run.exit);
        profiles.settle(
            at,
            adapter.harness(),
            &mut contract,
            attempt_cause(
                adapter.as_ref(),
                &run,
                stream.failure.as_deref(),
                endpoint.as_ref(),
            ),
        );
    }
    contract.child.version = version;
    // Read off the **compiled invocation** for the same reason, one step further: §3.1 makes the
    // marion-name → harness-name mapping the adapter's, and §6.7's `allowed_tools` records "the
    // compiled, harness-native constraint". So this records what went on the wire — which is
    // `None` for codex, whose `exec` surface carries no model argument, even when the request or
    // the agent type named one.
    //
    // **Except where the stream names the model that ran**, which answers the one case argv cannot:
    // a launch that named no model ran the harness's own default, and argv's `None` would record
    // an absence about a model that did run (live smoke s2: claude's `init` said
    // `claude-haiku-4-5-20251001` for `--model haiku`).
    contract.child.model = events
        .as_ref()
        .and_then(|es| es.model())
        .or_else(|| inv.model.clone());
    // Where an endpoint node's requests went, beside the model that went there.
    contract.child.provider = endpoint.as_ref().map(|e| e.provider.clone());
    contract.child.route = endpoint.as_ref().map(|e| e.route.as_str().to_string());
    contract.child.credential = endpoint.as_ref().map(|e| e.credential.to_string());
    contract.child.credential_failover = failovers;
    // **The third field sourced from what ran rather than from what was asked for**, joining
    // `harness` and `model` above (`32ec905`). §6.7's `allowed_tools` records *"the compiled,
    // harness-native constraint — or the harness's coarsest equivalent where it has no per-tool
    // allowlist at all"*, and only the adapter that just compiled `launch` knows which of those
    // this node got, or what it says.
    //
    // Derived from the same `launch` the invocation was compiled from, so the record cannot
    // describe a launch that did not happen. It varies per node now that an agent type can declare
    // tools: before this axis every child was `[report]` and a constant lost nothing, which is
    // exactly why the constant survived so long — the contract is the only durable record of what a
    // given child was actually permitted, and §11 item 24 is what happens when that record and the
    // run disagree.
    //
    // The `?` cannot fire in practice — `compile` above maps the same declaration and would have
    // refused first — and it is propagated rather than swallowed because an audit record that
    // silently guesses is worse than a spawn that stops.
    contract.allowed_tools = adapter.compiled_permissions(&launch)?;
    let tally = req
        .review
        .as_ref()
        .and_then(|t| record_review(&mut contract, t, adapter.spec().read_only));
    // §7.6's flags, from the gate that ran above — `reported_early`, `held_to_timeout`,
    // `died_before_gate` and the live set — written onto the completion before it reaches disk.
    gated.apply(&mut contract);
    landed.apply(&mut contract);
    if ended_by_kill {
        record_cancelled(&mut contract);
    }
    // **What the node spent, from the stream as it was recorded** — every generation's, added —
    // into the contract, and into the journal's one record of it, before the terminal records.
    let spent = spend.record();
    if let Some(completion) = contract.completion.as_mut() {
        completion.usage = spent;
    }
    let returned = persist_contract_and_close_stream(
        env,
        &agent_dir,
        &agent_id,
        &contract,
        events.as_ref(),
        &req.agent_type,
        ended_by_kill,
    )?;
    // §4.3: the journal records **that a contract exists and how it ended**, never its contents —
    // the file is the contract (§6.7), and copying it here would be a second source of truth.
    // Written after `persist_then_cap` returns, so the record cannot claim a file that was never
    // written; the node is `agent_id` (the child the contract is about) and `requester` is the
    // caller, which is §6.7's own distinction, kept.
    crate::journal::record(
        &env.project_dir,
        RecordKind::ContractPersisted(ContractPersisted {
            review: tally,
            agent_id: agent_id.clone(),
            task_id: task_id.clone(),
            requester: AgentId(caller.agent_id.clone()),
            status: contract.completion.as_ref().map(|c| c.status),
        }),
    );
    // The intent is resolved: `Spawned` and `Exited` are on the record above, so the abort this
    // guard would otherwise write would contradict them.
    resolution.armed = false;
    // Only a tree marion made is marion's to remove. A `shared-cwd` child ran in the caller's own
    // directory, and git refuses `worktree remove` only on the *main* working tree — so a caller in
    // a linked worktree would have its checkout deleted by the cleanup of a child it lent it to.
    // And not while it holds the only copy of work marion failed to commit.
    if let Workspace::Worktree { path, branch } = &contract.workspace
        && landed.may_reap()
    {
        cleanup(&req.repo, path, branch, contract.base_commit.as_ref());
    }
    Ok(returned)
}

/// **§6.7's classification of a child marion ended on an operator's kill**: `Cancelled`, whatever
/// the stream or the exit code would have made of the signalled process — the same precedence a
/// timeout has in `build_contract`, because both are marion's own attributed act. The description
/// keeps what marion observed of the process and names marion as the sender first, as §6.7 asks.
/// A reviewer's contract, finished: the row's read-only switch named in `allowed_tools` beside the
/// constraint the row always records, and its report read into `Completion::findings`. Returns the
/// tally the journal carries, or `None` where there is nothing to count — the reviewer never
/// reported, or its report could not be read, which is said in the exit description and is never
/// a block.
fn record_review(
    contract: &mut TaskContract,
    target: &crate::review::Target,
    read_only: marion_harness::spec::ReadOnly,
) -> Option<marion_core::review::ReviewTally> {
    contract
        .allowed_tools
        .push(format!("read-only:{}", read_only.kind()));
    let completion = contract.completion.as_mut()?;
    if !read_only.blocks_writes() {
        completion.exit.description = format!(
            "{}; {}",
            completion.exit.description,
            crate::review::UNGUARDED
        );
    }
    let narrative = completion.narrative.as_ref().map(|n| n.value.as_str());
    match crate::review::verdict(narrative, target)? {
        Ok(v) => {
            let tally = marion_core::review::ReviewTally::of(&v);
            completion.findings = Some(v.findings);
            Some(tally)
        }
        Err(e) => {
            completion.exit.description = format!("{}; {e}", completion.exit.description);
            None
        }
    }
}

fn record_cancelled(contract: &mut TaskContract) {
    if let Some(completion) = contract.completion.as_mut() {
        completion.status = ExitStatus::Cancelled;
        completion.exit.description = format!(
            "marion ended this node on the operator's kill (node/kill or session/quit KillTree); {}",
            completion.exit.description
        );
    }
}

/// The scope a child asked for, in §5.4's vocabulary: an empty `writable_scope` is the whole
/// workspace, not nothing.
fn requested_scope(req: &SpawnRequest) -> Vec<Glob> {
    // A reviewer may write nothing: an empty list matches no path, so every change it makes is a
    // scope violation on its contract — the record every row's read-only switch backs up.
    if req.review.is_some() {
        return Vec::new();
    }
    if req.writable_scope.is_empty() {
        vec![Glob("**".into())]
    } else {
        req.writable_scope.iter().cloned().map(Glob).collect()
    }
}

/// **§6.6's two workspaces, selected by §5.4's `isolation` and by nothing else.**
///
/// `make_worktree` used to run unconditionally, which is what made git a *precondition* for
/// running marion at all rather than one strategy for containing a child: §2 keys a project on
/// *"the git common-dir, falling back to cwd"* and is explicit that the alternative was
/// "refusing to run outside git", but a `spawn` in a directory with no repository died on git's
/// own stderr several steps further in. The two arms below are the whole of that fix.
///
/// The order matters. Everything that can be refused is refused before `make_worktree`, because
/// a worktree is the first *irreversible* thing a spawn does — a failed spawn that already added
/// one leaves a directory, a `.git/worktrees/` entry and a branch behind.
///
/// Returns the workspace, the commit §6.7's diff is taken against (`None` where there is none),
/// and the cwd claim the caller must hold for the child's whole run.
fn select_workspace(
    req: &SpawnRequest,
    agent_type: &AgentType,
    project: &ProjectDir,
    task_id: &TaskId,
    agent_id: &AgentId,
) -> Result<
    (
        Workspace,
        Option<Oid>,
        crate::spawn::CwdClaim,
        PrelaunchWorktree,
    ),
    SpawnError,
> {
    let agent_dir = &project.agent(agent_id);
    // **A resume takes the tree it left, and this arm is why the workspace is journaled at all.**
    // Neither branch below is right for a second life: `make_worktree` on a tree that already
    // exists fails, and a *fresh* worktree would be a directory the resumed session has never seen
    // — the harness would refuse it or start over, and marion would have called that a resume.
    // §6.6's occupancy claim is retaken for a `shared-cwd` node, because the claim died with the
    // supervisor that held it and the guarantee it makes has not changed.
    if let Some(r) = &req.resume {
        let base = crate::spawn::head_commit(r.workspace.path());
        let claim = match &r.workspace {
            Workspace::SharedCwd { path }
                if marion_harness::writes_files(agent_type) && !req.allow_concurrent_writes =>
            {
                crate::spawn::CwdClaim::claim(path, agent_id)?
            }
            _ => crate::spawn::CwdClaim::none(),
        };
        return Ok((r.workspace.clone(), base, claim, PrelaunchWorktree::none()));
    }
    match req.isolation {
        Isolation::Worktree => {
            // **Asked before it is attempted**, so the answer is marion's sentence and not git's.
            // `git_common_dir` is §2's own derivation, so "not a repository" here and "keyed on cwd"
            // there are one determination rather than two that could drift.
            if crate::socket::git_common_dir(&req.repo).is_none() {
                return Err(SpawnError::NotAGitRepo {
                    cwd: req.repo.clone(),
                });
            }
            let wt = agent_dir.worktree();
            crate::private_fs::create_dir_all(wt.parent().expect("agent worktree has a parent"))?;
            let branch = worktree_branch(task_id);
            // A reviewer's tree is the reviewed work as it landed, so it reads what it judges.
            let at = req.review.as_ref().and_then(|t| t.commit.as_ref());
            // A caller in a worktree marion made (under this project's agent dirs) hands its child
            // its current state; the operator's own checkout is never committed to. See
            // `make_worktree`.
            let carry_work = req.repo.starts_with(project.agents_dir());
            let base = crate::spawn::make_worktree_at(&req.repo, &wt, &branch, at, carry_work)?;
            let prelaunch = PrelaunchWorktree {
                repo: req.repo.clone(),
                target: Some((wt.clone(), branch.clone(), base.clone())),
            };
            Ok((
                Workspace::Worktree { path: wt, branch },
                Some(base),
                crate::spawn::CwdClaim::none(),
                prelaunch,
            ))
        }
        Isolation::SharedCwd => {
            // **The caller's own directory, untouched.** No worktree, no branch, and deliberately
            // no `git init`: §6.6 says marion never auto-merges, and creating a repository in
            // someone's directory is a larger uninvited act than merging into one.
            //
            // `base_commit` is HEAD *if there is a HEAD* — a `shared-cwd` child in a repository
            // still affords §6.7's diff. Outside a repository there is no commit, and `None` is the
            // honest value; `head_commit` returns it rather than inventing a zero oid, and
            // everything downstream that needs a base is `Option`-typed for exactly this case.
            let base = crate::spawn::head_commit(&req.repo);
            // §6.6: at most one write-capable node per cwd. Taken *before* the child exists and
            // released when this claim drops, which is every exit from `run_spawn_watched`.
            let claim = if marion_harness::writes_files(agent_type) && !req.allow_concurrent_writes
            {
                crate::spawn::CwdClaim::claim(&req.repo, agent_id)?
            } else {
                crate::spawn::CwdClaim::none()
            };
            Ok((
                Workspace::SharedCwd {
                    path: req.repo.clone(),
                },
                base,
                claim,
                PrelaunchWorktree::none(),
            ))
        }
    }
}

/// **A child's spend reaches the journal on every way out of its run.**
///
/// Measured live (s2, 2026-09-27): a claude child killed at its 180 s timeout recorded no usage,
/// and a run whose driver returns an error — a torn duplex stream, a process marion could not
/// journal — left through a `?` before the one line that recorded it. Whatever the stream stated,
/// every generation's, is written once: by [`Self::record`] on the contract path, before the
/// terminal records, or on drop by any other exit.
struct SpendOnEveryEnd<'a> {
    project: &'a ProjectDir,
    agent_id: &'a AgentId,
    events: Option<&'a crate::events::EventSink>,
    recorded: bool,
}

impl SpendOnEveryEnd<'_> {
    /// Record the spend now and say what it was; the drop then records nothing more.
    fn record(&mut self) -> Option<TokenUsage> {
        self.recorded = true;
        let spent = self.events.map(|es| es.spent()).unwrap_or_default();
        let usage = spent.usage;
        crate::journal::record_usage(self.project, self.agent_id, spent);
        usage
    }
}

impl Drop for SpendOnEveryEnd<'_> {
    fn drop(&mut self) {
        if !self.recorded {
            self.record();
        }
    }
}

/// A node's [`crate::app_server::AppServerSpec`], shared by a child and a root: the row's channel,
/// the adapter's opening request, and the readiness gate on marion's server where the launch
/// declares it. `None` for a row with no thread channel, which no app-server path reaches.
#[allow(clippy::too_many_arguments)]
pub(crate) fn app_server_spec<'a>(
    adapter: &dyn marion_harness::HarnessAdapter,
    inv: &'a Invocation,
    tmpdir: &'a std::path::Path,
    launch: &LaunchSpec,
    ctx: &SpawnCtx,
    prompt: &'a str,
    bound: StdDuration,
    on_started: &'a dyn Fn(i32),
    on_line: &'a dyn Fn(&str),
    turns: Option<crate::inbox::TurnFeed>,
    stop_on: duplex::StopOn<'a>,
) -> Result<crate::app_server::AppServerSpec<'a>, SpawnError> {
    let channel = adapter
        .spec()
        .surfaces
        .rpc()
        .ok_or(SpawnError::UnsupportedChildSurface(adapter.harness()))?;
    let opening = adapter
        .session_declaration(launch, ctx)?
        .ok_or(SpawnError::UnsupportedChildSurface(adapter.harness()))?;
    Ok(crate::app_server::AppServerSpec {
        inv,
        tmpdir,
        channel,
        opening,
        gate: (launch.mcp == marion_harness::McpDeclaration::Marion)
            .then_some(marion_harness::spec::MCP_ALIAS),
        mcp_ready: CHILD_MCP_READY_TIMEOUT,
        prompt,
        bound,
        on_started,
        on_line: Some(on_line),
        turns,
        stop_on: Some(stop_on),
    })
}

/// Not one of §4.3's normative files: marion's own start-up handshake with a process it did not
/// spawn. Only the duplex path has a frame to withhold, so only it has a marker to wait on.
fn child_ready_file(path: LaunchPath, agent_dir: &AgentDir) -> Option<PathBuf> {
    match path {
        LaunchPath::Duplex => {
            let f = agent_dir.path().join("mcp-ready");
            let _ = std::fs::remove_file(&f);
            Some(f)
        }
        // **The ACP gate is `session/new`'s own response, and it is the agent's rather than
        // marion's.** marion does not start this bridge: the agent does, off the `mcpServers` block
        // in the declaration, and it answers `session/new` when the session — its declared MCP
        // servers included — is open. That answer is what `run_acp_child` waits on before a prompt
        // goes out, so the frame *is* withheld behind a gate; the gate is just not a file marion
        // touches. S21 and S23 both measured the tool call landing after it, on two different
        // providers, which is the evidence a marker would otherwise be standing in for.
        //
        // A marker would also be a second gate with nothing behind it: the bridge writes it, and on
        // this path marion has no way to tell whether the agent even intends to start the bridge
        // before it has answered.
        LaunchPath::Acp => None,
        // **The app-server gate is the server's own startup notification** for marion's MCP
        // server, which `run_app_server` waits on before the first turn (S36 P3): the server
        // starts the bridge, and says when it is ready.
        LaunchPath::AppServer => None,
        // §9 gives a child a `TaskContract`, and a pane node takes no turn until a human presses
        // return — so there is no readiness gate to hold, and see `run_spawn_watched`'s launch arm
        // for why a contracted child does not get one at all.
        LaunchPath::LaunchOnly | LaunchPath::Terminal => None,
    }
}

/// The child's launch, in the neutral vocabulary the adapter compiles from. Every field is either
/// read off the resolved agent type, decided by the launch path, or decided by the child's `depth`
/// — never by a harness name.
///
/// Public for its second reader, the conformance battery (`tests/conformance`), which must drive
/// each harness under exactly the launch a spawn compiles rather than a copy that could drift.
#[allow(clippy::too_many_arguments)]
pub fn child_launch_spec(
    env: &Env,
    req: &SpawnRequest,
    agent_type: &AgentType,
    adapter: &dyn marion_harness::HarnessAdapter,
    path: LaunchPath,
    depth: u32,
    wt: &Path,
    ch: &Path,
) -> LaunchSpec {
    LaunchSpec {
        cwd: wt.to_path_buf(),
        // Was a hard `None` until now, which is why a gemini or opencode agent type could be named,
        // resolved and dispatched — and then refused at `compile`, since both adapters make an
        // explicit model a MUST. See `resolve_model`.
        model: resolve_model(req, agent_type, env.auth),
        // §6.1 step 8: on a typed control plane the prompt is a frame written **after** the
        // readiness gate, so nothing is compiled into argv and the adapter is told so by the empty
        // string — the neutral vocabulary's own signal for "written after launch".
        prompt: match path {
            // ACP for the same reason, one protocol over: the prompt is a `session/prompt` frame
            // and reaches argv on no ACP agent. `AcpAdapter::compile` ignores this field entirely,
            // and passing `req.prompt` here would put the task text in the audit record's argv
            // where the launch never put it.
            LaunchPath::Duplex | LaunchPath::Acp | LaunchPath::AppServer => String::new(),
            LaunchPath::LaunchOnly | LaunchPath::Terminal => req.prompt.clone(),
        },
        // The **availability** axis (§3.1), straight off the resolved agent type and still in
        // marion's vocabulary — the adapter about to run maps it, and refuses by name what its
        // harness cannot provide (`HarnessError::UnsupportedTool`).
        //
        // **This is the field `--tools ""` was hardcoded for want of** (§11 item 24). Until it
        // existed a claude or gemini child was read-only by construction: marion spawned it to do
        // work and declared it no tool with which to change anything, and the contract it persisted
        // — `changed_paths: []`, `scope_violations: []`, `scope_enforced: true` — was byte-identical
        // to a child whose write escaped its worktree. Empty on every built-in, so nothing marion
        // spawns today is launched any differently.
        // A reviewer declares no `write` on any row; its row's `ReadOnly` switch does the rest.
        tools: agent_type
            .tools
            .iter()
            .filter(|t| req.review.is_none() || t.as_str() != marion_core::agent_type::TOOL_WRITE)
            .cloned()
            .collect(),
        // The **permission** axis (§3.1), in marion's vocabulary translated by the adapter that is
        // about to run. A child's one load-bearing call is `report`; on Claude Code an unlisted
        // tool is auto-denied *in process*, and on a `LaunchOnly` child there is no control plane
        // for the denial to be asked about — so an empty list here is a run that completes having
        // reported nothing, with no error anywhere. The three harnesses whose adapters read no
        // permission list are unaffected: they ignore it, exactly as they did when it was empty.
        //
        // **Which of marion's verbs: one rule for every harness** (`agent_type::child_verbs`).
        // `report` always, and the delegation verbs while this child may still spawn — so an
        // allowlist harness (claude's `--allowedTools`, copilot's `--allow-tool`, qwen's
        // `--core-tools`) grants a grandchild exactly where codex, gemini and opencode, which
        // compile no allowlist, already reach the depth gate. It was `[report]` alone, which made a
        // claude child unable to delegate while a codex child could. At the bound it is `[report]`
        // again, and the gate still refuses a `spawn` that arrives anyway.
        //
        // **marion's own verbs only, and the declared tools are unioned in by the adapter.** §3.1's
        // table compiles this axis from *"the same list, plus marion's own `mcp__marion__*`"*, and
        // doing the union at the one place both axes are compiled is what makes them unable to
        // disagree. Appending here instead would grant permission without availability — the mirror
        // of item 24's dead end, and just as silent.
        allowed_tools: marion_core::agent_type::child_verbs(agent_type, depth)
            .into_iter()
            // A reviewer reports and does nothing else: it delegates no part of a judgement.
            .filter(|verb| req.review.is_none() || *verb == marion_core::agent_type::REPORT_VERB)
            .map(|verb| adapter.marion_tool_name(verb))
            .collect(),
        mcp: McpDeclaration::Marion,
        base_url: env.base_url.clone(),
        // **Present, and deliberately a placeholder.** The endpoint is marion's own, so this
        // authenticates nothing — but a credential *slot* that is empty is not the same as one that
        // is unused, and two of the four harnesses refuse outright when it is: gemini's adapter
        // selects `security.auth.selectedType = "gemini-api-key"` (without which S12 measured
        // `Invalid auth method selected.`, code 41), and 0.53.0 then exits **41** with *"you must
        // specify the GEMINI_API_KEY environment variable"* when the variable it named is absent.
        // A hard `None` here made that harness unlaunchable through `spawn` no matter what the
        // adapter compiled. codex has always been seeded the same way — its generated config names
        // `env_key = "MARION_PROVIDER_KEY"` and its row compiles it from this field — so this generalises an
        // existing decision rather than making a new one.
        //
        // Under `Auth::Inherited` there is nothing to placehold: the endpoint is the vendor's, the
        // credential is the operator's already-established login, and a placeholder pushed beside it
        // would be a second credential competing with the real one.
        api_key: match env.auth {
            Auth::Canned => Some(PLACEHOLDER_API_KEY.into()),
            // A supervisor never runs in endpoint mode; a node's stored key is placed by
            // `resolve_endpoint`.
            Auth::Inherited | Auth::Endpoint => None,
        },
        auth: env.auth,
        config_dir: ch.to_path_buf(),
        // **The agent type's own `acp_agent`, and the reason it is stated twice.** The adapter was
        // bound from this same value above, and `AcpAdapter::agent` refuses a launch where the two
        // disagree rather than letting one win. That is not redundancy: they are two routes to one
        // answer, and a launch that compiled agent A's argv while reading agent B's tool spelling
        // out of the transcript is precisely the bug the `HarnessAdapter` seam exists to end. One
        // assignment here keeps them the same value by construction, and the adapter's check is
        // what catches a second assignment appearing later.
        //
        // The session this launch resumes, handed to the row's measured resume flag — or `None` on
        // every fresh spawn. A row whose caps refuse resume refuses the launch by name here rather
        // than starting fresh under a resumed node's id.
        resume: req.resume.as_ref().map(|r| r.session.clone()),
        // Filled by endpoint resolution, where the child names a provider.
        wire: None,
        provider: None,
        extra: Extras {
            read_only: req.review.is_some(),
            acp_agent: agent_type.acp_agent.clone(),
            // The type's ACP session mode; any non-ACP adapter refuses a launch carrying one.
            approval_mode: agent_type.approval_mode.clone(),
            ..Extras::default()
        },
    }
}

/// The branch a child's worktree is cut on: one per task.
pub(crate) fn worktree_branch(task_id: &TaskId) -> String {
    format!("marion/{}", task_id.0)
}

/// **One launch of a node, declared and compiled**: the adapter's configuration documents written,
/// then its invocation compiled from the same spec. A node's first generation and every
/// continuation of it (`crate::continuation`) go through this one step, so a relaunch is declared
/// exactly as the launch it resumes was.
fn declare_and_compile(
    adapter: &dyn marion_harness::HarnessAdapter,
    launch: &LaunchSpec,
    ctx: &SpawnCtx,
) -> Result<Invocation, SpawnError> {
    write_config_documents(adapter.config_files(launch, ctx)?)?;
    Ok(adapter.compile(launch, ctx)?)
}

/// Write the configuration documents an adapter decided on, creating each document's directory.
///
/// The adapter decides *what and where*; marion writes. "Where" is not always directly under
/// `config_dir`: opencode's document lands at `<config_dir>/config/opencode/opencode.json`,
/// because `$XDG_CONFIG_HOME` is a directory whose layout the harness owns. Creating only
/// `config_dir` failed the whole launch with a bare `No such file or directory` naming nothing.
///
/// Returns the paths written, in order, for a caller that checks the declaration against them.
pub(crate) fn write_config_documents(
    files: Vec<(PathBuf, String)>,
) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::with_capacity(files.len());
    for (path, contents) in files {
        // Owner-only: a document can carry the node's capability token and, on an endpoint node,
        // the user's key.
        std::io::Write::write_all(&mut crate::private_fs::create(&path)?, contents.as_bytes())?;
        written.push(path);
    }
    Ok(written)
}

/// §6.1 step 7's confirmation for a child, built at the one instant `pid` is an identity.
///
/// **The identity is read here and nowhere else.** `announce_started` is the only instant at which
/// the read is race-free by construction: marion holds the `Child`, so the pid cannot be reaped and
/// cannot be recycled between `command.spawn()` returning it and this line. Reading it later — at
/// the exit, on a restart, from any other thread — would be reading a number that may already
/// belong to someone else, which is the very confusion the field exists to end. Measured: a zombie
/// still resolves, but a *reaped* pid does not, so anywhere after the reap is too late.
///
/// `None` on a platform that cannot read one. That is not a failure of the spawn and must not be
/// treated as one: it resolves to *cannot-tell* later, which is the honest answer, and refusing to
/// launch over it would take marion off every platform whose start-time read has not been measured
/// yet.
fn child_spawned_record(
    agent_id: &AgentId,
    version: &str,
    model: Option<&str>,
    pid: i32,
    endpoint: Option<&crate::endpoint::Endpoint>,
) -> Spawned {
    Spawned {
        agent_id: agent_id.clone(),
        harness_version: version.to_string(),
        // The **compiled** value, for §6.7's reason: what went on the wire, never what was asked
        // for. `None` on codex, whose `exec` surface carries no model argument.
        model: model.map(str::to_string),
        // A real signal target: where to send a signal *now*.
        pid: Some(pid),
        start_id: match crate::procid::read(pid) {
            crate::procid::Read::Id(id) => Some(id),
            // The process was spawned moments ago and marion is holding it, so neither of these
            // should be reachable here — but a `Spawned` record is not the place to assert that,
            // and a wrong identity would be far worse than a missing one.
            crate::procid::Read::NoSuchProcess | crate::procid::Read::Unavailable(_) => None,
        },
        provider: endpoint.map(|e| e.provider.clone()),
        route: endpoint.map(|e| e.route.as_str().to_string()),
        credential: endpoint.map(|e| e.credential.to_string()),
    }
}

/// Say so when the capture is a prefix. §6.7's rule for caps is that shortening is always
/// recorded; a drain abandoned with the pipe still open shortens stdout and stderr the same way,
/// and the reader would otherwise see a truncated transcript as a complete one.
fn note_capture_truncated(contract: &mut TaskContract, capture_truncated: bool) {
    if capture_truncated && let Some(completion) = contract.completion.as_mut() {
        completion.exit.description = note_truncated_capture(&completion.exit.description);
    }
}

/// **The contract reaches disk before the stream says the node is over**, and since §11 item 28
/// step 5 that ordering is load-bearing rather than incidental.
///
/// The bridge's synchronous `spawn` is now a composition over this stream: `agent/spawn`,
/// `node/attach`, read until the closing bookend, then read `contracts/<task_id>.json`. With the
/// bookend written first there is a window — one `create` plus one `write` wide — in which a
/// reader that did exactly what the stream told it to would open a file that does not exist yet
/// and report a finished child as one that produced nothing. That is a race no reader can close
/// from its own side: it cannot distinguish "not written yet" from "never written", which is the
/// absence-versus-silence distinction §4.1 exists to keep. Writing the file first makes the
/// bookend mean what a reader needs it to mean — *everything about this node is now on disk*.
///
/// **And the failing half must close the stream too**, or the invariant above holds only when
/// nothing goes wrong. Before this, a contract marion could not write still got an `Exited`
/// bookend, so the stream asserted a finished node whose contract was never there — the
/// false-success shape, arriving in the one place a reader trusts. The abort says what actually
/// happened; `AbortOnDrop` writes the matching `SpawnAborted` on the way out, so the journal and
/// the stream agree.
///
/// The closing bookend is read off the **same** `completion` the journal record is read from: two
/// derivations of one status are two chances to disagree about the same run. Without it a replayed
/// stream cannot tell a node that finished from one whose stream stopped mid-turn, which is the
/// single question §7.3.3's replay leg exists to answer about a node the client never saw.
fn persist_contract_and_close_stream(
    env: &Env,
    agent_dir: &AgentDir,
    agent_id: &AgentId,
    contract: &TaskContract,
    events: Option<&crate::events::EventSink>,
    requested_agent_type: &str,
    ended_by_kill: bool,
) -> Result<TaskContract, SpawnError> {
    let persisted = persist_contract_then_record_exit(
        &env.project_dir,
        agent_dir,
        agent_id,
        contract,
        ended_by_kill,
    );
    let returned = match persisted {
        Ok(returned) => returned,
        Err(e) => {
            if let Some(es) = events {
                es.lifecycle(marion_core::event::Lifecycle::Aborted {
                    reason: format!(
                        "the {requested_agent_type} node ran to a terminal state and marion could \
                         not persist its task contract: {e}"
                    ),
                });
            }
            return Err(e);
        }
    };
    if let (Some(completion), Some(es)) = (contract.completion.as_ref(), events) {
        es.lifecycle(marion_core::event::Lifecycle::Exited {
            status: completion.status,
            exit: completion.exit.clone(),
        });
    }
    Ok(returned)
}

/// The other half of [`crate::spawn::make_worktree`]'s serialization, and it needs the guard for
/// the same reason: `git worktree remove` takes the repository's own locks, so a sibling thread's
/// `worktree add` racing it fails with `index.lock: File exists`. The failure lands on the
/// *sibling's* spawn — a child refused because an unrelated child happened to be finishing — which
/// is exactly the kind of scheduling-dependent flake the guard exists to make impossible.
///
/// **The task branch goes too, but only while it is still at `base`.** A branch that never moved
/// holds no work, and leaving it made the task id single-use: `git worktree add -b` cannot recreate
/// it. `update-ref -d <ref> <base>` is git's compare-and-delete, so a branch that moved — marion's
/// commit of the child's work, or the child's own — fails the compare and survives. Both steps run
/// under the one guard so a sibling cannot recreate the branch between them, and a worktree that
/// could not be removed keeps its branch. Best-effort, as the removal always was.
fn cleanup(repo: &Path, wt: &Path, branch: &str, base: Option<&Oid>) {
    let _serialized = crate::spawn::repo_write_guard();
    let git = |args: &[&str]| {
        let mut command = SysCommand::new("git");
        command
            .current_dir(repo)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
            .spawn(&mut command)
            .and_then(std::process::Child::wait_with_output)
            .is_ok_and(|out| out.status.success())
    };
    let removed = git(&["worktree", "remove", "--force", &wt.to_string_lossy()]);
    if let (true, Some(task_ref), Some(base)) = (removed, task_branch_ref(branch), base) {
        git(&["update-ref", "-d", &task_ref, &base.0]);
    }
}

/// **A worktree this spawn created and no process has run in yet**: [`cleanup`] on drop, so a
/// refusal anywhere between [`select_workspace`] and the launch takes back the directory, its
/// `.git/worktrees/` registration and the unchanged task branch rather than stranding them in the
/// operator's repository. Minted only by the arm that calls `make_worktree`: a resumed worktree or
/// a `shared-cwd` directory is not this spawn's to remove, and gets [`Self::none`].
struct PrelaunchWorktree {
    repo: PathBuf,
    target: Option<(PathBuf, String, Oid)>,
}

impl PrelaunchWorktree {
    fn none() -> Self {
        PrelaunchWorktree {
            repo: PathBuf::new(),
            target: None,
        }
    }

    /// At the first-execution seam: from here the worktree may hold child work.
    fn disarm(&mut self) {
        self.target = None;
    }
}

impl Drop for PrelaunchWorktree {
    fn drop(&mut self) {
        if let Some((path, branch, base)) = self.target.take() {
            cleanup(&self.repo, &path, &branch, Some(&base));
        }
    }
}

/// The ref of a branch marion made for a task — exactly one segment under `marion/`, as
/// [`select_workspace`] names it — or `None`, so cleanup can never be pointed at another branch.
fn task_branch_ref(branch: &str) -> Option<String> {
    let task = branch.strip_prefix("marion/")?;
    let one_segment = !task.is_empty() && !task.contains('/') && !task.contains("..");
    one_segment.then(|| format!("refs/heads/{branch}"))
}

/// A fake harness for the `--version` probe tests: a shell script at `<dir>/<name>` that, run with
/// `--version`, appends one record to [`version_fake::log`] — its argv, the value of each variable
/// in `watch`, and the contents of the file that value names if it is one — then prints `version`.
/// So a test can see what switch reached the process, including a document deleted since.
#[cfg(test)]
pub(crate) mod version_fake {
    use std::path::{Path, PathBuf};

    pub(crate) fn write(dir: &Path, name: &str, version: &str, watch: &[&str]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let log = log(dir);
        let mut script = format!(
            "#!/bin/sh\ncase \" $* \" in *\" --version \"*) ;; *) exit 64 ;; esac\n\
             printf 'probe\\n' >> '{log}'\n\
             for a in \"$@\"; do printf 'arg:%s\\n' \"$a\" >> '{log}'; done\n",
            log = log.display()
        );
        for var in watch {
            script.push_str(&format!(
                "printf 'var:{var}=%s\\n' \"${{{var}:-}}\" >> '{log}'\n\
                 [ -f \"${{{var}:-}}\" ] && {{ printf 'doc:' >> '{log}'; cat \"${var}\" >> '{log}'; \
                 printf '\\n' >> '{log}'; }}\n",
                log = log.display()
            ));
        }
        script.push_str(&format!("echo '{version}'\n"));
        let path = dir.join(name);
        std::fs::write(&path, script).expect("the fake harness is written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("the fake harness is executable");
        path
    }

    /// What a probe of `h`'s row must carry, read off the row's own data: the variables a
    /// [`version_fake`] should record, and a check of one probe's record.
    pub(crate) fn switch_evidence(h: marion_core::harness::Harness) -> (Vec<String>, Vec<String>) {
        use marion_harness::probe::{DocumentChannel, ProbeSwitch};
        match ProbeSwitch::for_row(marion_harness::adapter::harness_spec(h)).unwrap() {
            ProbeSwitch::Env { key, value } => {
                (vec![key.clone()], vec![format!("var:{key}={value}")])
            }
            ProbeSwitch::Args(args) => {
                let mut want = vec!["arg:--version".to_string()];
                want.extend(args.iter().map(|a| format!("arg:{a}")));
                (vec![], want)
            }
            ProbeSwitch::Document {
                via: DocumentChannel::Env(key),
                body,
            } => (vec![key.to_string()], vec![format!("doc:{body}")]),
            other => panic!("{h}: no switch to look for: {other:?}"),
        }
    }

    pub(crate) fn log(dir: &Path) -> PathBuf {
        dir.join("probes.log")
    }

    /// Each probe's record, in order.
    pub(crate) fn probes(dir: &Path) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(log(dir)).unwrap_or_default();
        let mut out: Vec<Vec<String>> = Vec::new();
        for line in text.lines() {
            if line == "probe" {
                out.push(Vec::new());
            } else if let Some(last) = out.last_mut() {
                last.push(line.to_string());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    const SIGKILL: i32 = 9;
    use crate::spawn::ChildOutcome;
    use marion_core::agent_type::builtin;
    use marion_core::harness::Harness;
    use marion_harness::adapter_for;
    use marion_testsupport::{Scratch, scratch};
    use std::sync::Mutex;

    /// **A reviewer's contract names its row's read-only switch and reads its findings**; on a row
    /// that cannot refuse a write (scope-only, or an unverified tools axis) it also says so in
    /// plain words, derived from the row's strategy rather than the harness's name.
    #[test]
    fn a_reviewer_contract_says_when_its_harness_cannot_be_made_read_only() {
        use marion_harness::spec::ReadOnly;
        let contract = || {
            crate::spawn::build_contract(
                TaskId("t".into()),
                AgentId("child".into()),
                marion_core::Harness::Codex,
                RepoIdentity {
                    git_common_dir: None,
                    head_branch: None,
                },
                None,
                Workspace::SharedCwd { path: "/r".into() },
                "review",
                &[],
                &[Glob("**".into())],
                &[],
                Duration::from_secs(60),
                SystemTime(std::time::SystemTime::now()),
                &ChildOutcome {
                    narrative: Some(
                        r#"{"verdict":"allow","findings":[{"severity":"low","file":"a.rs","claim":"x"}]}"#
                            .into(),
                    ),
                    exit_code: Some(0),
                    ..ChildOutcome::default()
                },
                Some(vec![]),
                None,
                vec![],
                vec![],
            )
        };
        let target = crate::review::Target {
            agent_id: AgentId("child".into()),
            commit: None,
            changed_paths: vec!["a.rs".into()],
        };
        for (ro, warned) in [
            (
                ReadOnly::ToolsAxis {
                    verified: true,
                    note: "m",
                },
                false,
            ),
            (
                ReadOnly::Pair {
                    key: "k",
                    value: "v",
                    note: "m",
                },
                false,
            ),
            (
                ReadOnly::EnvVar {
                    key: "k",
                    value: "v",
                    note: "m",
                },
                false,
            ),
            (
                ReadOnly::ToolsAxis {
                    verified: false,
                    note: "m",
                },
                true,
            ),
            (ReadOnly::ScopeOnly { note: "m" }, true),
        ] {
            let mut c = contract();
            let tally = record_review(&mut c, &target, ro).expect("a readable report is tallied");
            assert_eq!(tally.findings, 1, "{ro:?}");
            assert!(
                c.allowed_tools
                    .contains(&format!("read-only:{}", ro.kind()))
            );
            let comp = c.completion.unwrap();
            assert_eq!(comp.findings.map(|f| f.findings.len()), Some(1));
            assert_eq!(
                comp.exit.description.contains(crate::review::UNGUARDED),
                warned,
                "{ro:?}: {}",
                comp.exit.description
            );
        }
    }

    /// **The cap is on the lines as the journal encodes them, at its exact boundary.** `["…"]`
    /// costs four bytes of framing around one line, so a line of `cap - 4` bytes is exactly the
    /// cap and is served, and one byte more is refused naming both numbers.
    #[test]
    fn verification_is_capped_at_its_encoded_size_on_the_boundary() {
        let line = |n: usize| vec!["a".repeat(n)];
        assert!(check_verification_size(&[]).is_ok(), "no lines, no cost");
        assert!(check_verification_size(&line(MAX_VERIFICATION_BYTES - 4)).is_ok());
        match check_verification_size(&line(MAX_VERIFICATION_BYTES - 3)) {
            Err(SpawnError::VerificationTooLarge { bytes, cap }) => {
                assert_eq!(
                    (bytes, cap),
                    (MAX_VERIFICATION_BYTES + 1, MAX_VERIFICATION_BYTES)
                );
            }
            other => panic!("one byte over is refused by name, got {other:?}"),
        }
    }

    /// **Escaping is counted, because the record carries the escaped bytes.** A NUL encodes as
    /// six (`\u0000`), so 2000 of them are well under the cap raw and far over it on disk — and a
    /// raw-length guard would let through an intent the journal then drops whole.
    #[test]
    fn verification_escaping_and_many_lines_count_against_the_cap() {
        match check_verification_size(&["\0".repeat(2000)]) {
            Err(SpawnError::VerificationTooLarge { bytes, .. }) => {
                assert_eq!(bytes, 2000 * 6 + 4)
            }
            other => panic!("the encoded size is what is capped, got {other:?}"),
        }
        let many = vec!["x".to_string(); MAX_VERIFICATION_BYTES / 4];
        assert!(
            matches!(
                check_verification_size(&many),
                Err(SpawnError::VerificationTooLarge { .. })
            ),
            "the cap is on the total, not per line"
        );
    }

    /// **Whose work a new worktree carries is decided by where the caller's tree is.** A caller in
    /// a worktree marion made under this project hands its child its uncommitted work; a caller in
    /// the operator's own checkout does not, and that checkout is left exactly as it was.
    #[test]
    fn only_a_caller_in_a_marion_worktree_hands_its_child_its_uncommitted_work() {
        let dir = scratch("select-workspace-carry");
        let repo = fixture_repo(&dir);
        let project = ProjectDir::new(&dir.join("state"), &repo);
        let agent_type = builtin("codex").unwrap();
        let request = |from: &Path| SpawnRequest {
            review: None,
            agent_type: "codex".into(),
            prompt: "test it".into(),
            repo: from.to_path_buf(),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec![],
            timeout_secs: 1,
            model: None,
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
        };
        let caller_wt = project.agent(&AgentId("caller".into())).worktree();
        std::fs::create_dir_all(caller_wt.parent().unwrap()).unwrap();
        crate::spawn::make_worktree(&repo, &caller_wt, "marion/caller", false).unwrap();
        std::fs::write(caller_wt.join("src/keep.txt"), "the caller's edit\n").unwrap();
        std::fs::write(repo.join("src/keep.txt"), "the operator's edit\n").unwrap();

        let child = AgentId("child".into());
        // Held: a `PrelaunchWorktree` dropped here would take the new tree back at once.
        let (ws, _, _, _child_tree) = select_workspace(
            &request(&caller_wt),
            &agent_type,
            &project,
            &TaskId("t-child".into()),
            &child,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("src/keep.txt")).unwrap(),
            "the caller's edit\n"
        );

        let sibling = AgentId("sibling".into());
        let (ws, _, _, _sibling_tree) = select_workspace(
            &request(&repo),
            &agent_type,
            &project,
            &TaskId("t-sibling".into()),
            &sibling,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("src/keep.txt")).unwrap(),
            "keep\n",
            "a child of the operator's checkout starts from its HEAD"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("src/keep.txt")).unwrap(),
            "the operator's edit\n"
        );
        let log = SysCommand::new("git")
            .current_dir(&repo)
            .args(["rev-list", "--count", "main"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "1",
            "and no commit was made on the operator's branch"
        );
    }

    /// **A resume takes the tree its session was created in, and cuts none.**
    ///
    /// The two arms of [`select_workspace`] are wrong for a second life in opposite ways:
    /// `Worktree` would run `make_worktree` on a tree that already exists (which fails) or produce
    /// a *fresh* tree the resumed session has never seen, and `SharedCwd` would read `req.repo`
    /// rather than where the node actually ran. So the recorded workspace short-circuits both.
    ///
    /// The oracle is that `req` still says `Worktree` and `req.repo` is **not a repository at
    /// all**: without the resume this call is `NotAGitRepo`, so returning the recorded tree proves
    /// the recorded value was read and neither arm ran.
    #[test]
    fn a_resume_takes_the_recorded_workspace_instead_of_cutting_a_second_one() {
        let dir = scratch("select-workspace-resume");
        let repo = dir.join("not-a-repo");
        std::fs::create_dir_all(&repo).unwrap();
        let first_life = dir.join("first-life-worktree");
        std::fs::create_dir_all(&first_life).unwrap();
        let recorded = Workspace::Worktree {
            path: first_life.clone(),
            branch: "marion/t-1".into(),
        };
        let agent_type = builtin("claude").unwrap();
        let agent_id = AgentId("child".into());
        let project = ProjectDir::new(&dir.join("state"), &repo);
        let task_id = TaskId("t-1".into());
        let mut req = SpawnRequest {
            review: None,
            agent_type: "claude".into(),
            prompt: "carry on".into(),
            repo: repo.clone(),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec![],
            timeout_secs: 1,
            model: None,
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
        };
        assert!(
            matches!(
                select_workspace(&req, &agent_type, &project, &task_id, &agent_id),
                Err(SpawnError::NotAGitRepo { .. })
            ),
            "the fixture's repo is deliberately not a repository, so a fresh spawn cannot cut a \
             worktree in it — which is what makes the resume below unambiguous"
        );

        req.resume = Some(ChildResume {
            agent_id: agent_id.clone(),
            session: "sess-1".into(),
            workspace: recorded.clone(),
            usage: None,
        });
        let (workspace, _base, _claim, _prelaunch) =
            select_workspace(&req, &agent_type, &project, &task_id, &agent_id)
                .expect("the recorded tree needs no repository question asked of it");
        assert_eq!(
            workspace, recorded,
            "the relaunch runs where the journal says the session was created"
        );
        assert!(
            !project.agent(&agent_id).worktree().exists(),
            "and no second tree was cut under the agent dir"
        );
    }

    /// **The journal's terminal record is never durable before the contract it is about.**
    ///
    /// `0827fc8` made the *stream's* closing bookend mean *"everything about this node is on
    /// disk"* by writing the contract before it, and said in as many words that the ordering is
    /// load-bearing. The **journal's** terminal record — `RecordKind::Exited`, which is what
    /// `Replay` folds into `NodeState::Exited`, and so what every reader of the journal takes to
    /// mean the node is over — was still written first, leaving exactly the same window one
    /// `write(2)` wide on the other surface. It was not hypothetical: `depth_gate.rs` measured it
    /// on gemini as a run with zero contracts where one was about to exist, and worked around it by
    /// polling for both conditions instead of asserting the rule.
    ///
    /// **Observed from inside the window, not sampled from outside it.** The difference between the
    /// right order and the wrong one is purely temporal — both orders end with the same records and
    /// the same file — so there is no after-the-fact reading that can tell them apart. A poll from
    /// another thread would have to catch one `create` plus one `write`, and would pass against the
    /// broken order almost every time; a test that usually passes against the defect is worse than
    /// none. So the seam offers the one instant that matters ([`at_contract_write`]) and this reads
    /// the journal from within it.
    #[test]
    fn the_terminal_record_is_not_journaled_until_the_contract_is_on_disk() {
        use marion_core::contract::*;
        use marion_core::encoding::{Duration as EncDuration, SystemTime as EncSystemTime};

        let dir = scratch("run-exit-order");
        let project = marion_core::paths::ProjectDir::new(&dir, std::path::Path::new("/repo/.git"));
        std::fs::create_dir_all(project.path()).expect("the project dir");
        let agent_id = AgentId("019fbf94-0000-7000-8000-0000000000aa".into());
        let agent_dir = project.agent(&agent_id);

        let contract = crate::spawn::build_contract(
            TaskId("019fbf94-53c8-7c60-9f4c-12695a5e79fe".into()),
            AgentId("019fbf94-0000-7000-8000-000000000001".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/repo/.git".into()),
                head_branch: Some("main".into()),
            },
            Some(Oid("a".repeat(40))),
            Workspace::Worktree {
                path: "/tmp/wt".into(),
                branch: "marion/t1".into(),
            },
            "add a flag",
            &["tests pass".to_string()],
            &[Glob("**".into())],
            &[Glob("src/**".into())],
            EncDuration::from_secs(900),
            EncSystemTime::from_unix_millis(1_785_625_628_619),
            &ChildOutcome {
                narrative: Some("done".into()),
                exit_code: Some(0),
                ..Default::default()
            },
            Some(vec![]),
            None,
            vec![],
            vec![],
        );
        assert!(
            contract.completion.is_some(),
            "the premise: only a contract with a completion produces an `Exited` record at all, so \
             a fixture without one would make every assertion below vacuous"
        );

        // Read from inside the window. `terminal` is what the journal said at the instant the
        // contract file was about to be created.
        let terminal_at_write = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let journal_path = project.journal();
        let seen = std::sync::Arc::clone(&terminal_at_write);
        let watched = agent_id.clone();
        let _hook = at_contract_write::install(move || {
            let bytes = std::fs::read(&journal_path).unwrap_or_default();
            let exited = marion_core::registry::replay(&bytes)
                .nodes()
                .iter()
                .any(|n| n.agent_id == watched && n.state.is_exited());
            seen.store(exited, std::sync::atomic::Ordering::SeqCst);
        });

        persist_contract_then_record_exit(&project, &agent_dir, &agent_id, &contract, false)
            .expect("the contract is written to a directory this test owns");

        assert!(
            !terminal_at_write.load(std::sync::atomic::Ordering::SeqCst),
            "**the rule.** At the instant the contract was about to be created the journal already \
             said this node had exited, so a reader acting on the terminal record — which is what a \
             terminal record is for — would have found no contract, and could not tell `not written \
             yet` from `never written` (§4.1)"
        );

        // And both halves really happened, so the assertion above is not satisfied by a run that
        // did nothing.
        assert!(
            agent_dir.contract(&contract.task_id).is_file(),
            "the contract reached disk"
        );
        let bytes = std::fs::read(project.journal()).expect("the journal was written");
        assert!(
            marion_core::registry::replay(&bytes)
                .nodes()
                .iter()
                .any(|n| n.agent_id == agent_id && n.state.is_exited()),
            "and the terminal record followed it"
        );
    }

    /// **`run_bounded` survives a duration no clock can hold, having already started a process.**
    ///
    /// The order is what makes this a leak rather than an error: `Command::spawn` runs first, the
    /// deadline is computed second, and `Instant + Duration` panics on overflow. Dropping a
    /// `std::process::Child` kills nothing, so the unwind abandoned a live process — and on the
    /// bridge's own thread it took the whole MCP server down with it.
    ///
    /// Asserted here rather than only through `run_spawn` because the clamp
    /// ([`effective_timeout`]) lives in `run_spawn`, and a `pub` function whose liveness depends on
    /// a guard one of its callers happens to apply is a defect waiting for the second caller.
    /// `run_bounded` already has one that is not `run_spawn`: the end-to-end test bounds a real
    /// `marion run` with it.
    #[test]
    fn a_duration_the_clock_cannot_represent_bounds_the_run_rather_than_unwinding_it() {
        let out = run_bounded(SysCommand::new("true").arg("--"), StdDuration::MAX)
            .expect("an unrepresentable bound is still a bound, not a panic");
        assert!(
            !out.timed_out,
            "the fallback deadline is the cap, not `now` — saturating to now would kill a healthy \
             child instantly, which is the most aggressive possible reading of `too long`"
        );
        assert_eq!(
            out.code,
            Some(0),
            "and the process really ran, and was really reaped"
        );
    }

    /// **Every spawn-time probe carries the row's no-self-update switch**, for each shape of
    /// switch: an Env row (claude), a Pair row (codex, on its own `-c`), a Document row (gemini, a
    /// settings file named by its own variable). `harness_version` is the probe `run_spawn`,
    /// `root::launch_inner` and the native launch all take.
    #[test]
    fn the_spawn_probe_carries_every_shape_of_switch() {
        for h in [Harness::ClaudeCode, Harness::Codex, Harness::Gemini] {
            let dir = scratch(&format!("vprobe-switch-{h}"));
            let (watch, want) = version_fake::switch_evidence(h);
            let watch: Vec<&str> = watch.iter().map(String::as_str).collect();
            let fake = version_fake::write(&dir, "harness", "9.9.9 (fake)", &watch);
            assert_eq!(
                harness_version(fake.to_str().unwrap(), h),
                "9.9.9 (fake)",
                "{h}"
            );
            let probes = version_fake::probes(&dir);
            assert_eq!(probes.len(), 1, "{h}: {probes:?}");
            for line in &want {
                assert!(
                    probes[0].contains(line),
                    "{h}: no {line:?} in {:?}",
                    probes[0]
                );
            }
        }
    }

    /// A row whose binary may update itself and has no known switch is **not run** to read a
    /// version: the probe is refused and the version is `"unknown"`.
    #[test]
    fn a_row_with_no_known_switch_is_never_probed() {
        let dir = scratch("vprobe-refused");
        let fake = version_fake::write(&dir, "agent", "1.0.0", &[]);
        let refused: Vec<Harness> = Harness::ALL
            .into_iter()
            .filter(|h| {
                matches!(
                    marion_harness::adapter::harness_spec(*h).updates,
                    marion_harness::spec::UpdatePolicy::None { .. }
                )
            })
            .collect();
        assert!(!refused.is_empty(), "the sweep needs a row to refuse");
        for h in refused {
            assert_eq!(harness_version(fake.to_str().unwrap(), h), "unknown", "{h}");
        }
        assert!(
            version_fake::probes(&dir).is_empty(),
            "the binary never ran"
        );
    }

    /// **One probe per binary, until the binary changes.** Two spawns of the same unchanged file
    /// fork `--version` once; touching it (mtime) or rewriting it (inode, ctime) probes again.
    #[test]
    fn the_version_is_read_once_per_binary_and_again_when_it_changes() {
        let dir = scratch("vprobe-cache");
        let fake = version_fake::write(&dir, "claude", "1.0.0", &[]);
        let program = fake.to_str().unwrap();
        let h = Harness::ClaudeCode;
        assert_eq!(harness_version(program, h), "1.0.0");
        assert_eq!(harness_version(program, h), "1.0.0");
        assert_eq!(
            version_fake::probes(&dir).len(),
            1,
            "the second spawn used the cache"
        );

        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&fake)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(harness_version(program, h), "1.0.0");
        assert_eq!(
            version_fake::probes(&dir).len(),
            2,
            "a touched binary is read again"
        );

        let tmp = version_fake::write(&dir, "claude.new", "2.0.0", &[]);
        std::fs::rename(&tmp, &fake).unwrap();
        assert_eq!(
            harness_version(program, h),
            "2.0.0",
            "a replaced binary's new version"
        );
        assert_eq!(harness_version(program, h), "2.0.0");
        assert_eq!(
            version_fake::probes(&dir).len(),
            3,
            "read once after the replacement"
        );

        let other = Harness::Codex;
        assert_eq!(harness_version(program, other), "2.0.0");
        assert_eq!(
            version_fake::probes(&dir).len(),
            4,
            "the same file probed as another harness runs that row's probe"
        );
    }

    /// A probe that fails is not cached: the next spawn asks again.
    #[test]
    fn a_failed_probe_is_asked_again() {
        let dir = scratch("vprobe-fail");
        let fake = dir.join("claude");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho probe >> \"$(dirname \"$0\")/probes.log\"\nexit 3\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = fake.to_str().unwrap();
        assert_eq!(harness_version(program, Harness::ClaudeCode), "unknown");
        assert_eq!(harness_version(program, Harness::ClaudeCode), "unknown");
        assert_eq!(version_fake::probes(&dir).len(), 2);
    }

    #[test]
    fn a_harness_version_is_its_first_non_blank_line() {
        assert_eq!(
            version_line(
                "GitHub Copilot CLI 1.0.83.\nRun 'copilot update' to check for updates.\n"
            )
            .as_deref(),
            Some("GitHub Copilot CLI 1.0.83.")
        );
        assert_eq!(
            version_line("2.1.223 (Claude Code)\n").as_deref(),
            Some("2.1.223 (Claude Code)")
        );
        assert_eq!(version_line("\n  0.147.0  \n").as_deref(), Some("0.147.0"));
        assert_eq!(version_line(""), None, "nothing printed is not a version");
        assert_eq!(version_line("\n\n"), None);
    }

    /// **A signalled exit alone does not skip verification.** Every ACP child ends on marion's own
    /// SIGINT (`acp_child::finish`), so `signal: Some(2), timed_out: false` is the *normal* end of
    /// a finished ACP turn; the commands must run. Only marion's own cut — `timed_out` — skips.
    #[test]
    fn verification_runs_for_a_signalled_exit_and_skips_only_marions_own_timeout() {
        let dir = scratch("supervisor-verify-signalled");
        let cmds = verification_commands(&["echo ok".into()], &dir);
        let shut_down_by_marion = ChildOutcome {
            signal: Some(2),
            timed_out: false,
            ..ChildOutcome::default()
        };
        let evidence = verification_evidence(&shut_down_by_marion, &cmds);
        assert_eq!(
            evidence.len(),
            1,
            "a signalled, un-timed-out child is verified"
        );
        assert_eq!(evidence[0].exit_code, Some(0));
        assert_eq!(evidence[0].stdout.value, "ok\n");

        let cut_short = ChildOutcome {
            signal: Some(9),
            timed_out: true,
            ..ChildOutcome::default()
        };
        assert!(
            verification_evidence(&cut_short, &cmds).is_empty(),
            "marion's own timeout kill is the one exit that skips"
        );
    }

    #[test]
    fn an_overrunning_process_group_is_killed_and_reported_as_timed_out() {
        let dir = scratch("supervisor-timeout");
        let marker = dir.join("survived");
        let script = format!("(sleep 1; touch '{}') & sleep 10", marker.display());
        let out = run_bounded(
            SysCommand::new("sh").args(["-c", &script]),
            StdDuration::from_millis(50),
        )
        .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.signal, Some(9));
        let contract = build_contract(
            TaskId("task".into()),
            AgentId("root".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
            Workspace::Worktree {
                path: "/wt".into(),
                branch: "b".into(),
            },
            "",
            &[],
            &[Glob("**".into())],
            &[Glob("**".into())],
            Duration::from_secs(1),
            SystemTime(std::time::SystemTime::now()),
            &ChildOutcome {
                timed_out: out.timed_out,
                signal: out.signal,
                ..ChildOutcome::default()
            },
            Some(vec![]),
            None,
            vec![],
            vec![],
        );
        assert_eq!(contract.completion.unwrap().status, ExitStatus::TimedOut);
        thread::sleep(StdDuration::from_millis(1100));
        assert!(
            !marker.exists(),
            "a descendant survived the killed process group"
        );
    }

    /// **A child that leaves its run early still records what it spent** — the spend the s2 claude
    /// child lost at its timeout: an early return drops the guard, the guard writes the stream's
    /// figure to the journal, and the contract path's own record is the only one when it runs.
    #[test]
    fn a_run_that_leaves_early_still_journals_what_its_stream_said_it_spent() {
        let dir = scratch("spend-on-every-end");
        let project = marion_core::paths::ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        std::fs::create_dir_all(project.path()).unwrap();
        let id = AgentId("n-spend".into());
        let sink = crate::events::EventSink::new(
            crate::events::EventWriter::open_path(&dir.join("events.jsonl"), &id).unwrap(),
            Harness::ClaudeCode,
            "unused".into(),
        );
        // A turn in flight: one request's counters and no `result`, as a killed child leaves it.
        sink.record_line(
            r#"{"type":"assistant","message":{"id":"msg_1","usage":{"input_tokens":12,"output_tokens":3}}}"#,
        );
        let usage_records = || {
            std::fs::read_to_string(project.journal())
                .unwrap_or_default()
                .lines()
                .filter(|l| l.contains("UsageRecorded"))
                .count()
        };
        let leave_early = || -> Result<(), SpawnError> {
            let _spend = SpendOnEveryEnd {
                project: &project,
                agent_id: &id,
                events: Some(&sink),
                recorded: false,
            };
            Err(SpawnError::NodeAborted("the driver failed".into()))
        };
        assert!(leave_early().is_err());
        assert_eq!(usage_records(), 1, "the early return journaled the spend");

        let mut spend = SpendOnEveryEnd {
            project: &project,
            agent_id: &id,
            events: Some(&sink),
            recorded: false,
        };
        assert_eq!(spend.record().map(|u| (u.input, u.output)), Some((12, 3)));
        drop(spend);
        assert_eq!(
            usage_records(),
            2,
            "recorded once on the contract path, not again on drop"
        );
    }

    /// A `LaunchOnly` child that writes `frame` every 100 ms and never ends on its own, run under a
    /// 30 s bound through the codex row's reader — its frames app-server's `error` notification
    /// (the conformance P-errors capture), whose error rules are what these cells exercise.
    fn retrying_child(tag: &str, frame: &str) -> (ChildRun, StdDuration) {
        let dir = scratch(tag);
        let project = marion_core::paths::ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        std::fs::create_dir_all(project.path()).unwrap();
        let id = AgentId(format!("n-{tag}"));
        let inv = Invocation {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!("while :; do echo '{frame}'; sleep 0.1; done"),
            ],
            env: vec![],
            cwd: dir.to_path_buf(),
            model: None,
            session_mode: None,
            env_remove: vec![],
        };
        let watch = crate::session_watch::SessionWatch::new(&project, &id, Harness::Codex, false);
        let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
        let at = Instant::now();
        let run = launch_only_child(
            &inv,
            &dir,
            StdDuration::from_secs(30),
            &|_| {},
            &watch,
            None,
            adapter.as_ref(),
        )
        .unwrap();
        (run, at.elapsed())
    }

    /// **A refused credential ends the run at once, whatever the harness's retry schedule** — no
    /// retry heals a 401, so waiting the harness out spends the node's clock for nothing. The run
    /// ends failed in the harness's own words, not timed out, and its cause is still auth.
    #[test]
    fn an_auth_refusal_the_harness_retries_ends_the_run_at_once() {
        let (run, took) = retrying_child(
            "run-auth-stop",
            r#"{"method":"error","params":{"error":{"message":"Reconnecting... 1/5","additionalDetails":"unexpected status 401 Unauthorized: Incorrect API key provided"},"willRetry":true}}"#,
        );
        assert!(took < StdDuration::from_secs(10), "took {took:?}");
        assert!(!run.exit.timed_out);
        let why = run.stopped.as_deref().expect("marion ended it");
        assert!(why.contains("401"), "{why}");
        let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
        assert!(matches!(
            attempt_cause(adapter.as_ref(), &run, None, None),
            Some(marion_core::contract::FailureCause::Auth { .. })
        ));
    }

    /// **A rate limit the harness retries is left to its backoff** — it can recover inside it, as
    /// an outage can — and a run the bound then ends still records the cause the retries said.
    #[test]
    fn a_retried_rate_limit_is_waited_out_and_its_cause_survives_the_timeout() {
        let dir = scratch("run-limit-bound");
        let project = marion_core::paths::ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        std::fs::create_dir_all(project.path()).unwrap();
        let id = AgentId("n-limit".into());
        let inv = Invocation {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                r#"while :; do echo '{"method":"error","params":{"error":{"message":"Reconnecting... 1/5","additionalDetails":"exceeded retry limit, last status: 429 Too Many Requests"},"willRetry":true}}'; sleep 0.1; done"#.into(),
            ],
            env: vec![],
            cwd: dir.to_path_buf(),
            model: None,
            session_mode: None,
            env_remove: vec![],
        };
        let watch = crate::session_watch::SessionWatch::new(&project, &id, Harness::Codex, false);
        let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
        let run = launch_only_child(
            &inv,
            &dir,
            StdDuration::from_secs(2),
            &|_| {},
            &watch,
            None,
            adapter.as_ref(),
        )
        .unwrap();
        assert!(run.exit.timed_out, "left to the harness until the bound");
        assert_eq!(run.stopped, None);
        // On the operator's own login a 429 is the account's window: the cause the contract
        // carries, a notice, and never a failover (`next_attempt`).
        assert!(matches!(
            attempt_cause(adapter.as_ref(), &run, None, None),
            Some(marion_core::contract::FailureCause::UsageLimit { .. })
        ));
    }

    /// **On the operator's own login only a refused credential fails over; a limit never switches
    /// profiles.** Two profiles are listed; a run whose retries said 429 is the account's window —
    /// its cause is recorded and nothing relaunches — while one that said 401 moves to the next
    /// profile. The rule the coordinator pinned: a usage limit is a notice, never an account swap.
    #[test]
    fn a_limit_never_switches_profiles_and_a_refused_login_does() {
        let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
        let profile = |name: &str| crate::profiles::Profile {
            name: name.into(),
            harness: Harness::Codex,
            dir: format!("/profiles/{name}"),
        };
        let launch = crate::profiles::Launch {
            chain: vec![profile("work"), profile("personal")],
            paths: None,
        };
        let failed = |message: &str| ChildRun {
            stdout: format!(
                r#"{{"method":"error","params":{{"error":{{"message":"{message}"}}}}}}"#
            ),
            stderr: String::new(),
            exit: ChildExit {
                code: Some(1),
                signal: None,
                timed_out: false,
            },
            capture_truncated: false,
            denied_permissions: vec![],
            stopped: None,
        };
        let wt = scratch("run-profile-policy");
        let next = |run: &ChildRun| {
            next_attempt(
                None,
                &launch,
                0,
                run,
                adapter.as_ref(),
                &wt,
                None,
                StdDuration::from_secs(60),
            )
        };
        assert!(
            next(&failed(
                "exceeded retry limit, last status: 429 Too Many Requests"
            ))
            .is_none(),
            "a limit relaunches nothing"
        );
        assert!(matches!(
            attempt_cause(
                adapter.as_ref(),
                &failed("exceeded retry limit, last status: 429 Too Many Requests"),
                None,
                None
            ),
            Some(marion_core::contract::FailureCause::UsageLimit { .. })
        ));
        assert!(matches!(
            next(&failed(
                "unexpected status 401 Unauthorized: Incorrect API key provided"
            )),
            Some((
                Next::Profile(1),
                marion_core::contract::FailureCause::Auth { .. }
            ))
        ));
    }

    /// **An endpoint child's key never reaches its live event record.** `launch_only_child`
    /// records each line as it lands, before the capture is redacted, so the node's sink has to
    /// scrub the line; a harness echoing its key in an error is the case this defends.
    #[test]
    fn a_launch_only_childs_live_record_carries_no_endpoint_key() {
        let dir = scratch("run-live-redact");
        let project = marion_core::paths::ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        std::fs::create_dir_all(project.path()).unwrap();
        let id = AgentId("n-redact".into());
        let events_path = dir.join("events.jsonl");
        let sink = crate::events::EventSink::new(
            crate::events::EventWriter::open_path(&events_path, &id).unwrap(),
            Harness::Codex,
            "unused".into(),
        )
        .scrubbing(Some("sk-endpoint-9f2c1e7a"));
        let key = "sk-endpoint-9f2c1e7a";
        let inv = Invocation {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                format!(r#"echo '{{"type":"error","message":"bad key {key}"}}'"#),
            ],
            env: vec![],
            cwd: dir.to_path_buf(),
            model: None,
            session_mode: None,
            env_remove: vec![],
        };
        let watch = crate::session_watch::SessionWatch::new(&project, &id, Harness::Codex, false);
        let adapter = marion_harness::adapter::adapter_for(Harness::Codex).unwrap();
        let run = launch_only_child(
            &inv,
            &dir,
            StdDuration::from_secs(20),
            &|_| {},
            &watch,
            Some(&sink),
            adapter.as_ref(),
        )
        .unwrap();
        drop(sink);
        assert!(
            run.stdout.contains(key),
            "the capture is redacted by the caller, later"
        );
        let recorded = std::fs::read_to_string(&events_path).unwrap();
        assert!(recorded.contains("bad key ***"), "{recorded}");
        assert!(!recorded.contains(key), "{recorded}");
    }

    /// `kill(pid, 0)`: `ESRCH` is the only answer that means *gone*. `EPERM` means the process
    /// exists and is not ours, which for this fix would still be a survivor.
    fn alive(pid: i32) -> bool {
        if unsafe { kill(pid, 0) } == 0 {
            return true;
        }
        const ESRCH: i32 = 3;
        std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
    }

    #[test]
    fn a_tool_call_child_that_setsid_escaped_the_group_is_dead_after_the_bound_expires() {
        // Case B from tests/fixtures/s7/README.md — the only leaking case: the escaped process is
        // STILL RUNNING when the bound expires. `POSIX::setsid()` reproduces what `codex exec` does
        // to every tool-call command: a new session AND a new process group, so `killpg` on the
        // group marion created cannot reach it.
        assert!(
            SysCommand::new("perl")
                .args(["-e", "1"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
            "this test needs perl to build a setsid escapee"
        );
        let dir = scratch("supervisor-setsid-escape");
        let pids = dir.join("pids");
        let script = format!(
            r#"use POSIX ();
               my $pid = fork();
               if ($pid == 0) {{
                   POSIX::setsid();
                   exec("/bin/sh", "-c", "sleep 900 & echo \$! >> '{p}'; wait");
               }}
               open(my $f, ">>", "{p}"); print $f "$pid\n"; close $f;
               sleep 900;"#,
            p = pids.display()
        );
        let out = run_bounded(
            SysCommand::new("perl").args(["-e", &script]),
            StdDuration::from_millis(1000),
        )
        .unwrap();
        assert!(out.timed_out);

        let recorded: Vec<i32> = std::fs::read_to_string(&pids)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        // Give a killed process a moment to leave the table; then clean up unconditionally, so a
        // failing assertion below can never leave a `sleep 900` behind — the very bug under test.
        let mut survivors: Vec<i32> = recorded.clone();
        for _ in 0..40 {
            survivors.retain(|p| alive(*p));
            if survivors.is_empty() {
                break;
            }
            thread::sleep(StdDuration::from_millis(50));
        }
        for p in &recorded {
            let _ = unsafe { kill(*p, SIGKILL) };
        }
        assert_eq!(
            recorded.len(),
            2,
            "expected the setsid'd shell and its sleep to record their pids, got {recorded:?}"
        );
        assert!(
            survivors.is_empty(),
            "processes in a session marion never created survived the timeout kill: {survivors:?}"
        );
    }

    /// The liveness property, on the path the kill sweep does *not* fix.
    ///
    /// The escapee `setsid`s (a new session and a new process group, exactly what `codex exec` does
    /// to every tool-call command) and keeps the inherited stdout open. `kill_tree` is injected as
    /// a killer that reaches only the direct child — the standing-in-for-reality case where the
    /// sweep misses something, e.g. the known race of a child forking into a fresh group between
    /// the `ps` snapshot and the first signal. Before the drain bound this test did not fail, it
    /// **hung**: `join` on a drain thread never returns while an escapee holds the write end.
    ///
    /// The whole call runs on a worker thread behind a `recv_timeout`, so a regression fails loudly
    /// instead of wedging CI, and the escapee is killed unconditionally before any assertion.
    #[test]
    fn the_bound_returns_even_when_an_escapee_survives_the_kill_and_holds_the_pipe() {
        assert!(
            SysCommand::new("perl")
                .args(["-e", "1"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false),
            "this test needs perl to build a setsid escapee"
        );
        /// Kills only the child marion started, leaving its setsid'd descendant alive — an
        /// incomplete sweep, by construction.
        fn kill_only_the_direct_child(pid: i32) {
            let _ = unsafe { kill(pid, SIGKILL) };
        }

        let dir = scratch("supervisor-drain-bound");
        let pids = dir.join("pids");
        // The escapee holds stdout open for 900 s and writes nothing, so the pipe stays open long
        // past every bound in this test.
        let script = format!(
            r#"use POSIX ();
               $| = 1;
               my $pid = fork();
               if ($pid == 0) {{
                   POSIX::setsid();
                   open(my $f, ">>", "{p}"); print $f "$$\n"; close $f;
                   sleep 900;
                   exit 0;
               }}
               print "before-the-bound\n";
               sleep 900;"#,
            p = pids.display()
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let out = run_bounded_with(
                SysCommand::new("perl").args(["-e", &script]),
                StdDuration::from_millis(300),
                kill_only_the_direct_child,
                None,
                None,
            );
            let _ = tx.send(out);
        });
        // 300 ms bound + 2 s drain grace, with slack. Anything past this is the hang.
        let result = rx.recv_timeout(StdDuration::from_secs(10));

        // Unconditional cleanup first: no assertion below may leave a `sleep 900` behind.
        let escapees: Vec<i32> = std::fs::read_to_string(&pids)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        for p in &escapees {
            let _ = unsafe { kill(*p, SIGKILL) };
        }
        let _ = handle.join();

        let out = result
            .expect("run_bounded_with hung: a surviving escapee held the pipe open")
            .expect("run_bounded_with errored");
        assert!(out.timed_out, "the bound expired, so this is a timeout");
        assert!(
            out.capture_truncated,
            "the pipe was still open at the drain deadline, so the capture must be marked short"
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "before-the-bound\n",
            "output drained before the bound is still returned"
        );
        assert_eq!(escapees.len(), 1, "expected one recorded escapee pid");
    }

    /// The abandoned drain is a stopped thread, not a wedged one: `finish` returns, and the thread
    /// it was waiting on has exited by then. A supervisor calling `spawn` in a loop must not
    /// accumulate one live thread and one live fd per timed-out spawn.
    #[test]
    fn a_drain_abandoned_with_the_pipe_still_open_stops_its_thread_rather_than_leaking_it() {
        // `exec`, so the shell *becomes* the sleeper: one process holds the pipe, and killing it
        // below leaves nothing behind even if an assertion fails first.
        let mut child = SysCommand::new("sh")
            .args(["-c", "echo drained; exec sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let drain = Drain::start(child.stdout.take().expect("stdout was piped"));
        thread::sleep(StdDuration::from_millis(200));
        let stop = Arc::clone(&drain.stop);
        let started = Instant::now();
        // A deadline already in the past: abandon immediately.
        let (bytes, complete) = drain.finish(Instant::now());
        let elapsed = started.elapsed();
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            !complete,
            "the pipe was still open, so the capture is short"
        );
        assert_eq!(String::from_utf8_lossy(&bytes), "drained\n");
        assert!(
            stop.load(Ordering::Relaxed),
            "the thread was told to stop, not detached"
        );
        // `finish` joined the thread, so its return proves the thread exited and closed the fd.
        assert!(
            elapsed < StdDuration::from_secs(1),
            "abandoning a drain must be prompt, took {elapsed:?}"
        );
    }

    #[test]
    fn a_child_that_closes_its_pipes_is_never_reported_as_truncated() {
        let out = run_bounded(
            SysCommand::new("sh").args(["-c", "echo out; echo err 1>&2"]),
            StdDuration::from_secs(10),
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
        assert!(!out.timed_out);
        assert!(!out.capture_truncated);
    }

    /// A drain must return every byte, not just the first pipe-buffer's worth: the chunked reader
    /// has to loop. 512 KiB is far past the 64 KiB pipe capacity.
    #[test]
    fn output_larger_than_the_pipe_buffer_is_drained_whole() {
        let out = run_bounded(
            SysCommand::new("sh").args([
                "-c",
                "yes 0123456789012345678901234567890123456789 | head -n 12800",
            ]),
            StdDuration::from_secs(30),
        )
        .unwrap();
        assert_eq!(out.stdout.len(), 12800 * 41);
        assert!(!out.capture_truncated);
    }

    /// The live line seam sees every line the capture does — **while the child still runs**, on
    /// the caller's thread, and the final unterminated line at EOF — and the capture is still
    /// whole afterwards. Line one is printed and flushed before a sleep, so its arrival before the
    /// child exits is what proves the seam is live rather than a replay of the capture.
    #[test]
    fn stdout_lines_reach_the_hook_while_the_child_runs_and_the_capture_stays_whole() {
        let seen: std::cell::RefCell<Vec<(String, bool)>> = std::cell::RefCell::new(Vec::new());
        let marker = scratch("supervisor-live-lines").join("exited");
        let script = format!(
            "printf 'first\\n'; sleep 0.4; printf 'second\\nthird-no-newline'; touch {}",
            marker.display()
        );
        let on_line = |line: &str| {
            seen.borrow_mut().push((line.to_string(), marker.exists()));
            ControlFlow::Continue(())
        };
        let out = run_bounded_with(
            SysCommand::new("sh").args(["-c", &script]),
            StdDuration::from_secs(30),
            kill_process_tree,
            None,
            Some(&on_line),
        )
        .unwrap();
        let seen = seen.into_inner();
        assert_eq!(
            seen.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
            ["first", "second", "third-no-newline"],
            "every line, the last one without its newline"
        );
        assert!(
            !seen[0].1,
            "`first` was delivered before the child had exited: the seam is live"
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "first\nsecond\nthird-no-newline"
        );
        assert!(!out.capture_truncated);
    }

    #[test]
    fn persisted_contract_is_complete_while_only_the_return_copy_is_capped() {
        let root = scratch("supervisor-persist");
        let project = ProjectDir::from_hash(&root, "0123456789ab");
        let agent = project.agent(&AgentId("agent".into()));
        let large = "x".repeat(20 * 1024);
        let contract = build_contract(
            TaskId("task".into()),
            AgentId("root".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
            Workspace::Worktree {
                path: "/wt".into(),
                branch: "b".into(),
            },
            "do it",
            &[],
            &[Glob("**".into())],
            &[Glob("**".into())],
            Duration::from_secs(1),
            SystemTime(std::time::SystemTime::now()),
            &ChildOutcome {
                narrative: Some(large),
                ..ChildOutcome::default()
            },
            Some(vec![]),
            None,
            vec![],
            vec![],
        );
        let returned = persist_then_cap(&agent, &contract).unwrap();
        let persisted: TaskContract =
            serde_json::from_slice(&std::fs::read(agent.contract(&contract.task_id)).unwrap())
                .unwrap();
        let persisted_narrative = persisted.completion.unwrap().narrative.unwrap();
        let returned_narrative = returned.completion.unwrap().narrative.unwrap();
        assert!(!persisted_narrative.truncated);
        assert!(returned_narrative.truncated);
        assert_eq!(persisted_narrative.original_bytes, 20 * 1024);
    }

    /// **A contract is replaced, never rewritten in place.** A reader holding the previous
    /// document — or a crash between truncate and write — must see the old contract whole or the
    /// new one whole, never an empty or half-written file. Observable as: a handle opened on the
    /// old file still reads the old bytes after the new contract lands, and the new file is `0600`.
    ///
    /// Mutation: write through `File::create` and the old handle reads the new (or a truncated)
    /// document.
    #[test]
    fn a_rewritten_contract_replaces_the_old_file_rather_than_truncating_it() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("supervisor-persist-atomic");
        let project = ProjectDir::from_hash(&root, "0123456789ab");
        let agent = project.agent(&AgentId("agent".into()));
        let task = TaskId("task".into());
        std::fs::create_dir_all(agent.contracts_dir()).unwrap();
        std::fs::write(agent.contract(&task), b"the old contract\n").unwrap();
        let mut old = std::fs::File::open(agent.contract(&task)).unwrap();
        let contract = build_contract(
            task.clone(),
            AgentId("root".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: None,
                head_branch: None,
            },
            None,
            Workspace::Worktree {
                path: "/wt".into(),
                branch: "b".into(),
            },
            "do it",
            &[],
            &[Glob("**".into())],
            &[Glob("**".into())],
            Duration::from_secs(1),
            SystemTime(std::time::SystemTime::now()),
            &ChildOutcome::default(),
            Some(vec![]),
            None,
            vec![],
            vec![],
        );
        persist_then_cap(&agent, &contract).unwrap();
        let mut seen = String::new();
        old.read_to_string(&mut seen).unwrap();
        assert_eq!(seen, "the old contract\n");
        let path = agent.contract(&task);
        let _: TaskContract = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let strays: Vec<_> = std::fs::read_dir(agent.contracts_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != path.file_name().unwrap())
            .collect();
        assert!(strays.is_empty(), "no staging file survives: {strays:?}");
    }

    /// **A child whose `SpawnIntent` cannot be made durable is refused before its first side
    /// effect.** The intent is what lets a restarted supervisor name the node; launching without it
    /// is the untracked live process §9's M2 criteria forbid. The fault is real: the journal path
    /// is a directory, so the append fails. No owner is told of an identity, no agent directory or
    /// worktree exists, and no branch was made.
    ///
    /// Mutation: write the intent through `journal::record` and the spawn goes on to take a
    /// worktree (and `identified` fires).
    #[test]
    fn a_child_whose_intent_cannot_be_journalled_is_refused_before_any_side_effect() {
        struct Counter(Mutex<Vec<AgentId>>);
        impl SpawnObserver for Counter {
            fn identified(&self, agent_id: &AgentId) -> Option<Secret> {
                self.0.lock().unwrap().push(agent_id.clone());
                None
            }
            fn started(&self, _: &AgentId, _: i32) {}
        }
        let root = scratch("supervisor-intent-barrier");
        let repo = fixture_repo(&root);
        let state = root.join("state");
        let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
        std::fs::create_dir_all(project.journal()).unwrap();
        let env = Env {
            project_dir: project.clone(),
            state: state.clone(),
            project_root: repo.clone(),
            bridge: PathBuf::from("/bin/marion-supervisor"),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
        };
        let req = SpawnRequest {
            review: None,
            agent_type: "codex".into(),
            prompt: "do the task".into(),
            repo: repo.clone(),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec!["src/**".into()],
            timeout_secs: 1,
            model: None,
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
        };
        let observer = Counter(Mutex::default());
        let e = run_spawn_watched(
            &env,
            &req,
            &TaskId("intent-barrier".into()),
            &Caller::root("root", builtin("codex").unwrap()),
            &observer,
        )
        .expect_err("no durable intent, no child");
        assert!(
            matches!(e, SpawnError::SpawnIntentBarrier { .. }),
            "the wrong refusal: {e}"
        );
        assert!(
            observer.0.lock().unwrap().is_empty(),
            "no identity announced"
        );
        let agents: Vec<_> = std::fs::read_dir(project.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "journal.jsonl")
            .collect();
        assert!(agents.is_empty(), "no agent directory: {agents:?}");
        let branches = marion_testsupport::git(&repo, &["branch", "--list", "marion/*"]);
        assert!(branches.trim().is_empty(), "no task branch: {branches}");
    }

    /// **A finished child's task branch is removed only while it still points at the base marion
    /// created it at.** An unchanged branch is residue that would make the task id single-use
    /// (`git worktree add -b` cannot recreate it); a branch that moved holds work and survives.
    /// The deletion is git's compare-and-delete, so the check and the delete are one step.
    ///
    /// Mutation: drop the `update-ref -d` and the unchanged branch survives; drop its old-value
    /// argument and the advanced branch (and the only name for its commit) is deleted.
    #[test]
    fn cleanup_deletes_an_unchanged_task_branch_and_keeps_an_advanced_one() {
        let root = scratch("run-cleanup-branch");
        let repo = fixture_repo(&root);
        let git = |dir: &Path, args: &[&str]| marion_testsupport::git(dir, args);
        let exists = |branch: &str| {
            SysCommand::new("git")
                .current_dir(&repo)
                .args([
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ])
                .output()
                .unwrap()
                .status
                .success()
        };

        let unchanged = root.join("wt-unchanged");
        let base =
            crate::spawn::make_worktree(&repo, &unchanged, "marion/unchanged", false).unwrap();
        cleanup(&repo, &unchanged, "marion/unchanged", Some(&base));
        assert!(!unchanged.exists(), "the worktree is removed");
        assert!(
            !exists("marion/unchanged"),
            "and its unchanged branch with it"
        );
        crate::spawn::make_worktree(&repo, &unchanged, "marion/unchanged", false)
            .expect("so the task id can be used again");

        let advanced = root.join("wt-advanced");
        let base = crate::spawn::make_worktree(&repo, &advanced, "marion/advanced", false).unwrap();
        git(
            &advanced,
            &[
                "-c",
                "user.email=m@example.invalid",
                "-c",
                "user.name=m",
                "commit",
                "--allow-empty",
                "-qm",
                "work",
            ],
        );
        cleanup(&repo, &advanced, "marion/advanced", Some(&base));
        assert!(!advanced.exists(), "the worktree is removed");
        assert!(
            exists("marion/advanced"),
            "but a branch holding work is kept"
        );

        assert_eq!(
            task_branch_ref("marion/0197f3aa-1c2d"),
            Some("refs/heads/marion/0197f3aa-1c2d".into())
        );
        for branch in [
            "main",
            "marion/",
            "marion/a/b",
            "marion/..",
            "refs/heads/main",
        ] {
            assert_eq!(task_branch_ref(branch), None, "{branch:?}");
        }
    }

    /// **A spawn refused after its worktree exists takes the worktree back.** The worktree is the
    /// first irreversible thing a spawn does, and several refusals can still follow it before any
    /// process runs — here the protocol row with no agent bound, which has no child surface to
    /// launch. Each one used to return with the directory, its `.git/worktrees/` registration and
    /// the unchanged `marion/<task>` branch left in the operator's repository.
    ///
    /// Mutation: drop the prelaunch guard (or disarm it before the refusal) and all three remain.
    #[test]
    fn a_spawn_refused_after_its_worktree_exists_leaves_no_worktree_or_branch() {
        let root = scratch("run-prelaunch-worktree");
        let repo = fixture_repo(&root);
        let types = repo.join(AGENT_TYPES_FILE);
        std::fs::create_dir_all(types.parent().unwrap()).unwrap();
        std::fs::write(
            &types,
            "[[agent]]\nname = \"bare-acp\"\nharness = \"acp\"\ndescription = \"No agent.\"\n",
        )
        .unwrap();
        let state = root.join("state");
        let project = ProjectDir::new(&state, &crate::socket::project_root(&repo));
        let env = Env {
            project_dir: project.clone(),
            state: state.clone(),
            project_root: repo.clone(),
            bridge: PathBuf::from("/bin/marion-supervisor"),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
        };
        let req = SpawnRequest {
            review: None,
            agent_type: "bare-acp".into(),
            prompt: "do the task".into(),
            repo: repo.clone(),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec!["src/**".into()],
            timeout_secs: 1,
            model: None,
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
        };
        let e = run_spawn_watched(
            &env,
            &req,
            &TaskId("prelaunch".into()),
            &Caller::root("root", builtin("codex").unwrap()),
            &Unwatched,
        )
        .expect_err("the protocol row with no agent cannot be launched");
        assert!(
            matches!(e, SpawnError::Harness(_)),
            "refused by the adapter, after the worktree: {e}"
        );
        let branches = marion_testsupport::git(&repo, &["branch", "--list", "marion/*"]);
        assert!(branches.trim().is_empty(), "no task branch: {branches}");
        let worktrees = marion_testsupport::git(&repo, &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            1,
            "only the operator's own checkout is registered: {worktrees}"
        );
        let node = crate::registry::Registry::boot(&project)
            .unwrap()
            .tree()
            .nodes()
            .iter()
            .map(|n| n.agent_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(node.len(), 1, "one node was intended: {node:?}");
        assert!(
            !project.agent(&node[0]).worktree().exists(),
            "no worktree directory"
        );
    }

    /// A short capture that reads as a whole one is the invisible failure design §6.7 exists to
    /// prevent, so the timeout description has to carry it — and has to keep the timeout wording.
    #[test]
    fn a_drain_bound_truncation_is_recorded_in_the_exit_description() {
        let noted =
            note_truncated_capture("child exceeded its timeout and its process group was killed");
        assert!(noted.starts_with("child exceeded its timeout"));
        assert!(noted.contains("output capture truncated"));
    }

    /// A repo with one commit, which is all `make_worktree` needs to get past `rev-parse HEAD`.
    fn fixture_repo(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/keep.txt"), "keep\n").unwrap();
        for args in [
            vec!["init", "-q", "-b", "main", "."],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.email=marion@example.invalid",
                "-c",
                "user.name=marion",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            let out = SysCommand::new("git")
                .current_dir(&repo)
                .args(&args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        repo
    }

    /// Every file under `dir`, recursively. Used to prove a *negative* — that nothing anywhere
    /// under the node's state is a Codex `config.toml` — because asserting the absence of one
    /// guessed path would pass if the adapter simply wrote it somewhere else.
    fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                files_under(&p, out);
            } else {
                out.push(p);
            }
        }
    }

    /// **The dispatch regression.** `run_spawn` selects its adapter from `agent_type.harness`; for
    /// as long as it selected `Harness::Codex` by constant, spawning the `claude` type wrote a
    /// Codex `config.toml`, exec'd `codex`, and returned a contract stamped `"claude-code"` — an
    /// audit record (§6.7) describing a run that never happened.
    ///
    /// Two assertions, and each one fails against the constant:
    /// 1. no `config.toml` exists anywhere under the node's state — the Codex adapter's one output;
    /// 2. whatever the run produced, it is **the Claude Code adapter's**: its MCP declaration on
    ///    disk, and `claude-code` in the contract.
    ///
    /// **Re-pointed twice, never weakened, and the note has always said which part is stable.**
    /// The original asserted a typed `MissingInput` naming `claude-code`, because `run_spawn` built
    /// a `SpawnCtx` with `ready_file: None`; the second revision asserted the argv-prompt launch
    /// that replaced it. Both notes said the same thing — *"this test asserts the harness it names,
    /// not the branch it took"*. The branch has moved again, to the one this harness's surfaces
    /// actually declare: a `claude` child is a **duplex** node, so §6.1 step 8's readiness gate now
    /// runs on it. Against a base URL nobody serves and a one-second bound the gate is what fires,
    /// and that refusal is itself proof the duplex path ran — no other path has a marker to wait on.
    ///
    /// So the acceptable outcomes are exactly the claude-code-shaped ones, and the arm that would
    /// have caught the original bug is untouched: a contract stamped with any other harness, or a
    /// Codex `config.toml` on disk, still fails.
    #[test]
    fn a_claude_agent_type_never_writes_a_codex_config_or_launches_codex() {
        if !marion_testsupport::harness_available("claude") {
            return;
        }
        let root = scratch("supervisor-dispatch");
        let repo = fixture_repo(&root);
        let state = root.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let env = Env {
            project_dir: ProjectDir::new(&state, &repo),
            state: state.clone(),
            project_root: repo.clone(),
            bridge: PathBuf::from("/bin/marion-supervisor"),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
        };
        let req = SpawnRequest {
            review: None,
            agent_type: "claude".into(),
            prompt: "do the task".into(),
            repo: repo.clone(),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec!["src/**".into()],
            // Short: the base URL below answers nothing, so this bounds the launch to a second —
            // and a regression re-running `codex` for real is a fast failure rather than a wait.
            timeout_secs: 1,
            model: None,
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
        };

        let result = run_spawn(
            &env,
            &req,
            &TaskId("dispatch".into()),
            &Caller::root("root", builtin("claude").unwrap()),
        );

        let mut written = Vec::new();
        files_under(&state, &mut written);
        assert!(
            !written
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n == "config.toml")),
            "a Codex config.toml was written for a claude agent type: {written:?}"
        );
        assert!(
            written
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n == "mcp.json")),
            "the Claude Code adapter's own output must be what is on disk: {written:?}"
        );
        match result {
            Ok(c) => assert_eq!(
                c.child.harness,
                Harness::ClaudeCode,
                "the contract must name the harness that ran"
            ),
            Err(e) => assert!(
                matches!(
                    &e,
                    SpawnError::Harness(marion_harness::HarnessError::MissingInput {
                        harness: Harness::ClaudeCode,
                        ..
                    })
                ) | matches!(
                    &e,
                    SpawnError::Duplex(crate::duplex::DuplexError::McpNeverReady(_, _))
                        | SpawnError::Duplex(crate::duplex::DuplexError::DiedBeforeInitialize)
                ),
                "a refusal is still an acceptable outcome — but a typed one belonging to the \
                 harness that was asked for, never a fallback onto another. §6.1 step 8's gate \
                 only exists on the duplex path, so its refusal names that path as surely as \
                 MissingInput names the adapter. Got: {e}"
            ),
        }
    }

    /// **The two instants an owner outside `run_spawn` has to be told about**, and the fact that
    /// each is told *before* the thing it is about becomes irreversible.
    ///
    /// §11 item 28 step 4 moves node ownership into the supervisor, and an owner that learns a
    /// node's identity only from `run_spawn`'s **return** has learned it after the node has run.
    /// Two hooks, and the ordering of each is the whole assertion:
    ///
    /// * `identified` fires once the `SpawnIntent` is on disk and **before any side effect** — no
    ///   worktree, no config document, no process. That is what lets it mint §5.4's token in time
    ///   for the declaration this test then reads off disk. Firing it later would put the token in
    ///   a file already written; firing it before the intent would name a node no restart could
    ///   find.
    /// * `started` fires at `command.spawn()`, carrying a real pid. It is the same instant
    ///   `Spawned` is journaled (step 1), so an owner that returns when this fires is making a
    ///   claim the journal already backs rather than one it is about to.
    ///
    /// The token is asserted **on the bytes marion wrote**, not on what the observer returned: the
    /// value only means anything if it reached the one file the node's own bridge reads. A run
    /// whose observer was consulted and whose answer was dropped would pass every other assertion
    /// here.
    ///
    /// The run itself is allowed to fail — the base URL answers nothing and the bound is a second,
    /// exactly as the dispatch test above arranges. What is asserted is what happened *before* it
    /// failed.
    #[test]
    fn an_owner_learns_a_nodes_identity_before_its_first_side_effect_and_its_pid_at_launch() {
        if !marion_testsupport::harness_available("claude") {
            return;
        }
        struct Recorder {
            project: ProjectDir,
            identified: Mutex<Vec<AgentId>>,
            /// What the world looked like **at the instant `identified` fired** — recorded from
            /// inside the hook, because no assertion afterwards can see that instant. Pairs of
            /// (the intent is already durable, a side effect has already been taken).
            at_identify: Mutex<Vec<(bool, bool)>>,
            started: Mutex<Vec<(AgentId, i32)>>,
        }
        const TOKEN: &str = "MARION-OBSERVER-TOKEN-b17f";
        impl SpawnObserver for Recorder {
            fn identified(&self, agent_id: &AgentId) -> Option<Secret> {
                // Read back through the same replay a restarted supervisor would use, not by
                // grepping the file: what matters is that a *reader* can find the node.
                let intent_durable = crate::registry::Registry::boot(&self.project)
                    .map(|r| r.tree().get(agent_id).is_some())
                    .unwrap_or(false);
                let side_effect_taken = self.project.agent(agent_id).config_dir().exists();
                self.at_identify
                    .lock()
                    .unwrap()
                    .push((intent_durable, side_effect_taken));
                self.identified.lock().unwrap().push(agent_id.clone());
                Some(TOKEN.into())
            }
            fn started(&self, agent_id: &AgentId, pid: i32) {
                self.started.lock().unwrap().push((agent_id.clone(), pid));
            }
        }

        let root = scratch("supervisor-observer");
        let repo = fixture_repo(&root);
        let state = root.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let env = Env {
            project_dir: ProjectDir::new(&state, &repo),
            state: state.clone(),
            project_root: repo.clone(),
            bridge: PathBuf::from("/bin/marion-supervisor"),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
        };
        let req = SpawnRequest {
            review: None,
            agent_type: "claude".into(),
            prompt: "do the task".into(),
            repo: repo.clone(),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec!["src/**".into()],
            timeout_secs: 1,
            model: None,
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
        };
        let observer = Recorder {
            project: env.project_dir.clone(),
            identified: Mutex::default(),
            at_identify: Mutex::default(),
            started: Mutex::default(),
        };
        let _ = run_spawn_watched(
            &env,
            &req,
            &TaskId("observer".into()),
            &Caller::root("root", builtin("claude").unwrap()),
            &observer,
        );

        let identified = observer.identified.lock().unwrap().clone();
        assert_eq!(
            identified.len(),
            1,
            "exactly one node was spawned, so exactly one identity was announced"
        );
        // **The position of the hook, asserted rather than described.** Both halves fail against a
        // different placement: announcing before the intent is journaled gives an owner a node no
        // restart could find, and announcing after the first side effect gives it a token decided
        // too late for the document that has to carry it.
        assert_eq!(
            observer.at_identify.lock().unwrap().as_slice(),
            &[(true, false)],
            "at the instant the owner is told, the SpawnIntent must be durable (first) and no side \
             effect taken (second)"
        );
        let started = observer.started.lock().unwrap().clone();
        assert_eq!(started.len(), 1, "one process, one pid: {started:?}");
        assert_eq!(
            started[0].0, identified[0],
            "the pid must be announced for the node whose identity was announced, or an owner \
             keyed by AgentId files it under a node that does not exist"
        );
        assert!(
            started[0].1 > 0,
            "a pid a signal could reach, not a placeholder: {}",
            started[0].1
        );

        // The identity marion told the owner is the identity marion journaled. Reading it back off
        // the tree rather than trusting the hook is what stops a hook that announces some *other*
        // node's id from passing.
        let journalled: Vec<_> = crate::registry::Registry::boot(&env.project_dir)
            .expect("the journal this run wrote is readable")
            .tree()
            .nodes()
            .iter()
            .map(|n| n.agent_id.clone())
            .collect();
        assert!(
            journalled.contains(&identified[0]),
            "the announced identity must be the journalled one: announced {identified:?}, \
             journalled {journalled:?}"
        );

        // And the token reached the one file the node's own bridge reads.
        let mut written = Vec::new();
        files_under(&state, &mut written);
        let mcp = written
            .iter()
            .find(|p| p.file_name().is_some_and(|n| n == "mcp.json"))
            .unwrap_or_else(|| panic!("the adapter wrote no declaration: {written:?}"));
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(mcp).unwrap()).unwrap();
        assert_eq!(
            doc["mcpServers"]["marion"]["env"][marion_harness::claude_code::NODE_TOKEN_ENV],
            serde_json::json!(TOKEN),
            "the token the owner minted must be in the declaration, beside MARION_AGENT_ID — a \
             token nobody wrote down is a capability nothing can present:\n{doc:#}"
        );
        assert_eq!(
            doc["mcpServers"]["marion"]["env"][marion_harness::claude_code::AGENT_ID_ENV],
            serde_json::json!(identified[0].0),
            "…and beside the identity it is bound to"
        );
    }

    /// The other half of item 1: the Codex path still resolves to the Codex adapter, so the
    /// dispatch change is observably a no-op for every M1 spawn.
    #[test]
    fn each_builtin_agent_type_dispatches_to_its_own_harness() {
        for (name, expected) in [
            ("claude", Harness::ClaudeCode),
            ("claude-impl", Harness::ClaudeCode),
            ("codex", Harness::Codex),
            ("codex-impl", Harness::Codex),
            ("gemini", Harness::Gemini),
            ("gemini-impl", Harness::Gemini),
            ("opencode", Harness::OpenCode),
            ("copilot", Harness::Copilot),
            ("copilot-impl", Harness::Copilot),
            ("goose", Harness::Goose),
            ("goose-impl", Harness::Goose),
            ("cline", Harness::Cline),
            ("qwen", Harness::Qwen),
            ("qwen-impl", Harness::Qwen),
            ("pi", Harness::Pi),
            ("pi-orchestrator", Harness::Pi),
        ] {
            let t = builtin(name).expect("built-in resolves");
            assert_eq!(t.harness, expected, "{name}");
            assert_eq!(
                adapter_for(t.harness)
                    .expect("every built-in's harness has an adapter")
                    .harness(),
                expected,
                "{name}: the adapter run_spawn selects must be this type's harness"
            );
        }
    }

    /// **Re-pointed, not weakened.** This test used to reach the refusal through `adapter_for`,
    /// because Gemini and OpenCode had no adapter. They do now, so that route is gone — and
    /// asserting it against some other harness would have been vacuous, since the registry is
    /// exhaustive over `Harness::ALL` (`marion-harness::adapter::…resolves_to_an_adapter`). What
    /// the test was actually defending is the *conversion*: whatever produces an `Unimplemented`,
    /// `run_spawn`'s `?` must surface it as a typed `SpawnError` naming the harness, never as a
    /// fallback or a flattened string. That is asserted here directly, for every harness.
    #[test]
    fn an_unimplemented_harness_reaches_run_spawn_as_a_refusal_that_names_it() {
        for h in Harness::ALL {
            let err: SpawnError = marion_harness::HarnessError::Unimplemented(h).into();
            assert!(
                matches!(
                    err,
                    SpawnError::Harness(marion_harness::HarnessError::Unimplemented(g)) if g == h
                ),
                "{h}: expected a typed Unimplemented refusal"
            );
            assert!(
                err.to_string().contains(h.as_str()),
                "{h}: the message must name the harness, got {err}"
            );
        }
    }

    fn launch_ctx() -> SpawnCtx {
        SpawnCtx {
            agent_id: AgentId("019f-child".into()),
            agent_type: "codex-impl".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
            repo: "/repo".into(),
            state_dir: "/state".into(),
            bridge: "/bin/marion-supervisor".into(),
            bridge_args: vec!["mcp".into()],
        }
    }

    fn launch_spec(model: Option<String>) -> LaunchSpec {
        LaunchSpec {
            cwd: "/wt".into(),
            model,
            prompt: "do the task".into(),
            tools: vec![],
            allowed_tools: vec![],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: None,
            auth: Auth::Canned,
            config_dir: "/state/x/config".into(),
            resume: None,
            wire: None,
            provider: None,
            extra: Extras::default(),
        }
    }

    /// **The child's two axes, composed the way `run_spawn` composes them.**
    ///
    /// `run_spawn` sets `tools` from the resolved agent type and `allowed_tools` to marion's verbs
    /// for the child's depth (`agent_type::child_verbs`), and leaves the union to the adapter.
    /// This drives that exact pair through the exact adapter the dispatch above selects, so the
    /// composition is checked without a process: a `claude-impl` child gets `Write` on **both**
    /// flags and marion's own verbs are not lost from the permission axis in the process.
    ///
    /// **It restates two lines of `run_spawn` rather than calling them, and that is stated rather
    /// than hidden.** `run_spawn` needs a repo, a worktree and a process, so the wiring itself is
    /// pinned end to end by the harness cross-product's writing cells; what this catches is the
    /// composition being wrong — the union done at the call site instead of in the adapter (which
    /// would grant permission without availability), or `report` dropped while unioning (which
    /// would leave a child that can write and cannot report, the §12 shape in a new place).
    #[test]
    fn a_child_of_an_impl_type_is_compiled_with_availability_and_permission_open_together() {
        let t = builtin("claude-impl").expect("the implementer type resolves");
        let adapter = adapter_for(t.harness).unwrap();
        let spec = LaunchSpec {
            // Verbatim from `run_spawn`, for a child at depth 1.
            tools: t.tools.clone(),
            allowed_tools: marion_core::agent_type::child_verbs(&t, 1)
                .into_iter()
                .map(|verb| adapter.marion_tool_name(verb))
                .collect(),
            // A duplex child's prompt is a frame written after launch, so argv carries none.
            prompt: String::new(),
            ..launch_spec(None)
        };
        let args = adapter.compile(&spec, &launch_ctx()).unwrap().args;
        let after = |flag: &str| -> String {
            let i = args.iter().position(|a| a == flag).expect("flag present");
            args[i + 1].clone()
        };
        assert_eq!(after("--tools"), "Read,Write,Edit,Bash", "availability");
        assert_eq!(
            after("--allowedTools"),
            "mcp__marion__report,mcp__marion__spawn,mcp__marion__status,mcp__marion__wait,\
             mcp__marion__list,mcp__marion__steer,Read,Write,Edit,Bash",
            "permission carries marion's verbs AND the whole declaration; either alone is a dead \
             end, and a verb that reached availability and not permission is item 22's"
        );
        // The orchestrator type through the same path: unchanged, which is what keeps this
        // additive.
        let orchestrator = LaunchSpec {
            tools: builtin("claude-orchestrator").unwrap().tools,
            ..spec
        };
        let args = adapter.compile(&orchestrator, &launch_ctx()).unwrap().args;
        let i = args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(
            args[i + 1],
            "",
            "a `claude-orchestrator` child is read-only, as plain `claude` used to be"
        );
    }

    /// **§6.7's `allowed_tools` is the compiled constraint, never the requested one — and codex is
    /// where those two are visibly different strings.**
    ///
    /// A `codex-impl` node asked for marion's `write`; what codex actually ran under is
    /// `sandbox:workspace-write`, because `codex exec` has no allowlist to check a call against.
    /// Recording the request would put marion's own vocabulary in a field §3.1 says must never
    /// carry it: *"echoing marion's own vocabulary there would make the field claim a constraint
    /// that never existed."*
    ///
    /// The four harnesses answer in four different shapes, which is the other half of the claim: a
    /// uniform answer is what the hardcoded `["apply_patch", "shell"]` was, and it was wrong on all
    /// four. Driven through `adapter_for` exactly as `run_spawn` drives it; the assignment into the
    /// contract is one line beside `child.harness` and `child.model`, and is pinned end to end by
    /// the harness cross-product.
    #[test]
    fn the_contract_records_the_compiled_constraint_and_never_the_requested_tool() {
        // Every harness driven with the SAME request — marion's `write` — so what differs in the
        // column below is only how each harness expresses the constraint. Declared explicitly
        // rather than read off a built-in: `codex-impl` and `opencode` state no tools, so a
        // built-in-only sweep could never put `write` in front of those two adapters and the
        // "never the requested word" assertion would pass vacuously on the two harnesses where it
        // is most likely to be violated.
        let declared = vec![marion_core::agent_type::TOOL_WRITE.to_string()];
        for (harness, want) in [
            (Harness::ClaudeCode, vec!["mcp__marion__report", "Write"]),
            // **The discriminating cell.** `write` in, `sandbox:workspace-write` out: the request
            // and the compiled constraint are visibly different strings, so a record sourced from
            // the request cannot pass here by coincidence the way it could where the two agree.
            (Harness::Codex, vec!["sandbox:workspace-write"]),
            (Harness::Gemini, vec!["approval-mode:auto_edit"]),
            (Harness::OpenCode, vec!["harness-default:unconstrained"]),
            // The pattern grammar, not the tool grammar: `write` is the kind that grants `create`.
            (
                Harness::Copilot,
                vec!["allow-tool:marion(report)", "allow-tool:write"],
            ),
            // The builtin grammar: `write` is one of the developer extension's tools, and the
            // extension is the unit goose grants.
            (Harness::Goose, vec!["with-builtin:developer"]),
            // opencode's record, for opencode's reason: the 26 built-ins are offered regardless.
            (Harness::Cline, vec!["harness-default:unconstrained"]),
            // The `--core-tools` list itself: marion's verb, then the declared built-in.
            (
                Harness::Qwen,
                vec!["core-tools:mcp__marion__report", "core-tools:write_file"],
            ),
            // The `--tools` list itself: marion's verb, then both built-ins that change a file.
            (
                Harness::Pi,
                vec!["tools:mcp__marion__report", "tools:write", "tools:edit"],
            ),
        ] {
            let adapter = adapter_for(harness).unwrap();
            let launch = LaunchSpec {
                tools: declared.clone(),
                allowed_tools: vec![adapter.marion_tool_name("report")],
                prompt: String::new(),
                model: Some("m/m".into()),
                ..launch_spec(None)
            };
            let recorded = adapter.compiled_permissions(&launch).unwrap();
            assert_eq!(recorded, want, "{harness}");
            assert!(
                !recorded.iter().any(|r| declared.contains(r)),
                "{harness}: `write` is marion's word for the request, not any harness's word for \
                 the constraint — recording it would be the request masquerading as the outcome. \
                 Got: {recorded:?}"
            );
        }
    }

    fn request(agent_type: &str, model: Option<&str>) -> SpawnRequest {
        SpawnRequest {
            review: None,
            agent_type: agent_type.into(),
            prompt: "do the task".into(),
            // A placeholder, overwritten by every caller that actually launches: `resolve_model`
            // is pure and never reaches a filesystem, so a real tree here would be scenery.
            repo: PathBuf::from("/repo"),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec![],
            timeout_secs: 1,
            model: model.map(str::to_string),
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
        }
    }

    /// **The gap this phase closed.** `run_spawn` used to build its `LaunchSpec` with a hard
    /// `model: None`, and §6.4 makes an explicit model a MUST on both new harnesses (gemini's
    /// `auto` router hung; opencode has no `OPENCODE_MODEL` env var) — so a `gemini` or `opencode`
    /// agent type could be named, resolved and dispatched, and then refused at `compile`. The
    /// resolved model is what makes them launchable, so the test asserts the *whole chain*: the
    /// built-in's default reaches `resolve_model`, and what `resolve_model` returns compiles.
    #[test]
    fn the_new_harnesses_now_compile_because_their_agent_types_carry_a_model() {
        for name in ["gemini", "opencode"] {
            let t = builtin(name).unwrap();
            let model = resolve_model(&request(name, None), &t, Auth::Canned);
            assert!(
                model.is_some(),
                "{name}: its adapter refuses without one, so its built-in must state one"
            );
            adapter_for(t.harness)
                .unwrap()
                .compile(&launch_spec(model), &launch_ctx())
                .unwrap_or_else(|e| panic!("{name} still cannot compile: {e}"));
        }
    }

    /// And the refusal is still there for anyone who defeats the default: it names its own harness
    /// rather than falling through to codex.
    #[test]
    fn a_new_harness_with_no_model_anywhere_still_refuses_and_names_itself() {
        for (name, h) in [
            ("gemini", Harness::Gemini),
            ("opencode", Harness::OpenCode),
            ("copilot", Harness::Copilot),
            ("goose", Harness::Goose),
            ("cline", Harness::Cline),
            ("qwen", Harness::Qwen),
            ("pi", Harness::Pi),
        ] {
            let adapter = adapter_for(builtin(name).unwrap().harness).unwrap();
            let err: SpawnError = adapter
                .compile(&launch_spec(None), &launch_ctx())
                .unwrap_err()
                .into();
            assert!(
                matches!(
                    err,
                    SpawnError::Harness(marion_harness::HarnessError::MissingInput {
                        harness: g,
                        ..
                    }) if g == h
                ),
                "{name}: expected a typed refusal naming {h}, got {err}"
            );
        }
    }

    /// §3.1's precedence, in one statement: the request wins, the agent type is the default, and
    /// the two harnesses that have always run without a model still resolve to `None` — which is
    /// what keeps `codex exec`'s measured argv, and `m1_hop` and `timeout_kill` with it, unchanged.
    #[test]
    fn the_request_overrides_the_agent_types_default_and_absence_stays_absence() {
        let gemini = builtin("gemini").unwrap();
        assert_eq!(
            resolve_model(
                &request("gemini", Some("gemini-2.5-pro")),
                &gemini,
                Auth::Canned
            )
            .as_deref(),
            Some("gemini-2.5-pro"),
        );
        assert_eq!(
            resolve_model(&request("gemini", None), &gemini, Auth::Canned).as_deref(),
            Some("gemini-2.5-flash"),
        );
        for name in ["codex-impl", "claude"] {
            assert_eq!(
                resolve_model(&request(name, None), &builtin(name).unwrap(), Auth::Canned),
                None,
                "{name}: a default here would change an argv that is measured, for nothing"
            );
        }
    }

    /// **A live child of a type whose default names marion's canned plumbing runs on the
    /// operator's own default model**: `marion/default` names a provider block only a canned run
    /// writes, so passing it live made every opencode child fail at launch.
    #[test]
    fn a_live_spawn_does_not_inherit_a_canned_plumbing_default_model() {
        let opencode = builtin("opencode").unwrap();
        assert_eq!(
            resolve_model(&request("opencode", None), &opencode, Auth::Inherited),
            None
        );
        assert_eq!(
            resolve_model(&request("opencode", None), &opencode, Auth::Canned).as_deref(),
            Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL)
        );
        assert_eq!(
            resolve_model(
                &request("opencode", Some("a/b")),
                &opencode,
                Auth::Inherited
            )
            .as_deref(),
            Some("a/b"),
            "an explicit model is always honoured"
        );
    }

    /// **The contract records the wire, not the ask** — the same rule that made `child.harness`
    /// come from the adapter. A caller can name a model for a codex child; `codex exec` carries
    /// none, so the contract must not claim one.
    #[test]
    fn a_model_asked_for_on_a_harness_that_takes_none_is_never_recorded_as_used() {
        let t = builtin("codex-impl").unwrap();
        let asked = resolve_model(
            &request("codex-impl", Some("gpt-5.6-sol")),
            &t,
            Auth::Canned,
        );
        assert_eq!(
            asked.as_deref(),
            Some("gpt-5.6-sol"),
            "the ask is honoured…"
        );
        let inv = adapter_for(t.harness)
            .unwrap()
            .compile(&launch_spec(asked), &launch_ctx())
            .unwrap();
        assert_eq!(
            inv.model, None,
            "…but nothing carried it, so `child.model` records nothing"
        );
        assert!(
            !inv.args.iter().any(|a| a == "gpt-5.6-sol"),
            "and it reached no argv either"
        );
    }

    /// An `Env`, a state dir and a fixture repo, for the tests that call `run_spawn` for real.
    ///
    /// The repo is returned separately because it is no longer part of the environment: it is a
    /// per-spawn input, so each of these tests states it on its own request.
    /// **The file is read from the working tree the spawn is against, at every spawn.** No file is
    /// the built-in table; a file that cannot be parsed is a refusal that names the file and the
    /// reason, distinct from an unknown type — the operator's fix is in the file, not the request.
    #[test]
    fn agent_types_reads_the_trees_file_or_refuses_by_name() {
        let root = scratch("supervisor-agent-types");
        let repo = fixture_repo(&root);
        assert_eq!(
            agent_types(&repo).unwrap(),
            marion_core::agent_type::AgentTypes::builtins_only(),
            "no file: the built-ins"
        );
        let file = repo.join(AGENT_TYPES_FILE);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"Reviews.\"\n",
        )
        .unwrap();
        let types = agent_types(&repo).unwrap();
        assert_eq!(
            types.resolve("reviewer").unwrap().harness,
            marion_core::harness::Harness::Codex
        );
        std::fs::write(
            &file,
            "[[agent]]\nname = \"codex\"\nharness = \"codex\"\ndescription = \"x\"\n",
        )
        .unwrap();
        let e = agent_types(&repo).unwrap_err();
        match &e {
            SpawnError::AgentTypesFile { path, .. } => assert_eq!(path, &file),
            other => panic!("a broken file is its own refusal, got {other:?}"),
        }
        let msg = e.to_string();
        assert!(msg.contains(&file.display().to_string()), "{msg}");
        assert!(
            msg.contains("shadow"),
            "the parser's reason survives: {msg}"
        );
        // A directory where the file should be is an io error, not "no file".
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(
            matches!(agent_types(&repo), Err(SpawnError::AgentTypesFile { .. })),
            "only NotFound means the built-ins"
        );
    }

    /// **A node's config documents are the owner's alone.** They carry the node's capability token
    /// in the bridge declaration and, on an endpoint node, the user's key (opencode's provider
    /// block, cline's `providers.json`), so no other user on the machine may read them.
    #[test]
    fn config_documents_are_written_owner_only_even_over_a_wider_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("config-docs-mode");
        let path = root.join("nested/dir/doc.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let fresh = root.join("fresh/doc.toml");
        write_config_documents(vec![
            (path.clone(), "{}".into()),
            (fresh.clone(), "x = 1".into()),
        ])
        .unwrap();
        for p in [&path, &fresh] {
            let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", p.display());
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    }

    /// **A row's `provider` must be one the user has** — a built-in, or one in their own
    /// `providers.toml` — checked at load, so a tree naming a provider nobody defined refuses every
    /// spawn by name instead of failing only the node that reaches for it. The registry is read
    /// only when some row names a provider.
    #[test]
    fn a_rows_provider_must_be_in_the_users_registry() {
        let parse = |p: &str| {
            marion_core::agent_type::AgentTypes::parse(&format!(
                "[[agent]]\nname = \"x\"\nharness = \"codex\"\ndescription = \"d\"\n{p}"
            ))
            .unwrap()
        };
        let seed = || Ok(marion_core::provider::Registry::seed());
        assert!(check_providers(&parse("provider = \"openrouter\"\n"), seed).is_ok());
        let err = check_providers(&parse("provider = \"nope\"\n"), seed).unwrap_err();
        assert!(
            err.contains("`nope`") && err.contains("marion login"),
            "{err}"
        );
        let custom = || {
            marion_core::provider::Registry::with_custom(
                "[providers.mine]\nbase_url = \"https://x/v1\"\nwires = [\"openai-chat\"]\n",
            )
            .map_err(|e| e.to_string())
        };
        assert!(check_providers(&parse("provider = \"mine\"\n"), custom).is_ok());
        // No row names a provider: the registry is never consulted, so a broken file cannot
        // refuse a tree that does not use it.
        let unread = || -> Result<marion_core::provider::Registry, String> {
            panic!("the registry was read for a tree that names no provider")
        };
        assert!(check_providers(&parse(""), unread).is_ok());
    }

    /// A type the tree's file does not define is refused before any side effect, exactly as an
    /// unknown built-in is — and the refusal is the same variant, so callers keep one arm.
    #[test]
    fn a_type_the_file_no_longer_defines_is_refused_before_anything_is_journaled() {
        let (_root, state, repo, env) = spawn_env("agent-types-unknown");
        let mut req = request("reviewer", None);
        req.repo = repo;
        let err = run_spawn(
            &env,
            &req,
            &TaskId("unknown".into()),
            &Caller::root("root", builtin("claude").unwrap()),
        )
        .expect_err("no file defines `reviewer`");
        assert!(
            matches!(&err, SpawnError::UnknownAgentType(t) if t == "reviewer"),
            "{err:?}"
        );
        let mut written = Vec::new();
        files_under(&state, &mut written);
        assert!(
            written.is_empty(),
            "nothing journaled, nothing started: {written:?}"
        );
    }

    /// A type's `prompt_prefix` goes in front of the prompt exactly once, and a type with none
    /// leaves the prompt untouched — byte for byte, so a built-in's node is launched from the
    /// bytes it was launched from before the field existed.
    #[test]
    fn a_user_types_prompt_prefix_is_prepended_once() {
        let plain = builtin("codex-impl").unwrap();
        assert_eq!(prefixed_prompt(&plain, "do the task"), "do the task");
        let reviewer = marion_core::agent_type::AgentTypes::parse(
            "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"r\"\n\
             prompt_prefix = \"Review only.\\n\\n\"\n",
        )
        .unwrap()
        .resolve("reviewer")
        .unwrap();
        let once = prefixed_prompt(&reviewer, "do the task");
        assert_eq!(once, "Review only.\n\ndo the task");
        assert_eq!(once.matches("Review only.").count(), 1);
    }

    /// **Every child is told, in its prompt, to call `report`.** Measured live (2026-09-22): ten
    /// claude and codex children did the work and never called `report`, because the tool's own
    /// description was the only place marion said so. The instruction comes last — after a type's
    /// prefix and the task — exactly once, and it names the tool by marion's server rather than
    /// in any one harness's spelling.
    #[test]
    fn a_childs_prompt_ends_with_marions_one_report_instruction() {
        let plain = builtin("codex-impl").unwrap();
        let prompt = child_prompt(&plain, "do the task");
        assert_eq!(
            prompt,
            format!("do the task\n\n{}", crate::bridge::REPORT_INSTRUCTION)
        );
        let reviewer = marion_core::agent_type::AgentTypes::parse(
            "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"r\"\n\
             prompt_prefix = \"Review only.\\n\\n\"\n",
        )
        .unwrap()
        .resolve("reviewer")
        .unwrap();
        let prefixed = child_prompt(&reviewer, "do the task");
        assert!(
            prefixed.starts_with("Review only.\n\ndo the task\n\n"),
            "{prefixed}"
        );
        assert_eq!(
            prefixed.matches(crate::bridge::REPORT_INSTRUCTION).count(),
            1,
            "{prefixed}"
        );
        let instruction = crate::bridge::REPORT_INSTRUCTION;
        assert!(
            instruction.contains("marion MCP server") && instruction.contains("`report`"),
            "{instruction}"
        );
        for spelling in ["mcp__marion__report", "marion_report", "marion-report"] {
            assert!(
                !instruction.contains(spelling),
                "harness-neutral: {instruction}"
            );
        }
    }

    /// **A prefix and a prompt are two sentences, and marion keeps them apart.** A row that ends
    /// its prefix on a letter gets one newline between it and the prompt; a row whose author
    /// already ended it in whitespace (`"\n\n"`) is joined exactly as written, because that
    /// whitespace is the author's own separator and marion must not add a third line to it.
    #[test]
    fn a_prefix_without_trailing_whitespace_is_separated_from_the_prompt_by_one_newline() {
        let ty = |prefix: &str| {
            marion_core::agent_type::AgentTypes::parse(&format!(
                "[[agent]]\nname = \"reviewer\"\nharness = \"codex\"\ndescription = \"r\"\n\
                 prompt_prefix = {prefix:?}\n",
            ))
            .unwrap()
            .resolve("reviewer")
            .unwrap()
        };
        assert_eq!(
            prefixed_prompt(&ty("You are a reviewer."), "DEMO: review"),
            "You are a reviewer.\nDEMO: review"
        );
        assert_eq!(
            prefixed_prompt(&ty("You are a reviewer.\n\n"), "DEMO: review"),
            "You are a reviewer.\n\nDEMO: review"
        );
        assert_eq!(
            prefixed_prompt(&ty("You are a reviewer. "), "DEMO: review"),
            "You are a reviewer. DEMO: review"
        );
        let plain = builtin("codex-impl").unwrap();
        assert_eq!(prefixed_prompt(&plain, "DEMO: review"), "DEMO: review");
    }

    /// **A `SpawnAborted` written for a refusal marion can name carries that name.** The guard's
    /// generic reason is for the exits it cannot see; a `?` that passed through [`AbortOnDrop::filed`]
    /// leaves the error's own sentence in the journal, so a reader of the record learns which tool
    /// on which harness rather than only that marion left.
    #[test]
    fn an_abort_files_the_refusals_own_sentence_when_it_has_one() {
        let (_root, _state, _repo, env) = spawn_env("abort-reason");
        let agent_id = AgentId("abort-1".into());
        let refused: Result<(), SpawnError> = Err(SpawnError::Harness(
            marion_harness::HarnessError::UnsupportedTool {
                harness: Harness::Codex,
                tool: "read".into(),
            },
        ));
        {
            let mut resolution = AbortOnDrop {
                project: &env.project_dir,
                agent_id: agent_id.clone(),
                armed: true,
                reason: None,
            };
            let err = resolution.filed(refused).unwrap_err();
            assert!(
                matches!(err, SpawnError::Harness(_)),
                "the error is returned untouched"
            );
        }
        let bytes = std::fs::read(env.project_dir.journal()).unwrap();
        let replay = marion_core::registry::replay(&bytes);
        let reason = replay
            .get(&agent_id)
            .and_then(|n| n.spawn_aborted.clone())
            .expect("the guard journaled the abort");
        assert!(
            reason.contains("`read`") && reason.contains("codex"),
            "the journal carries the adapter's sentence: {reason}"
        );
    }

    /// **The contract records the prompt the child actually saw**, prefix included: §6.7's audit
    /// record names what marion did, and what marion did was hand the node the prefixed text.
    #[test]
    fn the_contract_records_the_prefixed_prompt() {
        // opencode, a `LaunchOnly` row: its prompt rides argv, so a dead endpoint and a bridge that
        // does not exist still end in a contract. (A codex child over app-server is refused by name
        // before its first turn when marion's MCP server cannot start.)
        if !marion_testsupport::harness_available("opencode") {
            return;
        }
        let (_root, _state, repo, env) = spawn_env("prefixed-prompt");
        let file = repo.join(AGENT_TYPES_FILE);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "[[agent]]\nname = \"reviewer\"\nharness = \"opencode\"\ndescription = \"r\"\n\
             model = \"marion/default\"\nprompt_prefix = \"Review only.\\n\\n\"\n",
        )
        .unwrap();
        let mut req = request("reviewer", None);
        req.repo = repo;
        req.timeout_secs = 1;
        let contract = run_spawn(
            &env,
            &req,
            &TaskId("prefixed".into()),
            &Caller::root("root", builtin("claude").unwrap()),
        )
        .expect("an opencode child against a dead endpoint still ends in a contract");
        assert_eq!(
            contract.instructions.value,
            format!(
                "Review only.\n\ndo the task\n\n{}",
                crate::bridge::REPORT_INSTRUCTION
            )
        );
    }

    fn spawn_env(name: &str) -> (Scratch, PathBuf, PathBuf, Env) {
        let root = scratch(&format!("supervisor-{name}"));
        let repo = fixture_repo(&root);
        let state = root.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let env = Env {
            project_dir: ProjectDir::new(&state, &repo),
            state: state.clone(),
            project_root: repo.clone(),
            bridge: PathBuf::from("/bin/marion-supervisor"),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
        };
        (root, state, repo, env)
    }

    /// **The gate, and the side effects it must precede.**
    ///
    /// A caller already at its type's `max_depth` asks for one more level. §3.1: that spawn "is
    /// refused with a spawn error, never silently clamped" — so the assertion is threefold, and
    /// each part fails against the pre-fix code (which had no gate at all):
    ///
    /// 1. the error is the **typed** `SpawnError::Gate(DepthExceeded { .. })`, not a flattened
    ///    string and not some later failure that happens to look like a refusal;
    /// 2. its message **names the bound and the value** — a refusal that does not say `4` and `3`
    ///    tells the caller nothing it can act on;
    /// 3. **nothing was created.** Walked, not probed at a guessed path, for the same reason the
    ///    dispatch regression above walks: asserting the absence of one path would pass if the code
    ///    simply wrote it somewhere else. A worktree, a branch, an agent-dir and an OS process are
    ///    what this gate exists to prevent, so "refused" has to mean none of them happened.
    #[test]
    fn a_spawn_past_max_depth_is_refused_by_name_and_creates_nothing() {
        let (root, state, repo, env) = spawn_env("depth-gate");
        let caller = Caller {
            agent_id: "caller".into(),
            agent_type: builtin("claude").unwrap(),
            depth: marion_core::agent_type::DEFAULT_MAX_DEPTH,
            live_children: 0,
        };
        // codex-impl: a type whose child would really launch a process, so a missing gate is a real
        // grandchild rather than a failure somewhere else.
        let mut req = request("codex-impl", None);
        req.repo = repo;

        let err = run_spawn(&env, &req, &TaskId("too-deep".into()), &caller)
            .expect_err("a spawn past max_depth must be refused");

        assert!(
            matches!(
                err,
                SpawnError::Gate(marion_core::agent_type::SpawnGateError::DepthExceeded {
                    child_depth: 4,
                    max_depth: 3
                })
            ),
            "expected a typed depth refusal, got {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("max_depth 3") && msg.contains("depth 4"),
            "the refusal must name the bound AND the value that broke it, got: {msg}"
        );

        let mut written = Vec::new();
        files_under(&state, &mut written);
        assert!(
            written.is_empty(),
            "the gate runs before every side effect there is, so a refused spawn leaves no \
             agent-dir, no config and no contract: {written:?}"
        );
        let worktrees = SysCommand::new("git")
            .current_dir(repo_of(&root))
            .args(["worktree", "list"])
            .output()
            .expect("git runs");
        assert_eq!(
            String::from_utf8_lossy(&worktrees.stdout).lines().count(),
            1,
            "a refused spawn must not have created a worktree: {}",
            String::from_utf8_lossy(&worktrees.stdout)
        );
        let branches = SysCommand::new("git")
            .current_dir(repo_of(&root))
            .args(["branch", "--list", "marion/*"])
            .output()
            .expect("git runs");
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "nor a branch: {}",
            String::from_utf8_lossy(&branches.stdout)
        );
    }

    fn repo_of(root: &Path) -> PathBuf {
        root.join("repo")
    }

    /// The other half, so the gate cannot pass by refusing everything: one level shallower is
    /// **allowed through it**. The run then fails for its own reasons — there is no `codex` process
    /// worth starting against a base URL nobody serves — but whatever it fails as, it is not the
    /// gate, and it got far enough to create the state a refusal never would.
    #[test]
    fn a_spawn_within_max_depth_is_not_refused_by_the_gate() {
        // `_root` and not `_`: the underscore-prefixed binding still lives to the end of the test,
        // where its `Drop` removes the scratch dir. A bare `_` would drop it here, mid-test.
        let (_root, state, repo, env) = spawn_env("depth-allowed");
        let caller = Caller {
            agent_id: "caller".into(),
            agent_type: builtin("claude").unwrap(),
            // 2 → the child lands at 3, which is exactly `max_depth` and therefore legal.
            depth: marion_core::agent_type::DEFAULT_MAX_DEPTH - 1,
            live_children: 0,
        };
        let mut req = request("codex-impl", None);
        req.repo = repo;
        req.timeout_secs = 1;

        let result = run_spawn(&env, &req, &TaskId("deep-enough".into()), &caller);

        if let Err(e) = &result {
            assert!(
                !matches!(e, SpawnError::Gate(_)),
                "depth 3 is within max_depth 3 and must not be gated: {e}"
            );
        }
        let mut written = Vec::new();
        files_under(&state, &mut written);
        assert!(
            !written.is_empty(),
            "a spawn the gate let through gets an agent-dir and a config, which is precisely what \
             the refused one above must not have"
        );
    }

    /// The child's own depth is its caller's plus one, and that is what reaches its bridge — the
    /// link without which the gate above could never fire on a grandchild, because the grandchild's
    /// bridge would have no depth to check.
    #[test]
    fn a_childs_bridge_is_told_a_depth_one_below_its_callers() {
        // Held, not dropped: see the note in the test above.
        let (_root, state, repo, env) = spawn_env("depth-carried");
        let caller = Caller {
            agent_id: "caller".into(),
            agent_type: builtin("claude").unwrap(),
            depth: 1,
            live_children: 0,
        };
        let mut req = request("codex-impl", None);
        req.repo = repo;
        req.timeout_secs = 1;
        let _ = run_spawn(&env, &req, &TaskId("carry".into()), &caller);

        let mut written = Vec::new();
        files_under(&state, &mut written);
        let config = written
            .iter()
            .find(|p| p.file_name().is_some_and(|n| n == "config.toml"))
            .map(|p| std::fs::read_to_string(p).unwrap())
            .unwrap_or_else(|| panic!("the codex child's config was not written: {written:?}"));
        assert!(
            config.contains(r#"MARION_DEPTH = "2""#),
            "a child of a depth-1 caller is at depth 2, and its bridge must be told so:\n{config}"
        );
        assert!(
            config.contains(r#"MARION_AGENT_TYPE = "codex""#),
            "and told its own canonical type, whose max_depth its own spawns are gated on:\n{config}"
        );
    }

    /// **The concurrency half, now that it can bind.**
    ///
    /// This test used to be called
    /// `the_concurrency_gate_is_wired_and_cannot_bind_while_spawn_is_synchronous` and its doc said
    /// *"when backgrounding lands, the count changes and this test is the one that should start
    /// failing"*. It is that test, rewritten rather than deleted, because the fact it pinned has
    /// not gone away — it has inverted, and the inversion is the whole point of the change.
    ///
    /// What it pins now: the gate reads a *field*, so the caller's count is whatever the bridge
    /// measured, and the bound refuses at exactly `max_concurrent_children` on every built-in —
    /// refuses, never queues (§3.1). The end-to-end witness that a second **backgrounded** spawn
    /// is refused is `tests/background_spawn.rs`; this is the unit that would catch an off-by-one
    /// in the bound itself, which no end-to-end test could localize.
    #[test]
    fn the_concurrency_gate_refuses_at_the_bound_and_admits_below_it() {
        for name in marion_core::agent_type::builtin_names() {
            let t = builtin(name).unwrap();
            let max = t.max_concurrent_children;
            assert!(
                check_spawn_gates(&t, 0, max - 1).is_ok(),
                "{name}: a caller one below its bound may still spawn"
            );
            assert!(
                check_spawn_gates(&t, 0, max).is_err(),
                "{name}: at the bound the spawn is refused, not queued (§3.1)"
            );
            assert!(
                check_spawn_gates(&t, 0, max + 1).is_err(),
                "{name}: and above it too — the gate is `>=`, so a count that somehow overshot is \
                 still refused rather than wrapping back to admitted"
            );
        }
    }

    /// `Caller::root` answers the live count with a measurement, not with the old constant.
    ///
    /// Separate from the gate test above because it guards a different mistake: re-introducing a
    /// hardcoded zero by giving the field a default that no bridge ever overwrites. A root that
    /// `marion run` just minted genuinely has no children — that is why 0 is right here — and the
    /// bridge overwrites it on every `spawn` it serves.
    #[test]
    fn a_freshly_minted_root_has_no_live_children() {
        let c = Caller::root("root", builtin("claude").unwrap());
        assert_eq!(c.live_children, 0);
        assert_eq!(c.depth, crate::depth::ROOT_DEPTH);
    }

    #[test]
    fn a_requested_scope_outside_the_agent_type_ceiling_is_rejected_before_launch() {
        let ceiling = vec![Glob("src/**".into())];
        let requested = vec![Glob("docs/**".into())];
        assert!(check_spawn_scope(&ceiling, &requested).is_err());
    }

    // -----------------------------------------------------------------------------------------
    // §5.4's `verification`: shell lines run in the child's workspace at its terminal transition.
    // -----------------------------------------------------------------------------------------

    fn one_command(line: &str, cwd: &Path, timeout: StdDuration) -> Command {
        Command {
            program: "sh".into(),
            args: vec!["-c".into(), line.into()],
            cwd: cwd.to_path_buf(),
            timeout: Duration::from_secs(timeout.as_secs()),
        }
    }

    #[test]
    fn a_passing_verification_command_records_exit_zero_and_its_stdout() {
        let scratch = scratch("verif-pass");
        let cmds = verification_commands(&["echo verified".into()], &scratch);
        let out = run_verification(&cmds);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].command, cmds[0],
            "the outcome names the command that produced it"
        );
        assert_eq!(out[0].exit_code, Some(0));
        assert_eq!(out[0].stdout.value, "verified\n");
        assert_eq!(out[0].stderr.value, "");
        assert!(!out[0].timed_out);
    }

    #[test]
    fn a_failing_verification_command_records_its_exit_code_and_stderr() {
        let scratch = scratch("verif-fail");
        let cmds = verification_commands(&["echo broken 1>&2; exit 3".into()], &scratch);
        let out = run_verification(&cmds);
        assert_eq!(out[0].exit_code, Some(3));
        assert_eq!(out[0].stderr.value, "broken\n");
        assert_eq!(out[0].stdout.value, "");
        assert!(!out[0].timed_out);
    }

    /// The bound is the `Command`'s own, and expiry kills the whole group: the `sleep` is `sh`'s
    /// child, not `sh` itself, so a kill that reached only the direct child would leave it.
    #[test]
    fn a_verification_command_over_its_bound_is_killed_and_marked_timed_out() {
        let scratch = scratch("verif-timeout");
        // A fractional sleep no other test in this process runs, so the sweep below finds only
        // this sleeper — `pgrep -f` matches `sh -c`'s argv and the `sleep` it forked alike.
        let marker = format!("sleep 30.{}", std::process::id());
        let cmd = one_command(&marker, &scratch, StdDuration::from_secs(1));
        let started = Instant::now();
        let out = run_verification(std::slice::from_ref(&cmd));
        assert!(out[0].timed_out, "the bound expired");
        assert!(
            started.elapsed() < StdDuration::from_secs(10),
            "the bound is the command's 1 s, not §6.7's 300 s default"
        );
        let deadline = Instant::now() + StdDuration::from_secs(3);
        loop {
            let survivors = SysCommand::new("pgrep")
                .args(["-f", &marker])
                .output()
                .expect("pgrep runs");
            if survivors.stdout.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the sleeper and its shell survived the kill: pids {}",
                String::from_utf8_lossy(&survivors.stdout)
            );
            thread::sleep(StdDuration::from_millis(50));
        }
    }

    /// Sequential and in the parent's order, each in the workspace it was given: a later command
    /// sees what an earlier one wrote, which is what lets `cargo build` precede `cargo test`.
    #[test]
    fn verification_commands_run_in_the_workspace_in_the_parents_order() {
        let scratch = scratch("verif-order");
        let lines = vec![
            "echo first > order.txt".into(),
            "echo second >> order.txt".into(),
            "cat order.txt".into(),
        ];
        let cmds = verification_commands(&lines, &scratch);
        assert_eq!(cmds.len(), 3);
        for (c, line) in cmds.iter().zip(&lines) {
            assert_eq!(c.program, "sh");
            assert_eq!(c.args, vec!["-c".to_string(), line.clone()]);
            assert_eq!(&c.cwd, &*scratch);
            assert_eq!(
                c.timeout,
                Duration::from_secs(VERIFICATION_TIMEOUT.as_secs())
            );
        }
        let out = run_verification(&cmds);
        let codes: Vec<_> = out.iter().map(|o| o.exit_code).collect();
        assert_eq!(codes, vec![Some(0), Some(0), Some(0)]);
        assert_eq!(out[2].stdout.value, "first\nsecond\n");
        assert_eq!(
            std::fs::read_to_string(scratch.join("order.txt")).unwrap(),
            "first\nsecond\n",
            "the commands ran in the workspace, not in the test's cwd"
        );
    }

    /// The runner records `Capped::whole`: the persisted contract is the uncapped record
    /// (`m1_hop.rs` asserts it), and `cap_for_return` alone shortens the copy handed back.
    #[test]
    fn a_large_verification_output_is_persisted_whole_and_capped_only_on_return() {
        let scratch = scratch("verif-large");
        let cmds = verification_commands(
            &["yes 0123456789012345678901234567890123456789 | head -n 2000".into()],
            &scratch,
        );
        let evidence = run_verification(&cmds);
        let bytes = evidence[0].stdout.value.len();
        assert!(
            bytes > marion_core::cap::EVIDENCE_BUDGET,
            "the fixture must overflow the budget to test anything, got {bytes}"
        );
        assert!(!evidence[0].stdout.truncated);
        assert_eq!(evidence[0].stdout.original_bytes, bytes);

        let contract = build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: None,
                head_branch: None,
            },
            None,
            Workspace::Worktree {
                path: scratch.to_path_buf(),
                branch: "b".into(),
            },
            "do it",
            &[],
            &[Glob("**".into())],
            &[Glob("**".into())],
            Duration::from_secs(900),
            SystemTime(std::time::SystemTime::now()),
            &ChildOutcome {
                narrative: Some("done".into()),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            Some(vec![]),
            None,
            cmds.clone(),
            evidence,
        );
        let persisted = contract.completion.as_ref().unwrap();
        assert!(
            !persisted.evidence[0].stdout.truncated,
            "the persisted copy is whole"
        );
        assert_eq!(persisted.evidence[0].stdout.value.len(), bytes);
        let returned = cap_for_return(contract.clone());
        let ev = &returned.completion.unwrap().evidence[0];
        assert!(ev.stdout.truncated, "the returned copy is capped");
        assert!(ev.stdout.value.len() < bytes);
        assert_eq!(ev.stdout.original_bytes, bytes);
    }
}
