//! **§6.1 step 7's `Spawned` barrier is the one journal record marion may not lose quietly**, and
//! this file is the measurement of what happens when it does not land.
//!
//! # The property
//!
//! `Spawned` is the only record that carries a pid. Every other lifecycle record — `StateChanged`,
//! `Exited`, `PermissionDenied`, `SpawnAborted` — can be lost and leave a node marion can still
//! *name*: replay knows it exists, knows its pid, and `procid::audit` can still resolve it. Losing
//! `Spawned` is categorically different, because `procid::audit`'s scope is literally
//! `node.pid.is_some()`. A live process whose barrier never landed is not stale in the audit; it is
//! **absent from it**. That is §11 item 30's untracked live process, which is the exact shape §9's
//! M2 criterion 3 exists to exclude, and it is also the reply-matches-reality rule broken in the
//! worst direction available: marion answering *"the node exists"* while its own authoritative
//! record says no process does.
//!
//! So the rule under test is one sentence: **marion must not report success for a process it cannot
//! later account for.** `command.spawn()` cannot be taken back, so the only honest way to satisfy
//! it once the barrier has failed is to make the process stop existing — kill the tree, let the
//! driver reap it, and fail the run.
//!
//! # Injecting the fault without a test seam
//!
//! No `#[cfg(test)]` hook, no injected writer, no fake `Journal`: the fault here is one the shipped
//! code already refuses on its own. `marion_core::journal::MAX_RECORD_BYTES` caps a record at 16 KiB
//! at **encode** time — the cap that makes `O_APPEND`'s single-`write(2)` atomicity hold — and a
//! root's `Spawned` record embeds `Invocation::model`, which for gemini is `--model` verbatim
//! (`gemini::compile`). A `--model` argument past the cap is therefore a real, reachable,
//! production-path append failure that lands on the `Spawned` barrier and on nothing else: the
//! `SpawnIntent` written before it carries no model and fits comfortably.
//!
//! That the fault is genuinely *at the barrier* rather than earlier is not assumed. The control
//! below runs the identical scenario with an ordinary model and requires the run to succeed **with
//! a pid on the record** — so an oversized `--model` rejected at argument parsing, or by the
//! adapter, or anywhere before `spawn()`, would leave this file passing for a reason that has
//! nothing to do with the barrier and the control is what says so.
//!
//! # Why gemini, and why the root path
//!
//! gemini is `LaunchOnly`, so the whole run is a shell stub on `PATH` reached through the real
//! `marion` binary — no provider, no real CLI, no MCP handshake — and it is the one harness that
//! puts `--model` on the wire *and* records it in `Invocation::model` (codex compiles no `-m` at
//! all, so its `Spawned` carries `model: None` and cannot be made to overflow this way). The root
//! path and the child path make the *same* decision through the same `journal::append` — that
//! symmetry is the reason `journal::record` states its policy once for both — so measuring it here
//! measures the rule, and `run.rs`'s `announce_started` is its other spelling.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use marion_core::journal::MAX_RECORD_BYTES;
use marion_testsupport::{scratch, sweep};

/// Long enough that the encoded `Spawned` record cannot fit under the cap whatever else is in it,
/// and short enough to stay far below `ARG_MAX`.
const OVERSIZED_MODEL_BYTES: usize = MAX_RECORD_BYTES + 4096;

/// A real gemini model spelling, for the control. The adapter refuses to compile without one
/// (S12's `auto` router hang), so "no model" is not an available control.
const ORDINARY_MODEL: &str = "gemini-2.5-flash";

const RUN_BOUND: Duration = Duration::from_secs(60);

struct Run {
    code: Option<i32>,
    stderr: String,
    state: PathBuf,
}

/// A `gemini` on `PATH` that produces one **answered** marion tool call and exits 0.
///
/// The success shape on purpose: the failure this file is about must be the barrier's and only the
/// barrier's, so with the fix reverted the same run has to reach exit 0. A stub that failed on its
/// own would make the mutation indistinguishable from the fix.
///
/// # `lingers` is what makes the *live process* half falsifiable
///
/// A root that exits the instant it is spawned cannot demonstrate a process leak: by the time any
/// assertion runs it is gone whatever marion did. So the fault case's stub forks a descendant that
/// outlives it — a real tool-call grandchild's shape, and §11 item 30's runaway exactly — and then
/// exits 0 itself. The descendant carries `dir` in its own argv so `survivors` can name it; a bare
/// `sleep` would be invisible to a needle search and the assertion would pass vacuously.
///
/// The root launch probes `--version` first; answered on the stub's first line so the probe forks
/// no lingering descendant of its own for `survivors` to find.
///
/// Under the fix the kill lands at the failed barrier, before or during this script's first line,
/// so the descendant is either never forked or killed with the group — `kill_process_tree` targets
/// the group precisely because a child's own descendants are as unnameable as it is. Under the
/// mutation the stub runs to completion, `marion run` answers 0, and the descendant is still there
/// with nothing on the record able to name what started it.
fn gemini_stub(dir: &Path, lingers: bool) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("gemini");
    let linger = if lingers {
        format!("/bin/sh -c 'sleep 30; : {}' &\n", dir.display())
    } else {
        String::new()
    };
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = --version ]; then echo 0.0.0-stub; exit 0; fi\n\
             {linger}\
             cat <<'EOF'\n\
             {{\"type\":\"tool_use\",\"tool_name\":\"mcp_marion_spawn\",\"tool_id\":\"call-1\",\"args\":{{}}}}\n\
             {{\"type\":\"tool_result\",\"tool_id\":\"call-1\",\"status\":\"success\",\"output\":\"ok\"}}\n\
             EOF\n\
             exit 0\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn marion_run(dir: &Path, model: &str, lingers: bool) -> Run {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let bin = gemini_stub(dir, lingers);
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = marion_supervisor::run::run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                "gemini",
                "--prompt",
                "delegate one thing",
                "--repo",
                &repo.to_string_lossy(),
                "--state-dir",
                &state.to_string_lossy(),
                // Nothing listens here; the stub is the whole model side of this run.
                "--canned",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--timeout",
                "30",
                "--model",
                model,
            ])
            .env("PATH", path)
            .current_dir(dir),
        RUN_BOUND,
    )
    .expect("marion run starts");
    assert!(
        !out.timed_out,
        "marion run did not finish inside {RUN_BOUND:?}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Run {
        code: out.code,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        state,
    }
}

/// Every `Spawned` record in the project's journal, as the pid each one claims.
///
/// Read off the file rather than through `registry::replay`, deliberately: replay folds a `Spawned`
/// into a node and a mutation inside replay would move the assertion and the evidence together.
/// What is being asserted is *what is on the disk*.
fn spawned_pids(state: &Path) -> Vec<Option<i64>> {
    let mut out = Vec::new();
    let mut found_journal = false;
    for project in std::fs::read_dir(state)
        .expect("the state dir exists")
        .flatten()
    {
        let journal = project.path().join("journal.jsonl");
        let Ok(text) = std::fs::read_to_string(&journal) else {
            continue;
        };
        found_journal = true;
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(spawned) = v.pointer("/kind/Spawned") {
                out.push(spawned.get("pid").and_then(serde_json::Value::as_i64));
            }
        }
    }
    assert!(
        found_journal,
        "no journal.jsonl under {} — the run never reached the journal at all, so nothing below \
         would be measuring the barrier",
        state.display()
    );
    out
}

/// Whether the journal recorded the root's *intent* — the record written and fsynced before
/// anything is spawned, and the one that must survive for `SpawnIntent`-alone to mean anything.
fn has_spawn_intent(state: &Path) -> bool {
    for project in std::fs::read_dir(state)
        .expect("the state dir exists")
        .flatten()
    {
        let Ok(text) = std::fs::read_to_string(project.path().join("journal.jsonl")) else {
            continue;
        };
        if text.lines().any(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .is_ok_and(|v| v.pointer("/kind/SpawnIntent").is_some())
        }) {
            return true;
        }
    }
    false
}

/// **The control**, and it is not decoration: without it every assertion in the test below is
/// satisfiable by a `marion` that refused this run before it ever reached `spawn()`.
#[test]
fn a_root_whose_spawned_barrier_lands_is_answered_with_a_pid_on_the_record() {
    let dir = scratch("barrier-control");
    let run = marion_run(&dir, ORDINARY_MODEL, false);
    assert_eq!(
        run.code,
        Some(0),
        "the same run with an ordinary model must succeed, or the test below is measuring some \
         other refusal\nstderr:\n{}",
        run.stderr
    );
    let pids = spawned_pids(&run.state);
    assert_eq!(
        pids.len(),
        1,
        "exactly one Spawned record for one root: {pids:?}"
    );
    assert!(
        pids[0].is_some_and(|p| p > 0),
        "the barrier landed, so the record names a real signal target: {pids:?}"
    );
    assert!(
        sweep(&dir.to_string_lossy()).is_empty(),
        "the control must not leak the stub either"
    );
}

/// **The defect.** A failed `Spawned` barrier used to be swallowed by `journal::record`'s
/// print-and-continue policy: the process stayed alive, the owner was told it had started, and
/// `agent/spawn` answered successfully — leaving a live process that replay carried no pid for and
/// `procid::audit` therefore could not see.
///
/// Mutation, and it must die by assertion rather than by timeout: put the `Spawned` append in
/// `root::launch_inner`'s `started` closure back on `journal::record` and drop the `unaccountable`
/// substitution. The run then exits 0 with the barrier missing, and both the exit-code assertion
/// and the no-`Spawned`-with-a-pid assertion fire immediately.
#[test]
fn a_root_whose_spawned_barrier_fails_is_unwound_rather_than_answered_as_started() {
    let dir = scratch("barrier-fails");
    let run = marion_run(&dir, &"m".repeat(OVERSIZED_MODEL_BYTES), true);

    // 1. The reply matches reality: marion does not report success.
    assert_ne!(
        run.code,
        Some(0),
        "marion answered successfully for a node it could not record\nstderr:\n{}",
        run.stderr
    );
    // 2. …and it says *why*, naming the barrier rather than the symptom of the kill it performed.
    //    A signalled-exit message here would be marion reporting the consequence of its own
    //    decision as though the harness had died on its own.
    assert!(
        run.stderr.contains("could not record it"),
        "the refusal must name the unrecorded node, not the signal marion sent it\nstderr:\n{}",
        run.stderr
    );

    // 3. The record is consistent with that answer. `SpawnIntent` is present — it was written and
    //    fsynced before anything was spawned — and there is **no `Spawned` carrying a pid**, which
    //    after this fix means exactly one thing: no process exists.
    assert!(
        has_spawn_intent(&run.state),
        "the intent is written before the spawn and must still be there"
    );
    let pids = spawned_pids(&run.state);
    assert!(
        pids.iter().all(Option::is_none),
        "a `Spawned` with a pid is marion claiming a process it cannot account for: {pids:?}"
    );

    // 4. And the claim in (3) is true of the machine, not merely of the file. This is the whole
    //    defect: the old behaviour left this process running.
    // `sweep` rather than `survivors`: it reports the same set and kills it, so a *failing* run of
    // this test cannot itself leave the runaway behind for the next one to trip over.
    let stragglers = sweep(&dir.to_string_lossy());
    assert!(
        stragglers.is_empty(),
        "the unrecordable node was left running — an untracked live process, which is precisely \
         what §9 criterion 3 excludes: {stragglers:?}"
    );
}
