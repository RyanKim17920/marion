//! **The helpers marion's tests share, in one copy each.**
//!
//! # Why this is a crate and not a `tests/common/mod.rs`
//!
//! Rust compiles every file in `tests/` as its own crate, so integration tests cannot share code
//! without a mechanism, and there are two candidates. A `tests/common/mod.rs` reaches the eleven
//! integration files and **nothing else** — but four of the `Scratch` guards this crate replaces
//! live in `#[cfg(test)] mod tests` inside `src/journal.rs`, `src/run.rs` and `src/duplex.rs`, and
//! two more constructors in `src/root.rs` and `src/bin/marion.rs`. A module compiled into the
//! integration-test targets is invisible to those, so `tests/common` would leave a third of the
//! duplication in place while looking like it had solved the problem. A dev-dependency is visible
//! to a crate's own `#[cfg(test)]` code as well, which is the whole of the argument.
//!
//! It is also the choice this workspace has already made once, for the same reason: `marion-provider`
//! is a member crate whose only consumers are tests, wired in as marion-supervisor's single
//! `[dev-dependencies]` edge. Following that is not inventing structure. `#[path = …]`, by
//! contrast, appears nowhere in this repository.
//!
//! # Why a shared copy at all, when each copy was already documented
//!
//! Not tidiness. **The copies did not merely multiply — the strong version failed to propagate**,
//! and the drift always ran the same way, toward the weaker assertion:
//!
//! - [`survivors`] existed five times in two strengths. Three copies parsed the pid off each `ps`
//!   line and panicked when they could not; two returned the line as a string and silently dropped
//!   the pid. The comment explaining why that matters — *a survivor this test cannot name* — was
//!   only ever in the three strong copies. Two files had a leak check that could under-report.
//! - [`Scratch`] existed eleven times, and beside it four constructors that returned a bare
//!   `PathBuf` with no guard at all. Those four leaked their directory on every failing run, which
//!   is precisely the asymmetry the guard was introduced to remove.
//! - [`persisted_contracts`] existed five times in three different failure semantics.
//!
//! A per-file copy that has drifted is worse than either sharing or duplicating on purpose. So each
//! helper here is the **strongest** variant that existed, and the reasoning that made it the
//! strongest is kept with it, where the next person will read it before weakening it.
//!
//! # This crate must never be a normal dependency
//!
//! Same rule as `marion-provider`, and the same reason: nothing here belongs inside the shipped
//! `marion` command. It kills processes by pid, removes directories on drop, and shells out to
//! `git` and `ps`.

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use marion_core::Harness;
use marion_harness::spec::HarnessSpec;

// --- waiting ------------------------------------------------------------------------------------

/// The bound [`until`] polls under, and the one every `#[cfg(test)]` module in marion-supervisor
/// shares rather than inventing its own.
///
/// A tighter bound was tried at two seconds and produced one failure in roughly twenty full-suite
/// runs, on a `fork`/`exec` under a fully loaded machine — which is a measurement of the laptop,
/// not of the code.
pub const UNTIL_BOUND: Duration = Duration::from_secs(5);

/// The interval [`until`] re-asks `cond` at: short, because the waits it serves are for a
/// transition another thread is about to make, and a coarse step only adds latency to a pass.
pub const UNTIL_STEP: Duration = Duration::from_millis(2);

/// Poll `cond` for at most [`UNTIL_BOUND`], then answer.
///
/// Assert that something **happens**, never how long it takes: the bound is headroom the test
/// does not decide the answer with, so a fast machine does not pay for a slow one's, and no test
/// may be fixed by widening it. It is not a timeout in the "the suite hung" sense either — every
/// caller turns the `false` into an assertion whose message names what did not happen, and the
/// process-facing ones ask [`alive`] first so the message separates "the child died" from "the
/// event never came".
pub fn until(cond: impl FnMut() -> bool) -> bool {
    until_within(UNTIL_BOUND, UNTIL_STEP, cond)
}

/// [`until`] with its own budget and step, for a wait whose *expiry is itself a defect report* or
/// whose transition is known to be coarser than [`UNTIL_STEP`].
///
/// `cond` is asked one last time after the budget so a transition that lands exactly at the
/// deadline is still seen, and the answer is the condition's, never the clock's.
pub fn until_within(budget: Duration, step: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(step);
    }
    cond()
}

// --- processes ----------------------------------------------------------------------------------

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn killpg(pgrp: i32, sig: i32) -> i32;
}

/// `SIGKILL`. Not `SIGTERM`: a test cleaning up after a leak has already established that the
/// process ignored whatever polite signal its supervisor sent it.
const SIGKILL: i32 = 9;
/// Signal 0 — the null signal, which checks for the process's existence and delivers nothing.
const SIGNULL: i32 = 0;
/// "No such process": the *only* errno from `kill(pid, 0)` that means the pid is gone.
const ESRCH: i32 = 3;

/// Is a process with this pid still alive?
///
/// `kill(pid, 0)` rather than a `ps` scan: it answers about one pid without parsing anything, which
/// is what a test polling for a death wants.
///
/// **A non-zero return is not a death.** `kill(pid, 0)` fails with `EPERM` for a process that
/// exists and is not ours, and treating that as gone is backwards in exactly the direction a leak
/// check cannot afford: a survivor that changed uid — or any pid the test did not spawn — would be
/// reported as reaped. `ESRCH` is the only answer that means *gone*, so it is the only one this
/// reads as one.
pub fn alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 delivers nothing; it only reports whether the pid is reachable.
    if unsafe { kill(pid, SIGNULL) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

/// What `ps` says about one pid, in the four readings that are genuinely different.
///
/// [`alive`] answers a different question and keeps its own implementation on purpose: it is a
/// *poll for a death*, where "not yet gone" is the whole answer and a zombie counts as not gone.
/// A test that asserts a recorded pid **names a running process** cannot use that reading, because
/// a zombie satisfies `kill(pid, 0)` while being a process that can never run another instruction.
/// Those are two different claims and collapsing them is how a leak check passes over a corpse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The kernel has this pid and it is not a zombie: it can still run code.
    Alive,
    /// Signalled and gone, but not yet reaped by its parent. Dead for every purpose except the
    /// parent's outstanding `wait`, and `kill(pid, 0)` cannot tell it from [`Liveness::Alive`].
    Zombie,
    /// No such pid.
    Gone,
    /// `ps` could not be asked, or answered in a way this cannot read. **Never folded into
    /// `Gone`** — S15's rule, and this repo's documented history of reading a failure to observe
    /// as an observation of absence. A caller asserting liveness must fail on this, not pass.
    CannotTell,
}

/// Three-valued liveness for one pid — plus the zombie, which is the fourth reading that matters.
///
/// `ps -o stat=` rather than `kill(pid, 0)`, for the reason on [`Liveness::Zombie`]. This is the
/// Rust home of spike S15's `procid.liveness`, and `run.rs`'s `kill_process_tree_and_wait` makes
/// the same `Z`-versus-empty distinction inline while waiting for a death it caused; a test that
/// needs the classification rather than the wait reads it from here instead of growing a copy.
///
/// An empty `stat` with empty stderr is the only reading taken as [`Liveness::Gone`]: `ps` prints
/// nothing *and complains* about a pid it cannot look at, so requiring both is what keeps a
/// permissions failure from being reported as a death.
pub fn liveness(pid: i32) -> Liveness {
    let Ok(out) = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        return Liveness::CannotTell;
    };
    let state = String::from_utf8_lossy(&out.stdout);
    let state = state.trim();
    if state.starts_with('Z') {
        return Liveness::Zombie;
    }
    if !state.is_empty() {
        return Liveness::Alive;
    }
    if out.stderr.is_empty() {
        Liveness::Gone
    } else {
        Liveness::CannotTell
    }
}

/// Kill a leaked process outright, ignoring the result.
///
/// The result is ignored on purpose and in one place, so no call site has to decide: by the time a
/// test is killing survivors it has already recorded the leak as its verdict, and a kill that fails
/// because the process finally exited on its own must not become a second, misleading failure.
pub fn kill_hard(pid: i32) {
    // SAFETY: `kill` is a plain libc call; a pid that no longer exists yields `ESRCH`, which is the
    // ignored case above.
    let _ = unsafe { kill(pid, SIGKILL) };
}

/// Processes still alive with `needle` on their command line, as `(pid, line)`.
///
/// **Every way `ps` can fail to answer is a failure of the test rather than an empty answer.**
/// `ps` is the only witness a leak check has, so reporting "no survivors" because it was missing,
/// errored, or printed nothing would make the assertion pass for free on exactly the machines where
/// it cannot be checked — the silent pass the check exists to rule out.
///
/// **The pid is parsed, and an unparsable one panics.** That is the half that failed to propagate:
/// two of the five copies this replaces returned the raw line instead, so a matching line whose pid
/// would not parse was silently dropped — a survivor the test could not name, discarded by the code
/// whose entire job was to name it. Dropping it is the same silent pass one line down.
pub fn survivors(needle: &str) -> Vec<(i32, String)> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("`ps` must run: without it nothing here can tell a clean run from a leak");
    assert!(
        out.status.success(),
        "`ps -axo pid=,command=` exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let listing = String::from_utf8_lossy(&out.stdout);
    // `ps -ax` lists at minimum the calling test process, so an empty listing means a witness that
    // did not work, not a machine with nothing running on it.
    assert!(
        !listing.trim().is_empty(),
        "`ps` printed nothing; the leak check would report no survivors whatever had leaked"
    );
    listing
        .lines()
        .filter(|l| l.contains(needle))
        .map(|l| {
            let pid = l
                .split_whitespace()
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| {
                    panic!("`ps` line matches {needle:?} but carries no pid: {l:?}")
                });
            (pid, l.to_string())
        })
        .collect()
}

/// Kill everything carrying `needle` and **do not return until it is gone**.
///
/// `kill_hard` on each of [`survivors`] is the shape every fixture here reached for, and it has a
/// gap that only shows up when a [`Scratch`] guard removes the tree immediately afterwards:
/// `SIGKILL` is asynchronous. It marks a process for death; it does not unwind the syscall that
/// process is already inside. So a supervisor caught mid-spawn finishes its `create_dir_all` and
/// its `File::create` *after* the signal was sent — and if the tree was removed in between, what it
/// finishes writing is a fresh `state/<hash>/agents/<id>/contracts/<task>.json` under a directory
/// nothing will ever clean up again. That is exactly the shape of the leftovers found under
/// `/tmp/mn-501` after a full suite run: one contract file and the three empty directories above
/// it, with everything else the test wrote correctly gone.
///
/// Waiting closes it, and the wait is on an **event** rather than a duration — reaped or not, by
/// the time `ps` stops listing a pid its last write has landed. The bound only decides how a
/// pathological machine gives up; it never decides a verdict, because the return value is the
/// survivors that outlived it and a caller that cares can assert on it.
pub fn sweep(needle: &str) -> Vec<(i32, String)> {
    for (pid, _) in survivors(needle) {
        kill_hard(pid);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = survivors(needle);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// --- the harnesses this suite drives --------------------------------------------------------------

/// **The row that runs `program`, when the suite gates it**: a built-in row naming `program` as its
/// binary and listing the releases marion verified it on (`HarnessSpec::verified`).
///
/// That list is the one table. The gate admits a release it names, or a newer one this machine's
/// conformance battery admitted ([`on_path`]); nothing in this crate restates a version, a probe
/// switch or a release store, so a harness gains the gate by having a row.
pub fn gated_row(program: &str) -> Option<&'static HarnessSpec> {
    gated_rows().find(|spec| spec.program == Some(program))
}

/// Every row the suite gates, in [`Harness::ALL`]'s order.
pub fn gated_rows() -> impl Iterator<Item = &'static HarnessSpec> {
    Harness::ALL
        .into_iter()
        .map(marion_harness::adapter::harness_spec)
        .filter(|spec| spec.program.is_some() && !spec.verified.is_empty())
}

/// **The row that runs `program` natively** (`claude` → `claude-code`): the name its conformance
/// records are filed under, and the one `scripts/conformance.sh --harness` takes. `program` itself
/// where no row runs it.
pub fn row_name(program: &str) -> &str {
    Harness::ALL
        .into_iter()
        .find(|h| marion_harness::adapter::harness_spec(*h).program == Some(program))
        .map_or(program, |h| h.as_str())
}

/// The pinned version of `program` — entry zero of its row's `verified` list, the release its
/// measured behaviours were taken from.
///
/// For call sites that need to *say* the version rather than check it, so they say it from the same
/// list the check reads. Panics for a program this suite does not gate: there is no honest answer.
pub fn pinned_version(program: &str) -> &'static str {
    gated_row(program)
        .unwrap_or_else(|| panic!("{program:?} has no row with verified releases"))
        .verified[0]
}

/// The first dotted-numeric token in `--version` output, e.g. `2.1.222` out of
/// `2.1.222 (Claude Code)`.
///
/// **Each of the five formats it differently, so this was measured rather than assumed** (darwin
/// 25.5.0, 2026-08-04 for the first four and 2026-09-05 for copilot; all five print to *stdout*
/// and exit 0, four of them one line):
///
/// | program    | `--version` prints                                                       |
/// |------------|--------------------------------------------------------------------------|
/// | `claude`   | `2.1.222 (Claude Code)`                                                  |
/// | `codex`    | `codex-cli 0.146.0`                                                      |
/// | `opencode` | `1.17.3`                                                                 |
/// | `copilot`  | `GitHub Copilot CLI 1.0.83.` then `Run 'copilot update' to check for updates.` |
/// | `goose`    | ` 1.49.0` — a leading space, no name (measured 2026-09-05)                 |
/// | `cline`    | `3.0.61` — bare (measured 2026-09-05)                                      |
/// | `qwen`     | `0.23.0` — bare (measured 2026-09-05)                                      |
/// | `agy`      | `1.2.8` — bare (measured 2026-09-22)                                       |
///
/// Three carry a name and two do not, and the name comes first where it is present — so the rule
/// is "first token that is digits and dots", which skips `codex-cli` and `GitHub` (no leading
/// digit) and takes `2.1.222` ahead of `(Claude`. Copilot ends its sentence with a full stop, so a
/// **single trailing dot is stripped before the shape is judged** — `1.0.83.` reads as `1.0.83`,
/// while a bare `1.` strips to `1`, which has no dot and is still rejected as the truncation it
/// is. `None` is returned for output with no such token, and [`on_path`] turns that into a
/// failure rather than into a match: a shape this cannot read is a binary this suite has not
/// identified, which is the `launch_only_root.rs` stub case exactly.
fn parse_version(output: &str) -> Option<&str> {
    // One complete predicate rather than a `find` plus a `filter`: the two-stage form rejects the
    // *whole output* when its first candidate is a truncated `1.`, instead of reading on.
    output
        .split_whitespace()
        .map(|tok| tok.strip_suffix('.').unwrap_or(tok))
        .find(|tok| {
            tok.starts_with(|c: char| c.is_ascii_digit())
                && !tok.ends_with('.')
                && tok.contains('.')
                && tok.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
}

/// What the gate made of the `program` first on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// It answered `version`, which its row lists (`listed`) or this machine's conformance battery
    /// admitted for this exact binary.
    Admitted { version: String, listed: bool },
    /// Not on `PATH`, or `--version` did not run and exit 0. Callers phrase this one themselves.
    Absent,
    /// It ran and is not admitted: `found` is the version it reported, if it reported one, and
    /// `diagnosis` says why, naming the program, the version and the probes or command that matter.
    Refused {
        found: Option<String>,
        diagnosis: String,
    },
}

/// **The gate for `program`**, decided once per process: `on_path` is asked many times a suite, and
/// a harness's `--version` is a process start each time.
///
/// The decision is keyed on the program alone, with `PATH` read at the first call: a test that
/// changes `PATH` process-wide afterwards still gets the first answer. No test does; one that
/// needs another binary under the same name passes its own `PATH` to [`gate_at`].
pub fn gate(program: &str) -> Gate {
    static DECIDED: std::sync::Mutex<Vec<(String, Gate)>> = std::sync::Mutex::new(Vec::new());
    let mut decided = DECIDED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, g)) = decided.iter().find(|(p, _)| p == program) {
        return g.clone();
    }
    let g = match gated_row(program) {
        Some(spec) => gate_at(
            program,
            spec,
            &std::env::var_os("PATH").unwrap_or_default(),
            &marion_harness::row_file::conformance_state().unwrap_or_default(),
        ),
        None => Gate::Refused {
            found: None,
            diagnosis: format!("{program:?} has no row with verified releases"),
        },
    };
    decided.push((program.to_string(), g.clone()));
    g
}

/// How long the gate waits on a harness's `--version`. The slowest measured is copilot's node start,
/// well under ten seconds on a cold disk; a binary still silent at this ceiling is hung, not slow.
const VERSION_BUDGET: Duration = Duration::from_secs(30);

/// `command`'s output, or `None` if it has not exited within `budget`, in which case its whole
/// process group is killed.
///
/// **The probe leads its own group, and the group is what is killed.** An npm-installed harness is
/// a node wrapper that starts the native binary as its child; killing only the wrapper would leave
/// that binary running, reparented to pid 1.
///
/// **The child stays on this thread, unreaped until after the kill.** A helper thread only drains
/// the pipes and reports when both reach EOF; nothing waits on the leader before `killpg`, so its
/// pid (and with it the group id) cannot have been freed and reused by an unrelated process. The
/// wait blocks on the drain's channel, so nothing re-asks on a timer.
fn output_within(
    command: &mut Command,
    budget: Duration,
) -> Option<std::io::Result<std::process::Output>> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // A harness a sibling test thread just wrote can still be open for writing in a child that
    // thread forked: tried again rather than read as absent.
    let mut child = match marion_harness::probe::spawn_past_busy_text(|| command.spawn()) {
        Ok(child) => child,
        Err(e) => return Some(Err(e)),
    };
    let mut stdout = child.stdout.take().expect("stdout is piped above");
    let mut stderr = child.stderr.take().expect("stderr is piped above");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Both pipes at once: a probe that fills stderr while stdout is being read to EOF would
        // otherwise block on its write and look hung.
        let err = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf).map(|_| buf)
        });
        let mut out = Vec::new();
        let out = stdout.read_to_end(&mut out).map(|_| out);
        let err = err.join().unwrap_or_else(|_| Ok(Vec::new()));
        let _ = tx.send(out.and_then(|out| err.map(|err| (out, err))));
    });
    let deadline = Instant::now() + budget;
    let Ok(drained) = rx.recv_timeout(budget) else {
        cut_off(&mut child);
        return None;
    };
    // Pipes at EOF is not an exit: a leader that closed its stdio can still hang. Its exit is
    // waited on under what is left of the same budget, again without reaping it, so a cut-off
    // still kills a group whose id is this leader's.
    let pid = child.id();
    let (exited, on_exit) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        exited_unreaped(pid);
        let _ = exited.send(());
    });
    let status = leader_exit(
        &mut child,
        &on_exit,
        deadline.saturating_duration_since(Instant::now()),
    )?;
    Some(drained.and_then(|(stdout, stderr)| {
        status.map(|status| std::process::Output {
            status,
            stdout,
            stderr,
        })
    }))
}

/// The leader's exit status once `on_exit` says it has exited, within `remaining`; `None` if it
/// is still running then, in which case its group is killed. At the deadline it is looked at once
/// more, so a leader exiting just as the budget runs out is read, not reported as a hang.
fn leader_exit(
    child: &mut std::process::Child,
    on_exit: &std::sync::mpsc::Receiver<()>,
    remaining: Duration,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    if on_exit.recv_timeout(remaining).is_ok() {
        return Some(child.wait());
    }
    match child.try_wait() {
        Ok(Some(status)) => Some(Ok(status)),
        _ => {
            cut_off(child);
            None
        }
    }
}

/// Kills `child`'s whole group and reaps it.
fn cut_off(child: &mut std::process::Child) {
    // SAFETY: plain libc call. The leader is unreaped, so its pid is still this group's id.
    let _ = unsafe { killpg(child.id() as i32, SIGKILL) };
    let _ = child.wait();
}

/// Blocks until child `pid` has exited, leaving it unreaped (`waitid` with `WNOWAIT`), so the
/// caller still holds its pid. Returns at once for a pid that is no longer this process's child.
fn exited_unreaped(pid: u32) {
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid out-parameter for `waitid`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

/// [`gate`]'s decision, with `PATH` and the state directory the conformance records live under
/// passed in, so every branch is testable against a scratch tree.
///
/// The version is read the way marion reads every harness's (`marion_harness::probe`: the row's
/// no-self-update switch, no login variable), from the exact file `program` resolves to, which is
/// also the file whose digest a conformance record must carry.
pub fn gate_at(program: &str, spec: &HarnessSpec, path: &OsStr, state: &Path) -> Gate {
    let Some(binary) = marion_harness::row_file::binary_on_path(program, path) else {
        return Gate::Absent;
    };
    let mut probe = match marion_harness::probe::version_probe(spec, &binary) {
        Ok(p) => p,
        Err(e) => {
            return Gate::Refused {
                found: None,
                diagnosis: format!("`{program} --version` is not run: {e}"),
            };
        }
    };
    let out = match output_within(&mut probe.command, VERSION_BUDGET) {
        Some(Ok(out)) if out.status.success() => out,
        Some(_) => return Gate::Absent,
        None => {
            return Gate::Refused {
                found: None,
                diagnosis: format!(
                    "`{program} --version` did not answer within {}s, so the binary on PATH \
                     ({}) is unidentified and was killed",
                    VERSION_BUDGET.as_secs(),
                    binary.display()
                ),
            };
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let Some(found) = parse_version(&stdout) else {
        return Gate::Refused {
            found: None,
            diagnosis: format!(
                "`{program} --version` printed nothing this can read a version out of, so the \
                 binary on PATH is unidentified — and an unidentified binary is not a match. This \
                 suite expects {program} {pinned} or later.\nstdout: {stdout:?}\nstderr: {stderr:?}\n\
                 (A shell stub named for a harness is a real shape in this repo — see \
                 `launch_only_root.rs` — which is why this is a failure and not a shrug.)",
                pinned = spec.verified[0],
                stdout = stdout.trim(),
                stderr = String::from_utf8_lossy(&out.stderr).trim(),
            ),
        };
    };
    let admitted = |listed| Gate::Admitted {
        version: found.to_string(),
        listed,
    };
    if spec.verified.contains(&found) {
        return admitted(true);
    }
    match admitted_by_record(program, spec, found, &binary, state) {
        Ok(()) => admitted(false),
        Err(why) => Gate::Refused {
            found: Some(found.to_string()),
            diagnosis: format!(
                "`{program} --version` reports {found}, which {program}'s row does not list \
                 ({listed:?}), and {why}.\nA cell that drives {found} and reports a result \
                 attributed to the listed releases is the silent pass this gate rules out; the \
                 conformance battery is the basic test that admits it.",
                listed = spec.verified,
            ),
        },
    }
}

/// **The gate's second way in: `found`, a version the row does not list, is admitted when it is
/// at or past the pin and the conformance record under `state` admits `binary` at that version**
/// (`row_file::admission_for`: every probe the row's newest verified release passed in the
/// committed matrix passed again, on this file). `Err` is the clause the diagnosis carries.
fn admitted_by_record(
    program: &str,
    spec: &HarnessSpec,
    found: &str,
    binary: &Path,
    state: &Path,
) -> Result<(), String> {
    use marion_harness::row_file::{Admission, admission_for};
    let pinned = spec.verified[0];
    if marion_harness::cmp_versions(found, pinned).is_lt() {
        return Err(format!(
            "it is older than {pinned}, the release these behaviours were first measured on"
        ));
    }
    let row = row_name(program);
    match admission_for(state, row, found, binary) {
        Admission::Admitted => Ok(()),
        other => Err(other.describe(row, found)),
    }
}

/// Is `program` on `PATH`, runnable, **and — for a harness this suite gates — a release the gate
/// admits?** Callers assert on this and name the binary rather than skipping: §9's standing rule is
/// that a criterion which quietly passes on a machine that cannot run it is worth less than no
/// criterion.
///
/// # What the gate admits
///
/// A release the row's `verified` list names, or one at or past the pin that this machine's
/// conformance battery admitted, for that exact binary (its digest is in the record):
/// `scripts/conformance.sh --harness <row>` writes the record, and the nightly canary runs it on
/// every unlisted release and lists the ones it admits. Every harness updates itself, so this is
/// what keeps a newer release that passes the basic tests working with no one's hand on it, while a
/// release that breaks them fails here naming the program, the version and the probes.
///
/// # Why the version is checked here and not in a second function
///
/// `<prog> --version` exiting 0 says only that *something named* `codex` exists. That is not a
/// hypothetical gap in this repository: `launch_only_root.rs` deliberately puts a shell stub named
/// `codex` on `PATH`, so a name collision is a shape that already occurs here — and the matrix's
/// whole claim is that it drove the real binary. Folding the check into the function every call site
/// already calls means it cannot be forgotten and no call site changes.
///
/// # Why a refused release panics rather than returning `false`
///
/// *Absent* and *present but refused* are different findings and only one of them the caller can
/// phrase. A caller can write "put `codex` on PATH"; it cannot say what is wrong with the `codex`
/// that is there. And `cargo test` captures the stdout of a passing test, so a warning on a green
/// run is invisible: the choice is between failing and passing quietly.
pub fn on_path(program: &str) -> bool {
    if gated_row(program).is_none() {
        // Not a harness this suite gates — `git`, and anything else a caller probes for. Presence
        // is the whole question for those.
        return Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success());
    }
    match gate(program) {
        Gate::Admitted { .. } => true,
        Gate::Absent => false,
        Gate::Refused { diagnosis, .. } => panic!("{diagnosis}"),
    }
}

/// **Where `program` really lives on `PATH`**, so a test's wrapping shim can hand its one real
/// invocation on.
///
/// Panics rather than returning an `Option`: every caller is a test that drives the REAL program
/// and has already passed [`on_path`] or [`harness_available`], so an absent binary here is a
/// broken bed, not a skip. Resolved through `which` rather than by walking `PATH` here, so the
/// answer is the one a shell spawned with this process's environment would reach.
pub fn which(program: &str) -> PathBuf {
    let out = Command::new("which")
        .arg(program)
        .output()
        .expect("`which` runs");
    assert!(
        out.status.success(),
        "this test drives a REAL {program}; put it on PATH"
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// **Whether a version a harness reported is one the gate admits for `program`**: a token of
/// `reported` that starts with a version its row lists, or with the one [`gate`] admitted this
/// process (`2.1.285 (Claude Code)`, `codex-cli 0.159.2`). A test that checks a recorded
/// `harness_version` asks this rather than the list directly.
pub fn reported_version_admitted(program: &str, reported: &str) -> bool {
    let spec = gated_row(program)
        .unwrap_or_else(|| panic!("{program:?} has no row with verified releases"));
    if reported_in(spec.verified, reported) {
        return true;
    }
    match gate(program) {
        Gate::Admitted { version, .. } => reported_in(&[version.as_str()], reported),
        _ => false,
    }
}

/// The list half of [`reported_version_admitted`], pure.
fn reported_in(accepted: &[&str], reported: &str) -> bool {
    reported
        .split_whitespace()
        .any(|token| accepted.iter().any(|v| token.starts_with(v)))
}

/// The one variable that turns an absent harness into a named skip instead of a failure.
///
/// Set to `1` by `.github/workflows/ci.yml` and by nothing else. Every other value, including
/// unset, leaves [`on_path`]'s rule in force.
pub const NO_HARNESSES: &str = "MARION_CI_NO_HARNESSES";

/// **Is `program` installed at the pinned release — and if not, may this test skip?**
///
/// `true` means the binary is there and the caller should proceed. `false` is returned *only* on a
/// runner that has declared it has no harnesses, and the caller's contract is to `return`
/// immediately:
///
/// ```ignore
/// if !marion_testsupport::harness_available("claude") {
///     return;
/// }
/// ```
///
/// # Why this exists at all, when [`on_path`] already decides
///
/// §9's rule — a criterion that quietly passes on a machine which cannot run it is worth less than
/// no criterion — is about *silence*, not about failing. A GitHub runner has no `claude` and never
/// will; tests that spawn one there were not reporting a gap in marion, they were reporting the
/// runner. Ten of them failed in the first CI run, six inside one shared fixture, and every
/// failure named a journal invariant rather than the missing binary. A CI that is red for a reason
/// nobody can act on is the fastest way to teach a team to stop reading it.
///
/// So the gate moves *up*, to the first statement of the test, and the skip is loud and named. It
/// is not a weakening: the tests are unchanged, they still drive the real binary, and on every
/// machine that has one — every developer's, and the local matrix MILESTONES describes — absence
/// still panics. Only a runner that has explicitly said `MARION_CI_NO_HARNESSES=1` may step over,
/// and it has to say which harness it stepped over as it goes.
///
/// # Why absence still panics off the runner
///
/// A skip that is available everywhere is a skip that will be taken everywhere, and the pin gate
/// exists to make "I ran the matrix" mean something. Keeping the panic as the default means the
/// variable — and the one workflow file that sets it — is the complete list of places these
/// criteria do not run.
pub fn harness_available(program: &str) -> bool {
    if on_path(program) {
        return true;
    }
    if !declared_no_harnesses() {
        panic!(
            "this test drives a REAL {program}; put `{program}` ({version}) on PATH.\n\
             A runner with no harness binaries at all sets {NO_HARNESSES}=1, and this test then \
             skips by name instead of failing. Nothing else sets it.",
            version = pinned_version(program),
        );
    }
    announce_skip(
        &format!("no `{program}`"),
        &format!("drives a real {program}"),
    );
    false
}

/// Has this machine declared, explicitly, that it has no harness binaries at all?
///
/// Exposed beside [`harness_available`] for the tests whose requirement is not one named program —
/// `doctor`'s ACP row needs *any* of five agents to answer `initialize`, and there is no single
/// binary to ask about. Those tests keep every assertion that needs no binary and consult this only
/// for the half that does.
pub fn declared_no_harnesses() -> bool {
    std::env::var_os(NO_HARNESSES).as_deref() == Some(std::ffi::OsStr::new("1"))
}

/// One line on the real stderr saying a criterion did not run, and why.
///
/// Not `eprintln!`: libtest captures that macro's output for a passing test, so a skip announced
/// through it is invisible on exactly the runs it describes. The `std::io::stderr()` handle is not
/// captured. That is the whole difference between a loud skip and a silent pass.
pub fn announce_skip(because: &str, what: &str) {
    let _ = writeln!(
        std::io::stderr(),
        "SKIPPED ({because}, {NO_HARNESSES}=1): a test that {what}"
    );
}

// --- scratch directories -------------------------------------------------------------------------

/// A scratch dir that removes itself.
///
/// `Drop`, and not a `remove_dir_all` at the end of each test: a failing assertion unwinds straight
/// past any trailing cleanup, so an explicit call leaks on exactly the runs that fail — the ones a
/// developer re-runs most. `Drop` catches those, plus every `?`, every early return and every
/// `.expect` in a helper the test called. See `scratch_removes_its_dir_when_a_test_panics`, which
/// is that property as a test rather than as a claim.
pub struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // Ignored: the dir may already be gone (a test that removed it, or a `git worktree remove`
        // that took it), and a cleanup failure must not mask the test's own verdict.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// `Deref` alone is not enough: it coerces `&Scratch` to `&Path` at a call site expecting one, but
/// a generic `P: AsRef<Path>` — `std::fs::remove_dir_all`, `Command::current_dir` — never triggers
/// that coercion and fails to compile instead. Two of the eleven copies this replaces had learnt
/// that separately; the other nine made their call sites write `&*dir`.
impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// This thread's id as `t<n>`, and **never `{:?}` of the `ThreadId` itself**.
///
/// `ThreadId`'s `Debug` is `ThreadId(6)`, and those parentheses cannot go into these directory
/// names: a scratch path is interpolated into shell commands that a real harness then *parses*.
/// Measured, not hypothetical — `permission_round_trip.rs` drives Claude Code through a
/// `touch <scratch>/outside/marion-s9.txt`, and with `ThreadId(6)` in the path the CLI answered
/// `"decision_reason": "Parse error"` and the permission frame came back missing the fields the
/// test asserts on. The id is only there to separate parallel tests, so any injective spelling
/// does; this one is `[A-Za-z0-9]` and safe to interpolate anywhere.
///
/// `ThreadId::as_u64` is unstable, so the number is recovered from the `Debug` form rather than
/// asked for directly.
fn thread_tag() -> String {
    let raw = format!("{:?}", std::thread::current().id());
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    // A `Debug` form carrying no digits would make every thread share a tag, which is the collision
    // this exists to prevent — so fall back to the sanitised whole string rather than to nothing.
    if digits.is_empty() {
        format!(
            "t{}",
            raw.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
        )
    } else {
        format!("t{digits}")
    }
}

/// A fresh scratch dir named `marion-{tag}-{pid}-{thread}`, under a **short** private root.
///
/// **Bind the returned guard for as long as the test needs the directory.** `scratch("x").join("y")`
/// drops the guard at the end of that statement and deletes the dir out from under the run, and a
/// bare `let _ =` drops it on the spot. That is the one trap in this shape, and it is the reason
/// the constructor returns the guard rather than a path.
///
/// Four properties, each of which some copy of this had and some did not:
///
/// - **the thread id is in the name**, so two tests in one binary — which `cargo test` runs in
///   parallel by default — cannot collide on a tag. See [`thread_tag`] for why it is not spelled
///   the obvious way;
/// - **the dir is removed on the way *in* as well**, because a run killed hard enough to skip
///   `Drop` leaves a name behind and pids recycle, so a later run can inherit that exact name;
/// - **the path is canonicalised**, because on macOS the temp dir is a symlink (`/var` →
///   `/private/var`) and a test that compares a path marion reported against one built here
///   otherwise compares two spellings of the same directory;
/// - **the root is [`SCRATCH_ROOT`] and not `std::env::temp_dir()`**, which is what keeps a test's
///   leavings inside the directory this guard deletes. See [`SCRATCH_ROOT`].
pub fn scratch(tag: &str) -> Scratch {
    let p = scratch_root().join(leaf(tag));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    Scratch(p.canonicalize().expect("scratch dir canonicalises"))
}

/// Write `body` to `path` and make it executable (`0o755`): the stub, shim or wrapper a test puts
/// on a `PATH` or hands a launcher as its program.
///
/// Both steps panic naming the path, because a fixture that half-exists fails later and somewhere
/// else — an exec that reports `EACCES`, or a `PATH` lookup that silently falls through to a real
/// binary the test meant to stand in for.
pub fn write_executable(path: &Path, body: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body)
        .unwrap_or_else(|e| panic!("the executable {} is written: {e}", path.display()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| panic!("{} is made executable: {e}", path.display()));
}

/// Append `bytes` to the file at `path`, creating it if need be, as a journal writer's `O_APPEND`
/// `write` would land them — including a partial line or a line that is not a record at all,
/// which is why this takes bytes and not a record: the tests that use it are about what a reader
/// does with what a writer *actually* left behind, torn tails and garbage included.
pub fn append(path: &Path, bytes: &[u8]) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {} for append: {e}", path.display()));
    f.write_all(bytes)
        .unwrap_or_else(|e| panic!("append to {}: {e}", path.display()));
}

/// The directory name, **bounded**, because a caller cannot be asked to count bytes.
///
/// A scratch directory is where a test's supervisor socket ends up, and §2 gives that path 103
/// bytes before it moves the socket into a shared directory nothing ever cleans (see
/// [`SCRATCH_ROOT`]). Some tags are built at runtime — `journal_wiring.rs` composes one out of two
/// agent types — so "keep tags short" is a rule that cannot be checked at the call site and was
/// measured being broken: that file was the last one still leaving lock files behind after the root
/// was shortened.
///
/// So the bound lives here. A name that fits is used as it is; one that does not keeps its readable
/// head and ends in a hash of the **whole** tag, so two long tags that share a prefix still get two
/// directories. Uniqueness is what the name is for; legibility is what is traded, and only for the
/// tags that could not have both.
fn leaf(tag: &str) -> String {
    use std::hash::{Hash, Hasher};
    let whole = format!("marion-{tag}-{}-{}", std::process::id(), thread_tag());
    if whole.len() <= MAX_SCRATCH_LEAF {
        return whole;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut h);
    let digest = format!("{:04x}", h.finish() as u16);
    let suffix = format!("-{digest}-{}-{}", std::process::id(), thread_tag());
    let head: String = tag
        .chars()
        .take(MAX_SCRATCH_LEAF.saturating_sub("marion-".len() + suffix.len()))
        .collect();
    format!("marion-{head}{suffix}")
}

/// How long [`scratch`]'s directory name may be.
///
/// Arithmetic, not taste. §2 allows 103 bytes for a socket path; `/private/tmp/mn-<uid>/` spends 20
/// (the canonical spelling — `/tmp` is a symlink on macOS), `/<project-hash>/supervisor.sock` spends
/// 29 at the other end, and several tests put their state one directory deeper (`dir.join("state")`)
/// for another 6. That leaves 48, and this is 48.
const MAX_SCRATCH_LEAF: usize = 48;

/// Where [`scratch`] puts its directories, and **why it is not the system temp dir**.
///
/// `std::env::temp_dir()` on macOS is a 48-byte `/var/folders/…/T` path. A state directory under it
/// plus a tag, a pid and a thread id runs to ~80 bytes, and a supervisor socket is that plus
/// `/<project-hash>/supervisor.sock` — over the 103 bytes a `sun_path` may hold. §2's rule then puts
/// that project's socket, lock, identity and log in the **shared** `/tmp/marion-<uid>` fallback
/// instead, where they are outside the directory this guard removes and nothing ever deletes them.
///
/// The lock file is the one that accumulates, because `socket.rs` never unlinks a lock and must not:
/// two processes that `open` one path either side of an unlink hold two inodes, `flock` them
/// independently, and both conclude they are alone. There is no reclamation rule that survives that
/// argument — an age test proves nothing about a live holder, and a liveness test races the holder
/// that is about to open it — so the accumulation is fixed at its source instead, which is *this
/// path being long*. Measured: `/tmp/marion-<uid>` grew by ~122 lock files per full workspace run
/// and had reached 3,893 of them, one per (project root × run), all of them from tests.
///
/// A short root brings every scratch-based project back under §2's primary branch, so its socket
/// lives inside the scratch directory and leaves with it. It is spelled `/tmp/mn-<uid>` and not
/// something legible because the budget is 103 bytes for the *whole* socket path and this suite's
/// longest tag already spends 29 of them — `socket.rs`'s
/// `a_scratch_projects_socket_fits_without_falling_back_to_the_shared_tmp_directory` is what keeps
/// that arithmetic honest, and a longer root was measured overrunning it by four bytes. The uid is in the name for the reason
/// `socket.rs`'s own `/tmp` fallback carries one: `/tmp` is shared, and a directory two users can
/// both claim is a directory either can serve a socket through.
pub const SCRATCH_ROOT: &str = "/tmp/mn";

fn scratch_root() -> PathBuf {
    // SAFETY: `getuid` reads the calling process's real uid and cannot fail.
    let root = PathBuf::from(format!("{SCRATCH_ROOT}-{}", unsafe { getuid() }));
    // 0700 rather than the umask's default: /tmp is world-writable, and a test's scratch tree holds
    // sockets that grant control of a fleet exactly as a real one's does.
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root);
    static RECLAIMED: std::sync::Once = std::sync::Once::new();
    RECLAIMED.call_once(|| reclaim_orphans(&root));
    root
}

/// Remove every scratch dir whose owning test process is gone — **the leak `Drop` cannot close.**
///
/// A guard runs on a panic, but not on a `SIGKILL`, a timed-out run killed by its runner, an
/// abort, or a `static` that holds one for the process's life; and a dir a still-running
/// descendant was writing into survives the guard's `remove_dir_all`. Measured: 135 such dirs,
/// 1.1 GB, under this root after one day of runs, some of them 128 MB harness homes. Each
/// [`leaf`] ends in `-<pid>-t<n>`, so the owner is on the name; once per process, the first
/// [`scratch`] removes the dirs whose owner no longer exists ([`alive`]'s `ESRCH`, never a
/// guess). A live pid — this process, a parallel test binary, or an unrelated process that
/// reused the number — keeps its dir, and nothing else under the root is touched.
fn reclaim_orphans(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if let Some(pid) = name.to_str().and_then(owner_pid)
            && !alive(pid)
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The pid a [`leaf`] name was minted by: `marion-<tag>-<pid>-t<thread>`, or `None` for any
/// other name.
fn owner_pid(leaf: &str) -> Option<i32> {
    let mut parts = leaf.strip_prefix("marion-")?.rsplitn(3, '-');
    let thread = parts.next()?.strip_prefix('t')?;
    let pid = parts.next()?.parse().ok()?;
    let tag = parts.next()?;
    let named =
        !thread.is_empty() && thread.chars().all(|c| c.is_ascii_alphanumeric()) && !tag.is_empty();
    (named && pid > 0).then_some(pid)
}

unsafe extern "C" {
    fn getuid() -> u32;
}

// --- git ------------------------------------------------------------------------------------------

/// `git` in `dir`, asserting it succeeded and handing back its stdout.
///
/// Returning the output rather than `()` is the superset: the five copies that discarded it keep
/// compiling unchanged, and the two that needed it stop being a separate function.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = marion_harness::env_filter::git_command()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A one-commit repository for a child to worktree, at `root/repo`.
///
/// Its own repository and never marion's: the run under test writes to it. The identity is passed
/// per-invocation rather than configured globally, so the fixture does not depend on — or disturb —
/// whatever `user.email` the machine running the suite has.
///
/// # Why maintenance is turned off before anything is committed
///
/// `git commit` ends by launching `git maintenance run --auto --detach`. Detached means `git
/// commit` returns while that process is still alive, and it holds
/// `.git/objects/maintenance.lock` for as long as it runs — so a fixture repository hands back an
/// object store with an extra file in it that nobody wrote and that disappears again on its own,
/// a millisecond or so later.
///
/// A test that *counts* that store then reads two different numbers for one unchanged repository.
/// `root_change_record.rs`'s NC-3 does exactly that — `.git/objects` before a snapshot and after —
/// and it failed on both CI runners, where the machine is slow enough for the lock to still be
/// there at the first read and gone by the second. The store appeared to *shrink*, under an
/// assertion whose subject is marion never writing into it.
///
/// Set in the repository's own config rather than by scrubbing `HOME` or passing `-c` per call:
/// local config outranks global and system, so this holds for every git process that opens this
/// repository, including the ones marion itself spawns and any this suite never launched. A
/// fixture is a throwaway with four objects in it; there is nothing here to maintain and no reason
/// to let a second writer into a directory the tests are measuring.
pub fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/keep.txt"), "keep\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    // Before the first commit, because the first commit is already one of the launchers.
    git(&repo, &["config", "maintenance.auto", "false"]);
    git(&repo, &["config", "gc.auto", "0"]);
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=marion@example.invalid",
            "-c",
            "user.name=marion",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    repo
}

// --- a finished project ----------------------------------------------------------------------

/// What [`finished_project`] wrote: the two nodes' ids, and the branch the child landed.
#[derive(Debug, Clone)]
pub struct FinishedProject {
    pub root: marion_core::contract::AgentId,
    pub child: marion_core::contract::AgentId,
    pub branch: String,
}

/// A project whose run is over, written the way a supervisor leaves one — **a journal and a
/// contract, and no supervisor**: a root, and a child it delegated to that reported, landed a
/// branch and exited, with the spend its run recorded.
///
/// Written straight to the files, not through a supervisor, because the readers under test are the
/// ones that must work once every supervisor is gone; starting one to produce the fixture would
/// leave a lock and a socket behind for exactly the property those tests check. The harness is the
/// registry's first row and the agent type is not a built-in, so nothing here names a harness and
/// the recorded bound is what makes each node describable.
pub fn finished_project(project: &marion_core::paths::ProjectDir) -> FinishedProject {
    use marion_core::contract::{AgentId, ExitStatus, ProcessExit, TaskId, TokenUsage};
    use marion_core::journal::{
        ContractPersisted, Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, StateChanged,
        UsageRecorded, WriterId, encode,
    };
    use marion_core::node::NodeState;

    let harness = marion_core::harness::Harness::ALL[0];
    let root = AgentId("01a0e64e-1df0-7424-9ced-c33bec562fad".into());
    let child = AgentId("01a0e64e-433f-7ed8-aa38-c369b4e5918c".into());
    let task = TaskId("01a0e64e-433c-7375-a613-f205ee674759".into());
    let branch = format!("marion/{}", task.0);
    let intent = |id: &AgentId, parent: Option<&AgentId>, task: Option<&TaskId>| {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id.clone(),
            parent_id: parent.cloned(),
            agent_type: "fixture-worker".into(),
            harness,
            depth: u32::from(parent.is_some()),
            task_id: task.cloned(),
            timeout_secs: Some(300),
            verification: vec![],
            review_of: None,
            race: None,
            budget: None,
            workflow: None,
            trust: Default::default(),
        })
    };
    let spawned = |id: &AgentId| {
        RecordKind::Spawned(Spawned {
            agent_id: id.clone(),
            harness_version: "1.0.0".into(),
            model: None,
            pid: None,
            start_id: None,
            provider: None,
            route: None,
            credential: None,
        })
    };
    let running = |id: &AgentId| {
        RecordKind::StateChanged(StateChanged {
            agent_id: id.clone(),
            state: NodeState::Running,
            reason: None,
        })
    };
    let exited = |id: &AgentId| {
        RecordKind::Exited(Exited {
            agent_id: id.clone(),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "child exited with code 0".into(),
            },
        })
    };
    let kinds = vec![
        intent(&root, None, None),
        spawned(&root),
        running(&root),
        intent(&child, Some(&root), Some(&task)),
        spawned(&child),
        running(&child),
        RecordKind::UsageRecorded(UsageRecorded {
            agent_id: child.clone(),
            usage: TokenUsage {
                input: 1200,
                output: 340,
                cache_read: 5000,
                cache_write: 0,
                reasoning: None,
            },
            turns: vec![],
        }),
        RecordKind::ContractPersisted(ContractPersisted {
            changed: None,
            agent_id: child.clone(),
            task_id: task.clone(),
            requester: root.clone(),
            status: Some(ExitStatus::Ok),
            review: None,
        }),
        exited(&child),
        exited(&root),
    ];
    std::fs::create_dir_all(project.path()).unwrap();
    let mut journal = Vec::new();
    for (seq, kind) in kinds.into_iter().enumerate() {
        journal.extend(
            encode(&JournalRecord {
                writer: WriterId("fixture-writer".into()),
                seq: seq as u64,
                ts: marion_core::encoding::SystemTime::from_unix_millis(
                    1_790_000_000_000 + seq as u64,
                ),
                mono_ns: seq as u64,
                provenance: marion_core::ir::Provenance::marion(),
                src_seq: None,
                kind,
            })
            .unwrap(),
        );
    }
    std::fs::write(project.journal(), journal).unwrap();

    let at = "2026-09-28T04:38:03.839Z";
    let capped =
        |v: &str| serde_json::json!({"value": v, "truncated": false, "original_bytes": v.len()});
    let contract = serde_json::json!({
        "task_id": task.0,
        "requester": root.0,
        "child": {"harness": harness, "version": "1.0.0", "model": null},
        "repo": {"git_common_dir": null, "head_branch": null},
        "base_commit": null,
        "workspace": {"Worktree": {
            "path": project.agent(&child).worktree(),
            "branch": branch,
        }},
        "instructions": capped("Add a --top N option to wordfreq.py."),
        "acceptance_criteria": [capped("the tests pass")],
        "allowed_tools": [],
        "scope_ceiling": ["**"],
        "scope_requested": [],
        "timeout": 300,
        "verification": [],
        "timestamps": {"spawned": at, "first_output": null, "reported": at, "exited": at},
        "completion": {
            "status": "Ok",
            "died_before_gate": false,
            "reported_early": false,
            "held_to_timeout": false,
            "live_descendants_at_report": [],
            "narrative": capped("Added --top N and the tests pass."),
            "narrative_synthesized": false,
            "result_commits": [],
            "changed_paths": ["wordfreq.py"],
            "acceptance_criteria_omitted": 0,
            "changed_paths_omitted": 0,
            "result_commits_omitted": 0,
            "scope_violations_omitted": 0,
            "scope_enforced": true,
            "scope_violations": [],
            "diff": null,
            "evidence": [],
            "evidence_omitted": 0,
            "exit": {"code": 0, "signal": null, "description": "child exited with code 0"},
            "branch": branch,
        },
    });
    let contracts = project.agent(&child).contracts_dir();
    std::fs::create_dir_all(&contracts).unwrap();
    std::fs::write(
        project.agent(&child).contract(&task),
        serde_json::to_vec_pretty(&contract).unwrap(),
    )
    .unwrap();
    FinishedProject {
        root,
        child,
        branch,
    }
}

/// **`agent`'s files as a build that ran it on the retired harness `retired` left them**: its
/// journal `SpawnIntent`, every contract under its dir and every vendor frame of its
/// `events.jsonl` name `retired` in place of the harness they were written with. Everything else
/// is byte-for-byte what the project held, so a test reads exactly an old state dir.
pub fn retire_node(
    project: &marion_core::paths::ProjectDir,
    agent: &marion_core::contract::AgentId,
    retired: &str,
) {
    use serde_json::Value;
    let rewrite = |path: &std::path::Path, edit: &dyn Fn(&mut Value)| {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let mut out = String::new();
        for line in text.lines() {
            let mut v: Value = serde_json::from_str(line).unwrap();
            edit(&mut v);
            out.push_str(&serde_json::to_string(&v).unwrap());
            out.push('\n');
        }
        std::fs::write(path, out).unwrap();
    };
    let name = Value::from(retired);
    // `get_mut`, never `v[key]` on a miss: indexing would add the key to every other record.
    rewrite(&project.journal(), &|v| {
        let intent = v.pointer_mut("/kind/SpawnIntent");
        if let Some(i) = intent.filter(|i| i["agent_id"] == agent.0.as_str()) {
            i["harness"] = name.clone();
        }
    });
    let dir = project.agent(agent);
    if let Ok(contracts) = std::fs::read_dir(dir.contracts_dir()) {
        for c in contracts {
            let path = c.unwrap().path();
            let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            v["child"]["harness"] = name.clone();
            std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        }
    }
    rewrite(&dir.events(), &|v| {
        if let Some(frame) = v.pointer_mut("/payload/Vendor") {
            frame["harness"] = name.clone();
        }
    });
}

// --- persisted contracts ---------------------------------------------------------------------------

/// One `contracts/<task_id>.json` marion persisted, and what reading it back produced.
///
/// The parse outcome is carried rather than acted on — see [`persisted_contracts`] for why the
/// judgement is deliberately somewhere else.
#[derive(Debug)]
pub struct PersistedContract {
    pub path: PathBuf,
    /// `Err` holds the diagnosis, already formatted: an unreadable file, or one whose bytes are not
    /// JSON, together with the head of what was actually there.
    pub parsed: Result<Value, String>,
}

/// Every `contracts/<task_id>.json` under `state`, read back — **without judging any of them**.
///
/// # Why the tree is walked rather than probed at a known path
///
/// `depth_gate.rs`'s reason, and it generalises: asserting *"the grandchild's contract is not at
/// `<x>`"* would pass if the contract had simply been written somewhere else, and the claim those
/// tests make is that it was **never written at all**. A walk can support that claim; a probe at a
/// guessed path cannot.
///
/// # Why the walk is fallible and the judgement is not here
///
/// These two look like competing designs and are not. The walk propagates its `read_dir` errors
/// rather than swallowing them — `harness_matrix.rs`'s reason, kept in its own terms: a directory
/// that will not enumerate is a state tree the test cannot see, and reporting the contracts it
/// happened to reach would let *"exactly one contract is persisted"* pass on a run that **wrote two
/// and hid one**.
///
/// But the *judgement* — panicking over a contract that will not read back — must not happen here,
/// and that is the part worth stating. A caller runs this while its canned provider is still up and
/// its scratch dir still on disk. Panicking at the read site unwinds through both, turning one
/// corrupt contract into the leaked processes and stranded directory the caller's next block exists
/// to prevent. So the shape is: **fallible walk, collected results, judgement at a point the caller
/// chooses — after the server is dropped and the survivor sweep has run.** [`judge`] is that point.
///
/// The count and the parse are still the same question, which is why the parse happens here at all:
/// a `filter_map` that silently dropped an unparsable file made one good contract beside one
/// corrupt one count as one, and pass.
pub fn persisted_contracts(state: &Path) -> std::io::Result<Vec<PersistedContract>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        // **The failing directory is named in the error, not just the root the walk started from.**
        // `io::Error` from `read_dir` carries the errno and no path, so a caller that reports only
        // its own `state` argument says "somewhere under here" about a tree that may be several
        // levels deep — which is what `worktree_reap.rs`'s own copy avoided by panicking with `dir`
        // in hand. Rebuilding it here keeps that, for every caller rather than one.
        let entries = std::fs::read_dir(dir).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("{} does not enumerate: {e}", dir.display()),
            )
        })?;
        for entry in entries {
            let p = entry?.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().is_some_and(|x| x == "json")
                && p.parent().is_some_and(|d| d.ends_with("contracts"))
            {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(state, &mut paths)?;
    // Stable across runs and machines: `read_dir` order is not, and a caller indexing `[0]` after
    // asserting a count of one should get the same file every time it fails.
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| {
            let parsed = std::fs::read(&path)
                .map_err(|e| {
                    format!(
                        "marion persisted {} and this test cannot read it back: {e}",
                        path.display()
                    )
                })
                .and_then(|bytes| {
                    serde_json::from_slice(&bytes).map_err(|e| {
                        format!(
                            "{} is not JSON: {e}\nfirst 512 bytes:\n{}",
                            path.display(),
                            String::from_utf8_lossy(&bytes)
                                .chars()
                                .take(512)
                                .collect::<String>()
                        )
                    })
                });
            PersistedContract { path, parsed }
        })
        .collect())
}

/// The judgement [`persisted_contracts`] deliberately withholds: a contract marion wrote and cannot
/// read back is a defect whichever half is wrong, so this panics naming the file.
///
/// **Call it after cleanup, not before.** That is the whole reason it is a second function.
pub fn judge(contracts: &[PersistedContract]) -> Vec<(&Path, &Value)> {
    contracts
        .iter()
        .map(|c| match &c.parsed {
            Ok(v) => (c.path.as_path(), v),
            Err(diagnosis) => panic!("{diagnosis}"),
        })
        .collect()
}

/// **The one contract marion persisted for `task_id`**, walked, judged and parsed — the
/// **uncapped** copy (`cap_for_return` applies to the returned one only), and so the only candidate
/// for a durable record of work a reaped worktree no longer holds.
///
/// For a caller with nothing left to clean up, so the walk and the judgement happen together here
/// rather than at a point the caller chooses (see [`persisted_contracts`] for when that matters).
/// The walk's error is a panic for the same reason it is propagated there: a state dir that will
/// not enumerate would otherwise report "no contract", and every claim a caller makes about what
/// did or did not survive would be made against a file it simply failed to look at. Exactly one
/// match is required, so a contract written twice fails rather than being read at random.
pub fn persisted_contract(state: &Path, task_id: &str) -> marion_core::contract::TaskContract {
    let walked = persisted_contracts(state)
        .unwrap_or_else(|e| panic!("{} does not enumerate: {e}", state.display()));
    let wanted = format!("{task_id}.json");
    let found: Vec<(&Path, &Value)> = judge(&walked)
        .into_iter()
        .filter(|(p, _)| p.file_name().is_some_and(|f| f == wanted.as_str()))
        .collect();
    let [(path, value)] = found.as_slice() else {
        let paths: Vec<&Path> = found.iter().map(|(p, _)| *p).collect();
        panic!("expected exactly one persisted contract for {task_id}, found {paths:?}");
    };
    serde_json::from_value((*value).clone())
        .unwrap_or_else(|e| panic!("{} does not parse as a contract: {e}", path.display()))
}

/// Does any string anywhere in `v` contain `needle`?
///
/// The same whole-body scan the CannedServer uses to decide a request's role (`marion_provider`'s
/// routing), restated for assertions so a test reads the provider's log the way the server read it.
pub fn carries(v: &Value, needle: &str) -> bool {
    match v {
        Value::String(s) => s.contains(needle),
        Value::Array(a) => a.iter().any(|x| carries(x, needle)),
        Value::Object(o) => o.values().any(|x| carries(x, needle)),
        _ => false,
    }
}

/// **The server's side of an S36 `codex app-server` capture** (`tests/fixtures/app-server-0.155.1/`),
/// one frame per line: exactly the stdout marion's app-server driver records for a node. The
/// captures log both directions and the provider as `{t, dir, msg}` lines; this keeps `dir: "s2c"`.
pub fn app_server_capture(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/app-server-0.155.1")
        .join(file);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("an S36 capture line is JSON"))
        .filter(|o| o["dir"] == "s2c")
        .map(|o| format!("{}\n", o["msg"]))
        .collect()
}

/// A `--canned --base-url` for a run whose harness is a stub that never dials it: a real listener
/// that accepts connections and answers nothing, so `marion run --canned`'s reachability probe
/// passes. The listener lives as long as the test process, so a run spawned in the background
/// still finds it.
pub fn silent_canned_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port binds");
    let port = listener.local_addr().expect("a bound address").port();
    // Deliberately kept for the process's life: the probe may run after the caller returns.
    Box::leak(Box::new(listener));
    format!("http://127.0.0.1:{port}/v1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_scans_every_string_at_every_depth_and_nothing_else() {
        let v = serde_json::json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "MARK-1"}]}],
            "n": 7,
            "flag": true,
        });
        assert!(
            carries(&v, "MARK-1"),
            "a string nested in array-in-object-in-array"
        );
        assert!(carries(&v, "user"), "a string at depth two");
        assert!(!carries(&v, "7"), "a number is not a string that spells it");
        assert!(!carries(&v, "true"), "nor is a boolean");
        assert!(
            !carries(&v, "messages"),
            "keys are not scanned, only values"
        );
    }

    /// **The property every `Scratch` in this repo was introduced for, and the one nobody had
    /// written down.** It has been proved by hand — forcing a failure, listing `$TMPDIR` — three
    /// separate times in one session, once per leaking call site. A property proved by hand three
    /// times should be a test.
    ///
    /// `catch_unwind` rather than a `#[should_panic]` test, because the assertion is about what is
    /// on disk *after* the unwind, which a `#[should_panic]` test has no way to check.
    #[test]
    fn a_scratch_name_says_which_process_owns_it() {
        assert_eq!(
            owner_pid(&leaf("detail-running")),
            Some(std::process::id() as i32)
        );
        assert_eq!(
            owner_pid(&leaf(&"x".repeat(80))),
            Some(std::process::id() as i32)
        );
        assert_eq!(owner_pid("marion-token-argv-home-13454-t2"), Some(13454));
        for other in [
            "shim-KhXu55",
            "native-ctx",
            "marion-13454-t2",
            "marion-tag-13454-2",
            "marion-tag-pid-t2",
            "marion-tag-0-t2",
        ] {
            assert_eq!(owner_pid(other), None, "{other}");
        }
    }

    /// A dir whose owner died without dropping its guard goes; one whose owner lives stays; and
    /// nothing that is not a scratch name is touched.
    #[test]
    fn orphaned_scratch_dirs_are_reclaimed_and_live_ones_are_kept() {
        let root = scratch("reclaim-root");
        let mut child = Command::new("true").spawn().expect("`true` runs");
        let dead = child.id();
        child.wait().unwrap();
        let orphan = root.join(format!("marion-killed-run-{dead}-t2"));
        let mine = root.join(format!("marion-live-run-{}-t2", std::process::id()));
        let other = root.join("shim-KhXu55");
        for dir in [&orphan, &mine, &other] {
            std::fs::create_dir_all(dir.join("home/Library/Caches")).unwrap();
        }

        reclaim_orphans(&root);

        assert!(
            !orphan.exists(),
            "pid {dead} is gone, so its dir is an orphan"
        );
        assert!(mine.is_dir() && other.is_dir());
    }

    #[test]
    fn scratch_removes_its_dir_when_a_test_panics() {
        let payload = std::panic::catch_unwind(|| {
            let dir = scratch("unwind-probe");
            assert!(dir.is_dir(), "the probe needs a dir to lose");
            std::fs::write(dir.join("work.txt"), b"in progress").unwrap();
            panic!("a test failing with work in progress");
        })
        .expect_err("the probe panics on purpose");
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert!(
            message.contains("work in progress"),
            "the probe's own panic must be what came back, not a failure inside the guard: \
             {message}"
        );
        // The guard was dropped by the unwind, so nothing it made is left. Re-derived from the same
        // inputs rather than smuggled out of the closure, which `catch_unwind` will not let it be.
        let expected = std::env::temp_dir().join(format!(
            "marion-unwind-probe-{}-{}",
            std::process::id(),
            thread_tag()
        ));
        assert!(
            !expected.exists(),
            "a panicking test left {} behind — the exact asymmetry `Drop` replaced a trailing \
             `remove_dir_all` to remove",
            expected.display()
        );
    }

    /// **A scratch path is not just a path: it gets interpolated into shell commands that a real
    /// harness parses.** The obvious `{:?}` of a `ThreadId` puts `ThreadId(6)` in the name, and
    /// `permission_round_trip.rs` — which drives Claude Code through a `touch <scratch>/…` — then
    /// gets `"decision_reason": "Parse error"` back instead of the permission frame it asserts on.
    /// That is how this was found: by an end-to-end test failing, after review had passed it.
    ///
    /// So the whole name is held to `[A-Za-z0-9._-]`, which is safe unquoted in every shell.
    #[test]
    fn a_scratch_name_carries_nothing_a_shell_would_have_to_quote() {
        let dir = scratch("shell-safe");
        let name = dir
            .file_name()
            .expect("the scratch dir has a name")
            .to_str()
            .expect("and it is utf-8");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
            "{name:?} contains a character a shell would take as syntax — parentheses are the one \
             that actually bit, via `ThreadId(6)`"
        );
    }

    #[test]
    fn scratch_removes_its_dir_on_an_ordinary_return() {
        let path = {
            let dir = scratch("ordinary");
            assert!(dir.is_dir());
            dir.to_path_buf()
        };
        assert!(!path.exists(), "{} outlived its guard", path.display());
    }

    /// The entry-side removal, which the exit-side one does not make redundant: a run killed hard
    /// enough to skip `Drop` leaves the name behind, and pids recycle.
    #[test]
    fn scratch_starts_from_an_empty_dir_even_when_the_name_was_already_taken() {
        let first = scratch("reused");
        let path = first.to_path_buf();
        std::fs::write(path.join("stale.txt"), b"from a run that died").unwrap();
        // Deliberately leaked: this is the state a killed run leaves, which `Drop` never got to.
        std::mem::forget(first);
        assert!(
            path.join("stale.txt").exists(),
            "the probe needs the stale file"
        );

        let second = scratch("reused");
        assert_eq!(*second, *path, "the same tag claims the same name");
        assert!(
            !second.join("stale.txt").exists(),
            "a re-run inherited the previous run's contents, so anything it asserted about an \
             empty tree would have been asserting about someone else's leftovers"
        );
    }

    /// `Deref` covers `&Scratch` where `&Path` is expected; `AsRef` covers a generic `P: AsRef<Path>`,
    /// which never triggers deref coercion. Both are load-bearing at real call sites, so both are
    /// exercised here — this test failing to *compile* is the assertion.
    #[test]
    fn scratch_is_accepted_both_by_a_path_parameter_and_by_a_generic_asref() {
        fn takes_path(p: &Path) -> bool {
            p.is_dir()
        }
        fn takes_asref<P: AsRef<Path>>(p: P) -> bool {
            p.as_ref().is_dir()
        }
        let dir = scratch("coercions");
        assert!(takes_path(&dir));
        assert!(takes_asref(&dir));
    }

    /// The needle is this process's own command line, which is guaranteed present — so a `survivors`
    /// that returned nothing here would be broken rather than reporting a clean machine.
    #[test]
    fn survivors_finds_a_live_process_and_parses_its_pid() {
        let needle = format!(
            "marion-survivor-probe-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        // `ps` reports argv, so the needle has to *be* in argv. BSD `sleep` takes exactly one
        // operand and would exit on a second, so the probe is a shell whose own `-c` string carries
        // the needle in a comment.
        //
        // **The `; true` is load-bearing.** A `sh -c` whose script is a single simple command is
        // `exec`ed in place by every shell here, which replaces the shell's argv with `sleep 30`
        // and takes the needle with it — measured: this test failed exactly that way without it. A
        // second command makes the shell a real process for the duration.
        let mut child = Command::new("sh")
            .args(["-c", &format!("sleep 30; true # {needle}")])
            .spawn()
            .expect("the probe process starts");
        // `spawn` returns before the child is necessarily in `ps` output.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let found = survivors(&needle);
        assert!(
            !found.is_empty(),
            "a process with {needle:?} in its command line is running and `survivors` did not see it"
        );
        assert!(
            found.iter().any(|(pid, _)| *pid == child.id() as i32),
            "the parsed pid must be the probe's own {}: {found:?}",
            child.id()
        );

        kill_hard(child.id() as i32);
        let _ = child.wait();
    }

    #[test]
    fn survivors_is_empty_for_a_needle_nothing_carries() {
        let needle = format!(
            "marion-nothing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        assert!(survivors(&needle).is_empty());
    }

    /// **The half that a naive `kill(pid, 0) == 0` gets wrong, and that this crate nearly shipped
    /// wrong.** `pid 1` is `launchd`/`init`: it always exists and is owned by root, so `kill(1, 0)`
    /// from a test process returns `-1` with `EPERM`. A version that read any non-zero return as a
    /// death would call the most obviously-running process on the machine dead — and in a leak
    /// check, "dead" is the answer that passes.
    #[test]
    fn a_process_that_exists_but_is_not_ours_is_alive_rather_than_gone() {
        assert!(
            alive(1),
            "pid 1 exists and is not ours: `kill` answers EPERM, which is a survivor and not a \
             death. Only ESRCH means gone"
        );
    }

    #[test]
    fn alive_tracks_a_process_across_its_death() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("probe starts");
        let pid = child.id() as i32;
        assert!(alive(pid), "a just-spawned `sleep 30` is alive");
        kill_hard(pid);
        // Reaped before asking again: a killed child stays a zombie — and therefore signalable —
        // until its parent waits on it, so `alive` would keep answering true.
        let _ = child.wait();
        assert!(!alive(pid), "the probe was killed and reaped");
    }

    #[test]
    fn on_path_answers_for_a_program_that_exists_and_one_that_cannot() {
        assert!(on_path("git"), "this suite cannot run without git");
        assert!(!on_path("marion-no-such-program-anywhere"));
    }

    /// The measured formats, as a test rather than as a comment. Three of the five carry a program
    /// name and two do not, so a parser that assumed either shape would be wrong about half the
    /// matrix — `codex-cli 0.146.0` is the one that punishes "first token", and copilot's
    /// sentence-final `1.0.83.` is the one that punishes "reject a token ending in a dot".
    #[test]
    fn the_version_parser_reads_all_five_measured_formats() {
        for (raw, want) in [
            ("2.1.222 (Claude Code)\n", "2.1.222"), // claude
            ("codex-cli 0.146.0\n", "0.146.0"),     // codex
            ("3.0.61\n", "3.0.61"),                 // cline: bare
            ("1.17.3\n", "1.17.3"),                 // opencode
            // copilot: two lines, and the version ends the first sentence with a full stop.
            (
                "GitHub Copilot CLI 1.0.83.\nRun 'copilot update' to check for updates.\n",
                "1.0.83",
            ),
        ] {
            assert_eq!(
                parse_version(raw),
                Some(want),
                "the measured `--version` output {raw:?} must read as {want}"
            );
        }
    }

    /// **Output this cannot read is not a match**, and that is the whole difference between this
    /// gate and the exit-status one it replaces. Every line here is something a shell stub named
    /// for a harness plausibly prints.
    #[test]
    fn output_with_no_version_in_it_is_not_read_as_one() {
        for raw in [
            "",
            "\n",
            "stub\n",
            "usage: codex [options]\n",
            "version unknown\n",
            // A trailing dot is a truncated version, not a version.
            "1.\n",
        ] {
            assert_eq!(
                parse_version(raw),
                None,
                "{raw:?} carries no version, and reading one out of it would let a stub pass the \
                 gate the real binary is supposed to pass"
            );
        }
    }

    /// The rows are the single source, so the pin they hand out is a real entry, every gated row
    /// has one, and a program no row gates has none.
    #[test]
    fn the_pin_comes_from_the_row_and_an_ungated_program_has_none() {
        let gated: Vec<&str> = gated_rows().filter_map(|s| s.program).collect();
        assert!(
            gated.contains(&"claude") && gated.contains(&"codex"),
            "{gated:?}"
        );
        for spec in gated_rows() {
            let program = spec.program.unwrap();
            assert_eq!(pinned_version(program), spec.verified[0]);
            for v in spec.verified {
                assert!(
                    parse_version(&format!("{v}\n")) == Some(v),
                    "{program}: {v:?} is listed but the gate could never read it"
                );
            }
        }
        assert!(gated_row("git").is_none());
        assert!(
            std::panic::catch_unwind(|| pinned_version("not-a-harness")).is_err(),
            "an ungated program has no pin to name"
        );
    }

    #[test]
    fn a_reported_version_is_matched_per_token_against_the_admitted_list() {
        let accepted = ["2.1.285", "0.159.2"];
        assert!(reported_in(&accepted, "2.1.285 (Claude Code)"));
        assert!(reported_in(&accepted, "codex-cli 0.159.2"));
        assert!(!reported_in(&accepted, "2.1.286 (Claude Code)"));
        assert!(!reported_in(&accepted, ""));
    }

    /// A scratch `bin/<program>` printing `prints`, and the `PATH` that holds only it.
    fn fake(dir: &Path, program: &str, prints: &str) -> (PathBuf, std::ffi::OsString) {
        let bin = dir.join("bin").join(program);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        write_executable(&bin, format!("#!/bin/sh\nprintf '%s\\n' '{prints}'\n"));
        (
            bin.canonicalize().unwrap(),
            dir.join("bin").into_os_string(),
        )
    }

    /// This machine's conformance record for `row` at `version`, taken on `binary`.
    fn record(state: &Path, row: &str, version: &str, binary: &Path, why: &[&str]) {
        let path = marion_harness::row_file::admission_path(state, row, version);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "admitted": why.is_empty(),
            "why": why,
            "binary_sha256": marion_harness::file_sha256(binary).unwrap(),
        });
        std::fs::write(path, body.to_string()).unwrap();
    }

    fn claude() -> &'static HarnessSpec {
        gated_row("claude").expect("claude is gated")
    }

    fn refused(g: Gate) -> String {
        match g {
            Gate::Refused { diagnosis, .. } => diagnosis,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A listed release passes, read through the row's own probe (the no-self-update variable
    /// reaches it: the fake prints a listed version only when it is set).
    #[test]
    fn a_listed_release_passes_and_is_probed_with_the_rows_no_self_update_switch() {
        let dir = scratch("gate-listed");
        let pinned = claude().verified[0];
        let bin = dir.join("bin/claude");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        write_executable(
            &bin,
            format!(
                "#!/bin/sh\nif [ \"$DISABLE_AUTOUPDATER\" = 1 ]; then echo '{pinned} (Claude Code)'; \
                 else echo '99.9.9 (Claude Code)'; fi\n"
            ),
        );
        let g = gate_at(
            "claude",
            claude(),
            dir.join("bin").as_os_str(),
            &dir.join("state"),
        );
        assert_eq!(
            g,
            Gate::Admitted {
                version: pinned.into(),
                listed: true
            }
        );
    }

    /// **The release the row does not list yet, admitted on its basic tests**: a record for this
    /// exact binary at this version admits it.
    #[test]
    fn a_newer_release_its_conformance_record_admits_passes() {
        let dir = scratch("gate-recorded");
        let (bin, path) = fake(&dir, "claude", "99.0.0 (Claude Code)");
        record(&dir.join("state"), "claude-code", "99.0.0", &bin, &[]);
        assert_eq!(
            gate_at("claude", claude(), &path, &dir.join("state")),
            Gate::Admitted {
                version: "99.0.0".into(),
                listed: false
            }
        );
    }

    /// **A test that rewrote `$HOME` still finds this machine's record**: the state directory is
    /// the account's, so a scratch `HOME` (`token_off_argv` sets one for codex) cannot move it,
    /// while `MARION_STATE_DIR`, which moves every marion process's state, still does.
    #[test]
    fn a_rewritten_home_does_not_move_the_records_the_gate_reads() {
        let dir = scratch("gate-home-moved");
        let account = dir.join("account");
        let scratch_home = dir.join("scratch-home").display().to_string();
        let env = |k: &str| (k == "HOME").then(|| scratch_home.clone());
        let state =
            marion_harness::row_file::conformance_state_from(env, Some(account.clone())).unwrap();
        assert_eq!(state, account.join(".local/state/marion"));

        let (bin, path) = fake(&dir, "claude", "99.0.0 (Claude Code)");
        record(&state, "claude-code", "99.0.0", &bin, &[]);
        assert_eq!(
            gate_at("claude", claude(), &path, &state),
            Gate::Admitted {
                version: "99.0.0".into(),
                listed: false
            }
        );
        // Read through the scratch HOME, as the gate did, the same record is not found.
        let moved = marion_harness::row_file::conformance_state_from(env, None).unwrap();
        assert_ne!(moved, state);
        assert!(matches!(
            gate_at("claude", claude(), &path, &moved),
            Gate::Refused { .. }
        ));

        let explicit = dir.join("explicit").display().to_string();
        let env = |k: &str| match k {
            "MARION_STATE_DIR" => Some(explicit.clone()),
            "HOME" => Some(scratch_home.clone()),
            _ => None,
        };
        assert_eq!(
            marion_harness::row_file::conformance_state_from(env, Some(account)),
            Some(PathBuf::from(&explicit))
        );
    }

    /// A newer release whose basics failed, one never probed, one whose record was taken on
    /// another binary, and one older than the pin are each refused, naming the program, the
    /// version and why — with the command that would probe it where probing could help.
    #[test]
    fn an_unlisted_release_without_an_admitting_record_is_refused_naming_why() {
        let dir = scratch("gate-refused");
        let state = dir.join("state");
        let (bin, path) = fake(&dir, "claude", "99.0.0 (Claude Code)");
        let e = refused(gate_at("claude", claude(), &path, &state));
        assert!(e.contains("reports 99.0.0"), "{e}");
        assert!(e.contains("never ran against claude-code 99.0.0"), "{e}");
        assert!(
            e.contains("scripts/conformance.sh --harness claude-code"),
            "{e}"
        );

        record(
            &state,
            "claude-code",
            "99.0.0",
            &bin,
            &["P-approval PASS -> FAIL"],
        );
        let e = refused(gate_at("claude", claude(), &path, &state));
        assert!(
            e.contains("did NOT admit claude-code 99.0.0: P-approval PASS -> FAIL"),
            "{e}"
        );

        let other = scratch("gate-other-binary");
        let (elsewhere, _) = fake(&other, "claude", "99.0.0 (Claude Code) elsewhere");
        record(&state, "claude-code", "99.0.0", &elsewhere, &[]);
        let e = refused(gate_at("claude", claude(), &path, &state));
        assert!(e.contains("is for another binary"), "{e}");

        let old = scratch("gate-older");
        let (bin, path) = fake(&old, "claude", "0.0.1 (Claude Code)");
        record(&old.join("state"), "claude-code", "0.0.1", &bin, &[]);
        let e = refused(gate_at("claude", claude(), &path, &old.join("state")));
        assert!(e.contains("older than"), "{e}");
    }

    /// **A `--version` that never answers is cut off at the budget** and killed, rather than
    /// hanging the suite; one that answers is read as before.
    #[test]
    fn a_version_probe_that_hangs_is_cut_off_and_killed() {
        let dir = scratch("gate-hang");
        let marker = dir.join("still-running");
        let hang = dir.join("hang");
        write_executable(
            &hang,
            format!("#!/bin/sh\nsleep 1; touch '{}'\n", marker.display()),
        );
        let started = Instant::now();
        assert!(output_within(&mut Command::new(&hang), Duration::from_millis(100)).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            !marker.exists(),
            "the hung probe was killed, not left to finish"
        );

        let out = output_within(Command::new("echo").arg("1.2.3"), Duration::from_secs(10))
            .expect("echo answers")
            .expect("echo runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1.2.3");
    }

    /// **A leader that has exited by the deadline is read, not cut off**: with no time left and no
    /// word from the exit waiter, the last look still finds its status.
    #[test]
    fn a_leader_exited_at_the_deadline_is_not_a_timeout() {
        let mut child = Command::new("true").spawn().expect("`true` runs");
        exited_unreaped(child.id());
        let (_never, on_exit) = std::sync::mpsc::channel::<()>();
        let status = leader_exit(&mut child, &on_exit, Duration::ZERO)
            .expect("an exited leader is not a timeout")
            .expect("its status is read");
        assert!(status.success());
    }

    /// **A leader that closes its stdio and then hangs is cut off too**: pipes at EOF are not an
    /// exit, and the wait for the exit is under the same budget.
    #[test]
    fn a_probe_that_closes_its_output_then_hangs_is_cut_off() {
        let dir = scratch("gate-hang-closed");
        let marker = dir.join("still-running");
        let hang = dir.join("hang");
        write_executable(
            &hang,
            format!(
                "#!/bin/sh\necho 1.2.3\nexec >/dev/null 2>&1\nsleep 3; touch '{}'\n",
                marker.display()
            ),
        );
        // A budget well past a loaded machine's shell start, so the pipes reach EOF inside it and
        // only the exit wait can run out.
        let started = Instant::now();
        assert!(output_within(&mut Command::new(&hang), Duration::from_millis(1500)).is_none());
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "{:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_secs(2));
        assert!(!marker.exists(), "the hung leader was killed");
    }

    /// **A hung probe's descendants die with it.** The shape is an npm wrapper's: the leader starts
    /// the real work as a child and waits on it. Killing only the leader would leave the child to
    /// finish (and touch the marker); killing the group does not. The second script's leader has
    /// already exited, leaving a child that holds its stdout open, which is the same leak with the
    /// leader a zombie when the budget runs out.
    #[test]
    fn a_hung_probe_is_killed_with_its_whole_process_group() {
        for (name, tail) in [("waits", "wait"), ("exits", "exit 0")] {
            let dir = scratch(&format!("gate-hang-group-{name}"));
            let marker = dir.join("descendant-finished");
            let hang = dir.join("hang");
            write_executable(
                &hang,
                format!(
                    "#!/bin/sh\n(sleep 1; touch '{}') &\n{tail}\n",
                    marker.display()
                ),
            );
            let got = output_within(&mut Command::new(&hang), Duration::from_millis(200));
            assert!(
                got.is_none(),
                "{name}: the descendant holds the pipe open past the budget, but got {got:?}"
            );
            std::thread::sleep(Duration::from_millis(1500));
            assert!(
                !marker.exists(),
                "{name}: the probe's descendant was killed with it, not orphaned"
            );
        }
    }

    /// **A probe of a file still open for writing is run once the writer lets go**: Linux refuses
    /// the exec with `ETXTBSY` while any descriptor writes the file — a sibling test thread's
    /// fork holds one until its own exec — and the spawn is tried again rather than the harness
    /// read as absent.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_probe_of_a_file_still_open_for_writing_runs_once_it_is_closed() {
        let dir = scratch("gate-busy-text");
        let probe = dir.join("probe");
        write_executable(&probe, "#!/bin/sh\necho ran\n");
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&probe)
            .expect("the probe, open for writing");
        // The fixture only means something while the exec is refused; a kernel that lets a file
        // open for writing be executed has no race to retry, so the test says so and stops. Any
        // other error is a broken fixture, not a kernel's choice.
        match Command::new(&probe).spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {}
            Err(e) => panic!("the probe fixture cannot be run at all: {e}"),
            Ok(mut child) => {
                let _ = child.wait();
                eprintln!("skipped: this kernel runs a file still open for writing");
                return;
            }
        }
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(writer);
        });
        let got = output_within(&mut Command::new(&probe), Duration::from_secs(10));
        release.join().unwrap();
        let out = match got {
            Some(Ok(out)) => out,
            other => panic!("the probe ran once its writer let go, but got {other:?}"),
        };
        assert_eq!(out.stdout, b"ran\n");
    }

    /// **The stub case the old exit-status gate could not tell from the real thing**: exits 0,
    /// prints something, is not a harness. `launch_only_root.rs` puts a shell stub named `codex` on
    /// `PATH`, so this is not a hypothetical collision; and an absent binary is absent.
    #[test]
    fn a_stub_without_a_version_is_refused_and_a_missing_binary_is_absent() {
        let dir = scratch("gate-stub");
        let codex = gated_row("codex").unwrap();
        let (_, path) = fake(&dir, "codex", "codex stub");
        match gate_at("codex", codex, &path, &dir.join("state")) {
            Gate::Refused {
                found: None,
                diagnosis,
            } => {
                assert!(diagnosis.contains("codex stub"), "{diagnosis}");
                assert!(diagnosis.contains(codex.verified[0]), "{diagnosis}");
            }
            other => panic!("a stub is not a harness: {other:?}"),
        }
        assert_eq!(
            gate_at(
                "codex",
                codex,
                dir.join("empty").as_os_str(),
                &dir.join("state")
            ),
            Gate::Absent
        );
    }

    /// **The near-miss, which is the case that actually occurred.** 2.1.222 is a distinct token
    /// from 2.1.220: a gate that compared prefixes would call them equal.
    #[test]
    fn a_patch_bump_is_not_a_match_for_the_version_it_differs_from_by_one_digit() {
        assert_eq!(parse_version("2.1.222 (Claude Code)\n"), Some("2.1.222"));
        assert_ne!(
            parse_version("2.1.222 (Claude Code)\n"),
            parse_version("2.1.220 (Claude Code)\n"),
        );
    }

    #[test]
    fn a_fixture_repo_is_a_real_one_commit_repository() {
        let dir = scratch("fixture-repo");
        let repo = fixture_repo(&dir);
        assert_eq!(repo, dir.join("repo"));
        assert_eq!(
            std::fs::read_to_string(repo.join("src/keep.txt")).unwrap(),
            "keep\n"
        );
        assert_eq!(
            git(&repo, &["rev-list", "--count", "HEAD"]).trim(),
            "1",
            "one commit, so a caller's base_commit is unambiguous"
        );
        assert!(
            git(&repo, &["status", "--porcelain"]).trim().is_empty(),
            "a fixture that starts dirty would put its own noise in every changed_paths"
        );
    }

    /// **A fixture repository must not have a second, invisible writer.**
    ///
    /// `git commit` ends by launching `git maintenance run --auto --detach`, which is a *detached*
    /// process: `git commit` returns while it is still running. It takes
    /// `.git/objects/maintenance.lock` on the way in and removes it on the way out, so for a few
    /// milliseconds after `fixture_repo` returns, the repository's object store contains a file
    /// nobody in the test wrote and which will vanish on its own.
    ///
    /// That is what broke `root_change_record.rs`'s NC-3 on both CI runners while passing
    /// everywhere else: it reads `.git/objects` before and after a snapshot, and it happened to
    /// read the lock the first time and not the second — so the store *shrank*, and an assertion
    /// about marion never writing there failed naming a file marion does not know exists.
    ///
    /// The repository is a throwaway, so there is nothing for maintenance to do and no reason to
    /// let it race. Turned off locally rather than by scrubbing the environment: a repository's own
    /// config outranks the global and system files, so this holds for every git process that ever
    /// opens this repo — including marion's, and including ones this suite never spawned.
    ///
    /// Watched red before the two `git config` calls existed: both reads come back empty.
    #[test]
    fn a_fixture_repo_never_runs_git_maintenance_behind_the_test() {
        let dir = scratch("fixture-repo-quiet");
        let repo = fixture_repo(&dir);
        assert_eq!(
            git(&repo, &["config", "--local", "--get", "maintenance.auto"]).trim(),
            "false",
            "`git commit` must not detach a maintenance run into this repository"
        );
        assert_eq!(
            git(&repo, &["config", "--local", "--get", "gc.auto"]).trim(),
            "0",
            "…nor the auto-gc that packs and prunes the object store under a reader's feet"
        );
    }

    #[test]
    fn persisted_contracts_reads_back_what_is_under_a_contracts_dir() {
        let dir = scratch("contracts-ok");
        let contracts = dir.join("proj/agents/a1/contracts");
        std::fs::create_dir_all(&contracts).unwrap();
        std::fs::write(contracts.join("t1.json"), br#"{"task_id":"t1"}"#).unwrap();
        // Neither of these is a contract: one is the wrong extension, the other the wrong dir.
        std::fs::write(contracts.join("notes.txt"), b"ignore me").unwrap();
        std::fs::write(dir.join("proj/agents/a1/loose.json"), b"{}").unwrap();

        let all = persisted_contracts(&dir).expect("the walk succeeds");
        let found = judge(&all);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, contracts.join("t1.json"));
        assert_eq!(found[0].1["task_id"], "t1");
    }

    /// The half that must NOT panic where it is read. A corrupt contract is carried out as data, so
    /// the caller can drop its provider and sweep for survivors before judging it.
    #[test]
    fn a_corrupt_contract_is_carried_out_rather_than_panicking_at_the_read() {
        let dir = scratch("contracts-corrupt");
        let contracts = dir.join("proj/agents/a1/contracts");
        std::fs::create_dir_all(&contracts).unwrap();
        std::fs::write(contracts.join("bad.json"), b"{ this is not json").unwrap();

        let found = persisted_contracts(&dir).expect("the walk still succeeds");
        assert_eq!(found.len(), 1, "a file that will not parse is still a file");
        let diagnosis = found[0]
            .parsed
            .as_ref()
            .expect_err("it does not parse")
            .clone();
        assert!(diagnosis.contains("is not JSON"), "{diagnosis}");
        assert!(
            diagnosis.contains("this is not json"),
            "the diagnosis carries what was actually there: {diagnosis}"
        );

        // And judging it is what panics — at a point the caller chose.
        let panicked =
            std::panic::catch_unwind(|| judge(&persisted_contracts(&dir).unwrap()).len());
        assert!(panicked.is_err(), "judge must not tolerate it either");
    }

    /// A state dir that cannot be enumerated is not a state dir with no contracts in it — **and the
    /// error names the directory that failed**, which `read_dir`'s own `io::Error` does not carry.
    /// A caller that could only report the root it started the walk from would say "somewhere under
    /// here" about a tree several levels deep.
    #[test]
    fn a_walk_that_cannot_read_a_directory_is_an_error_that_names_that_directory() {
        let dir = scratch("contracts-missing");
        let gone = dir.join("never-created");
        let e = persisted_contracts(&gone).expect_err(
            "answering `no contracts` for a directory that could not be read makes a count \
             assertion pass for the wrong reason",
        );
        assert!(
            e.to_string().contains(&gone.display().to_string()),
            "the error must name the directory that would not enumerate, got {e}"
        );
    }
}
