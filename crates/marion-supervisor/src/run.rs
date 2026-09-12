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

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command as SysCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use marion_core::agent_type::{AgentType, builtin, check_spawn_gates};
use marion_core::cap::cap_for_return;
use marion_core::contract::*;
use marion_core::encoding::{Duration, Millis, SystemTime};
use marion_core::ids::new_agent_id;
use marion_core::journal::{
    ContractPersisted, Exited, RecordKind, SpawnAborted, SpawnIntent, Spawned,
};
use marion_core::paths::{AgentDir, ProjectDir};
use marion_core::scope::check_spawn_scope;
use marion_harness::{
    Auth, ChildExit, Extras, Invocation, LaunchSpec, McpDeclaration, SpawnCtx, adapter_for_type,
};

pub(crate) use crate::clock::{entropy, unix_millis};
use crate::duplex::{self, DuplexSpec, LaunchPath, launch_path};
use crate::spawn::{
    ChildOutcome, SpawnError, build_contract, changed_paths, diff_text, make_worktree,
};

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
    /// could quietly falsify, and what depends on it is not internal: it is `MARION_STATE` in every
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

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpgrp() -> i32;
    fn poll(fds: *mut PollFd, nfds: NfdsT, timeout_ms: i32) -> i32;
}

const SIGKILL: i32 = 9;

/// The credential a spawned child presents to marion's own endpoint.
///
/// Not a secret and not checked by anything: the endpoint is marion's canned provider or its proxy,
/// which authenticates nothing. It exists because a harness can refuse to *start* over an absent
/// credential — see the `api_key` field in [`run_spawn`]'s `LaunchSpec` — and because codex's
/// generated config has always named a variable that had to hold something (`env_key =
/// "MARION_DUMMY_KEY"`).
pub const PLACEHOLDER_API_KEY: &str = "dummy";

/// `nfds_t`: `unsigned long` on Linux, `unsigned int` everywhere else marion runs.
#[cfg(target_os = "linux")]
type NfdsT = u64;
#[cfg(not(target_os = "linux"))]
type NfdsT = u32;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

const POLLIN: i16 = 0x0001;

/// How often an idle drain thread wakes to notice it has been told to stop. Also the worst-case
/// delay between `Drain::stop` and the thread exiting, which is what makes the join bounded.
const DRAIN_POLL_MS: i32 = 20;

/// How long a pipe that is still open *after the child has been reaped and the tree killed* is
/// given before the drain is abandoned. On every healthy path the write ends are already closed by
/// then and the drains finish in microseconds, so this is dead time only when something escaped.
pub(crate) const DRAIN_GRACE: StdDuration = StdDuration::from_secs(2);

/// A pipe drain that can be stopped while the pipe is still open.
///
/// The thread never blocks in `read` for longer than `DRAIN_POLL_MS`: it waits for readiness with
/// `poll`, which takes a timeout, and only then reads bytes it knows are there. So a stop request
/// is honoured promptly and the thread *exits* — it is not detached and left wedged. That matters
/// because `spawn` is called repeatedly by a long-lived supervisor: one leaked thread (and one
/// leaked fd, and its buffer) per timed-out spawn would be its own unbounded leak, traded for the
/// hang it fixed.
pub(crate) struct Drain {
    handle: thread::JoinHandle<(Vec<u8>, bool)>,
    stop: Arc<AtomicBool>,
}

impl Drain {
    pub(crate) fn start<R: Read + AsRawFd + Send + 'static>(pipe: R) -> Self {
        Self::start_with_lines(pipe, None)
    }

    /// [`Self::start`], and every **complete line** forwarded on `lines` as it lands — the live
    /// seam the `LaunchOnly` path otherwise lacks. The drain thread only forwards; whoever holds the
    /// receiver reads it on its own thread, so nothing a caller does with a line has to be `Send`.
    /// Bytes after the last newline are forwarded at EOF, so a stream whose final frame has no
    /// trailing newline is not read one frame short. The whole capture is still returned by
    /// [`Self::finish`]: the lines are a copy, not a diversion.
    pub(crate) fn start_with_lines<R: Read + AsRawFd + Send + 'static>(
        mut pipe: R,
        lines: Option<std::sync::mpsc::Sender<String>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let fd = pipe.as_raw_fd();
            let mut bytes = Vec::new();
            let mut buf = [0u8; 8192];
            // The start of the first byte not yet forwarded as part of a line.
            let mut forwarded = 0usize;
            loop {
                if flag.load(Ordering::Relaxed) {
                    // Abandoned with the pipe still open: what we have is a prefix.
                    return (bytes, false);
                }
                let Ok(readable) = poll_readable(fd) else {
                    return (bytes, false);
                };
                if !readable {
                    continue;
                }
                // Readable, hung up, or errored. Only `read` can tell the three apart, and with a
                // single reader it cannot block now.
                match pipe.read(&mut buf) {
                    Ok(0) => {
                        // EOF: every write end is closed.
                        forward_lines(lines.as_ref(), &bytes, &mut forwarded, true);
                        return (bytes, true);
                    }
                    Ok(n) => {
                        bytes.extend_from_slice(&buf[..n]);
                        forward_lines(lines.as_ref(), &bytes, &mut forwarded, false);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return (bytes, false),
                }
            }
        });
        Self { handle, stop }
    }

    /// Collect the drained bytes, waiting no later than `deadline` for a natural EOF.
    ///
    /// Returns `(bytes, complete)`; `complete` is false exactly when the pipe was still open at the
    /// deadline, i.e. when the capture is a prefix.
    pub(crate) fn finish(self, deadline: Instant) -> (Vec<u8>, bool) {
        while !self.handle.is_finished() && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(5));
        }
        self.stop.store(true, Ordering::Relaxed);
        // Bounded by one `poll` interval: the thread checks `stop` every `DRAIN_POLL_MS`.
        self.handle.join().unwrap_or((Vec::new(), false))
    }
}

/// Wait at most [`DRAIN_POLL_MS`] for `fd` to become readable, hung up or errored.
///
/// `Ok(true)` is any of those three — only `read` can tell them apart. `Ok(false)` is a poll that
/// timed out or was interrupted, which the caller treats alike: check `stop`, then ask again. `Err`
/// is a `poll` that failed for any other reason, on which the drain gives up with what it has.
fn poll_readable(fd: std::os::fd::RawFd) -> Result<bool, ()> {
    let mut pfd = PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    let ready = unsafe { poll(&mut pfd, 1, DRAIN_POLL_MS) };
    if ready < 0 {
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(());
    }
    Ok(ready > 0)
}

/// Forward every complete line in `bytes[*forwarded..]` on `lines`, advancing `forwarded` past
/// them. At EOF the bytes after the last newline are forwarded too, so a stream whose final frame
/// has no trailing newline is not read one frame short. A `None` sender forwards nothing.
fn forward_lines(
    lines: Option<&std::sync::mpsc::Sender<String>>,
    bytes: &[u8],
    forwarded: &mut usize,
    at_eof: bool,
) {
    let Some(tx) = lines else { return };
    while let Some(nl) = bytes[*forwarded..].iter().position(|b| *b == b'\n') {
        let line = &bytes[*forwarded..*forwarded + nl];
        let _ = tx.send(String::from_utf8_lossy(line).into_owned());
        *forwarded += nl + 1;
    }
    if at_eof && *forwarded < bytes.len() {
        let _ = tx.send(String::from_utf8_lossy(&bytes[*forwarded..]).into_owned());
        *forwarded = bytes.len();
    }
}

/// One `ps` row: a process, its parent, and its process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcRow {
    pid: i32,
    ppid: i32,
    pgid: i32,
}

fn parse_ps_rows(s: &str) -> Vec<ProcRow> {
    s.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let ppid = f.next()?.parse().ok()?;
            let pgid = f.next()?.parse().ok()?;
            Some(ProcRow { pid, ppid, pgid })
        })
        .collect()
}

/// A whole-system process snapshot. `ps` rather than a dependency: this workspace hand-rolls its
/// primitives (see `marion-core::encoding`'s civil-date arithmetic) and one `ps` sweep is all the
/// remedy S7 measured needs.
fn ps_rows() -> Vec<ProcRow> {
    let mut command = SysCommand::new("ps");
    command
        .args(["-axo", "pid=,ppid=,pgid="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut command)
        .and_then(std::process::Child::wait_with_output)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_ps_rows(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Every pid in `root`'s tree: `root` itself, the rest of `root`'s own group, and all of their
/// descendants. Step 1 of the two-step group kill, and the set the M1 criterion asserts over.
fn descendant_pids(rows: &[ProcRow], root: i32) -> Vec<i32> {
    let root_pgid = rows.iter().find(|r| r.pid == root).map(|r| r.pgid);
    let mut seen: Vec<i32> = vec![root];
    if let Some(g) = root_pgid {
        for r in rows.iter().filter(|r| r.pgid == g) {
            if !seen.contains(&r.pid) {
                seen.push(r.pid);
            }
        }
    }
    // Breadth-first over the pid→children relation. `seen` grows as we walk it.
    let mut i = 0;
    while i < seen.len() {
        let cur = seen[i];
        for r in rows.iter().filter(|r| r.ppid == cur) {
            if !seen.contains(&r.pid) {
                seen.push(r.pid);
            }
        }
        i += 1;
    }
    seen
}

/// Every distinct process group among the pids of `descendant_pids` — the set S7 measured as
/// sufficient to leave zero survivors.
///
/// `codex exec` calls `setsid` for each tool-call command, so those children sit in their own
/// session **and** their own process group: `killpg` on the group marion created reaches codex but
/// not them (`tests/fixtures/s7/README.md`). Their groups can only be found by ancestry, and only
/// while the tree is intact.
fn pgids_of(rows: &[ProcRow], pids: &[i32]) -> Vec<i32> {
    let mut pgids: Vec<i32> = Vec::new();
    for pid in pids {
        if let Some(g) = rows.iter().find(|r| r.pid == *pid).map(|r| r.pgid)
            && !pgids.contains(&g)
        {
            pgids.push(g);
        }
    }
    pgids
}

/// The pids enumerated in step 1 of the most recent expiry sweep in this process.
///
/// Read back by design §9's M1 criterion 6, whose assertion is over *marion's own* enumeration
/// rather than one the test reconstructs — a test that re-walked `ps` itself would be asserting
/// about a different set than the one the kill was aimed at. Exposing it here rather than through
/// `run_spawn`'s return or the contract keeps the tool surface and the persisted schema untouched:
/// the sweep is a process-wide event (it signals process *groups*), so a process-wide record of
/// the last one is the same shape as the thing it describes. Only the last sweep is kept, so a
/// long-lived supervisor accumulates nothing; concurrent expiries therefore leave only one behind.
static LAST_SWEEP: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

/// The descendant pids marion enumerated at the most recent timeout expiry. Empty before the
/// first one.
pub fn last_kill_sweep() -> Vec<i32> {
    LAST_SWEEP
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|e| e.into_inner().clone())
}

/// Filter a collected pgid set down to what is safe to `killpg`.
///
/// Three groups are never signalled, and a bug here is far worse than the leak this fixes:
/// - **0** means "every process in the *caller's* group" — it would kill marion and, if marion
///   inherited its shell's group, the user's shell with it;
/// - **1** is init/launchd's group;
/// - **marion's own group** is suicide by a longer route, and would take the supervisor's siblings
///   with it. The child is spawned with `process_group(0)`, so this can only fire if that failed.
///
/// Negative and other non-positive values are rejected for the same reason as 0: `kill(-pgid, …)`
/// turns the sign inside out and a bad value addresses something we never enumerated.
fn signal_targets(pgids: &[i32], own_pgid: i32) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    for g in pgids {
        if *g > 1 && *g != own_pgid && !out.contains(g) {
            out.push(*g);
        }
    }
    out
}

/// Kill the child's whole descendant tree at timeout expiry.
///
/// **The ordering is load-bearing.** The enumeration must complete *before* the first signal:
/// once the child dies its descendants reparent to pid 1 and no ancestry walk can find them
/// again (S7, design §11 item 18). SIGKILL with no SIGTERM grace: design §9 expires the child with
/// `killpg` and says `ControlPlane::shutdown` is "the *graceful* path and is deliberately not used
/// here"; §6.7's status derivation reads `TimedOut` off marion's own attributed kill, not off which
/// signal was used, so a grace period would buy nothing and would only widen the window in which a
/// runaway keeps running.
pub(crate) fn kill_process_tree(child_pid: i32) {
    let rows = ps_rows();
    let pids = descendant_pids(&rows, child_pid);
    if let Ok(mut last) = LAST_SWEEP.lock() {
        last.clone_from(&pids);
    }
    let mut pgids = pgids_of(&rows, &pids);
    // The child's own group, in case the `ps` sweep failed or raced its exit: `process_group(0)`
    // made the child its own group leader, so its pid is its pgid.
    if !pgids.contains(&child_pid) {
        pgids.push(child_pid);
    }
    for pgid in signal_targets(&pgids, unsafe { getpgrp() }) {
        // Negative pid addresses a process group.
        let _ = unsafe { kill(-pgid, SIGKILL) };
    }
}

/// Apply §6.7's two-step kill and wait until the addressed process is absent or a zombie.
///
/// The journal's confirmation means *observed dead*, not merely "SIGKILL was sent". A zombie is
/// dead for that purpose — it can run no code and its parent alone owns the remaining wait record
/// — while `kill(pid, 0)` would misclassify it as alive. `ps` supplies that distinction. The bound
/// is a safety refusal, not a grace period: SIGKILL has no graceful leg, and a caller that cannot
/// observe death leaves its already-durable intent unconfirmed for §7.2-style recovery.
pub(crate) fn kill_process_tree_and_wait(child_pid: i32) -> bool {
    kill_process_tree(child_pid);
    let deadline = Instant::now() + StdDuration::from_secs(5);
    while Instant::now() < deadline {
        let mut command = SysCommand::new("ps");
        command
            .args(["-o", "stat=", "-p", &child_pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let observation = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
            .spawn(&mut command)
            .and_then(std::process::Child::wait_with_output);
        match observation {
            Ok(output) => {
                let state = String::from_utf8_lossy(&output.stdout);
                let state = state.trim();
                if state.starts_with('Z') {
                    return true;
                }
                if state.is_empty() && output.stderr.is_empty() {
                    return true;
                }
                std::thread::yield_now();
            }
            // Failing to observe is not observing death. In particular, treating an unavailable
            // `ps` as an absent PID would append the confirmation whose claim this loop exists to
            // earn.
            Err(_) => std::thread::yield_now(),
        }
    }
    false
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
pub(crate) fn run_bounded_watched(
    command: &mut SysCommand,
    timeout: StdDuration,
    on_started: &dyn Fn(i32),
    on_line: Option<&dyn Fn(&str)>,
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
    on_line: Option<&dyn Fn(&str)>,
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
    let stdout_drain = Drain::start_with_lines(stdout, lines_tx);
    let stderr_drain = Drain::start(stderr);
    let deliver_lines = || {
        if let (Some(hook), Some(rx)) = (on_line, &lines_rx) {
            for line in rx.try_iter() {
                hook(&line);
            }
        }
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
    let (status, timed_out) = loop {
        deliver_lines();
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            kill_tree(child.id() as i32);
            break (child.wait()?, true);
        }
        thread::sleep(StdDuration::from_millis(10));
    };
    // The child is reaped; anything still holding a write end is an escapee. One deadline for both
    // drains, so the total wait is `DRAIN_GRACE`, not twice it.
    let drain_deadline = Instant::now() + DRAIN_GRACE;
    let (stdout, stdout_complete) = stdout_drain.finish(drain_deadline);
    let (stderr, stderr_complete) = stderr_drain.finish(drain_deadline);
    // Whatever landed between the last poll and the drain's end, including a final unterminated
    // line: the live view sees every line the capture does.
    deliver_lines();
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
fn persist_contract_then_record_exit(
    project_dir: &ProjectDir,
    agent_dir: &AgentDir,
    agent_id: &AgentId,
    contract: &TaskContract,
) -> Result<TaskContract, SpawnError> {
    let persisted = persist_then_cap(agent_dir, contract);
    if let Some(completion) = contract.completion.as_ref() {
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
    std::fs::create_dir_all(agent.contracts_dir())?;
    let path = agent.contract(&contract.task_id);
    let mut file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(&mut file, contract)?;
    file.write_all(b"\n")?;
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
fn resolve_model(
    req: &SpawnRequest,
    agent_type: &marion_core::agent_type::AgentType,
) -> Option<String> {
    req.model.clone().or_else(|| agent_type.model.clone())
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
/// A timed-out or failed probe is `"unknown"`, never an error: the version is a field in an audit
/// record, and losing a whole node's contract because a version string did not arrive would trade a
/// large truth for a small one.
fn harness_version(program: &str) -> String {
    let mut cmd = SysCommand::new(program);
    cmd.arg("--version");
    run_bounded(&mut cmd, HARNESS_VERSION_TIMEOUT)
        .ok()
        .filter(|o| !o.timed_out && o.code == Some(0))
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
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
}

/// The `LaunchOnly` child, **unchanged**: the prompt is already in argv, so there is nothing to
/// withhold and nothing to steer. codex, gemini and opencode all declare
/// `launch_only_with_protocol_events()` and all take this path; §6.1 step 8 asserts their MCP
/// readiness *post hoc* from their own streams instead.
fn launch_only_child(
    inv: &Invocation,
    auth: Auth,
    bound: StdDuration,
    on_started: &dyn Fn(i32),
    session: &crate::session_watch::SessionWatch<'_>,
) -> Result<ChildRun, SpawnError> {
    let mut cmd = SysCommand::new(&inv.program);
    cmd.args(&inv.args)
        .envs(inv.env.iter().cloned())
        .current_dir(&inv.cwd);
    if auth == Auth::Canned {
        cmd.env("MARION_DUMMY_KEY", PLACEHOLDER_API_KEY);
    }
    let on_line = |line: &str| session.observe_line(line);
    let output = run_bounded_watched(&mut cmd, bound, on_started, Some(&on_line))?;
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
    })
}

/// The **duplex child**: §6.1 step 8's gate, then the prompt as a `user` frame.
///
/// Driven by [`crate::duplex`], which is the same code `marion run` drives a duplex *root* with —
/// deliberately, because the gate is normative for any launcher driving Claude Code headlessly and
/// a second implementation of it here would be exactly the drift §9 warns about. What a child adds
/// is what a child *is*: a wall clock (its contract's `timeout_secs`), which a root is offered
/// none of.
///
/// **The `Blocked` budget is zero, and that is a decision — but not the one this comment used to
/// claim.** It said the child is compiled *without* `--permission-prompt-tool`, so §5.2's rule
/// applies and no `can_use_tool` ever reaches marion. **That premise is false.** There is one
/// compile path for a Claude Code node — `ClaudeCodeAdapter::compile` over `claude_code::SPEC` — and it
/// emits `--permission-prompt-tool stdio` **unconditionally**, guarded by its own test
/// (`permission_prompt_tool_is_set_or_can_use_tool_never_fires`). A duplex child is compiled with
/// `--tools ""` plus exactly one allowlisted verb (`report`), so **every other tool call it makes
/// asks** — the ask is reachable, not hypothetical, and its denial is journaled below.
///
/// The zero survives the correction, for the half of the original reasoning that was never about
/// the premise. §9's rule — block until the bound, then deny and let the node proceed — is written
/// for a **root**, whose `Blocked` budget stands outside any wall clock. A child has no such
/// separate budget: its only bound is the wall clock its contract records (`timeout_secs`), so time
/// spent holding an ask is spent out of the task's own, and can turn a run that should have been
/// `Ok` into `TimedOut` — a worse outcome, on the same evidence, than the denial that is coming
/// anyway. And the wait could not produce an answer if it were taken: M1 has no permission answerer,
/// and the wait is a blocking `sleep` inside the single reader loop, so there is no channel on which
/// one could arrive (§9). Zero therefore reaches §9's outcome — denied, node proceeds — at the
/// only cost that is honest to pay, and the denial is no longer silent: it leaves a
/// `PermissionDenied` record.
/// What a child's duplex launch needs beyond its compiled [`Invocation`] — the node, rather than the
/// program.
///
/// A struct because these travel together and always have: they are one node's identity, its
/// turn and its bounds, and every one of them is read straight into a [`DuplexSpec`] field. Passing
/// them positionally was already at the edge of readable and went over it when recording landed.
struct ChildDuplex<'a> {
    agent_id: &'a AgentId,
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
}

fn duplex_child(
    inv: &Invocation,
    auth: Auth,
    child: ChildDuplex<'_>,
) -> Result<ChildRun, SpawnError> {
    let ChildDuplex {
        agent_id,
        ready_file,
        prompt,
        bound,
        depth,
        events,
        on_started,
        session,
    } = child;
    let mut cmd = SysCommand::new(&inv.program);
    cmd.args(&inv.args)
        .envs(inv.env.iter().cloned())
        .current_dir(&inv.cwd);
    if auth == Auth::Canned {
        cmd.env("MARION_DUMMY_KEY", PLACEHOLDER_API_KEY);
    }
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
            blocked_bound: StdDuration::ZERO,
            // The child's own depth, not the caller's — the same value its bridge is told, so the
            // driver and the bridge answer the same question about the same node.
            depth,
            wall_clock: Some(bound),
            sink,
            on_started: Some(on_started),
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
/// The reason is generic because the error is gone by the time `Drop` runs — a `?` has already
/// moved it into the caller's `Err`. That is the honest trade: the journal records *that* marion
/// abandoned the spawn (which is what replay needs, and what keeps the node out of `unresolved()`),
/// and the error itself reaches the caller, who is the one who can act on it.
struct AbortOnDrop<'a> {
    project: &'a ProjectDir,
    agent_id: AgentId,
    armed: bool,
}

impl Drop for AbortOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            crate::journal::record(
                self.project,
                RecordKind::SpawnAborted(SpawnAborted {
                    agent_id: self.agent_id.clone(),
                    reason: "marion left the spawn path before the child reached a terminal \
                             record; the error was returned to the caller (§7.2: a node marion \
                             decided the fate of is never one marion lost)"
                        .into(),
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
    fn identified(&self, agent_id: &AgentId) -> Option<String>;
    /// A process exists. `pid` is a signal target, not an identity — see the `Spawned` writer below
    /// and `restart.rs` for why those are different claims.
    fn started(&self, agent_id: &AgentId, pid: i32);
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
    fn identified(&self, _: &AgentId) -> Option<String> {
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

/// §6.1's spawn, with the node's owner told about it as it happens. See [`SpawnObserver`].
pub fn run_spawn_watched(
    env: &Env,
    req: &SpawnRequest,
    task_id: &TaskId,
    caller: &Caller,
    observer: &dyn SpawnObserver,
) -> Result<TaskContract, SpawnError> {
    let agent_type = builtin(&req.agent_type)
        .ok_or_else(|| SpawnError::UnknownAgentType(req.agent_type.clone()))?;
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
    crate::journal::record(
        &env.project_dir,
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: agent_id.clone(),
            // §3.1's bound for *this* child, from the one clamp above — so `marion tree` shows the
            // clock the node is running under rather than its agent type's default.
            timeout_secs: Some(bound.as_secs()),
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
    );
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
    };
    let agent_dir = env.project_dir.agent(&agent_id);
    let ch = agent_dir.config_dir();
    std::fs::create_dir_all(&ch)?;
    // Everything that can be refused is refused before this line — see [`select_workspace`] for
    // why a worktree is the first irreversible thing a spawn does.
    let (workspace, base, cwd_claim) =
        select_workspace(req, &agent_type, &agent_dir, task_id, &agent_id)?;
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
    let launch = child_launch_spec(env, req, &agent_type, adapter.as_ref(), path, &wt, &ch);
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
    write_config_documents(adapter.config_files(&launch, &ctx)?)?;
    let inv = adapter.compile(&launch, &ctx)?;
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
    let mut events = crate::events::EventSink::open(
        &agent_dir,
        &agent_id,
        adapter.harness(),
        init_request_id(&agent_id),
    );
    // The opening bookend, before the process exists — the stream begins where marion started
    // watching, not where the node first spoke, so a node that says nothing at all is still
    // distinguishable from one that was never recorded.
    if let Some(es) = &events {
        es.lifecycle(marion_core::event::Lifecycle::Opened);
    }
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
    .in_workspace(Some(workspace.clone()));
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
    let version = harness_version(&inv.program);
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
    let announce_started = |pid: i32| {
        let appended = crate::journal::append(
            &env.project_dir,
            child_spawned_record(&agent_id, &version, inv.model.as_deref(), pid),
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
    let run = match path {
        // **A contracted child does not get a pane, and the refusal is the design rather than a
        // gap.** A child is defined by §9's `TaskContract`: it is spawned to do a task and to
        // `report`, under the wall clock its contract records. A pane node is a TUI, which takes
        // no turn at all until a human presses return — so a contracted child in a pane is a task
        // that can only ever time out, and the contract would record that as the child's failure.
        // A pane belongs to a **root**: a node an operator started and is watching.
        LaunchPath::Terminal => {
            return Err(SpawnError::UnsupportedChildSurface(agent_type.harness));
        }
        LaunchPath::LaunchOnly => {
            launch_only_child(&inv, env.auth, bound, &announce_started, &session)
        }
        // **The fifth harness, as a child.** §9's M5 clause 1 asks for ACP agents running *as
        // children through the single ACP adapter*, and until this arm existed the only thing that
        // had ever driven one was `marion doctor --adapter` — a probe, which has no worktree, no
        // contract, no journal and no bridge, so it could not answer the clause however green it
        // was. That is the sixth "fully tested in isolation and unreachable from any binary" of
        // the day, and this arm is the fix.
        //
        // The declaration is asked of the adapter here rather than rebuilt in the driver, so the
        // frame marion sends and the frame `McpRoute::Session` verified are the same object.
        LaunchPath::Acp => crate::acp_child::run_acp_child(crate::acp_child::AcpChildSpec {
            inv: &inv,
            session_declaration: adapter.session_declaration(&launch, &ctx)?,
            prompt: &req.prompt,
            bound,
            on_started: &announce_started,
        })
        .map(|r| ChildRun {
            stdout: r.stdout,
            stderr: r.stderr,
            exit: r.exit,
            capture_truncated: r.capture_truncated,
            // ACP has a permission surface (`session/request_permission`) and marion answers it
            // permissively in the driver, so nothing is denied on this path yet. An empty vector
            // here is therefore "marion refused nothing", which is true, and **not** the
            // `LaunchOnly` arm's "no ask could reach marion at all". When a policy lands, this is
            // the field it fills; §11 item 24's two axes are why the distinction is written down
            // rather than left to look identical.
            denied_permissions: vec![],
        })
        .map_err(SpawnError::from),
        LaunchPath::Duplex => duplex_child(
            &inv,
            env.auth,
            ChildDuplex {
                agent_id: &agent_id,
                ready_file: ready_file
                    .as_deref()
                    .expect("the duplex path always mints a marker"),
                prompt: &req.prompt,
                bound,
                depth: ctx.depth,
                events: events.as_ref(),
                on_started: &announce_started,
                session: &session,
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
    let run = run?;
    record_capture_after_the_fact(path, events.as_mut(), &run.stdout);
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
        "denied immediately: a child's Blocked bound is zero, because its only bound is the wall \
         clock its contract records and M1 has no answerer to spend it on",
    );
    // §6.1 step 9, through the same seam as step 5. This was codex-JSONL-specific until now, so a
    // gemini or opencode child's report was unreadable and its contract said `Unreported` about a
    // run that had reported — the §12 silent-failure shape, one layer down from the dispatch bug.
    let outcome = ChildOutcome::from_stream(
        adapter.parse_stream(&run.stdout, run.exit),
        run.exit,
        run.stderr.clone(),
    );
    // **§7.6's descendant gate, at the only moment it can run**: the process has stopped and
    // nothing terminal is written yet — no `Exited`, no contract, no closing bookend. A voluntary,
    // unreported stop with a live descendant is *held* here, on the remainder of `bound`, and the
    // verdict is applied to the contract below once `build_contract` has assembled it.
    let gated = crate::descendant_gate::gate(
        observer,
        &agent_id,
        &env.project_dir,
        &outcome,
        spawned_at.0,
        bound,
    );

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
    // **After `changed_paths` and the diff, never before.** Verification writes into the worktree
    // — a `cargo test` leaves a `target/`, a formatter rewrites files — and §6.7's diff is the
    // child's work, so the measurement is taken first and the commands run over the sealed
    // result. Skipped for a child that was killed: its workspace is whatever the kill left, and
    // evidence gathered over it would be judged against work that never finished. The request
    // itself is still recorded (`verification_commands`), so the contract says what was asked.
    let verification = verification_commands(&req.verification, &wt);
    let evidence = if outcome.timed_out || outcome.signal.is_some() {
        vec![]
    } else {
        run_verification(&verification)
    };
    let mut contract = build_contract(
        task_id.clone(),
        AgentId(caller.agent_id.clone()),
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
    // Read off the **adapter**, not off `agent_type`: the contract is §6.7's audit record, so the
    // harness it names must be the one that actually produced the work, never the one that was
    // asked for. The two agree today precisely because the dispatch above reads the same field —
    // sourcing it here makes that an invariant the code enforces rather than one a reader has to
    // check, and it is the reason a divergence between the two could never again be silent.
    contract.child.harness = adapter.harness();
    contract.child.version = version;
    // Read off the **compiled invocation** for the same reason, one step further: §3.1 makes the
    // marion-name → harness-name mapping the adapter's, and §6.7's `allowed_tools` records "the
    // compiled, harness-native constraint". So this records what went on the wire — which is
    // `None` for codex, whose `exec` surface carries no model argument, even when the request or
    // the agent type named one.
    contract.child.model = inv.model.clone();
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
    // §7.6's flags, from the gate that ran above — `reported_early`, `held_to_timeout`,
    // `died_before_gate` and the live set — written onto the completion before it reaches disk.
    gated.apply(&mut contract);
    let returned = persist_contract_and_close_stream(
        env,
        &agent_dir,
        &agent_id,
        &contract,
        events.as_ref(),
        &req.agent_type,
    )?;
    // §4.3: the journal records **that a contract exists and how it ended**, never its contents —
    // the file is the contract (§6.7), and copying it here would be a second source of truth.
    // Written after `persist_then_cap` returns, so the record cannot claim a file that was never
    // written; the node is `agent_id` (the child the contract is about) and `requester` is the
    // caller, which is §6.7's own distinction, kept.
    crate::journal::record(
        &env.project_dir,
        RecordKind::ContractPersisted(ContractPersisted {
            agent_id: agent_id.clone(),
            task_id: task_id.clone(),
            requester: AgentId(caller.agent_id.clone()),
            status: contract.completion.as_ref().map(|c| c.status),
        }),
    );
    // The intent is resolved: `Spawned` and `Exited` are on the record above, so the abort this
    // guard would otherwise write would contradict them.
    resolution.armed = false;
    cleanup(&req.repo, &wt);
    Ok(returned)
}

/// The scope a child asked for, in §5.4's vocabulary: an empty `writable_scope` is the whole
/// workspace, not nothing.
fn requested_scope(req: &SpawnRequest) -> Vec<Glob> {
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
    agent_dir: &AgentDir,
    task_id: &TaskId,
    agent_id: &AgentId,
) -> Result<(Workspace, Option<Oid>, crate::spawn::CwdClaim), SpawnError> {
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
                if agent_type.writes_files() && !req.allow_concurrent_writes =>
            {
                crate::spawn::CwdClaim::claim(path, agent_id)?
            }
            _ => crate::spawn::CwdClaim::none(),
        };
        return Ok((r.workspace.clone(), base, claim));
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
            std::fs::create_dir_all(wt.parent().expect("agent worktree has a parent"))?;
            let branch = format!("marion/{}", task_id.0);
            let base = make_worktree(&req.repo, &wt, &branch)?;
            Ok((
                Workspace::Worktree { path: wt, branch },
                Some(base),
                crate::spawn::CwdClaim::none(),
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
            let claim = if agent_type.writes_files() && !req.allow_concurrent_writes {
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
            ))
        }
    }
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
        // §9 gives a child a `TaskContract`, and a pane node takes no turn until a human presses
        // return — so there is no readiness gate to hold, and see `run_spawn_watched`'s launch arm
        // for why a contracted child does not get one at all.
        LaunchPath::LaunchOnly | LaunchPath::Terminal => None,
    }
}

/// The child's launch, in the neutral vocabulary the adapter compiles from. Every field is either
/// read off the resolved agent type or decided by the launch path — never by a harness name.
fn child_launch_spec(
    env: &Env,
    req: &SpawnRequest,
    agent_type: &AgentType,
    adapter: &dyn marion_harness::HarnessAdapter,
    path: LaunchPath,
    wt: &Path,
    ch: &Path,
) -> LaunchSpec {
    LaunchSpec {
        cwd: wt.to_path_buf(),
        // Was a hard `None` until now, which is why a gemini or opencode agent type could be named,
        // resolved and dispatched — and then refused at `compile`, since both adapters make an
        // explicit model a MUST. See `resolve_model`.
        model: resolve_model(req, agent_type),
        // §6.1 step 8: on a typed control plane the prompt is a frame written **after** the
        // readiness gate, so nothing is compiled into argv and the adapter is told so by the empty
        // string — the neutral vocabulary's own signal for "written after launch".
        prompt: match path {
            // ACP for the same reason, one protocol over: the prompt is a `session/prompt` frame
            // and reaches argv on no ACP agent. `AcpAdapter::compile` ignores this field entirely,
            // and passing `req.prompt` here would put the task text in the audit record's argv
            // where the launch never put it.
            LaunchPath::Duplex | LaunchPath::Acp => String::new(),
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
        tools: agent_type.tools.clone(),
        // The **permission** axis (§3.1), in marion's vocabulary translated by the adapter that is
        // about to run. A child's one load-bearing call is `report`; on Claude Code an unlisted
        // tool is auto-denied *in process*, and on a `LaunchOnly` child there is no control plane
        // for the denial to be asked about — so an empty list here is a run that completes having
        // reported nothing, with no error anywhere. The three harnesses whose adapters read no
        // permission list are unaffected: they ignore it, exactly as they did when it was empty.
        //
        // **marion's own verbs only, and the declared tools are unioned in by the adapter.** §3.1's
        // table compiles this axis from *"the same list, plus marion's own `mcp__marion__*`"*, and
        // doing the union at the one place both axes are compiled is what makes them unable to
        // disagree. Appending here instead would grant permission without availability — the mirror
        // of item 24's dead end, and just as silent.
        allowed_tools: vec![adapter.marion_tool_name("report")],
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
        // `env_key = "MARION_DUMMY_KEY"` and the run below pushes it — so this generalises an
        // existing decision rather than making a new one.
        //
        // Under `Auth::Inherited` there is nothing to placehold: the endpoint is the vendor's, the
        // credential is the operator's already-established login, and a placeholder pushed beside it
        // would be a second credential competing with the real one.
        api_key: match env.auth {
            Auth::Canned => Some(PLACEHOLDER_API_KEY.to_string()),
            Auth::Inherited => None,
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
        extra: Extras {
            acp_agent: agent_type.acp_agent.clone(),
            ..Extras::default()
        },
    }
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
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, contents)?;
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
) -> RecordKind {
    RecordKind::Spawned(Spawned {
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
    })
}

/// **The `LaunchOnly` half of §7.3.3's wiring, and the asymmetry is the harness's, not marion's.**
///
/// codex, gemini and opencode have no live seam at all — the prompt rides argv and `run_bounded`
/// drains the pipe whole — so their stream can only be recovered from the capture, after the fact,
/// and every event it produces is honestly `observed_live: false`. Never both: the duplex path
/// already recorded these frames live, and recording them again here would be the duplicate
/// §7.3.3's seam is stated in ordinals to prevent.
///
/// ACP is here and not on the live side: the driver owns the frame loop for the whole turn and
/// hands the transcript back at the end, so every event recovered from it is honestly
/// `observed_live: false`. It is a *typed* plane whose events are nonetheless after the fact, which
/// is why this branches on where the frames came from rather than on `has_typed_control_plane`.
fn record_capture_after_the_fact(
    path: LaunchPath,
    events: Option<&mut crate::events::EventSink>,
    stdout: &str,
) {
    if matches!(path, LaunchPath::LaunchOnly | LaunchPath::Acp)
        && let Some(es) = events
    {
        es.record_capture(stdout);
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
) -> Result<TaskContract, SpawnError> {
    let persisted =
        persist_contract_then_record_exit(&env.project_dir, agent_dir, agent_id, contract);
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
fn cleanup(repo: &Path, wt: &Path) {
    let _serialized = crate::spawn::repo_write_guard();
    let mut command = SysCommand::new("git");
    command
        .current_dir(repo)
        .args(["worktree", "remove", "--force", &wt.to_string_lossy()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let _ = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut command)
        .and_then(std::process::Child::wait_with_output);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::ChildOutcome;
    use marion_core::harness::Harness;
    use marion_harness::adapter_for;
    use marion_testsupport::{Scratch, scratch};
    use std::sync::Mutex;

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
        let agent_dir = ProjectDir::new(&dir.join("state"), &repo).agent(&agent_id);
        let task_id = TaskId("t-1".into());
        let mut req = SpawnRequest {
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
        };
        assert!(
            matches!(
                select_workspace(&req, &agent_type, &agent_dir, &task_id, &agent_id),
                Err(SpawnError::NotAGitRepo { .. })
            ),
            "the fixture's repo is deliberately not a repository, so a fresh spawn cannot cut a \
             worktree in it — which is what makes the resume below unambiguous"
        );

        req.resume = Some(ChildResume {
            agent_id: agent_id.clone(),
            session: "sess-1".into(),
            workspace: recorded.clone(),
        });
        let (workspace, _base, _claim) =
            select_workspace(&req, &agent_type, &agent_dir, &task_id, &agent_id)
                .expect("the recorded tree needs no repository question asked of it");
        assert_eq!(
            workspace, recorded,
            "the relaunch runs where the journal says the session was created"
        );
        assert!(
            !agent_dir.worktree().exists(),
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

        persist_contract_then_record_exit(&project, &agent_dir, &agent_id, &contract)
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

    /// `kill(pid, 0)`: `ESRCH` is the only answer that means *gone*. `EPERM` means the process
    /// exists and is not ours, which for this fix would still be a survivor.
    fn alive(pid: i32) -> bool {
        if unsafe { kill(pid, 0) } == 0 {
            return true;
        }
        const ESRCH: i32 = 3;
        std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
    }

    /// The tree S7 measured, verbatim from `tests/fixtures/s7/README.md`: codex in marion's group,
    /// the code-mode host in its own, and the tool-call child a session leader with its own group.
    fn s7_tree() -> Vec<ProcRow> {
        parse_ps_rows(
            "55056 55049 55049\n\
             55091 55056 55091\n\
             55296 55091 55296\n\
             55386 55091 55386\n\
             55395 55386 55386\n\
             99999     1 99999\n",
        )
    }

    #[test]
    fn ps_columns_are_read_as_pid_ppid_pgid() {
        assert_eq!(
            parse_ps_rows("  55091 55056 55091 \nnot a row\n"),
            vec![ProcRow {
                pid: 55091,
                ppid: 55056,
                pgid: 55091
            }]
        );
    }

    /// The pgid set of a pid set, which is how `kill_process_tree` composes the two steps.
    fn descendant_pgids(rows: &[ProcRow], root: i32) -> Vec<i32> {
        pgids_of(rows, &descendant_pids(rows, root))
    }

    #[test]
    fn every_pgid_a_setsid_tool_call_child_escaped_into_is_collected() {
        // The measured remedy set. 55296 and 55386 are outside codex's group and are exactly what a
        // single killpg on 55091 leaves running.
        assert_eq!(
            descendant_pgids(&s7_tree(), 55091),
            vec![55091, 55296, 55386]
        );
    }

    #[test]
    fn step_one_enumerates_the_pids_not_just_their_groups() {
        // The set design §9's M1 criterion 6 asserts `ESRCH` over: codex, the code-mode host, the
        // setsid'd tool-call child and its own child — every process the two-step kill must reach.
        assert_eq!(
            descendant_pids(&s7_tree(), 55091),
            vec![55091, 55296, 55386, 55395]
        );
    }

    #[test]
    fn an_unrelated_process_group_is_never_collected() {
        // 99999 is nobody's descendant. Collecting it would make the timeout kill a system hazard.
        assert!(!descendant_pgids(&s7_tree(), 55091).contains(&99999));
    }

    #[test]
    fn a_pgid_of_zero_is_never_signalled_because_it_means_the_callers_own_group() {
        // kill(-0, SIGKILL) is kill(0, SIGKILL): marion itself, and the user's shell with it.
        assert!(signal_targets(&[0], 4242).is_empty());
    }

    #[test]
    fn pgid_one_is_never_signalled() {
        assert!(signal_targets(&[1], 4242).is_empty());
    }

    #[test]
    fn marions_own_process_group_is_never_signalled() {
        assert_eq!(signal_targets(&[4242, 55386], 4242), vec![55386]);
    }

    #[test]
    fn negative_and_duplicate_pgids_are_dropped_before_signalling() {
        assert_eq!(
            signal_targets(&[-1, -55386, 55386, 55386], 4242),
            vec![55386]
        );
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
            fn identified(&self, agent_id: &AgentId) -> Option<String> {
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
                Some(TOKEN.to_string())
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
            extra: Extras::default(),
        }
    }

    /// **The child's two axes, composed the way `run_spawn` composes them.**
    ///
    /// `run_spawn` sets `tools` from the resolved agent type and `allowed_tools` to marion's
    /// `report` alone, and leaves the union to the adapter. This drives that exact pair through
    /// the exact adapter the dispatch above selects, so the composition is checked without a
    /// process: a `claude-impl` child gets `Write` on **both** flags and marion's own verb is not
    /// lost from the permission axis in the process.
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
            // Verbatim from `run_spawn`.
            tools: t.tools.clone(),
            allowed_tools: vec![adapter.marion_tool_name("report")],
            // A duplex child's prompt is a frame written after launch, so argv carries none.
            prompt: String::new(),
            ..launch_spec(None)
        };
        let args = adapter.compile(&spec, &launch_ctx()).unwrap().args;
        let after = |flag: &str| -> String {
            let i = args.iter().position(|a| a == flag).expect("flag present");
            args[i + 1].clone()
        };
        assert_eq!(after("--tools"), "Read,Write", "availability");
        assert_eq!(
            after("--allowedTools"),
            "mcp__marion__report,Read,Write",
            "permission carries marion's verb AND the whole declaration; either alone is a dead \
             end, and a verb that reached availability and not permission is item 22's"
        );
        // The orchestrator type through the same path: unchanged, which is what keeps this
        // additive.
        let orchestrator = LaunchSpec {
            tools: builtin("claude").unwrap().tools,
            ..spec
        };
        let args = adapter.compile(&orchestrator, &launch_ctx()).unwrap().args;
        let i = args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(
            args[i + 1],
            "",
            "a `claude` child is read-only as it always was"
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
            let model = resolve_model(&request(name, None), &t);
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
            resolve_model(&request("gemini", Some("gemini-2.5-pro")), &gemini).as_deref(),
            Some("gemini-2.5-pro"),
        );
        assert_eq!(
            resolve_model(&request("gemini", None), &gemini).as_deref(),
            Some("gemini-2.5-flash"),
        );
        for name in ["codex-impl", "claude"] {
            assert_eq!(
                resolve_model(&request(name, None), &builtin(name).unwrap()),
                None,
                "{name}: a default here would change an argv that is measured, for nothing"
            );
        }
    }

    /// **The contract records the wire, not the ask** — the same rule that made `child.harness`
    /// come from the adapter. A caller can name a model for a codex child; `codex exec` carries
    /// none, so the contract must not claim one.
    #[test]
    fn a_model_asked_for_on_a_harness_that_takes_none_is_never_recorded_as_used() {
        let t = builtin("codex-impl").unwrap();
        let asked = resolve_model(&request("codex-impl", Some("gpt-5.6-sol")), &t);
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
            config.contains(r#"MARION_AGENT_TYPE = "codex-impl""#),
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
