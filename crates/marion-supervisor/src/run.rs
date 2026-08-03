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

use marion_core::agent_type::builtin;
use marion_core::cap::cap_for_return;
use marion_core::contract::*;
use marion_core::encoding::{Duration, SystemTime};
use marion_core::ids::{RAND_BYTES, new_agent_id};
use marion_core::paths::{AgentDir, ProjectDir};
use marion_core::scope::check_spawn_scope;
use marion_harness::{
    ChildExit, Extras, Invocation, LaunchSpec, McpDeclaration, SpawnCtx, adapter_for,
};

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

pub struct SpawnRequest {
    pub agent_type: String,
    pub prompt: String,
    pub acceptance_criteria: Vec<String>,
    pub writable_scope: Vec<String>,
    pub timeout_secs: u64,
    /// The model to run the child on, in marion's request vocabulary. **Optional, with the agent
    /// type's own `model` key as the default** (§3.1) — see [`resolve_model`].
    pub model: Option<String>,
}

pub struct Env {
    pub repo: PathBuf,
    pub project_dir: ProjectDir,
    pub bridge: PathBuf,
    pub base_url: String,
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
    pub(crate) fn start<R: Read + AsRawFd + Send + 'static>(mut pipe: R) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let fd = pipe.as_raw_fd();
            let mut bytes = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                if flag.load(Ordering::Relaxed) {
                    // Abandoned with the pipe still open: what we have is a prefix.
                    return (bytes, false);
                }
                let mut pfd = PollFd {
                    fd,
                    events: POLLIN,
                    revents: 0,
                };
                let ready = unsafe { poll(&mut pfd, 1, DRAIN_POLL_MS) };
                if ready < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return (bytes, false);
                }
                if ready == 0 {
                    continue;
                }
                // Readable, hung up, or errored. Only `read` can tell the three apart, and with a
                // single reader it cannot block now.
                match pipe.read(&mut buf) {
                    Ok(0) => return (bytes, true), // EOF: every write end is closed.
                    Ok(n) => bytes.extend_from_slice(&buf[..n]),
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
    SysCommand::new("ps")
        .args(["-axo", "pid=,ppid=,pgid="])
        .output()
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

/// Run `command` to completion or to `timeout`, whichever comes first, killing its whole
/// descendant tree on expiry. Public because the end-to-end test bounds a real `marion run` with
/// it: a test that leaked a `claude`, a `codex` and their tool-call grandchildren would be the
/// very failure S7 exists for, and this is the one implementation in the workspace that has been
/// measured to leave no survivors.
pub fn run_bounded(
    command: &mut SysCommand,
    timeout: StdDuration,
) -> Result<CommandOutput, SpawnError> {
    run_bounded_with(command, timeout, kill_process_tree)
}

/// `run_bounded` with the expiry kill injected, so tests can run the path where the sweep *fails*
/// to reach an escapee — the case whose liveness must not depend on the sweep.
fn run_bounded_with(
    command: &mut SysCommand,
    timeout: StdDuration,
    kill_tree: fn(i32),
) -> Result<CommandOutput, SpawnError> {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let stdout_drain = Drain::start(stdout);
    let stderr_drain = Drain::start(stderr);

    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
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

/// Fresh entropy for an id. Shared with `root`, which mints the root's `AgentId` the same way.
pub fn entropy() -> std::io::Result<[u8; RAND_BYTES]> {
    let mut bytes = [0; RAND_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Wall-clock milliseconds, the timestamp half of a UUIDv7.
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn persist_then_cap(agent: &AgentDir, contract: &TaskContract) -> Result<TaskContract, SpawnError> {
    std::fs::create_dir_all(agent.contracts_dir())?;
    let path = agent.contract(&contract.task_id);
    let mut file = std::fs::File::create(path)?;
    serde_json::to_writer_pretty(&mut file, contract)?;
    file.write_all(b"\n")?;
    Ok(cap_for_return(contract.clone()))
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

fn harness_version(program: &str) -> String {
    SysCommand::new(program)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
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
}

/// The `LaunchOnly` child, **unchanged**: the prompt is already in argv, so there is nothing to
/// withhold and nothing to steer. codex, gemini and opencode all declare
/// `launch_only_with_protocol_events()` and all take this path; §6.1 step 8 asserts their MCP
/// readiness *post hoc* from their own streams instead.
fn launch_only_child(inv: &Invocation, bound: StdDuration) -> Result<ChildRun, SpawnError> {
    let output = run_bounded(
        SysCommand::new(&inv.program)
            .args(&inv.args)
            .envs(inv.env.iter().cloned())
            .env("MARION_DUMMY_KEY", PLACEHOLDER_API_KEY)
            .current_dir(&inv.cwd),
        bound,
    )?;
    Ok(ChildRun {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit: ChildExit {
            code: output.code,
            signal: output.signal,
            timed_out: output.timed_out,
        },
        capture_truncated: output.capture_truncated,
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
/// **The `Blocked` budget is zero, and that is a decision.** The child is compiled with
/// `--allowedTools` carrying marion's verbs and **without** `--permission-prompt-tool`, so §5.2's
/// rule applies: a non-allowlisted call is auto-denied *in process* and no `can_use_tool` ever
/// reaches marion. §9's rule — block until the bound, then deny and let the node proceed — is
/// written for a **root**, whose bound is a per-episode `Blocked` budget standing outside any wall
/// clock. A child has no such separate budget: its only bound is the wall clock its contract
/// records, so holding an unanswerable ask would spend the task's own time and could turn a run
/// that should have been `Ok` into `TimedOut`. Since the child's tools are `--tools ""` plus
/// marion's allowlisted verbs, the only call that *could* ask is one marion means to refuse, and a
/// fast in-process denial reaches §9's outcome — denied, node proceeds — without a bound to burn.
/// The zero here is therefore the answer to a frame that cannot arrive, kept only so a future
/// harness that does send one is denied promptly rather than silently held.
fn duplex_child(
    inv: &Invocation,
    agent_id: &AgentId,
    ready_file: &Path,
    prompt: &str,
    bound: StdDuration,
) -> Result<ChildRun, SpawnError> {
    let out = duplex::run_duplex(
        SysCommand::new(&inv.program)
            .args(&inv.args)
            .envs(inv.env.iter().cloned())
            .env("MARION_DUMMY_KEY", PLACEHOLDER_API_KEY)
            .current_dir(&inv.cwd),
        &DuplexSpec {
            ready_file,
            prompt,
            init_id: format!("marion-init-{}", agent_id.0),
            mcp_ready_timeout: CHILD_MCP_READY_TIMEOUT.min(bound),
            blocked_bound: StdDuration::ZERO,
            wall_clock: Some(bound),
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
    })
}

pub fn run_spawn(
    env: &Env,
    req: &SpawnRequest,
    task_id: &TaskId,
    requester: &str,
) -> Result<TaskContract, SpawnError> {
    let agent_type = builtin(&req.agent_type)
        .ok_or_else(|| SpawnError::UnknownAgentType(req.agent_type.clone()))?;
    let requested: Vec<Glob> = if req.writable_scope.is_empty() {
        vec![Glob("**".into())]
    } else {
        req.writable_scope.iter().cloned().map(Glob).collect()
    };
    check_spawn_scope(&agent_type.scope_ceiling, &requested)?;

    let spawned_at = SystemTime(std::time::SystemTime::now());
    let agent_id = new_agent_id(unix_millis(), entropy()?);
    let agent_dir = env.project_dir.agent(&agent_id);
    let wt = agent_dir.worktree();
    let ch = agent_dir.config_dir();
    std::fs::create_dir_all(&ch)?;
    std::fs::create_dir_all(wt.parent().expect("agent worktree has a parent"))?;
    let branch = format!("marion/{}", task_id.0);
    let base = make_worktree(&env.repo, &wt, &branch)?;

    // §6.1 step 5, through the seam, **dispatched on the agent type's harness**. This was a
    // constant until now, which meant a `claude` agent type wrote a Codex config, exec'd `codex`,
    // and still recorded `"claude-code"` in the contract below — §6.7's audit record asserting
    // something that never happened.
    //
    // A harness marion can *name* but not yet *run* stops here, as a typed
    // `HarnessError::Unimplemented` naming it. There is deliberately no fallback: silently running
    // some other harness is the failure mode §12's correction rows keep recording, and a loud
    // refusal is always the cheaper one to diagnose.
    let adapter = adapter_for(agent_type.harness)?;
    // §3.4, the same derivation `marion run` uses for a root: **branch on the surfaces, never on a
    // harness name.** Until this branch existed `run_spawn` drove every child as `LaunchOnly` —
    // correct for the three harnesses that declare it, and the reason a `claude` child took turn
    // one with `"tools":[]` and exited 0 having called nothing.
    let path = launch_path(&adapter.surfaces())
        .ok_or(SpawnError::UnsupportedChildSurface(agent_type.harness))?;
    // Not one of §4.3's normative files: marion's own start-up handshake with a process it did not
    // spawn. Only the duplex path has a frame to withhold, so only it has a marker to wait on.
    let ready_file = match path {
        LaunchPath::Duplex => {
            let f = agent_dir.path().join("mcp-ready");
            let _ = std::fs::remove_file(&f);
            Some(f)
        }
        LaunchPath::LaunchOnly => None,
    };
    let launch = LaunchSpec {
        cwd: wt.clone(),
        // Was a hard `None` until now, which is why a gemini or opencode agent type could be named,
        // resolved and dispatched — and then refused at `compile`, since both adapters make an
        // explicit model a MUST. See `resolve_model`.
        model: resolve_model(req, &agent_type),
        // §6.1 step 8: on a typed control plane the prompt is a frame written **after** the
        // readiness gate, so nothing is compiled into argv and the adapter is told so by the empty
        // string — the neutral vocabulary's own signal for "written after launch".
        prompt: match path {
            LaunchPath::Duplex => String::new(),
            LaunchPath::LaunchOnly => req.prompt.clone(),
        },
        // The **permission** axis (§3.1), in marion's vocabulary translated by the adapter that is
        // about to run. A child's one load-bearing call is `report`; on Claude Code an unlisted
        // tool is auto-denied *in process*, and on a `LaunchOnly` child there is no control plane
        // for the denial to be asked about — so an empty list here is a run that completes having
        // reported nothing, with no error anywhere. The three harnesses whose adapters read no
        // permission list are unaffected: they ignore it, exactly as they did when it was empty.
        allowed_tools: vec![adapter.marion_tool_name("report")],
        mcp: McpDeclaration::Marion,
        base_url: Some(env.base_url.clone()),
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
        api_key: Some(PLACEHOLDER_API_KEY.to_string()),
        config_dir: ch.clone(),
        extra: Extras::default(),
    };
    let ctx = SpawnCtx {
        agent_id: agent_id.clone(),
        // `Some` only on the duplex path. On a `LaunchOnly` child the prompt rides argv, so there
        // is no frame to withhold and no marker to wait on (§6.1 step 8); its MCP readiness is
        // asserted post hoc from its JSONL stream.
        ready_file: ready_file.clone(),
        repo: env.repo.clone(),
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
    for (path, contents) in adapter.config_files(&launch, &ctx)? {
        // The adapter decides *what and where*; the caller writes. "Where" is not always directly
        // under `config_dir`: opencode's document lands at
        // `<config_dir>/config/opencode/opencode.json`, because `$XDG_CONFIG_HOME` is a directory
        // the harness owns the layout of. Writing without creating that layout failed the whole
        // spawn with a bare `No such file or directory` naming nothing.
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, contents)?;
    }
    let inv = adapter.compile(&launch, &ctx)?;
    let bound = StdDuration::from_secs(req.timeout_secs);
    let run = match path {
        LaunchPath::LaunchOnly => launch_only_child(&inv, bound)?,
        LaunchPath::Duplex => duplex_child(
            &inv,
            &agent_id,
            ready_file
                .as_deref()
                .expect("the duplex path always mints a marker"),
            &req.prompt,
            bound,
        )?,
    };
    // §6.1 step 9, through the same seam as step 5. This was codex-JSONL-specific until now, so a
    // gemini or opencode child's report was unreadable and its contract said `Unreported` about a
    // run that had reported — the §12 silent-failure shape, one layer down from the dispatch bug.
    let outcome = ChildOutcome::from_stream(
        adapter.parse_stream(&run.stdout, run.exit),
        run.exit,
        run.stderr.clone(),
    );

    let changed = changed_paths(&wt, &base).unwrap_or_default();
    let diff = diff_text(&wt, &base).ok().filter(|d| !d.is_empty());
    let mut contract = build_contract(
        task_id.clone(),
        AgentId(requester.to_string()),
        RepoIdentity {
            git_common_dir: env.repo.join(".git"),
            head_branch: None,
        },
        base,
        Workspace::Worktree {
            path: wt.clone(),
            branch,
        },
        &req.prompt,
        &req.acceptance_criteria,
        &agent_type.scope_ceiling,
        &requested,
        Duration::from_secs(req.timeout_secs),
        spawned_at,
        &outcome,
        changed,
        diff,
        vec![],
    );
    // Say so when the capture is a prefix. §6.7's rule for caps is that shortening is always
    // recorded; a drain abandoned with the pipe still open shortens stdout and stderr the same way,
    // and the reader would otherwise see a truncated transcript as a complete one.
    if run.capture_truncated
        && let Some(completion) = contract.completion.as_mut()
    {
        completion.exit.description = note_truncated_capture(&completion.exit.description);
    }
    // Read off the **adapter**, not off `agent_type`: the contract is §6.7's audit record, so the
    // harness it names must be the one that actually produced the work, never the one that was
    // asked for. The two agree today precisely because the dispatch above reads the same field —
    // sourcing it here makes that an invariant the code enforces rather than one a reader has to
    // check, and it is the reason a divergence between the two could never again be silent.
    contract.child.harness = adapter.harness();
    contract.child.version = harness_version(&inv.program);
    // Read off the **compiled invocation** for the same reason, one step further: §3.1 makes the
    // marion-name → harness-name mapping the adapter's, and §6.7's `allowed_tools` records "the
    // compiled, harness-native constraint". So this records what went on the wire — which is
    // `None` for codex, whose `exec` surface carries no model argument, even when the request or
    // the agent type named one.
    contract.child.model = inv.model.clone();
    let returned = persist_then_cap(&agent_dir, &contract)?;
    cleanup(&env.repo, &wt);
    Ok(returned)
}

fn cleanup(repo: &Path, wt: &Path) {
    let _ = SysCommand::new("git")
        .current_dir(repo)
        .args(["worktree", "remove", "--force", &wt.to_string_lossy()])
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn::ChildOutcome;
    use marion_core::harness::Harness;

    fn temp(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("marion-supervisor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn an_overrunning_process_group_is_killed_and_reported_as_timed_out() {
        let dir = temp("timeout");
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
                git_common_dir: "/r/.git".into(),
                head_branch: None,
            },
            Oid("a".repeat(40)),
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
            vec![],
            None,
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
        let dir = temp("setsid-escape");
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

        let dir = temp("drain-bound");
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

    #[test]
    fn persisted_contract_is_complete_while_only_the_return_copy_is_capped() {
        let root = temp("persist");
        let project = ProjectDir::from_hash(&root, "0123456789ab");
        let agent = project.agent(&AgentId("agent".into()));
        let large = "x".repeat(20 * 1024);
        let contract = build_contract(
            TaskId("task".into()),
            AgentId("root".into()),
            RepoIdentity {
                git_common_dir: "/r/.git".into(),
                head_branch: None,
            },
            Oid("a".repeat(40)),
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
            vec![],
            None,
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
        let root = temp("dispatch");
        let repo = fixture_repo(&root);
        let state = root.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let env = Env {
            repo: repo.clone(),
            project_dir: ProjectDir::new(&state, &repo),
            bridge: PathBuf::from("/bin/marion-supervisor"),
            base_url: "http://127.0.0.1:8099/v1".into(),
        };
        let req = SpawnRequest {
            agent_type: "claude".into(),
            prompt: "do the task".into(),
            acceptance_criteria: vec![],
            writable_scope: vec!["src/**".into()],
            // Short: the base URL below answers nothing, so this bounds the launch to a second —
            // and a regression re-running `codex` for real is a fast failure rather than a wait.
            timeout_secs: 1,
            model: None,
        };

        let result = run_spawn(&env, &req, &TaskId("dispatch".into()), "root");

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

    /// The other half of item 1: the Codex path still resolves to the Codex adapter, so the
    /// dispatch change is observably a no-op for every M1 spawn.
    #[test]
    fn each_builtin_agent_type_dispatches_to_its_own_harness() {
        for (name, expected) in [
            ("claude", Harness::ClaudeCode),
            ("codex", Harness::Codex),
            ("codex-impl", Harness::Codex),
            ("gemini", Harness::Gemini),
            ("opencode", Harness::OpenCode),
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
            allowed_tools: vec![],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: None,
            config_dir: "/state/x/config".into(),
            extra: Extras::default(),
        }
    }

    fn request(agent_type: &str, model: Option<&str>) -> SpawnRequest {
        SpawnRequest {
            agent_type: agent_type.into(),
            prompt: "do the task".into(),
            acceptance_criteria: vec![],
            writable_scope: vec![],
            timeout_secs: 1,
            model: model.map(str::to_string),
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
        for (name, h) in [("gemini", Harness::Gemini), ("opencode", Harness::OpenCode)] {
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

    #[test]
    fn a_requested_scope_outside_the_agent_type_ceiling_is_rejected_before_launch() {
        let ceiling = vec![Glob("src/**".into())];
        let requested = vec![Glob("docs/**".into())];
        assert!(check_spawn_scope(&ceiling, &requested).is_err());
    }
}
