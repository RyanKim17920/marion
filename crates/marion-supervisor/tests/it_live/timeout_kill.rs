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
//! kill, read back through [`marion_supervisor::kill::last_kill_sweep`]: `kill(pid, 0)` must return
//! `ESRCH` for every pid in it, and the set must be non-empty — an empty one would make the
//! `ESRCH` check vacuous. Asserting only that the child's own process died is *not* this
//! criterion and does not imply it: a naive `killpg` satisfies that while every `setsid`-ed
//! tool-call process keeps running, reparented to pid 1.
//!
//! The same criterion runs on a **real `opencode run`** child, whose tool call is opencode's own
//! `bash` running the same case-B script (s36's opencode parity work).
//! And on a **real `pi --mode rpc`** child, whose `bash` runs it too.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test it_live timeout_kill::
//! ```
//!
//! It needs a real `codex`, `opencode` and `pi` on `PATH` at versions
//! [`marion_testsupport::PINNED_HARNESSES`] accepts, and it is not `#[ignore]`d: like `cross_product`, a
//! criterion that quietly passes on a machine that cannot run it is worth less than no criterion.
//! No model is called — everything is served by the in-process `CannedServer`.

use std::path::Path;
use std::time::{Duration, Instant};

use marion_core::contract::Isolation;
use marion_core::contract::{ExitStatus, TaskId};
use marion_provider::{CannedServer, Config, NodeScript, Script, ScriptedCall, TurnGate};
use marion_supervisor::kill::last_kill_sweep;
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{
    alive, fixture_repo, kill_hard, on_path, pinned_version, scratch, write_executable,
};

use crate::common;

use common::canned::canned_env;

/// The child's bound: long enough for its harness to boot, take turn one and get its tool call
/// running, and the tool call is still in flight when it expires — that is the whole point.
///
/// The boot is the row's budget ([`common::boot::budget`]) and not a number typed here: this was a
/// flat 25 s, measured against `codex exec` reaching its call in ~4 s on a quiet machine, and
/// every opencode cell failed it under load with no tool call recorded — opencode had not finished
/// booting. The turn after the boot is one small request and a spawn.
fn child_timeout_secs(agent_type: &str) -> u64 {
    (common::boot::budget(agent_type) + TOOL_CALL_TURN).as_secs()
}

/// What a child spends after its boot to get its tool call running: one small request answered by
/// the canned provider, and the spawn of the call's process.
const TOOL_CALL_TURN: Duration = Duration::from_secs(10);

/// How long the runaway `sleep`s live if nothing kills them. Far past this test, so a survivor is
/// unmistakable rather than a race with its own exit.
const RUNAWAY_SECS: u64 = 900;

/// Case B's command, from `spikes/s7/runaway.sh`: a shell that backgrounds one sleeper and then
/// `exec`s into another, recording both pids. It never exits on its own, so `exec_command`'s short
/// `yield_time_ms` returns with it still alive.
fn runaway_script(path: &Path, pidfile: &Path) {
    write_executable(
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
    );
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
    // `model: None`, exactly as before this field existed: a canned codex launch compiles none.
    // codex runs over app-server since S36, so the expiry is its driver's: `turn/interrupt`, then
    // marion's kill of every process below the server (`kill::kill_descendants`).
    a_timed_out_child_leaves_no_surviving_tool_call_descendant(
        "s7-timeout",
        "codex-impl",
        None,
        "Start the long-running command and keep it running.",
        Expiry::Driver,
        // **codex's second request is held open**, so marion's bound always fires first: codex
        // ends its own session about 30 s into a code-mode `exec` it cannot finish, and a bound
        // longer than that (the row's boot budget is) let the child exit before it expired.
        Some(TurnGate::holding_from("responses", 2)),
        |runaway, pidfile| Script {
            child_exec_js: Some(case_b_js(runaway, pidfile)),
            ..Script::default()
        },
    );
}

/// The prompt marker an opencode child's canned turn is keyed on.
const OPENCODE_MARKER: &str = "MARION-TIMEOUT-OPENCODE-5b21";

/// **The same criterion on a real `opencode run` child.** Its one tool call is opencode's own
/// `bash` running the same case-B script — a sleeper backgrounded and a second one `exec`'d, so
/// the call never returns — and the child's bound expires with the call in flight. opencode starts
/// a `bash` command as its own process tree, so what is asserted is marion's sweep reaching it,
/// exactly as for codex's `setsid`'d `exec_command`.
#[test]
fn a_timed_out_opencode_child_leaves_no_surviving_tool_call_descendant() {
    assert!(
        on_path("opencode"),
        "this cell drives a REAL opencode child; put `opencode` ({}) on PATH",
        pinned_version("opencode")
    );
    a_timed_out_child_leaves_no_surviving_tool_call_descendant(
        "s7-timeout-opencode",
        "opencode",
        Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL.into()),
        &format!("{OPENCODE_MARKER}: Start the long-running command and keep it running."),
        Expiry::TwoStepSweep,
        None,
        opencode_bash_script,
    );
}

/// **And over ACP**: an `opencode acp` child's expiry is the ACP driver's `session/cancel`, SIGINT
/// and group kill rather than the launch-only path's, and the same `bash` tree must not outlive it.
#[test]
fn a_timed_out_acp_opencode_child_leaves_no_surviving_tool_call_descendant() {
    assert!(
        on_path("opencode"),
        "this cell drives a REAL opencode acp child; put `opencode` ({}) on PATH",
        pinned_version("opencode")
    );
    a_timed_out_child_leaves_no_surviving_tool_call_descendant(
        "s7-timeout-acp-opencode",
        "acp-opencode",
        None,
        &format!("{OPENCODE_MARKER}: Start the long-running command and keep it running."),
        Expiry::Driver,
        None,
        opencode_bash_script,
    );
}

/// The prompt marker a pi child's canned turn is keyed on.
const PI_MARKER: &str = "MARION-TIMEOUT-PI-3e90";

/// **And on a real `pi --mode rpc` child.** Its one tool call is pi's own `bash` running case B's
/// script. Its expiry is the JSONL channel's `abort`, which pi answers only once the in-flight call
/// finishes (S34 `pi-rpc-abort-mid-tool`) and this one never does, then the group kill, and the
/// `bash` tree must not outlive it.
#[test]
fn a_timed_out_pi_child_leaves_no_surviving_tool_call_descendant() {
    assert!(
        on_path("pi"),
        "this cell drives a REAL pi child; put `pi` ({}) on PATH",
        pinned_version("pi")
    );
    a_timed_out_child_leaves_no_surviving_tool_call_descendant(
        "s7-timeout-pi",
        "pi",
        None,
        &format!("{PI_MARKER}: Start the long-running command and keep it running."),
        Expiry::Driver,
        None,
        |runaway, pidfile| Script {
            nodes: vec![NodeScript {
                marker: PI_MARKER.into(),
                call_prefix: "pitimeout".into(),
                turns: vec![ScriptedCall::new(
                    "bash",
                    serde_json::json!({
                        "command": format!("/bin/sh {} {}", runaway.display(), pidfile.display()),
                    }),
                )],
                final_text: "The command finished.".into(),
            }],
            ..Script::default()
        },
    );
}

/// opencode's own `bash` running case B's script: the call never returns on its own.
fn opencode_bash_script(runaway: &Path, pidfile: &Path) -> Script {
    Script {
        nodes: vec![NodeScript {
            marker: OPENCODE_MARKER.into(),
            call_prefix: "octimeout".into(),
            turns: vec![ScriptedCall::new(
                "bash",
                serde_json::json!({
                    "command": format!("/bin/sh {} {}", runaway.display(), pidfile.display()),
                    "description": "Start the long-running command",
                    "timeout": RUNAWAY_SECS * 1000,
                }),
            )],
            final_text: "The command finished.".into(),
        }],
        ..Script::default()
    }
}

/// How marion ends an expired child, which decides what the cell can read back of the kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expiry {
    /// The launch-only path's two-step group kill, whose step-1 enumeration `last_kill_sweep`
    /// returns — so the cell asserts the tool call's pids were in it.
    TwoStepSweep,
    /// A typed driver's own shutdown — ACP's `session/cancel`, app-server's `turn/interrupt` and
    /// its kill of the turn's processes, then SIGINT and the group kill — whose last sweep runs
    /// after the server has exited and so is not the enumeration to read; the cell asserts on the
    /// tool call's recorded pids alone.
    Driver,
}

/// One cell at a time: see the lock's use in the helper.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// One child of `agent_type`, given `script` for its provider and a short bound, whose tool call is
/// still running when the bound expires: every process marion's step 1 enumerated, and every pid
/// the tool call recorded, must be gone, and the contract must say the child timed out.
fn a_timed_out_child_leaves_no_surviving_tool_call_descendant(
    tag: &str,
    agent_type: &str,
    model: Option<String>,
    prompt: &str,
    expiry: Expiry,
    hold: Option<std::sync::Arc<TurnGate>>,
    script: impl FnOnce(&Path, &Path) -> Script,
) {
    // `last_kill_sweep` is one process-wide record, and every cell here expires at about the same
    // moment: run concurrently, one cell would read another's sweep.
    let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root_dir = scratch(tag);
    let repo = fixture_repo(&root_dir);
    let state = root_dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let pidfile = root_dir.join("runaway-pids.txt");
    let runaway = root_dir.join("runaway.sh");
    runaway_script(&runaway, &pidfile);

    let server = CannedServer::start_held(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: root_dir.join("provider-requests.jsonl"),
            script: script(&runaway, &pidfile),
        },
        hold.clone()
            .map(|g| g as std::sync::Arc<dyn marion_provider::Hold>),
    )
    .expect("the canned provider binds");

    // Unsandboxed: the tool call records its runaway pids in the scratch dir, outside the node's
    // workspace, which marion's sandbox refuses.
    let env = Env {
        os_sandbox: false,
        ..canned_env(&state, &repo, Some(server.base_url()))
    };
    let req = SpawnRequest {
        budget: None,
        review: None,
        agent_type: agent_type.into(),
        prompt: prompt.into(),
        repo: repo.clone(),
        acceptance_criteria: vec!["the command is running".into()],
        verification: vec![],
        writable_scope: vec!["src/**".into()],
        timeout_secs: child_timeout_secs(agent_type),
        model,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        race: None,
        read_only: false,
        workflow: None,
    };
    let started = Instant::now();
    // A root caller (§6.1 step 2): depth 0, the same top-level `spawn` `marion run` produces.
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let contract = run_spawn(&env, &req, &TaskId(tag.into()), &caller)
        .expect("the spawn path runs to a contract");
    let elapsed = started.elapsed();
    // The child is gone; nothing may stay parked on the provider it left.
    if let Some(gate) = &hold {
        gate.release();
    }

    // What marion enumerated in step 1 of its own two-step kill, plus what the tool call recorded.
    let enumerated = match expiry {
        Expiry::TwoStepSweep => last_kill_sweep(),
        Expiry::Driver => Vec::new(),
    };
    let recorded = pids_in(&pidfile);

    // Give killed processes a moment to leave the table, then clean up UNCONDITIONALLY — before a
    // single assertion — so a failing run can never be the leak it is testing for.
    let mut survivors: Vec<i32> = enumerated.iter().chain(recorded.iter()).copied().collect();
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
        "the {agent_type} tool call did not record its runaway pids in {} within {elapsed:?}, so this \
         run never reached case B and the criterion would be vacuous. Got {recorded:?}",
        pidfile.display()
    );

    // ---- criterion 6, first half: a non-empty enumerated set. ----------------------------------
    if expiry == Expiry::TwoStepSweep {
        assert!(
            !enumerated.is_empty(),
            "marion enumerated no descendants at expiry; an empty set makes the ESRCH check vacuous"
        );
        assert!(
            recorded.iter().all(|p| enumerated.contains(p)),
            "marion's step-1 enumeration missed the tool call's own processes: enumerated \
             {enumerated:?}, tool call recorded {recorded:?}"
        );
    }

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
         ran for {elapsed:?} against a {}s bound. exit: {}",
        child_timeout_secs(agent_type),
        completion.exit.description
    );
}
