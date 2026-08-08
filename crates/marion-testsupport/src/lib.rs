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

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

// --- processes ----------------------------------------------------------------------------------

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
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

/// One real CLI the matrix drives, and the versions its measured behaviours were taken from.
pub struct PinnedHarness {
    /// The name marion `exec`s, which is also the name the binary must have on `PATH`.
    pub program: &'static str,
    /// **The first entry is the pin** — the version README, MILESTONES and these tests' own doc
    /// comments name, and the one a failure message quotes as expected. The rest are versions
    /// since observed green, each with the observation beside it in [`PINNED_HARNESSES`].
    pub accepted: &'static [&'static str],
}

/// **The one table.** Every version check reads from here; nothing else in the workspace decides
/// what version is acceptable.
///
/// The auth wire spelling had exactly the other shape today (`as_wire` and `from_wire` each
/// carrying their own `"inherited"` literal, nothing tying them), and it broke in the silent
/// direction. Two tables that must agree and do not know about each other is the bug; this is one.
///
/// # Why a set rather than a single pin, and why entries are added and never edited
///
/// A version is added here **only after the suite has been observed green against it**, and the pin
/// — entry zero — is never replaced, because the pin is what the prose claims. That keeps the two
/// questions apart: *"which version were these behaviours measured on"* is answered by the pin and
/// does not move; *"which versions has this been checked against since"* is answered by the tail and
/// grows by deliberate edit. An upgrade that has not been re-checked is a red suite, which is the
/// point — the four measured behaviours below are version-specific, so attributing them to a version
/// that never ran is the failure this table exists to prevent.
pub const PINNED_HARNESSES: &[PinnedHarness] = &[
    PinnedHarness {
        program: "claude",
        // 2.1.220 is the pin: it is the version that sends turn one `"tools":[]` when the prompt
        // rides argv, and whose `can_use_tool` frame `tests/fixtures/s9` was captured from.
        //
        // 2.1.222: observed green on darwin 25.5.0, 2026-08-04. Every test that drives a real
        // `claude` passed against it on the run that added this check — `m1_hop`,
        // `permission_round_trip` (6), `journal_wiring` (17), `cross_product` (17),
        // `harness_matrix` (4), `depth_gate` (4), none `#[ignore]`d — so the turn-one `"tools":[]`
        // shape and the `can_use_tool` frame were both re-asserted against 2.1.222 rather than
        // assumed to have survived the bump. **That is the evidence an entry here needs**, and the
        // reason it is entry one and not a replacement for the pin.
        //
        // 2.1.223: observed green on darwin 25.5.0, 2026-08-06, after the **second** harness
        // auto-update of that one day — codex moved in the morning, claude in the evening. The
        // whole workspace passed against it, which covers every gated test that drives a real
        // `claude`: `m1_hop`, `permission_round_trip` (6), `journal_wiring` (17), `cross_product`
        // (17), `harness_matrix` (4), `depth_gate` (4), none `#[ignore]`d.
        //
        // `permission_round_trip` reads `tests/fixtures/s9` off disk, so a green suite genuinely
        // re-asserts the `can_use_tool` frame. **Nothing else about claude is re-asserted that
        // way**: `s10`, `s11`, `s14` and `s16` live in the workspace as prose rather than as
        // fixture reads, and `child_stream`, `child_events` and `node_attach` drive a real
        // `claude` without ever calling [`on_path`], so the pin does not reach them at all. Those
        // four spikes were re-probed rather than assumed, every one against a canned local
        // provider, total spend **$0.00**:
        //
        // * **s14** — `probe-claude.sh` re-run; all eight cells match `declarations.json` field
        //   for field and seq for seq. `--tools NotATool` still declares `[]` at exit 0 with the
        //   same stderr the *good* runs print, `Read,NotATool` still keeps `Read` and drops the
        //   bad name, marion's own lowercase `read` still declares nothing, and `--tools` absent
        //   and `--tools default` still declare the same eleven built-ins. **An unknown tool name
        //   is still silently ignored**, which is the negative this fixture exists for.
        // * **s10** — the probe re-run in all three modes: 14-key `SubagentStop`, 11-key `Stop`,
        //   key set identical fire for fire; `agent_id` still 17 lowercase hex and still equal to
        //   `task_started.task_id`, `task_notification.task_id` and the `tool_result`'s `agentId`;
        //   `session_id` and `transcript_path` still the *parent's*; `decision: block` still
        //   re-prompts with `parent_tool_use_id` on the injected `user` frame; `num_turns` still 2
        //   in every mode; the bad-shape control still fires **zero** times.
        // * **s11** — the pty ceiling holds: `read_size_max` 1,024 on a pty against 46,692 on a
        //   pipe, zero reads at or above 4,096, 94 of 231 reads (41%) carrying no frame boundary
        //   against zero on pipes, every pty line still `\r\n` and every `OPOST`-cleared line
        //   still bare `\n`, and `claude -p` still refuses a pty *stdin* with exit 1 and a
        //   byte-identical `Error: Input must be provided …`.
        // * **s16** — the kill shape holds exactly: no `stdin_eof` in any harness run and one in
        //   the control, SIGINT then SIGTERM at 100.2–101.1 ms across four runs, a third signal
        //   nothing can catch, all pid-targeted (the own-process-group run is identical), and the
        //   grandchild still unsignalled, reparented to pid 1 and beating 59–60 times after the
        //   harness is gone.
        //
        // **Two readings moved, and neither is a shape.** SIGTERM→server-gone widened from
        // 418–429 ms to 477–539 ms, measured one axis at a time — 2.1.222 is still on disk beside
        // 2.1.223, so both were run on this machine within the same minute, and the ranges do not
        // overlap. The grace a bridge would have to work inside got *larger*, so nothing that
        // depends on it breaks; but `background.rs`'s "~450 ms" is a 2.1.222 number and is a
        // *floor* for 2.1.223. And s11's `collapsed_sequence_identical` came back `false` on a
        // `system/hook_progress` frame — **not** a 2.1.223 finding: that frame carries the
        // *operator's* own `SessionStart:startup` hook output, because `S1_ARGV_TAIL` carries no
        // `--setting-sources ""` the way the s14 probe does. It appeared once in eighteen runs and
        // the `hook_started`/`hook_response` counts are 9/9 on both versions. s11's probe is
        // environment-dependent in a way its fixture does not say; that is a finding about the
        // probe, not about claude.
        //
        // **Nothing was re-recorded.** Every capture was compared, not refreshed.
        //
        // 2.1.224: observed green on darwin 25.5.0, 2026-08-07, the day after 2.1.223 and one day
        // after codex's own second bump. The whole workspace passed against it, which covers every
        // gated test that drives a real `claude`: `m1_hop`, `permission_round_trip` (6),
        // `journal_wiring` (17), `cross_product` (17), `harness_matrix` (4), `depth_gate` (4), none
        // `#[ignore]`d.
        //
        // **The same four spikes `9b0a06d` re-probed were re-probed here** — s10, s11, s14 and s16,
        // every one against a canned local provider, total spend **$0.00** — so this entry does not
        // rest on a thinner basis than the one above it. s14 and s16 were additionally re-run
        // against 2.1.223, which is still on disk under
        // `~/.local/share/claude/versions/`, so the readings that carry a *number* were taken on
        // both versions on this machine within the same hour, one axis at a time.
        //
        // * **s14** — `probe-claude.sh` re-run on 2.1.223 **and** 2.1.224; all eight cells match
        //   `declarations.json` field for field, `body_tools`, `system_init_tools`, exit and stderr
        //   alike. `--tools NotATool` still declares `[]` at exit 0 with the *good* runs' stderr,
        //   `Read,NotATool` still keeps `Read` and drops the bad name, lowercase `read` still
        //   declares nothing, and `--tools` absent and `--tools default` still declare the same
        //   eleven built-ins. **An unknown tool name is still silently ignored.**
        // * **s10** — the probe re-run in all three modes: 14-key `SubagentStop` and 11-key `Stop`,
        //   both key sets identical to the fixture's; `agent_id` still 17 lowercase hex and still
        //   equal to the stream's `task_id`; `session_id` and `transcript_path` still the
        //   *parent's* with `agent_transcript_path` at `<session_id>/subagents/agent-<agent_id>`;
        //   `decision: block` still re-prompts (2 `SubagentStop` fires against 1) with
        //   `parent_tool_use_id` on the injected `user` frame; `num_turns` still 2 in every mode;
        //   the bad-shape control still fires **zero** times.
        // * **s11** — `compare.json`'s whole `comparisons` block is **byte-identical** to the
        //   committed one: `collapsed_sequence_identical: true` for `pipes` against both `pty-out`
        //   and `pty-out-raw`, `first_divergence: null`, 38 kinds each. The pty ceiling holds —
        //   `read_size_max` 1,024 on every pty transport against 46,692 on a pipe, **zero** reads
        //   at or above 4,096 on a pty against 5 on a pipe, 94 of 231 reads (41%) carrying no
        //   frame boundary against zero on pipes. Every pty line is still `\r\n` and every
        //   `OPOST`-cleared line still bare `\n`. `claude -p` still refuses a pty *stdin* with
        //   exit 1 and a byte-identical `Error: Input must be provided …`. 2.1.223's one-off
        //   `collapsed_sequence_identical: false` did **not** recur, which is the confirmation the
        //   entry above predicted: it was the operator's own `SessionStart` hook, not claude.
        // * **s16** — every discriminating *shape* holds across all four runs: no `stdin_eof` in
        //   any harness run and one in the control, SIGINT then SIGTERM then a third signal nothing
        //   can catch, `server_signal_handlers_installed` 29 with none refused,
        //   `server_voluntary_exit` / `server_orderly_shutdown` / `server_crashed` all false, all
        //   pid-targeted (the own-process-group run is identical), and the grandchild still
        //   unsignalled, reparented to pid 1 and beating 58–59 times after the harness is gone.
        //
        // **The 100 ms timer got noisier, and that is the one reading that moved.** SIGINT→SIGTERM
        // came back **98.9, 101.7, 102.5, 103.2 ms** on 2.1.224 against **100.0–101.0 ms** on
        // 2.1.223 re-run the same hour, and 100.0–100.5 ms in the committed 2.1.222 captures. The
        // nominal timer is unmoved and no reading is near a different value; what changed is that
        // the spread went from ±1 ms to ±3 ms, and one run landed *below* 100 ms for the first
        // time. It is recorded because §11's reading of that gap as "a fixed timer rather than a
        // race" rested on the tightness, and the tightness is now weaker evidence than it was. A
        // bridge that treats 100 ms as an exact deadline rather than a nominal one is newly wrong.
        //
        // **And 2.1.223's widened SIGKILL grace does not reproduce.** Re-running the same probe
        // today gives SIGTERM→server-gone **347–549 ms on 2.1.223** and **363–458 ms on 2.1.224** —
        // 2.1.223's own range now straddles the 477–539 ms the entry above recorded for it and the
        // 418–429 ms it recorded for 2.1.222. So that widening was **machine load, not a version
        // property**, and the entry above over-attributed it. The honest reading of the grace is
        // that it is noisy on the order of ±100 ms and `background.rs`'s "~450 ms" is a *nominal*
        // figure with no floor under it: 347 ms was observed today. Nothing depends on a floor —
        // marion's bridge must survive SIGKILL at any moment, not at a deadline — which is why this
        // is a correction to the record rather than a blocker.
        //
        // **Nothing was re-recorded.** Every capture was compared, not refreshed.
        //
        // 2.1.225: admitted on the run that added `marion run --pane`, and admitted for the
        // narrower reason this table asks for rather than because the suite was green. The CLI
        // self-updated mid-session, so this was not a chosen upgrade; every test that drives a
        // real claude was re-run against it and held, and the new pane path was measured on it
        // directly — the trust dialog still lands on the **main** screen before the alternate
        // screen is entered, `-p` still refuses an isatty(0) stdin, and the TUI still seeds an
        // argv prompt into the composer rather than submitting it. What was **not** re-measured is
        // the SIGINT/SIGTERM timing above; that probe is not part of the suite and this entry
        // makes no claim about it on 2.1.225.
        accepted: &["2.1.220", "2.1.222", "2.1.223", "2.1.224", "2.1.225"],
    },
    PinnedHarness {
        program: "codex",
        // 0.146.0 is the pin: the version that accepts a bogus `-c` key with a clean exit, that
        // leaks the background `git fetch` MILESTONES records, and that `tests/fixtures/s6`, `s7`
        // and `s14`'s codex halves were captured from.
        //
        // 0.146.1: observed green on darwin 25.5.0, 2026-08-06, after the local install
        // auto-updated mid-session — §7.7's hazard, arriving exactly as described. The whole
        // workspace passed against it (888 tests, no `#[ignore]`s), which covers every test that
        // drives a real `codex`: `m1_hop`, `timeout_kill`, `harness_matrix`, `depth_gate`,
        // `worktree_reap` (7), `cross_product` (7 codex cells), `journal_wiring` (7), and the
        // ungated-but-real `child_stream`, `child_events` and `node_attach`.
        //
        // **A green suite alone would have been the weaker half of the evidence**, because the s6
        // and s7 shapes live in this workspace as transcriptions rather than as fixture reads, and
        // `s14`'s `codex-*.declaration.json` are read by no test at all — so the captures were
        // re-probed rather than assumed. `tests/fixtures/s14/probe-codex.sh` was re-run against
        // 0.146.1 and its three load-bearing fields still match the committed 0.146.0 capture
        // byte for byte: `body.tools` absent, `code_mode_tool_names` the same eight names
        // (`apply_patch, create_goal, exec_command, get_goal, update_goal, update_plan,
        // view_image, write_stdin`, every `namespace` null), and `additional_tools` still
        // `exec, wait, request_user_input, collaboration` — identical under `--sandbox read-only`
        // and `--sandbox workspace-write`, so the sandbox still constrains at call time and not at
        // declaration, and `--tools` is still `exit 2`. `--help`, `exec --help`, `features list`
        // and `mcp --help` are byte-identical to 0.146.0 modulo the version string, so the `-m,
        // --model` flag s6 and the adapter depend on has not moved. s7's `setsid` finding is
        // re-asserted live by `timeout_kill`, which passed. **Nothing was re-recorded**; the
        // captures were compared, not refreshed.
        //
        // 0.147.0: observed green on darwin 25.5.0, 2026-08-07, after the third harness
        // auto-update in two days — codex has now moved twice and claude twice. The whole
        // workspace passed against it, which covers the same real-`codex` set as above.
        //
        // 0.146.1 is still on disk under `~/.codex/packages/standalone/releases/`, so **every
        // reading below was taken on both versions on this machine within the same minute**, one
        // axis at a time. Both probes drive canned local providers on `127.0.0.1`; total spend
        // **$0.00**.
        //
        // * **s14** — `probe-codex.sh` re-run. `body.tools` is still **absent** and
        //   `code_mode_tool_names` is still the same eight names (`apply_patch, create_goal,
        //   exec_command, get_goal, update_goal, update_plan, view_image, write_stdin`, every
        //   `namespace` null), identical under `--sandbox read-only` and `--sandbox
        //   workspace-write`, and `codex exec --tools Read` is still `exit 2`.
        // * **s6** — all three questions re-answered live, not transcribed. (1) `exec` still hosts
        //   a stdio MCP server: `initialize` (protocol `2025-06-18`, client
        //   `codex-mcp-client/0.147.0`), `tools/list`, `tools/call`, and the server's text comes
        //   back inside a `mcp_tool_call` item at `status: completed` — via marion's own
        //   `marion-supervisor mcp` in one arrangement and the s6 stub server in the other. The
        //   `tools/call` still carries `_meta.x-codex-turn-metadata` with `sandbox: "seatbelt"`
        //   and the per-workspace `latest_git_commit_hash` + `has_changes`. (2) `file_change`
        //   items still carry absolute `path` + `kind`, at both `item.started` and
        //   `item.completed`. (3) `--output-schema` is still forwarded as `text.format =
        //   {type: json_schema, strict: true, name: "codex_output_schema"}` and
        //   `--output-last-message` writes a file byte-identical to
        //   `tests/fixtures/s6/output-last-message.txt`.
        //
        // **One shape moved, and marion does not read it.** In 0.146.1 the `additional_tools`
        // developer message holds four top-level entries — `exec`, `wait`, `request_user_input`
        // flat, plus a `collaboration` group of six — which is what
        // `s14`'s `codex-*.declaration.json` records as `input.additional_tools[].name`. In
        // 0.147.0 the first three are **nested inside a `functions` group**, so the top level is
        // `functions, collaboration`. No tool appeared or disappeared; only the grouping did. The
        // fixture is therefore a 0.146.x reading and is left as one — nothing re-recorded. It is
        // safe because `marion-provider`'s `classify_child` deliberately refuses to read the
        // catalogue at all (only *call* and *call-output* items count as evidence), and the direct
        // `{"type":"function_call","name":"report","namespace":"mcp__marion"}` dispatch s6 rests on
        // still executes end to end under 0.147.0.
        //
        // **A second reading moved and it is not 0.147.0's.** `tests/fixtures/s6/README.md` records
        // that codex "spawns the server more than once per run" (two full `initialize` +
        // `tools/list` for one `codex exec`). Both 0.146.1 and 0.147.0 spawn it **once** — one
        // `initialize`, one `notifications/initialized`, one `tools/list`, one `tools/call`. So
        // that changed at 0.146.1 and `fd2fc1d` did not catch it, because `fd2fc1d` re-ran the s14
        // probe and not the s6 one. A bridge that tolerates a second spawn is still correct; one
        // that *requires* it never was.
        //
        // `--help`, `exec --help` and `mcp --help` are unchanged modulo two additions
        // (`--approve-for-me` on both help surfaces, and `--disable` moved in the `exec` listing),
        // so `-m, --model` has not moved. `features list` gains four rows
        // (`executed_tool_call_metadata`, `image_resize_notice`, `recommended_plugins`,
        // `view_image`) and removes none; `code_mode_host` is still `stable true`. s7's `setsid`
        // finding is re-asserted live by `timeout_kill`, which passed. **Nothing was re-recorded.**
        accepted: &["0.146.0", "0.146.1", "0.147.0"],
    },
    PinnedHarness {
        program: "gemini",
        // The version that omits MCP tools entirely without `trust: true`, silently.
        accepted: &["0.53.0"],
    },
    PinnedHarness {
        program: "opencode",
        // The version that never exits on a provider hang.
        accepted: &["1.17.3"],
    },
];

/// The pinned version of `program` — entry zero of its [`PinnedHarness::accepted`].
///
/// For call sites that need to *say* the version rather than check it, so they say it from the same
/// table the check reads.
///
/// Panics for a program this suite does not drive: there is no honest answer, and returning one
/// would let a typo silently name a version nothing pins.
pub fn pinned_version(program: &str) -> &'static str {
    PINNED_HARNESSES
        .iter()
        .find(|p| p.program == program)
        .unwrap_or_else(|| {
            panic!("{program:?} is not one of the harnesses this suite pins; see PINNED_HARNESSES")
        })
        .accepted[0]
}

/// The first dotted-numeric token in `--version` output, e.g. `2.1.222` out of
/// `2.1.222 (Claude Code)`.
///
/// **Each of the four formats it differently, so this was measured rather than assumed** (darwin
/// 25.5.0, 2026-08-04; all four print one line to *stdout* and exit 0):
///
/// | program    | `--version` prints         |
/// |------------|----------------------------|
/// | `claude`   | `2.1.222 (Claude Code)`    |
/// | `codex`    | `codex-cli 0.146.0`        |
/// | `gemini`   | `0.53.0`                   |
/// | `opencode` | `1.17.3`                   |
///
/// Two carry a name and two do not, and the name comes first where it is present — so the rule is
/// "first token that is digits and dots", which skips `codex-cli` (no leading digit) and takes
/// `2.1.222` ahead of `(Claude`. `None` is returned for output with no such token, and [`on_path`]
/// turns that into a failure rather than into a match: a shape this cannot read is a binary this
/// suite has not identified, which is the `launch_only_root.rs` stub case exactly.
fn parse_version(output: &str) -> Option<&str> {
    // One complete predicate rather than a `find` plus a `filter`: the two-stage form rejects the
    // *whole output* when its first candidate is a truncated `1.`, instead of reading on.
    output.split_whitespace().find(|tok| {
        tok.starts_with(|c: char| c.is_ascii_digit())
            && !tok.ends_with('.')
            && tok.contains('.')
            && tok.chars().all(|c| c.is_ascii_digit() || c == '.')
    })
}

/// Is `program` on `PATH`, runnable, **and — for a harness this suite pins — the version these
/// tests' conclusions were measured on?**
///
/// Callers assert on this and name the binary rather than skipping: §9's standing rule is that a
/// criterion which quietly passes on a machine that cannot run it is worth less than no criterion.
///
/// # Why the version is checked here and not in a second function
///
/// `<prog> --version` exiting 0 says only that *something named* `codex` exists. That is not a
/// hypothetical gap in this repository: `launch_only_root.rs` deliberately puts a shell stub named
/// `codex` on `PATH`, so a name collision is a shape that already occurs here — and the matrix's
/// whole claim is that it drove the real binary. A separate `require_version(program)` would leave
/// the weak `on_path` in place beside it, and this crate exists because the weak variant of a
/// duplicated helper is the one that propagates. Folding the check into the function every call site
/// already calls means it cannot be forgotten and no call site changes.
///
/// # Why a wrong version panics rather than returning `false`
///
/// *Absent* and *present but wrong* are different findings and only one of them the caller can
/// phrase. A caller can write "put `codex` on PATH"; it cannot write "the `codex` on your PATH is
/// 0.150.0 and these behaviours were measured on 0.146.0", because it does not know what is there.
/// Returning `false` would file the second finding under the first message. So the diagnosis is
/// raised where the evidence is, naming the program, the pinned version and what was actually found.
///
/// # Why hard failure rather than a warning
///
/// Mechanically, first: `cargo test` captures the stdout of a passing test, so a "loud warning" on a
/// green run is *invisible*. There is no such thing as a warning here — the choice is between
/// failing and passing quietly, and passing quietly means the matrix reports conclusions about
/// 0.146.0 that were produced by whatever else was installed. A hard pin does cost a red suite the
/// day a CLI is upgraded; that cost is the correct one and it is bounded to a one-line edit next to
/// the reason, made after re-running. The alternative cost is unbounded and silent.
pub fn on_path(program: &str) -> bool {
    let Ok(out) = Command::new(program).arg("--version").output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let Some(pin) = PINNED_HARNESSES.iter().find(|p| p.program == program) else {
        // Not a harness this suite pins — `git`, and anything else a caller probes for. Presence is
        // the whole question for those.
        return true;
    };
    // Only stdout is parsed, because that is where all four measurably print it; stderr is carried
    // into the diagnosis so a harness that *moved* its version there fails with the evidence in
    // hand, rather than being read out of a stream this has never checked.
    match check_version(
        pin,
        &String::from_utf8_lossy(&out.stdout),
        &String::from_utf8_lossy(&out.stderr),
    ) {
        Ok(()) => true,
        Err(diagnosis) => panic!("{diagnosis}"),
    }
}

/// The judgement [`on_path`] makes, as a pure function of what the binary printed.
///
/// Split out so the diagnosis can be *tested* — and tested without touching `PATH`. Putting a stub
/// on `PATH` from a test would mean `set_var` in a process whose other tests are concurrently
/// resolving `ps`, `git` and `sh`, which is a data race on the environment block: this crate cannot
/// preach about leak checks that pass by failing to look and then introduce UB to test a message.
/// The spawn half is covered by every real `on_path("claude")` in the suite; this is the half that
/// decides, so this is the half with the assertions on it.
fn check_version(pin: &PinnedHarness, stdout: &str, stderr: &str) -> Result<(), String> {
    let program = pin.program;
    let expected = pin.accepted[0];
    match parse_version(stdout) {
        Some(found) if pin.accepted.contains(&found) => Ok(()),
        Some(found) => Err(format!(
            "`{program} --version` reports {found}, but this suite's conclusions about {program} \
             were measured on {expected}.\n\
             Accepted: {accepted:?}.\n\
             A matrix cell that runs {found} and reports a result attributed to {expected} is the \
             silent pass this check exists to rule out. Re-run the cells that drive {program}, and \
             if they hold, add {found:?} to that program's entry in \
             `marion_testsupport::PINNED_HARNESSES` with the observation beside it.",
            accepted = pin.accepted,
        )),
        None => Err(format!(
            "`{program} --version` printed nothing this can read a version out of, so the binary \
             on PATH is unidentified — and an unidentified binary is not a match. This suite \
             expects {program} {expected}.\n\
             stdout: {stdout:?}\n\
             stderr: {stderr:?}\n\
             (A shell stub named for a harness is a real shape in this repo — see \
             `launch_only_root.rs` — which is why this is a failure and not a shrug.)",
            stdout = stdout.trim(),
            stderr = stderr.trim(),
        )),
    }
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
    root
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
    let out = Command::new("git")
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
pub fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/keep.txt"), "keep\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// **The property every `Scratch` in this repo was introduced for, and the one nobody had
    /// written down.** It has been proved by hand — forcing a failure, listing `$TMPDIR` — three
    /// separate times in one session, once per leaking call site. A property proved by hand three
    /// times should be a test.
    ///
    /// `catch_unwind` rather than a `#[should_panic]` test, because the assertion is about what is
    /// on disk *after* the unwind, which a `#[should_panic]` test has no way to check.
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

    /// The measured formats, as a test rather than as a comment. Two of the four carry a program
    /// name and two do not, so a parser that assumed either shape would be wrong about half the
    /// matrix — and `codex-cli 0.146.0` is the one that punishes "first token".
    #[test]
    fn the_version_parser_reads_all_four_measured_formats() {
        for (raw, want) in [
            ("2.1.222 (Claude Code)\n", "2.1.222"), // claude
            ("codex-cli 0.146.0\n", "0.146.0"),     // codex
            ("0.53.0\n", "0.53.0"),                 // gemini
            ("1.17.3\n", "1.17.3"),                 // opencode
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

    /// The table is the single source, so the pin it hands out must be a real entry — a `program`
    /// nothing pins is a typo that would otherwise name a version out of thin air.
    #[test]
    fn the_pin_comes_from_the_table_and_an_unpinned_program_has_no_pin() {
        for h in PINNED_HARNESSES {
            assert!(
                !h.accepted.is_empty(),
                "{}: entry zero is the pin, so the list cannot be empty",
                h.program
            );
            assert_eq!(pinned_version(h.program), h.accepted[0]);
            for v in h.accepted {
                assert_eq!(
                    parse_version(v),
                    Some(*v),
                    "{}: {v:?} is not a version this could ever match against a real \
                     `--version`, so the entry could never be satisfied",
                    h.program
                );
            }
        }
        assert!(
            std::panic::catch_unwind(|| pinned_version("git")).is_err(),
            "`git` is probed by on_path but not pinned; asking for its pin must not invent one"
        );
    }

    fn pin(program: &str) -> &'static PinnedHarness {
        PINNED_HARNESSES
            .iter()
            .find(|p| p.program == program)
            .expect("pinned")
    }

    /// **The failure a wrong version produces, asserted rather than assumed** — because a check
    /// whose message does not name both versions leaves the reader with a red suite and no idea
    /// which upgrade caused it.
    #[test]
    fn a_program_reporting_an_unpinned_version_fails_naming_both_versions() {
        let e = check_version(pin("gemini"), "0.99.0\n", "")
            .expect_err("a gemini that is not the pinned one must not pass the gate");
        assert!(e.contains("gemini"), "names the program: {e}");
        assert!(e.contains("0.99.0"), "names what was actually found: {e}");
        assert!(
            e.contains(pinned_version("gemini")),
            "names the expected version, from the table: {e}"
        );
    }

    /// **The stub case the old exit-status gate could not tell from the real thing**: exits 0,
    /// prints something, is not a harness. `launch_only_root.rs` puts a shell stub named `codex` on
    /// `PATH`, so this is not a hypothetical collision.
    #[test]
    fn a_stub_that_exits_cleanly_without_a_version_fails_rather_than_passing() {
        let e = check_version(pin("codex"), "codex stub\n", "").expect_err(
            "`codex --version` exiting 0 is what the OLD gate accepted; a stub satisfies it",
        );
        assert!(e.contains("codex"), "{e}");
        assert!(
            e.contains("codex stub"),
            "the diagnosis carries what was actually printed: {e}"
        );
        assert!(
            e.contains(pinned_version("codex")),
            "and the version that was expected: {e}"
        );
    }

    /// **The near-miss, which is the case that actually occurred.** The pin says 2.1.220 and the
    /// installed CLI is 2.1.222 — two versions differing in one digit, sharing the prefix `2.1.22`.
    /// A gate that compared with `starts_with`, or that parsed only `major.minor`, would call these
    /// equal and hand back a pass; the whole point is that `"tools":[]` on turn one is a property of
    /// a *build*, not of a minor series. So this asserts both halves: 2.1.222 is a distinct token
    /// from 2.1.220, and a table listing only 2.1.220 rejects it by name.
    #[test]
    fn a_patch_bump_is_not_a_match_for_the_version_it_differs_from_by_one_digit() {
        assert_eq!(parse_version("2.1.222 (Claude Code)\n"), Some("2.1.222"));
        assert_ne!(
            parse_version("2.1.222 (Claude Code)\n"),
            parse_version("2.1.220 (Claude Code)\n"),
            "2.1.222 and 2.1.220 must not collapse to the same parsed version"
        );
        let only_the_pin = PinnedHarness {
            program: "claude",
            accepted: &["2.1.220"],
        };
        let e = check_version(&only_the_pin, "2.1.222 (Claude Code)\n", "")
            .expect_err("a table that lists only 2.1.220 must not accept 2.1.222");
        assert!(e.contains("2.1.222"), "names what is installed: {e}");
        assert!(e.contains("2.1.220"), "names what was measured: {e}");
    }

    /// A version on the wrong *stream* is still not a match — and the diagnosis carries the stream
    /// it was actually on, so the next reader can see what changed rather than guessing.
    #[test]
    fn a_version_printed_only_on_stderr_is_reported_with_the_evidence() {
        let e = check_version(pin("claude"), "", "2.1.220 (Claude Code)\n")
            .expect_err("stdout is where all four measurably print it");
        assert!(
            e.contains("2.1.220 (Claude Code)"),
            "stderr is in the diagnosis: {e}"
        );
    }

    /// Every accepted entry must actually be accepted. This is the check that would have caught an
    /// entry added with a stray space or a `v` prefix — an unsatisfiable pin fails every run on a
    /// correct machine, which looks exactly like a real regression.
    #[test]
    fn every_accepted_version_is_one_the_gate_accepts() {
        for h in PINNED_HARNESSES {
            for v in h.accepted {
                assert!(
                    check_version(h, &format!("{v}\n"), "").is_ok(),
                    "{}: {v:?} is listed as accepted but the gate rejects it",
                    h.program
                );
            }
        }
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
