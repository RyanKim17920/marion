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
use marion_harness::{ExecSpec, compile_exec, config_toml};

use crate::spawn::{
    SpawnError, build_contract, changed_paths, diff_text, make_worktree, parse_child_stream,
};

pub struct SpawnRequest {
    pub agent_type: String,
    pub prompt: String,
    pub acceptance_criteria: Vec<String>,
    pub writable_scope: Vec<String>,
    pub timeout_secs: u64,
}

pub struct Env {
    pub repo: PathBuf,
    pub project_dir: ProjectDir,
    pub bridge: PathBuf,
    pub base_url: String,
}

struct CommandOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    /// A pipe was still open when the drain bound expired, so the captured bytes are a prefix of
    /// what the child wrote. Recorded, never silent (design §6.7).
    capture_truncated: bool,
}

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpgrp() -> i32;
    fn poll(fds: *mut PollFd, nfds: NfdsT, timeout_ms: i32) -> i32;
}

const SIGKILL: i32 = 9;

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
const DRAIN_GRACE: StdDuration = StdDuration::from_secs(2);

/// A pipe drain that can be stopped while the pipe is still open.
///
/// The thread never blocks in `read` for longer than `DRAIN_POLL_MS`: it waits for readiness with
/// `poll`, which takes a timeout, and only then reads bytes it knows are there. So a stop request
/// is honoured promptly and the thread *exits* — it is not detached and left wedged. That matters
/// because `spawn` is called repeatedly by a long-lived supervisor: one leaked thread (and one
/// leaked fd, and its buffer) per timed-out spawn would be its own unbounded leak, traded for the
/// hang it fixed.
struct Drain {
    handle: thread::JoinHandle<(Vec<u8>, bool)>,
    stop: Arc<AtomicBool>,
}

impl Drain {
    fn start<R: Read + AsRawFd + Send + 'static>(mut pipe: R) -> Self {
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
    fn finish(self, deadline: Instant) -> (Vec<u8>, bool) {
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

/// Every distinct process group among `root`, the rest of `root`'s own group, and all of their
/// descendants — the set S7 measured as sufficient to leave zero survivors.
///
/// `codex exec` calls `setsid` for each tool-call command, so those children sit in their own
/// session **and** their own process group: `killpg` on the group marion created reaches codex but
/// not them (`tests/fixtures/s7/README.md`). Their groups can only be found by ancestry, and only
/// while the tree is intact.
fn descendant_pgids(rows: &[ProcRow], root: i32) -> Vec<i32> {
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
    let mut pgids: Vec<i32> = Vec::new();
    for pid in &seen {
        if let Some(g) = rows.iter().find(|r| r.pid == *pid).map(|r| r.pgid)
            && !pgids.contains(&g)
        {
            pgids.push(g);
        }
    }
    pgids
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
fn kill_process_tree(child_pid: i32) {
    let rows = ps_rows();
    let mut pgids = descendant_pgids(&rows, child_pid);
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

fn run_bounded(
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

fn entropy() -> Result<[u8; RAND_BYTES], SpawnError> {
    let mut bytes = [0; RAND_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn unix_millis() -> u64 {
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

    std::fs::write(
        ch.join("config.toml"),
        config_toml(&env.bridge.to_string_lossy(), &["mcp"], &env.base_url),
    )?;
    let inv = compile_exec(&ExecSpec {
        cwd: wt.clone(),
        codex_home: ch,
        prompt: req.prompt.clone(),
        output_schema: None,
        output_last_message: None,
    });
    let output = run_bounded(
        SysCommand::new(&inv.program)
            .args(&inv.args)
            .envs(inv.env.iter().cloned())
            .env("MARION_DUMMY_KEY", "dummy")
            .current_dir(&inv.cwd),
        StdDuration::from_secs(req.timeout_secs),
    )?;
    let stream = String::from_utf8_lossy(&output.stdout);
    let mut outcome = parse_child_stream(&stream);
    outcome.exit_code = output.code;
    outcome.signal = output.signal;
    outcome.timed_out = output.timed_out;
    outcome.stderr = String::from_utf8_lossy(&output.stderr).into_owned();

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
    if output.capture_truncated
        && let Some(completion) = contract.completion.as_mut()
    {
        completion.exit.description = note_truncated_capture(&completion.exit.description);
    }
    contract.child.harness = agent_type.harness;
    contract.child.version = harness_version(&inv.program);
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

    #[test]
    fn every_pgid_a_setsid_tool_call_child_escaped_into_is_collected() {
        // The measured remedy set. 55296 and 55386 are outside codex's group and are exactly what a
        // single killpg on 55091 leaves running.
        assert_eq!(descendant_pgids(&s7_tree(), 55091), vec![55091, 55296, 55386]);
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
        assert_eq!(signal_targets(&[-1, -55386, 55386, 55386], 4242), vec![55386]);
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

        assert!(!complete, "the pipe was still open, so the capture is short");
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
            SysCommand::new("sh")
                .args(["-c", "yes 0123456789012345678901234567890123456789 | head -n 12800"]),
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

    #[test]
    fn a_requested_scope_outside_the_agent_type_ceiling_is_rejected_before_launch() {
        let ceiling = vec![Glob("src/**".into())];
        let requested = vec![Glob("docs/**".into())];
        assert!(check_spawn_scope(&ceiling, &requested).is_err());
    }
}
