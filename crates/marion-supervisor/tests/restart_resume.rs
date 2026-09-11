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
//! * **The resumed child takes its next turn and exits clean** — a Responses request that arrives
//!   *after* the relaunch carries the resume prompt **and** the first life's marker, because codex
//!   `exec resume` sends the session's earlier transcript back: the proof the resume was a *real*
//!   resume of the harness's own session and not a fresh run wearing the same id. Then the second
//!   life's `ProcessExit` is code 0. Without the second clause this test was green while the demo's
//!   relaunch died at exit 2 on an argv `exec resume` rejects (`tests/fixtures/s29/`): the relaunch
//!   and the journal were asserted, the resumed child's success was not.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test restart_resume
//! ```
//!
//! It needs a real `codex` on `PATH`. Every model call is the CannedServer's: no paid tokens.
//! Bounded throughout, so a wedged binary fails as a named timeout rather than a hang.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use marion_core::contract::AgentId;
use marion_provider::{CannedServer, Config, RootScript, RootTurn, Script, TurnGate};
use marion_testsupport::{Liveness, fixture_repo, liveness, on_path, scratch, until_within};
use serde_json::json;

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
/// The root's wall clock (codex is a LaunchOnly surface, so `--timeout` is a wall-clock bound).
const ROOT_TIMEOUT: &str = "150";
const BOUND: Duration = Duration::from_secs(120);

fn project(state: &Path, repo: &Path) -> marion_core::paths::ProjectDir {
    marion_core::paths::ProjectDir::new(state, &marion_supervisor::socket::project_root(repo))
}

fn journal_bytes(state: &Path, repo: &Path) -> Vec<u8> {
    std::fs::read(project(state, repo).journal()).unwrap_or_default()
}

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

fn park_a_live_tree(dir: &Path, repo: &Path, state: &Path) -> Parked {
    assert!(
        on_path("codex"),
        "this E2E drives a real codex; put it on PATH"
    );
    // Hold the **second** Responses request — the child's first turn — so the tree parks with both
    // the root and the child holding live processes, exactly as `client_run.rs`'s crit-3 does.
    let gate = TurnGate::holding_from(WIRE, 2);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: codex_script(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");
    let base_url = server.base_url();

    let run = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "codex-impl",
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
    } = park_a_live_tree(&dir, &repo, &state);
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
    // Backgrounded: `marion resume` hands off to a live attach after the relaunch, so it does not
    // return on its own. The journal is the oracle, and the process is killed once it has answered.
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
        .stderr(std::process::Stdio::null())
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
    // codex `exec resume` sends the session's earlier transcript back, so the second life's first
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

    // ---- teardown -----------------------------------------------------------------------------
    let _ = resume.kill();
    let _ = resume.wait();
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
    let dir = scratch("restart-resume-child-e2e");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let Parked {
        server,
        gate,
        mut run,
    } = park_a_live_tree(&dir, &repo, &state);
    let base_url = server.base_url();

    // ---- the child has everything a resume of it needs, before the kill -----------------------
    // Its session and the workspace it was created in land on one record, so waiting for the
    // session is waiting for both.
    assert!(
        until(
            || child_of(&journal_nodes(&state, &repo)).is_some_and(|c| c.harness_session.is_some())
        ),
        "the child must name its codex session before the kill: {:?}",
        journal_nodes(&state, &repo)
    );
    let before = journal_nodes(&state, &repo);
    let root_id = root_of(&before).expect("exactly one root");
    let child_before = child_of(&before).expect("exactly one child");
    let child_id = child_before.agent_id.clone();
    assert_eq!(child_before.spawn_generation, 1, "one life so far");
    let recorded_tree = child_before
        .launch_workspace
        .clone()
        .expect("the child's launch recorded the tree it ran in");
    assert!(
        recorded_tree.path().is_dir(),
        "and that tree is on disk: {}",
        recorded_tree.path().display()
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
    // Backgrounded for the reason the root test's is: `marion resume` hands off to a live attach,
    // so it does not return on its own and the journal is the oracle.
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
        .stderr(std::process::Stdio::null())
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
    assert_eq!(
        journal_nodes(&state, &repo)
            .iter()
            .find(|n| n.agent_id == child_id)
            .and_then(|n| n.launch_workspace.clone()),
        Some(recorded_tree.clone()),
        "the relaunch did not re-record a different tree"
    );
    assert!(
        recorded_tree.path().is_dir(),
        "the first life's worktree is still the one on disk: {}",
        recorded_tree.path().display()
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
                && r["wire"].as_str() == Some(WIRE)
                && r.to_string().contains(RESUME_PROMPT)
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
        "the resumed codex child must finish its turn and exit clean: {exit:?}"
    );

    // ---- teardown -----------------------------------------------------------------------------
    let _ = resume.kill();
    let _ = resume.wait();
    if let Some(pid) = supervisor_pid(&state, &repo) {
        unsafe { kill(pid, 9) };
    }
}
