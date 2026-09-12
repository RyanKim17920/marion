//! The one kill marion sends at a bound's expiry, and the enumeration that makes it whole.
//!
//! A child that `setsid`s or forks a tool-call host has descendants outside its process group,
//! and a `kill(-pgid)` alone leaves them running under pid 1 (S7, design §11 item 18). So the
//! tree is walked with `ps` *first* — parent links and process groups both — and every group it
//! reaches is signalled, marion's own excepted. [`kill_process_tree_and_wait`] adds §6.7's second
//! step: a confirmation that means *observed dead*, not merely "SIGKILL was sent".

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpgrp() -> i32;
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
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let mut command = Command::new("ps");
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
