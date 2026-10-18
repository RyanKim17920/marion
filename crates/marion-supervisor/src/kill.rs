//! The one kill marion sends at a bound's expiry, and the enumeration that makes it whole.
//!
//! A child that `setsid`s or forks a tool-call host has descendants outside its process group,
//! and a `kill(-pgid)` alone leaves them running under pid 1 (S7, design §11 item 18). So the
//! tree is walked with `ps` *first* — parent links and process groups both — and every group it
//! reaches is signalled, marion's own excepted. [`kill_process_tree_and_wait`] adds §6.7's second
//! step: a confirmation that means *observed dead*, not merely "SIGKILL was sent".
//!
//! [`kill_process_tree_and_wait`]: crate::kill::kill_process_tree_and_wait

use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpgrp() -> i32;
    fn getsid(pid: i32) -> i32;
    fn setsid() -> i32;
}

/// **Start `command` leading a session of its own**, and so a process group of its own: what
/// every node marion runs under a bound needs. The group is what the expiry kill addresses without
/// touching marion's own (`signal_targets` refuses marion's group); the session is what still
/// names a descendant that changed its group and lost its parent (`session_orphans`). A node has
/// no controlling terminal to lose: marion gives a headless node none (§5.2).
pub fn lead_own_session(command: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe and touches nothing but the forked child, which has not
    // yet `exec`ed; its failure is reported as the spawn's.
    unsafe {
        command.pre_exec(|| {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })
    }
}

const SIGKILL: i32 = 9;

/// How long a pipe that is still open *after the child has been reaped and the tree killed* is
/// given before the drain is abandoned. On every healthy path the write ends are already closed by
/// then and the drains finish in microseconds, so this is dead time only when something escaped.
pub(crate) const DRAIN_GRACE: Duration = Duration::from_secs(2);

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
    let mut command = Command::new("ps");
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

/// **What `root` left behind under pid 1 in its own session**, with their descendants.
///
/// A process that changed only its group (a job-control shell's background job, say) and whose
/// parent then exited belongs to pid 1, in a group no ancestry walk reaches. Its session is still
/// `root`'s when `root` leads its own session, as every node marion starts does, so the session
/// finds it. A `root` that leads no session shares one with others — the supervisor's — and is
/// given nothing here, since that session is not the node's to sweep.
fn session_orphans(rows: &[ProcRow], root: i32) -> Vec<i32> {
    // SAFETY: `getsid` reads one process's session id and cannot fail other than by returning -1.
    if unsafe { getsid(root) } != root {
        return Vec::new();
    }
    let mut out: Vec<i32> = Vec::new();
    for r in rows.iter().filter(|r| r.ppid == 1 && r.pid != root) {
        // SAFETY: as above.
        if unsafe { getsid(r.pid) } == root {
            for p in descendant_pids(rows, r.pid) {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// `root`'s tree and its session's orphans: every pid the two-step kill enumerates.
fn swept_pids(rows: &[ProcRow], root: i32) -> Vec<i32> {
    let mut pids = descendant_pids(rows, root);
    for p in session_orphans(rows, root) {
        if !pids.contains(&p) {
            pids.push(p);
        }
    }
    pids
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
    let pids = swept_pids(&rows, child_pid);
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

/// Kill everything below `root` and leave `root` itself running — for a server that ends its own
/// session cleanly once asked (codex's app-server exits 0 on stdin EOF, S36 P9) but leaves a
/// turn's processes running after an interrupt (P7).
///
/// The same two steps as [`kill_process_tree`], enumeration first: every group reached by ancestry
/// is signalled except `root`'s own, and the processes that share `root`'s group are signalled one
/// by one, `root` excepted.
pub(crate) fn kill_descendants(root: i32) {
    let rows = ps_rows();
    let pids = swept_pids(&rows, root);
    if let Ok(mut last) = LAST_SWEEP.lock() {
        last.clone_from(&pids);
    }
    let root_pgid = rows.iter().find(|r| r.pid == root).map(|r| r.pgid);
    let groups: Vec<i32> = pgids_of(&rows, &pids)
        .into_iter()
        .filter(|g| Some(*g) != root_pgid)
        .collect();
    for pgid in signal_targets(&groups, unsafe { getpgrp() }) {
        let _ = unsafe { kill(-pgid, SIGKILL) };
    }
    for r in rows
        .iter()
        .filter(|r| r.pid != root && Some(r.pgid) == root_pgid)
    {
        if pids.contains(&r.pid) {
            let _ = unsafe { kill(r.pid, SIGKILL) };
        }
    }
}

/// **Kill whatever is left in `pgid` after its leader ended on its own** — a cancelled node that
/// closed its turn within its grace can leave its MCP bridge, or a tool call, in its group.
/// Addressed by the group marion recorded at launch, never by the leader's pid, which may already
/// be reaped: a group with no members left is signalled nothing, and one only [`signal_targets`]
/// allows is signalled at all. The members found are recorded as the last sweep.
pub(crate) fn sweep_group(pgid: i32) {
    let rows = ps_rows();
    let members: Vec<i32> = rows
        .iter()
        .filter(|r| r.pgid == pgid)
        .map(|r| r.pid)
        .collect();
    if members.is_empty() {
        return;
    }
    let pids: Vec<i32> = members
        .iter()
        .flat_map(|m| descendant_pids(&rows, *m))
        .fold(Vec::new(), |mut all, p| {
            if !all.contains(&p) {
                all.push(p);
            }
            all
        });
    if let Ok(mut last) = LAST_SWEEP.lock() {
        last.clone_from(&pids);
    }
    let mut pgids = pgids_of(&rows, &pids);
    if !pgids.contains(&pgid) {
        pgids.push(pgid);
    }
    for g in signal_targets(&pgids, unsafe { getpgrp() }) {
        let _ = unsafe { kill(-g, SIGKILL) };
    }
}

/// **Every process group below `root`, enumerated now**, for a caller about to do something that
/// breaks the tree before it kills — an app-server's `turn/interrupt` ends the host of a command
/// that `setsid`s, and the command's parent is pid 1 before [`kill_descendants`] runs. `root`'s
/// own group is left out, as [`kill_descendants`] leaves it; [`kill_groups`] signals the rest.
pub(crate) fn groups_below(root: i32) -> Vec<i32> {
    let rows = ps_rows();
    let pids = descendant_pids(&rows, root);
    let root_pgid = rows.iter().find(|r| r.pid == root).map(|r| r.pgid);
    pgids_of(&rows, &pids)
        .into_iter()
        .filter(|g| Some(*g) != root_pgid)
        .collect()
}

/// SIGKILL each of `groups` that [`signal_targets`] allows — never 0, 1 or marion's own.
pub(crate) fn kill_groups(groups: &[i32]) {
    for pgid in signal_targets(groups, unsafe { getpgrp() }) {
        let _ = unsafe { kill(-pgid, SIGKILL) };
    }
}

/// **End a node: what is left in the session its leader leads, then the leader's reap** — the one
/// end every node transport takes (launch-only, duplex, rpc, pty) once marion is done with the
/// leader, whether it exited or is to be ended now.
///
/// **The sweep runs before the reap, and that order is the safety.** An unreaped leader keeps its
/// pid, and so its session id, from being issued again, so every process `end_session` finds in
/// that session is the node's own. Swept after the reap, the id is the kernel's to hand out: a
/// stranger that took the pid and led a session of its own would be signalled. A leader already
/// reaped (an earlier `wait`, or not this caller's child) is not swept at all; its status is
/// returned and nothing is signalled. A leader that led no session of its own has nothing in one:
/// no process's session id is its pid.
pub fn end_node(child: &mut Child) -> std::io::Result<ExitStatus> {
    end_node_counting(child).map(|(status, _)| status)
}

/// [`end_node`], and how many processes left in the leader's session it had to kill: what a
/// caller that reports on a process (`marion doctor`'s probe) says was left behind.
pub fn end_node_counting(child: &mut Child) -> std::io::Result<(ExitStatus, usize)> {
    let leader = child.id() as i32;
    let killed = if is_unreaped_child(leader) {
        end_session(leader, DRAIN_GRACE)
    } else {
        0
    };
    Ok((child.wait()?, killed))
}

/// Whether `pid` is this process's own child and not yet reaped — running or a zombie. `waitid`
/// with `WNOWAIT` reads that without consuming the status; any other process is `ECHILD`.
fn is_unreaped_child(pid: i32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return false;
    };
    rustix::process::waitid(
        rustix::process::WaitId::Pid(pid),
        rustix::process::WaitIdOptions::EXITED
            | rustix::process::WaitIdOptions::NOHANG
            | rustix::process::WaitIdOptions::NOWAIT,
    )
    .is_ok()
}

/// **End every process of session `sid` and wait until none is left**: [`end_node`]'s sweep, whose
/// caller holds `sid`'s leader unreaped so the id cannot name a stranger's session.
///
/// [`kill_process_tree`] enumerates once and signals: a process forked after its `ps` snapshot, in
/// a group of its own, outlives it, and its parent's death leaves it under pid 1 in the same
/// session. So this reads the session afresh each round — every process whose session is `sid`,
/// with their descendants while their parents live — SIGKILLs what is still running, and repeats
/// until a round finds nothing, or `bound` passes. A zombie counts as ended. Returns how many
/// processes it signalled, each counted once however many rounds it took. Refuses marion's own
/// session, and 0 or 1, returning 0 having signalled nothing.
///
/// **Each signal is preceded by a fresh look at its target** ([`still_in_session`]): a process that
/// ended after the `ps` snapshot leaves its pid free, and only one still in the session, or still
/// below a process that is, is signalled.
fn end_session(sid: i32, bound: Duration) -> usize {
    // SAFETY: `getsid(0)` reads this process's own session id.
    let own = unsafe { getsid(0) };
    if sid <= 1 || sid == own {
        return 0;
    }
    let mut killed: Vec<i32> = Vec::new();
    let session_of = |pid: i32| unsafe { getsid(pid) };
    let deadline = Instant::now() + bound;
    let mut pause = Duration::from_millis(1);
    loop {
        let rows = ps_rows();
        // SAFETY (in `session_of`): `getsid` reads one process's session id; -1 for one gone.
        let members: Vec<i32> = rows
            .iter()
            .map(|r| r.pid)
            .filter(|p| session_of(*p) == sid)
            .collect();
        let mut pids: Vec<i32> = Vec::new();
        for m in &members {
            for p in descendant_pids(&rows, *m) {
                if !pids.contains(&p) {
                    pids.push(p);
                }
            }
        }
        let live: Vec<i32> = pids
            .into_iter()
            .filter(|p| {
                !matches!(
                    crate::procid::run_state(*p),
                    crate::procid::RunState::Gone | crate::procid::RunState::Zombie
                )
            })
            .collect();
        if live.is_empty() {
            return killed.len();
        }
        for pid in live {
            if still_in_session(pid, sid, &members, session_of, crate::procid::lineage)
                && unsafe { kill(pid, SIGKILL) } == 0
                && !killed.contains(&pid)
            {
                killed.push(pid);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return killed.len();
        }
        std::thread::sleep(pause.min(left));
        pause = (pause * 2).min(OBSERVE_PAUSE_MAX);
    }
}

/// **Whether `pid`, read now, is still session `sid`'s**: in it, or below one of `members` (the
/// round's members, for a descendant that led a session of its own). What a pid that changed hands
/// since the round's `ps` snapshot fails: a stranger is in another session and below none of them.
/// The walk starts at the parent, so a member's pid now worn by a stranger is not its own proof.
fn still_in_session(
    pid: i32,
    sid: i32,
    members: &[i32],
    session_of: impl Fn(i32) -> i32,
    lineage: impl Fn(i32) -> Option<crate::procid::Lineage>,
) -> bool {
    session_of(pid) == sid
        || lineage(pid)
            .is_some_and(|l| crate::procid::descends_from(l.ppid, members, &[], &lineage))
}

/// Apply §6.7's two-step kill and wait until the addressed process is absent or a zombie.
///
/// The journal's confirmation means *observed dead*, not merely "SIGKILL was sent". A zombie is
/// dead for that purpose — it can run no code and its parent alone owns the remaining wait record
/// — while `kill(pid, 0)` would misclassify it as alive, so the process table's own state is read
/// (`crate::procid::run_state`: one `sysctl` or one `/proc` read, no fork). The bound is a safety
/// refusal, not a grace period: SIGKILL has no graceful leg, and a caller that cannot observe death
/// leaves its already-durable intent unconfirmed for §7.2-style recovery.
///
/// This used to fork `ps -o stat=` in a `yield_now` loop — up to five seconds of back-to-back
/// forks per kill. A SIGKILLed process is usually gone within a millisecond, so the reads back off
/// from 1 ms to `OBSERVE_PAUSE_MAX`.
pub fn kill_process_tree_and_wait(child_pid: i32) -> bool {
    kill_process_tree(child_pid);
    observe_dead(child_pid, Instant::now() + Duration::from_secs(5))
}

/// The longest pause between two reads of a killed process's state.
const OBSERVE_PAUSE_MAX: Duration = Duration::from_millis(50);

/// Read `pid`'s state until it is gone or a zombie (`true`) or `deadline` passes (`false`). An
/// unreadable state is not death: treating it as absent would append the confirmation whose claim
/// this loop exists to earn.
fn observe_dead(pid: i32, deadline: Instant) -> bool {
    use crate::procid::RunState;
    let mut pause = Duration::from_millis(1);
    loop {
        if matches!(
            crate::procid::run_state(pid),
            RunState::Gone | RunState::Zombie
        ) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(pause.min(left));
        pause = (pause * 2).min(OBSERVE_PAUSE_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Everything below the root dies, and the root is not signalled**: a shell running a
    /// background sleeper in its own group (`setsid`-like) and a foreground one in the shell's
    /// group loses both, and itself exits on its own when its foreground job does.
    #[test]
    fn kill_descendants_spares_the_root_and_takes_every_group_below_it() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let mut root = Command::new("sh")
            .args(["-c", "set -m; sleep 61 & set +m; sleep 62"])
            .process_group(0)
            .spawn()
            .expect("spawn sh");
        let pid = root.id() as i32;
        let below = || -> Vec<i32> {
            let rows = ps_rows();
            descendant_pids(&rows, pid)
                .into_iter()
                .filter(|p| *p != pid)
                .collect()
        };
        let until = Instant::now() + Duration::from_secs(5);
        while below().len() < 2 {
            assert!(
                Instant::now() < until,
                "the shell never started both sleepers"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let victims = below();
        kill_descendants(pid);
        for v in &victims {
            assert!(
                observe_dead(*v, Instant::now() + Duration::from_secs(5)),
                "{v} survived"
            );
        }
        let status = root.wait().unwrap();
        assert_eq!(
            status.signal(),
            None,
            "the root was not signalled: {status:?}"
        );
    }

    /// **An orphan in its own group but the node's session dies with the node.** A job-control
    /// shell puts a background job in a group of its own; once the subshell that started it exits,
    /// the job's parent is pid 1 and no ancestry walk reaches it. The node leads its session, so
    /// the session still names it.
    #[test]
    fn an_orphan_reparented_to_pid_one_in_the_nodes_session_is_killed_with_the_node() {
        let dir = marion_testsupport::scratch("kill-session-orphan");
        let recorded = dir.join("orphan.pid");
        // bash, not sh: dash (Debian's sh) turns job control off without a terminal, and its
        // background job then stays in the node's own group.
        let mut root = Command::new("bash");
        root.args([
            "-c",
            &format!(
                "set -m; (sleep 64 & echo $! > {}); sleep 65",
                recorded.display()
            ),
        ]);
        let mut root = lead_own_session(&mut root).spawn().expect("spawn sh");
        let pid = root.id() as i32;
        // SAFETY: reads one process's session id.
        assert_eq!(
            unsafe { getsid(pid) },
            pid,
            "the node leads its own session"
        );
        let orphan = || -> Option<i32> {
            let p: i32 = std::fs::read_to_string(&recorded)
                .ok()?
                .trim()
                .parse()
                .ok()?;
            let l = crate::procid::lineage(p)?;
            // Reparented out of the node: to pid 1, or to a subreaper (a CI runner, systemd --user)
            // — either way no ancestry walk from the node reaches it any more.
            let under_node = l.ppid == pid
                || crate::procid::lineage(l.ppid).is_some_and(|parent| parent.ppid == pid);
            (!under_node && l.pgid != pid).then_some(p)
        };
        assert!(
            marion_testsupport::until(|| orphan().is_some()),
            "the job never became an orphan in a group of its own"
        );
        let orphan = orphan().unwrap();
        kill_process_tree(pid);
        let _ = root.wait();
        assert!(
            observe_dead(orphan, Instant::now() + Duration::from_secs(5)),
            "{orphan} survived its node"
        );
    }

    /// **A node's end takes what its leader left in its session, and nothing outside it.** The
    /// leader exits leaving a child in a group of its own; a stranger leading its own session runs
    /// beside it. [`end_node`] ends the child, reaps the leader, and the stranger lives.
    #[test]
    fn a_nodes_end_takes_its_sessions_leftovers_and_spares_a_stranger() {
        let dir = marion_testsupport::scratch("kill-end-node");
        let recorded = dir.join("left.pid");
        let mut node = Command::new("bash");
        node.args([
            "-c",
            &format!("set -m; sleep 66 & echo $! > {}", recorded.display()),
        ]);
        let mut node = lead_own_session(&mut node).spawn().expect("spawn bash");
        let mut stranger = Command::new("sleep");
        stranger.arg("67");
        let mut stranger = lead_own_session(&mut stranger)
            .spawn()
            .expect("spawn sleep");
        let leader = node.id() as i32;
        let left =
            || -> Option<i32> { std::fs::read_to_string(&recorded).ok()?.trim().parse().ok() };
        assert!(marion_testsupport::until(|| left().is_some()));
        let left = left().unwrap();
        // The leader has exited and is unreaped: its zombie holds the session id.
        assert!(marion_testsupport::until(|| matches!(
            crate::procid::run_state(leader),
            crate::procid::RunState::Zombie
        )));
        let status = end_node(&mut node).expect("reaped");
        let stranger_ran = crate::procid::run_state(stranger.id() as i32);
        let _ = stranger.kill();
        let _ = stranger.wait();
        assert!(status.success(), "{status:?}");
        assert!(
            observe_dead(left, Instant::now() + Duration::from_secs(5)),
            "{left} survived its node"
        );
        assert_eq!(stranger_ran, crate::procid::RunState::Running);
    }

    /// **A leader already reaped is not swept**: its pid, and so its session id, is the kernel's to
    /// issue again, and what wears it may be a stranger's session. Nothing is signalled — what it
    /// left in its session is still running — and the status is the one reaped.
    #[test]
    fn a_reaped_leaders_session_is_never_swept() {
        let dir = marion_testsupport::scratch("kill-end-reaped");
        let recorded = dir.join("left.pid");
        let mut node = Command::new("bash");
        node.args([
            "-c",
            &format!("set -m; sleep 68 & echo $! > {}", recorded.display()),
        ]);
        let mut node = lead_own_session(&mut node).spawn().expect("spawn bash");
        let status = node.wait().expect("reaped");
        let left: i32 = std::fs::read_to_string(&recorded)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let again = end_node(&mut node).expect("the reaped status");
        let after = crate::procid::run_state(left);
        marion_testsupport::kill_hard(left);
        assert_eq!(again, status);
        assert_eq!(
            after,
            crate::procid::RunState::Running,
            "a reaped session was swept"
        );
    }

    /// **The look before each signal refuses a pid that changed hands.** A member whose pid a
    /// stranger now wears is in another session and below no member; a member still in the session
    /// and a descendant that led a session of its own below one are the node's.
    #[test]
    fn a_pid_that_changed_hands_since_the_snapshot_is_not_signalled() {
        use crate::procid::Lineage;
        let sid = 500;
        let members = [500, 501];
        // 501 was a member; a stranger in session 900, child of 899, wears the number now.
        let session_of = |p: i32| match p {
            500 | 502 => 500,
            501 => 900,
            503 => 503,
            _ => 1,
        };
        let lineage = |p: i32| {
            Some(match p {
                501 => Lineage {
                    ppid: 899,
                    pgid: 900,
                },
                899 => Lineage { ppid: 1, pgid: 899 },
                503 => Lineage {
                    ppid: 500,
                    pgid: 503,
                },
                _ => Lineage { ppid: 1, pgid: p },
            })
        };
        assert!(!still_in_session(501, sid, &members, session_of, lineage));
        assert!(still_in_session(502, sid, &members, session_of, lineage));
        assert!(
            still_in_session(503, sid, &members, session_of, lineage),
            "a setsid child of a member"
        );
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

    /// **Observed dead, and promptly**: a SIGKILLed child that nobody has reaped is a zombie, which
    /// counts, and a process that will not die is refused at the deadline rather than confirmed.
    #[test]
    fn a_killed_process_is_observed_dead_and_a_live_one_is_refused_at_the_deadline() {
        let mut live = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = live.id() as i32;
        let start = Instant::now();
        assert!(
            !observe_dead(pid, Instant::now() + Duration::from_millis(200)),
            "a running process is not observed dead"
        );
        assert!(start.elapsed() >= Duration::from_millis(200));
        live.kill().unwrap();
        let start = Instant::now();
        assert!(observe_dead(pid, Instant::now() + Duration::from_secs(5)));
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "the death was read, not waited out"
        );
        live.wait().unwrap();
        assert!(observe_dead(pid, Instant::now()), "a reaped pid is gone");
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
}
