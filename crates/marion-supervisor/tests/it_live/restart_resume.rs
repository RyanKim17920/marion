//! **Restart-and-resume, end to end** (`plan-restart-resume.md` step 8).
//!
//! The whole arc of the feature in one real run, on a **non-claude** harness because this machine's
//! `claude` is pinned ahead of the fixtures: a codex root spawns a codex child, both are driven
//! through marion's canned provider until they have live processes and journaled sessions, their
//! supervisor is SIGKILLed out from under them, and then a node is brought back.
//!
//! **Once per depth.** The two tests run the same arc on the root and on the child, because the
//! two recover different things: a root's cwd is derivable from the project marion keys the
//! supervisor on, while a child's is a linked worktree that only its journaled workspace names.
//!
//! What the arc has to show, and why each clause is here:
//!
//! * **`marion attach` refuses** once the supervisor is gone — attaching deliberately starts no
//!   supervisor, so a lost node has nothing to attach to.
//! * **`marion resume` starts one and relaunches the node into its own id** — the same `agent_id`,
//!   back to `Live`, `spawn_generation` 2, a **new** pid (the old process is killed first, because
//!   it was `AliveAndOurs`), and a second `Spawned` on the one journal.
//! * **The journal is contiguous across the restart** — replay reconstructs the node from ordinal 0
//!   including the records written before the kill, so nothing about the first life was lost.
//! * **`marion resume` returns on its own, with `run`'s verdict** — a headless node is watched to
//!   its exit exactly as `marion run` watches a root, never handed to `marion attach`, which has
//!   no pane to attach to on a headless node and refused (exit 1, node still running).
//! * **The resumed child takes its next turn and exits clean** — a Responses request that arrives
//!   *after* the relaunch carries the resume prompt **and** the first life's marker, because codex's
//!   `thread/resume` (app-server, S36 P8) sends the thread's earlier transcript back: the proof the resume was a *real*
//!   resume of the harness's own session and not a fresh run wearing the same id. Then the second
//!   life's `ProcessExit` is code 0. Without the second clause this test was green while the demo's
//!   relaunch died at exit 2 on an argv `exec resume` rejected (`tests/fixtures/s29/`): the relaunch
//!   and the journal were asserted, the resumed child's success was not.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test restart_resume
//! ```
//!
//! The lost-child arc runs again on an **opencode** root and child (s36): the second life is
//! `opencode run --session <ses_…>` over the session store the first life left in its own
//! directory, and it is parked one request later than codex's, because opencode names its session
//! only on a frame of its first response. And once more on an **`opencode acp`** child, whose
//! session is its `session/new` answer's and whose second life reopens it with `session/load`.
//! And on an opencode child killed **during its first request**, before any frame named its
//! session: the resume finds the session by the title marion launched it under, in the harness's
//! own listing, along with the tree it was created in.
//!
//! It needs a real `codex` and `opencode` on `PATH`. Every model call is the CannedServer's: no
//! paid tokens.
//! Bounded throughout, so a wedged binary fails as a named timeout rather than a hang.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use marion_core::contract::AgentId;
use marion_provider::{CannedServer, Config, EditTurn, RootScript, RootTurn, Script, TurnGate};
use marion_testsupport::{Liveness, fixture_repo, liveness, on_path, scratch, until_within};
use serde_json::json;

use crate::common;

use common::journal::{journal_bytes, project};

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// codex speaks the OpenAI **Responses** wire under a canned provider.
const WIRE: &str = "responses";
/// In the root's prompt and nowhere else, so a provider request can be attributed to the root even
/// though root and child share the Responses wire.
const ROOT_MARKER: &str = "MARION-RESTART-RESUME-ROOT-2f7a";
const CHILD_FILE: &str = "src/restart-resume-marker.txt";
const CHILD_CONTENT: &str = "restart-resume marker\n";
const NARRATIVE: &str = "Wrote the marker under src/ and reported back.";
/// The resume's own prompt: on argv only after the relaunch, so a provider request that carries it
/// was made by the second life.
const RESUME_PROMPT: &str = "Continue: confirm the marker and report.";
/// The child's one verification line. The spawn journals it on the child's intent, so a resumed
/// child re-runs it and its second life's contract carries the outcome as evidence.
const VERIFICATION: &str = "touch VERIFIED";
/// The root's wall clock.
const ROOT_TIMEOUT: &str = "150";
const BOUND: Duration = Duration::from_secs(120);

fn journal_nodes(state: &Path, repo: &Path) -> Vec<marion_core::registry::ReplayedNode> {
    marion_core::registry::replay(&journal_bytes(state, repo))
        .nodes()
        .to_vec()
}

/// The one root of the run, by depth.
fn root_of(nodes: &[marion_core::registry::ReplayedNode]) -> Option<AgentId> {
    let roots: Vec<&marion_core::registry::ReplayedNode> =
        nodes.iter().filter(|n| n.depth() == Some(0)).collect();
    (roots.len() == 1).then(|| roots[0].agent_id.clone())
}

/// The one child of the run, by depth — the script spawns exactly one.
fn child_of(
    nodes: &[marion_core::registry::ReplayedNode],
) -> Option<marion_core::registry::ReplayedNode> {
    let children: Vec<&marion_core::registry::ReplayedNode> =
        nodes.iter().filter(|n| n.depth() == Some(1)).collect();
    (children.len() == 1).then(|| children[0].clone())
}

/// The supervisor's pid, from the first record's `<pid>-<uuid>` writer id.
fn supervisor_pid(state: &Path, repo: &Path) -> Option<i32> {
    let bytes = journal_bytes(state, repo);
    let first = String::from_utf8_lossy(&bytes).lines().next()?.to_string();
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    v["writer"]
        .as_str()
        .and_then(|w| w.split('-').next())
        .and_then(|p| p.parse().ok())
}

fn until(cond: impl FnMut() -> bool) -> bool {
    until_within(BOUND, Duration::from_millis(10), cond)
}

/// A codex root that spawns a codex child, both answered by the canned provider. The child writes
/// the marker file and reports; the root reports the child completed. The shape is journal_wiring's
/// Codex arms, for one root/child pairing.
fn codex_script() -> Script {
    let mut s = Script {
        root: Some(RootScript {
            marker: ROOT_MARKER.into(),
            turn: RootTurn {
                tool: "spawn".into(),
                args: json!({
                    "agent_type": "codex-impl",
                    "prompt": "Add the marker file under src/ and report back.",
                    "acceptance_criteria": ["a file exists under src/ containing the marker"],
                    "writable_scope": ["src/**"],
                    "verification": [VERIFICATION],
                    "timeout_secs": 60,
                }),
                final_text: "The child completed the task and reported back.".into(),
            },
        }),
        ..Script::default()
    };
    s.child_narrative = NARRATIVE.into();
    s.child_patch = format!(
        "*** Begin Patch\n*** Add File: {CHILD_FILE}\n+{}\n*** End Patch",
        CHILD_CONTENT.trim_end()
    );
    s.child_final_text = json!({ "narrative": NARRATIVE, "result_commits": [] }).to_string();
    s
}

/// An opencode root that spawns an opencode child, both answered by the canned provider on the
/// Chat Completions wire: the child writes the marker through opencode's own `write`, then reports.
fn opencode_script() -> Script {
    opencode_root_script("opencode")
}

/// The same opencode root, spawning an `opencode acp` child: the same binary, the same wire and
/// the same tool spellings, driven over ACP.
fn acp_opencode_script() -> Script {
    opencode_root_script("acp-opencode")
}

fn opencode_root_script(child_type: &str) -> Script {
    let mut s = Script {
        root: Some(RootScript {
            marker: ROOT_MARKER.into(),
            turn: RootTurn {
                tool: "marion_spawn".into(),
                args: json!({
                    "agent_type": child_type,
                    "prompt": "Add the marker file under src/ and report back.",
                    "acceptance_criteria": ["a file exists under src/ containing the marker"],
                    "writable_scope": ["src/**"],
                    "verification": [VERIFICATION],
                    "timeout_secs": 60,
                    "model": marion_core::agent_type::OPENCODE_DEFAULT_MODEL,
                }),
                final_text: "The child completed the task and reported back.".into(),
            },
        }),
        ..Script::default()
    };
    s.openai_report_tool = "marion_report".into();
    s.openai_report_args = json!({ "narrative": NARRATIVE });
    s.openai_edit = Some(EditTurn {
        tool: "write".into(),
        args: json!({ "filePath": CHILD_FILE, "content": CHILD_CONTENT }),
    });
    s
}

/// One harness's root-and-child tree, as the arc needs it.
struct Tree {
    program: &'static str,
    root_type: &'static str,
    /// The wire both nodes speak under a canned provider.
    wire: &'static str,
    /// The first request on [`Self::wire`] the gate holds: the child's first turn for codex, which
    /// names its session before it asks anything; the child's **second** for opencode, which names
    /// its session only on a frame of the first response (s36 `held-first/`).
    hold_from: u64,
    /// The child has named its session on the journal by the time it is parked — every tree but
    /// the one parked on opencode's first request, which prints no frame until it is answered.
    named_before_kill: bool,
    script: fn() -> Script,
}

const CODEX_TREE: Tree = Tree {
    program: "codex",
    root_type: "codex-impl",
    wire: WIRE,
    hold_from: 2,
    named_before_kill: true,
    script: codex_script,
};

const OPENCODE_TREE: Tree = Tree {
    program: "opencode",
    root_type: "opencode",
    wire: "openai",
    hold_from: 3,
    named_before_kill: true,
    script: opencode_script,
};

/// The opencode tree parked on the child's **first** request (the root's is the first on the
/// wire): opencode prints nothing, so no `sessionID`, while that request is in flight (s36
/// `held-first/`), and the supervisor dies with the child's session unjournaled.
const OPENCODE_FIRST_REQUEST_TREE: Tree = Tree {
    hold_from: 2,
    named_before_kill: false,
    ..OPENCODE_TREE
};

/// An `opencode acp` child names its session in the `session/new` answer, before any request, so
/// holding from the third request parks it with its session journaled whichever of its title
/// request and first turn comes first.
const ACP_OPENCODE_TREE: Tree = Tree {
    program: "opencode",
    root_type: "opencode",
    wire: "openai",
    hold_from: 3,
    named_before_kill: true,
    script: acp_opencode_script,
};

/// A codex root and its codex child, both with **live processes**, parked on the child's first
/// turn — the state a supervisor SIGKILL is applied to. Both tests below start here.
///
/// Returns the canned provider, its turn gate and the `marion run` client, which the caller reaps
/// once the supervisor it was talking to is gone.
struct Parked {
    server: CannedServer,
    gate: Arc<TurnGate>,
    run: std::process::Child,
}

fn park_a_live_tree(dir: &Path, repo: &Path, state: &Path, tree: &Tree) -> Parked {
    assert!(
        on_path(tree.program),
        "this E2E drives a real {}; put it on PATH",
        tree.program
    );
    // Hold the **second** Responses request — the child's first turn — so the tree parks with both
    // the root and the child holding live processes, exactly as `client_run.rs`'s crit-3 does.
    let gate = TurnGate::holding_from(tree.wire, tree.hold_from);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: (tree.script)(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");
    let base_url = server.base_url();

    let run = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            tree.root_type,
            "--prompt",
            &format!("{ROOT_MARKER}: delegate the marker-file task to a child."),
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--base-url",
            &base_url,
            "--canned",
            "--timeout",
            ROOT_TIMEOUT,
        ])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("marion run starts");

    assert!(
        until(|| gate.parked() >= 1),
        "the child's turn is held, so both nodes are parked with live processes"
    );
    assert!(
        until(|| journal_nodes(state, repo).len() == 2),
        "both nodes are on the journal before the supervisor dies"
    );
    Parked { server, gate, run }
}

/// SIGKILL the supervisor this run's journal names, wait for it to be gone, and reap the client
/// that died with its socket. Returns nothing: everything afterwards is read off the journal.
/// The resume's own exit, waited for under [`BOUND`] rather than forever: a resume that never
/// returns is the defect this suite now asserts against, and it must fail by name, not hang. The
/// process is killed at the bound so the test's scratch can be torn down.
fn resume_exit(resume: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let mut status = None;
    let exited = until(|| {
        status = resume.try_wait().expect("polling marion resume");
        status.is_some()
    });
    if !exited {
        let _ = resume.kill();
        let _ = resume.wait();
    }
    status
}

fn sigkill_the_supervisor(state: &Path, repo: &Path, run: &mut std::process::Child) {
    let sup = supervisor_pid(state, repo).expect("the journal names its supervisor");
    // SAFETY: `kill` on the pid this test's own supervisor journaled.
    unsafe { kill(sup, 9) };
    assert!(
        until(|| liveness(sup) == Liveness::Gone),
        "the supervisor must be gone before anything is attributed to its absence"
    );
    let _ = run.kill();
    let _ = run.wait();
}

/// **The full restart-and-resume arc.** Ignored by default because it drives a real `codex` and
/// SIGKILLs a detached supervisor: it is the acceptance test, run deliberately, not part of the
/// unit sweep. Remove `#[ignore]` (or `--include-ignored`) to run it.
#[test]
#[ignore = "drives a real codex binary and a detached supervisor; run deliberately"]
fn a_node_resumes_into_the_same_id_after_its_supervisor_is_sigkilled_and_a_new_client_hears_its_next_turn()
 {
    let dir = scratch("restart-resume-e2e");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let Parked {
        server,
        gate,
        mut run,
    } = park_a_live_tree(&dir, &repo, &state, &CODEX_TREE);
    let base_url = server.base_url();

    let before = journal_nodes(&state, &repo);
    let root_id = root_of(&before).expect("exactly one root");
    let root_before = before
        .iter()
        .find(|n| n.agent_id == root_id)
        .expect("the root replays");
    assert_eq!(root_before.spawn_generation, 1, "one life so far");
    assert!(
        root_before.harness_session.is_some(),
        "the root named its codex session before the kill: {root_before:?}"
    );
    let old_pid = root_before
        .pid
        .expect("the root has a live process on the record");
    assert_eq!(liveness(old_pid), Liveness::Alive, "its process is running");
    let records_before = marion_core::registry::replay(&journal_bytes(&state, &repo))
        .nodes()
        .iter()
        .find(|n| n.agent_id == root_id)
        .map(|n| n.records)
        .unwrap_or(0);

    // ---- the supervisor dies uncatchably ------------------------------------------------------
    sigkill_the_supervisor(&state, &repo, &mut run);

    // ---- attach refuses to start a supervisor -------------------------------------------------
    let attach = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "attach",
            &root_id.0,
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
        ])
        .output()
        .expect("marion attach runs");
    let attach_err = String::from_utf8_lossy(&attach.stderr);
    assert!(
        attach_err.contains("no supervisor is serving")
            && attach_err.contains("deliberately does not start one"),
        "attach must refuse to start a supervisor for a lost node: {attach_err}"
    );
    assert!(!attach.status.success());

    // ---- resume starts a supervisor and relaunches the root into its own id -------------------
    // Every provider request so far predates the resume; the second life's are the ones after it.
    let seq_before = server
        .requests()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r["seq"].as_u64())
        .max()
        .unwrap_or(0);
    // Backgrounded because it runs for the node's whole second life: a headless resume follows its
    // node to the exit exactly as `marion run` does, so the process returns on its own once the
    // journal has the exit — and its status and stderr are asserted below. Its stderr goes to a
    // file rather than a pipe, because nobody reads a pipe while the journal is being watched and
    // a live view that filled one would stall the run it was showing.
    let resume_stderr = dir.join("resume.stderr");
    let mut resume = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "resume",
            &root_id.0,
            "--prompt",
            RESUME_PROMPT,
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--base-url",
            &base_url,
            "--canned",
        ])
        .current_dir(&*dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&resume_stderr).expect("a stderr file"))
        .spawn()
        .expect("marion resume starts");

    // A second `Spawned` for the same id, under a new supervisor.
    assert!(
        until(|| {
            journal_nodes(&state, &repo)
                .iter()
                .find(|n| n.agent_id == root_id)
                .is_some_and(|n| n.spawn_generation >= 2)
        }),
        "the root did not come back as generation two"
    );
    // The old process was AliveAndOurs, so resume killed it before relaunching.
    assert!(
        until(|| liveness(old_pid) != Liveness::Alive),
        "the surviving pre-kill process must be killed before a second runs against the transcript"
    );

    let after = journal_nodes(&state, &repo);
    let root_after = after
        .iter()
        .find(|n| n.agent_id == root_id)
        .expect("the root still replays under its own id");
    assert_eq!(
        root_after.spawn_generation, 2,
        "the second life of one node"
    );
    assert_eq!(
        root_after.reap_state,
        marion_core::node::ReapState::Live,
        "a resumed node is Live again, not still Orphaned"
    );
    let new_pid = root_after.pid.expect("the relaunch has a pid");
    assert_ne!(new_pid, old_pid, "a new process, not the killed one");
    assert!(
        root_after.records > records_before,
        "the second life adds records to the same node's history, contiguously: {} then {}",
        records_before,
        root_after.records
    );

    // ---- the journal is contiguous across the restart -----------------------------------------
    let tree = marion_core::registry::replay(&journal_bytes(&state, &repo));
    assert!(
        tree.gaps.is_empty() && tree.truncation.is_none(),
        "replay is contiguous from ordinal 0, including the pre-kill records: {:?} / {:?}",
        tree.gaps,
        tree.truncation
    );

    // ---- the resumed child reaches its next turn ----------------------------------------------
    // The gate parks every Responses request from the second on, the second life's included; it
    // has done its job (both lives were caught with live processes), so let the turn through.
    gate.release();
    // codex's `thread/resume` sends the thread's earlier transcript back, so the second life's first
    // request — one that arrived after the relaunch — carries the resume prompt, the first life's
    // marker, and more than a single user turn. A relaunch that dies at the argv parser makes no
    // request at all; its exit is on the journal instead, so the wait ends on whichever comes
    // first and the exit is quoted rather than waited out.
    let took_turn = || {
        server.requests().unwrap_or_default().iter().any(|r| {
            r["seq"].as_u64().is_some_and(|seq| seq > seq_before)
                && r["wire"].as_str() == Some(WIRE)
                && r.to_string().contains(RESUME_PROMPT)
                && r.to_string().contains(ROOT_MARKER)
                && r["body"]["input"]
                    .as_array()
                    .is_some_and(|input| input.len() > 1)
        })
    };
    let second_life_exit = || {
        journal_nodes(&state, &repo)
            .iter()
            .find(|n| n.agent_id == root_id && n.spawn_generation >= 2)
            .and_then(|n| n.exit.clone())
    };
    assert!(
        until(|| took_turn() || second_life_exit().is_some()),
        "the resumed child neither took a turn nor exited within the bound"
    );
    assert!(
        took_turn(),
        "the resumed child exited without a request carrying the resume prompt and the session's \
         prior turns — the relaunch did not resume: {:?}",
        second_life_exit()
    );

    // ---- and exits clean ----------------------------------------------------------------------
    // The provider answers the resumed root's turn with its final text (the transcript already
    // quotes the spawn call), so the second life runs to a code-0 exit on the same journal.
    assert!(
        until(|| second_life_exit().is_some()),
        "the second life never recorded its exit"
    );
    let done = journal_nodes(&state, &repo);
    let root_done = done
        .iter()
        .find(|n| n.agent_id == root_id)
        .expect("the root still replays under its own id");
    assert_eq!(
        root_done.spawn_generation, 2,
        "the exit is the second life's"
    );
    let exit = root_done.exit.as_ref().expect("checked above");
    assert_eq!(
        (exit.code, exit.signal),
        (Some(0), None),
        "the resumed codex must finish its turn and exit clean: {exit:?}"
    );
    assert!(
        root_done.state.is_exited(),
        "the terminal transition is recorded: {:?}",
        root_done.state
    );

    // ---- and the resume itself returns, having watched the node rather than attached to it ----
    // A headless node has no display plane, so a resume that handed off to `marion attach` was
    // refused there and exited 1 while the node ran on — after detaching from it, which printed
    // §7.3.2's "unattended at a permission gate" disclosure on the way out. It now follows `marion
    // run`'s own rule for a headless root, and its exit code is `run`'s verdict on the node's
    // terminal status: 0 for `Ok`, 1 otherwise. Here that status is the supervisor's, not this
    // client's: a resumed root whose second turn is plain text is refused under §6.1 step 8
    // (`root::bridge_never_reached_exit`) exactly as a first life would be, so the code mirrors
    // whichever status the journal recorded rather than assuming a clean one.
    let status = resume_exit(&mut resume);
    let stderr = std::fs::read_to_string(&resume_stderr).unwrap_or_default();
    let verdict = match root_done.state {
        marion_core::node::NodeState::Exited(marion_core::contract::ExitStatus::Ok) => 0,
        _ => 1,
    };
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(verdict),
        "a headless resume exits with `run`'s verdict on {:?}; stderr:\n{stderr}",
        root_done.state
    );
    assert!(
        !stderr.contains("no display plane") && !stderr.contains("nobody can approve a permission"),
        "a headless resume must neither try to attach nor detach from a running node:\n{stderr}"
    );
    assert!(
        stderr.contains("frame     turn/completed"),
        "the resume rendered the second life's stream as `run` would:\n{stderr}"
    );

    // ---- teardown -----------------------------------------------------------------------------
    // Best-effort: kill any supervisor the resume started so nothing lingers past the test.
    if let Some(pid) = supervisor_pid(&state, &repo) {
        unsafe { kill(pid, 9) };
    }
}

/// **The same arc, one level down: a lost *child* comes back into its own node id.**
///
/// A root's cwd is derivable from the project marion keys the supervisor on, so the test above
/// never had to record one. A child's is a linked worktree marion cut under its agent dir, and
/// until it was journaled this case could only be refused by name — which is what
/// `MILESTONES.md`'s "child resume" open item was.
///
/// What this shows that the root arc cannot:
///
/// * **the relaunch is the same node, in the same place in the tree** — its own `agent_id`, depth
///   1, the same `parent_id`, `spawn_generation` 2;
/// * **it ran in the tree it left** — the worktree the first life was given still exists and is the
///   directory the journal recorded, and the second life did not cut another beside it;
/// * **its parent was not relaunched with it** — the root is still at generation 1, so this really
///   is a child resumed on its own and not a tree relaunched from the top;
/// * **it took its own next turn and exited clean** — a Responses request after the relaunch
///   carrying the resume prompt, then a code-0 `ProcessExit` on the second life.
#[test]
#[ignore = "drives a real codex binary and a detached supervisor; run deliberately"]
fn a_lost_child_resumes_into_its_own_node_id_under_its_parent_and_takes_its_next_turn() {
    a_lost_child_resumes(&CODEX_TREE, "restart-resume-child-e2e");
}

/// **The same arc on an opencode child**: its second life is `opencode run --session <ses_…>`
/// against the session store its first life left under its own directory, so the request after
/// the relaunch carries the resume prompt and the first life's task.
#[test]
#[ignore = "drives a real opencode binary and a detached supervisor; run deliberately"]
fn a_lost_opencode_child_resumes_its_own_session_under_its_parent_and_takes_its_next_turn() {
    a_lost_child_resumes(&OPENCODE_TREE, "restart-resume-oc-child-e2e");
}

/// **And on an `opencode acp` child**: its session is the `session/new` answer's `sessionId`, and
/// its second life opens it again with `session/load` before prompting.
#[test]
#[ignore = "drives a real opencode binary and a detached supervisor; run deliberately"]
fn a_lost_acp_opencode_child_loads_its_own_session_under_its_parent_and_takes_its_next_turn() {
    a_lost_child_resumes(&ACP_OPENCODE_TREE, "restart-resume-acp-oc-child-e2e");
}

/// **And on an opencode child whose supervisor was SIGKILLed during its first request**, before
/// its stream named a session: nothing on the journal names the session or the tree, so the resume
/// finds both in `opencode session list` under the `marion-<agent id>` title the launch gave it,
/// and the second life continues that session from that tree.
#[test]
#[ignore = "drives a real opencode binary and a detached supervisor; run deliberately"]
fn an_opencode_child_lost_during_its_first_request_resumes_the_session_its_title_names() {
    a_lost_child_resumes(
        &OPENCODE_FIRST_REQUEST_TREE,
        "restart-resume-oc-first-request-e2e",
    );
}

fn a_lost_child_resumes(tree: &Tree, tag: &str) {
    let dir = scratch(tag);
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let Parked {
        server,
        gate,
        mut run,
    } = park_a_live_tree(&dir, &repo, &state, tree);
    let base_url = server.base_url();

    // ---- the child has everything a resume of it needs, before the kill -----------------------
    // Its session and the workspace it was created in land on one record, so waiting for the
    // session is waiting for both. A child parked on its first request has neither: its harness
    // has named nothing yet, which is the case the title lookup exists for.
    if tree.named_before_kill {
        assert!(
            until(|| child_of(&journal_nodes(&state, &repo))
                .is_some_and(|c| c.harness_session.is_some())),
            "the child must name its {} session before the kill: {:?}",
            tree.program,
            journal_nodes(&state, &repo)
        );
    } else {
        assert!(
            until(|| child_of(&journal_nodes(&state, &repo)).is_some_and(|c| c.pid.is_some())),
            "the child must be running before the kill: {:?}",
            journal_nodes(&state, &repo)
        );
    }
    let before = journal_nodes(&state, &repo);
    let root_id = root_of(&before).expect("exactly one root");
    let child_before = child_of(&before).expect("exactly one child");
    let child_id = child_before.agent_id.clone();
    assert_eq!(child_before.spawn_generation, 1, "one life so far");
    if !tree.named_before_kill {
        assert_eq!(
            (
                &child_before.harness_session,
                &child_before.launch_workspace
            ),
            (&None, &None),
            "a child killed during its first request has journaled no session and no tree"
        );
    }
    // The tree it ran in: journaled with its session, or — before one was named — the worktree
    // marion cut for it, which the resume has to find by itself.
    let recorded_tree = child_before.launch_workspace.clone().map_or_else(
        || project(&state, &repo).agent(&child_id).worktree(),
        |w| w.path().to_path_buf(),
    );
    assert!(
        recorded_tree.is_dir(),
        "and that tree is on disk: {}",
        recorded_tree.display()
    );
    let old_pid = child_before
        .pid
        .expect("the child has a live process on the record");
    assert_eq!(liveness(old_pid), Liveness::Alive, "its process is running");
    let records_before = child_before.records;

    // ---- the supervisor dies uncatchably ------------------------------------------------------
    sigkill_the_supervisor(&state, &repo, &mut run);

    // ---- resume the child, by its own id ------------------------------------------------------
    let seq_before = server
        .requests()
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r["seq"].as_u64())
        .max()
        .unwrap_or(0);
    // Backgrounded for the reason the root test's is: a headless resume follows its node to the
    // exit and returns on its own, with the status asserted at the end.
    let resume_stderr = dir.join("resume.stderr");
    let mut resume = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "resume",
            &child_id.0,
            "--prompt",
            RESUME_PROMPT,
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--base-url",
            &base_url,
            "--canned",
        ])
        .current_dir(&*dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&resume_stderr).expect("a stderr file"))
        .spawn()
        .expect("marion resume starts");

    assert!(
        until(|| journal_nodes(&state, &repo)
            .iter()
            .find(|n| n.agent_id == child_id)
            .is_some_and(|n| n.spawn_generation >= 2)),
        "the child did not come back as generation two"
    );
    assert!(
        until(|| liveness(old_pid) != Liveness::Alive),
        "the surviving pre-kill process must be gone before a second runs against the transcript"
    );

    let after = journal_nodes(&state, &repo);
    let child_after = after
        .iter()
        .find(|n| n.agent_id == child_id)
        .expect("the child still replays under its own id");
    assert_eq!(
        child_after.spawn_generation, 2,
        "the second life of one node"
    );
    assert_eq!(
        child_after.reap_state,
        marion_core::node::ReapState::Live,
        "a resumed node is Live again, not still Orphaned"
    );
    assert_eq!(
        child_after.depth(),
        Some(1),
        "still a child, not a new root"
    );
    assert_eq!(
        child_after
            .intent
            .as_ref()
            .and_then(|i| i.parent_id.clone()),
        Some(root_id.clone()),
        "and still under the same parent: §7.5 never re-parents"
    );
    assert_ne!(
        child_after.pid.expect("the relaunch has a pid"),
        old_pid,
        "a new process, not the killed one"
    );
    assert!(
        child_after.records > records_before,
        "the second life adds records to the same node's history, contiguously: {} then {}",
        records_before,
        child_after.records
    );

    // ---- it ran in the tree it left, and cut no second one ------------------------------------
    assert!(
        recorded_tree.is_dir(),
        "the first life's worktree is still the one on disk: {}",
        recorded_tree.display()
    );

    // ---- and the parent was not relaunched with it --------------------------------------------
    // **Generation, not `reap_state`.** The root's own codex process survived the SIGKILL in its
    // own group, so the new supervisor's derived `Orphaned` marking is retracted the moment any
    // record about the root lands — which makes `reap_state` a race here and the generation the
    // fact: this was a child resumed on its own, not a tree relaunched from the top.
    let root_after = after
        .iter()
        .find(|n| n.agent_id == root_id)
        .expect("the root still replays");
    assert_eq!(
        root_after.spawn_generation, 1,
        "only the child was resumed; the root was not relaunched with it"
    );

    // ---- the resumed child takes its next turn and exits clean -------------------------------
    gate.release();
    let took_turn = || {
        server.requests().unwrap_or_default().iter().any(|r| {
            r["seq"].as_u64().is_some_and(|seq| seq > seq_before)
                && r["wire"].as_str() == Some(tree.wire)
                && r.to_string().contains(RESUME_PROMPT)
                // The first life's task, sent back from the harness's own session: a real resume.
                && r.to_string().contains("Add the marker file under src/")
        })
    };
    let second_life_exit = || {
        journal_nodes(&state, &repo)
            .iter()
            .find(|n| n.agent_id == child_id && n.spawn_generation >= 2)
            .and_then(|n| n.exit.clone())
    };
    assert!(
        until(|| took_turn() || second_life_exit().is_some()),
        "the resumed child neither took a turn nor exited within the bound"
    );
    assert!(
        took_turn(),
        "the resumed child exited without a request carrying the resume prompt — the relaunch did \
         not resume: {:?}",
        second_life_exit()
    );
    assert!(
        until(|| second_life_exit().is_some()),
        "the second life never recorded its exit"
    );
    let exit = second_life_exit().expect("checked above");
    assert_eq!(
        (exit.code, exit.signal),
        (Some(0), None),
        "the resumed {} child must finish its turn and exit clean: {exit:?}",
        tree.program
    );
    // The second life named its session, and it ran in the tree the first life left: the one the
    // journal recorded, or the one the harness's listing said its session was created in.
    assert_eq!(
        journal_nodes(&state, &repo)
            .iter()
            .find(|n| n.agent_id == child_id)
            .and_then(|n| n.launch_workspace.clone())
            .map(|w| w.path().to_path_buf()),
        Some(recorded_tree.clone()),
        "the relaunch did not run in, or record, a different tree"
    );

    // ---- and it re-ran the verification its spawn asked for ----------------------------------
    // The lines are on the child's intent, not on a contract the first life never wrote, so the
    // second life's contract is the only place their outcome can be — and a resume that relaunched
    // with none would record an empty `evidence` and a clean status that verified nothing.
    let task = child_after
        .intent
        .as_ref()
        .and_then(|i| i.task_id.clone())
        .expect("a child runs under a task");
    // Walked until one walk completes: the root is still running here, and its own worktree reap
    // can remove a directory between a walk's listing and its read.
    let mut walked = None;
    until(|| {
        walked = marion_testsupport::persisted_contracts(&state).ok();
        walked.is_some()
    });
    let contracts = walked.expect("contracts enumerate");
    let wanted = format!("{}.json", task.0);
    let found: Vec<&serde_json::Value> = marion_testsupport::judge(&contracts)
        .into_iter()
        .filter(|(p, _)| p.file_name().is_some_and(|f| f == wanted.as_str()))
        .map(|(_, v)| v)
        .collect();
    let [contract] = found.as_slice() else {
        panic!("exactly one persisted contract for {}: {found:?}", task.0);
    };
    let evidence = contract["completion"]["evidence"]
        .as_array()
        .unwrap_or_else(|| panic!("the second life completed with evidence: {contract}"));
    assert!(
        evidence.iter().any(|e| {
            e["command"]["args"] == json!(["-c", VERIFICATION]) && e["exit_code"] == json!(0)
        }),
        "the resumed child re-ran its spawn's verification and it passed: {evidence:?}"
    );
    let status = resume_exit(&mut resume);
    let stderr = std::fs::read_to_string(&resume_stderr).unwrap_or_default();
    assert!(
        status.is_some_and(|s| s.success()),
        "a headless child resume exits 0 with the child's clean exit, got {status:?}; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("no display plane"),
        "a headless resume must not hand off to attach:\n{stderr}"
    );

    // ---- teardown -----------------------------------------------------------------------------
    if let Some(pid) = supervisor_pid(&state, &repo) {
        unsafe { kill(pid, 9) };
    }
}
