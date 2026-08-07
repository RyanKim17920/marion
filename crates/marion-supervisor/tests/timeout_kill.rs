//! M1 acceptance criterion 6 (design §9): **a timed-out child leaves no surviving tool-call
//! descendant.**
//!
//! A **real `codex exec`** child is driven, through marion's own `run_spawn`, into §11 item 18's
//! **case B** — a `tools.exec_command` whose command is *still running when `exec` yields* — and
//! given a deliberately short `timeout`. That is the only case that leaks. A case-A-only test (a
//! tool call that has already completed) passes against a broken implementation, because codex
//! reaps its own command's session on completion; so does a test whose escapee is a `setsid`
//! process the *test itself* constructed, since that proves the sweep works against a tree marion
//! built rather than against the harness's actual behaviour.
//!
//! The assertion is over the descendant set **marion enumerated in step 1** of its two-step group
//! kill, read back through [`marion_supervisor::run::last_kill_sweep`]: `kill(pid, 0)` must return
//! `ESRCH` for every pid in it, and the set must be non-empty — an empty one would make the
//! `ESRCH` check vacuous. Asserting only that the child's own process died is *not* this
//! criterion and does not imply it: a naive `killpg` satisfies that while every `setsid`-ed
//! tool-call process keeps running, reparented to pid 1.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test timeout_kill
//! ```
//!
//! It needs a real `codex` on `PATH` at a version [`marion_testsupport::PINNED_HARNESSES`]
//! accepts, and it is not `#[ignore]`d: like `m1_hop`, a
//! criterion that quietly passes on a machine that cannot run it is worth less than no criterion.
//! No model is called — everything is served by the in-process `CannedServer`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::contract::{ExitStatus, TaskId};
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{Caller, Env, SpawnRequest, last_kill_sweep, run_spawn};
use marion_testsupport::{alive, fixture_repo, kill_hard, on_path, pinned_version, scratch};

/// The child's bound. Long enough for `codex exec` to boot, take turn one and get its tool call
/// running (measured at ~4 s here); short enough that the test is not a wait. The tool call is
/// still in flight when it expires — that is the whole point.
const CHILD_TIMEOUT_SECS: u64 = 25;

/// How long the runaway `sleep`s live if nothing kills them. Far past this test, so a survivor is
/// unmistakable rather than a race with its own exit.
const RUNAWAY_SECS: u64 = 900;

/// Case B's command, from `spikes/s7/runaway.sh`: a shell that backgrounds one sleeper and then
/// `exec`s into another, recording both pids. It never exits on its own, so `exec_command`'s short
/// `yield_time_ms` returns with it still alive.
fn runaway_script(path: &Path, pidfile: &Path) {
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\n\
             : > '{pidfile}'\n\
             /bin/sleep {RUNAWAY_SECS} &\n\
             echo \"$!\" >> '{pidfile}'\n\
             echo \"$$\" >> '{pidfile}'\n\
             exec /bin/sleep {RUNAWAY_SECS}\n",
            pidfile = pidfile.display()
        ),
    )
    .expect("runaway script is written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The code-mode JavaScript for the child's single `exec` tool call.
///
/// Two `exec_command`s. The first is case B: `yield_time_ms` of 250 ms against a command that runs
/// for 900 s, so `exec` yields back with a `session_id` and the process still alive. The second
/// simply keeps the tool call itself in flight, so the whole call is still running when marion's
/// bound expires rather than having handed control back to the model.
fn case_b_js(runaway: &Path, pidfile: &Path) -> String {
    let cmd = serde_json::to_string(&format!(
        "/bin/sh {} {}",
        runaway.display(),
        pidfile.display()
    ))
    .unwrap();
    let hold = serde_json::to_string(&format!("/bin/sleep {RUNAWAY_SECS}")).unwrap();
    format!(
        "// @exec: {{\"yield_time_ms\": 900000, \"max_output_tokens\": 2000}}\n\
         const b = await tools.exec_command({{ cmd: {cmd}, shell: \"/bin/sh\", login: false, \
         yield_time_ms: 250, max_output_tokens: 2000 }});\n\
         text(JSON.stringify({{caseB: b}}));\n\
         const held = await tools.exec_command({{ cmd: {hold}, shell: \"/bin/sh\", login: false, \
         yield_time_ms: 900000, max_output_tokens: 200 }});\n\
         text(JSON.stringify({{held: held}}));\n"
    )
}

fn pids_in(path: &Path) -> Vec<i32> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[test]
fn a_timed_out_codex_child_leaves_no_surviving_tool_call_descendant() {
    assert!(
        // `on_path`, not an inlined `--version` spawn: this file carried its own copy, which was
        // the pre-consolidation shape and — once the shared helper started checking the version —
        // would have been the one gate in the suite that still accepted any binary of that name.
        on_path("codex"),
        "M1's sixth acceptance criterion is about a REAL codex child; put `codex` ({}) on PATH",
        pinned_version("codex")
    );

    let root_dir = scratch("s7-timeout");
    let repo = fixture_repo(&root_dir);
    let state = root_dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let pidfile = root_dir.join("runaway-pids.txt");
    let runaway = root_dir.join("runaway.sh");
    runaway_script(&runaway, &pidfile);

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root_dir.join("provider-requests.jsonl"),
        script: Script {
            child_exec_js: Some(case_b_js(&runaway, &pidfile)),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");

    let env = Env {
        project_dir: ProjectDir::new(&state, &repo),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    let req = SpawnRequest {
        agent_type: "codex-impl".into(),
        prompt: "Start the long-running command and keep it running.".into(),
        repo: repo.clone(),
        acceptance_criteria: vec!["the command is running".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        // None, exactly as before this field existed: `codex exec` takes no model argument, so
        // this criterion's invocation is byte-identical to the one it has always measured.
        model: None,
    };
    let started = Instant::now();
    // A root caller (§6.1 step 2): depth 0, the same top-level `spawn` `marion run` produces.
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let contract = run_spawn(&env, &req, &TaskId("s7-timeout".into()), &caller)
        .expect("the spawn path runs to a contract");
    let elapsed = started.elapsed();

    // What marion enumerated in step 1 of its own two-step kill, plus what the tool call recorded.
    let enumerated = last_kill_sweep();
    let recorded = pids_in(&pidfile);

    // Give killed processes a moment to leave the table, then clean up UNCONDITIONALLY — before a
    // single assertion — so a failing run can never be the leak it is testing for.
    let mut survivors: Vec<i32> = enumerated.clone();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        survivors.retain(|p| alive(*p));
        if survivors.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let recorded_survivors: Vec<i32> = recorded.iter().copied().filter(|p| alive(*p)).collect();
    for p in enumerated.iter().chain(recorded.iter()) {
        kill_hard(*p);
    }
    drop(server);

    // ---- the tool call really reached case B. --------------------------------------------------
    assert_eq!(
        recorded.len(),
        2,
        "the codex tool call did not record its runaway pids in {} within {elapsed:?}, so this \
         run never reached case B and the criterion would be vacuous. Got {recorded:?}",
        pidfile.display()
    );

    // ---- criterion 6, first half: a non-empty enumerated set. ----------------------------------
    assert!(
        !enumerated.is_empty(),
        "marion enumerated no descendants at expiry; an empty set makes the ESRCH check vacuous"
    );
    assert!(
        recorded.iter().all(|p| enumerated.contains(p)),
        "marion's step-1 enumeration missed the tool call's own processes: enumerated \
         {enumerated:?}, tool call recorded {recorded:?}"
    );

    // ---- criterion 6, second half: ESRCH for every pid in that set. ----------------------------
    assert!(
        survivors.is_empty(),
        "processes marion enumerated survived the timeout kill — the S7 leak: {survivors:?} \
         (kill(pid, 0) did not return ESRCH; EPERM counts as a survivor). Enumerated set was \
         {enumerated:?}"
    );
    assert!(
        recorded_survivors.is_empty(),
        "the tool call's own setsid'd processes outlived the expired child: {recorded_survivors:?}"
    );

    // ---- the node was expired, not merely finished. --------------------------------------------
    let completion = contract
        .completion
        .as_ref()
        .expect("a terminal node has a completion");
    assert_eq!(
        completion.status,
        ExitStatus::TimedOut,
        "the child must have been expired by marion's bound, not have exited on its own; \
         ran for {elapsed:?} against a {CHILD_TIMEOUT_SECS}s bound. exit: {}",
        completion.exit.description
    );
}
