//! **A review, end to end: a canned claude reviews a canned codex child's committed work.**
//!
//! One run of real binaries against marion's canned provider. A claude root delegates a one-file
//! change to a codex child (the shared [`claude_delegates_to_codex`] script); once both have
//! ended, the operator asks for a review of the child over the socket, exactly as `marion review`
//! does. The reviewer is scripted to try a write first — refused by the harness, because a
//! reviewer's claude launch declares no `Write` — and then to report one high finding on the
//! child's file.
//!
//! What is asserted is the whole of phase 1: the reviewer sits under the child with `review_of`
//! on its intent; its contract carries the finding, grounded and blocking, and names its read-only
//! switch; nothing it tried to write landed; the refused attempt is on its recorded stream; the
//! journal carries marion's tally; and the tree shows it. Plain refusals for an unknown node and
//! for a root close the run.

use std::process::Command;
use std::time::Duration;

use marion_core::contract::{AgentId, TaskContract};
use marion_core::journal::RecordKind;
use marion_core::paths::ProjectDir;
use marion_core::proto::params::AgentSpawnParams;
use marion_core::proto::{Call, Method, MethodResult, Outcome};
use marion_core::review::{Decision, Severity};
use marion_provider::{CannedServer, Config, NodeScript, Script, ScriptedCall};
use marion_supervisor::courier::{Delivered, await_contract};
use marion_supervisor::socket::project_root;
use marion_testsupport::{fixture_repo, scratch};
use serde_json::json;

use crate::common;
use common::client::{Client, paths_for};
use common::journal::records;
use common::script::{Delegation, claude_delegates_to_codex};

const BOUND: Duration = Duration::from_secs(180);
const ROOT_MARKER: &str = "MARION-REVIEW-ROOT-TURN-4c1e";
const CHILD_FILE: &str = "src/review-target.txt";
const NARRATIVE: &str = "Added the review target file under src/.";
/// The review prompt's opening words (`review::prompt`), which only a reviewer's task carries.
const REVIEW_MARKER: &str = "Review the work of marion node";
const FORBIDDEN: &str = "src/reviewer-wrote-this.txt";

fn script() -> Script {
    let claude = marion_harness::adapter_for(marion_core::harness::Harness::ClaudeCode)
        .expect("claude has an adapter");
    let report = json!({
        "verdict": "block",
        "summary": "One problem in the added file.",
        "findings": [{
            "severity": "high",
            "file": CHILD_FILE,
            "line": 1,
            "claim": "the marker line is not what the task asked for",
            "evidence": "line 1",
            "recommendation": "write the requested text"
        }]
    })
    .to_string();
    Script {
        nodes: vec![NodeScript {
            marker: REVIEW_MARKER.into(),
            call_prefix: "rvw".into(),
            turns: vec![
                // Scripted although the launch offers no `Write`: the harness, not the model's
                // manners, must be what stops it.
                ScriptedCall::new(
                    "Write",
                    json!({"file_path": FORBIDDEN, "content": "a reviewer must not write"}),
                ),
                ScriptedCall::new(
                    claude.marion_tool_name("report"),
                    json!({"narrative": report}),
                ),
            ],
            final_text: "Reviewed.".into(),
        }],
        ..claude_delegates_to_codex(Delegation {
            root_marker: ROOT_MARKER,
            root_final_text: "The child completed the task and reported back.",
            child_narrative: NARRATIVE,
            child_file: CHILD_FILE,
            child_file_line: "marion review target",
            child_final_narrative: NARRATIVE,
            child_timeout_secs: 60,
        })
    }
}

fn review_params(target: &str, repo: &std::path::Path) -> AgentSpawnParams {
    AgentSpawnParams {
        wider_children: None,
        budget_tokens: None,
        review_of: Some(AgentId(target.into())),
        candidates: vec![],
        race: None,
        notify_parent: false,
        agent_type: "claude".into(),
        prompt: String::new(),
        native_launch: None,
        caller: None,
        repo: Some(repo.to_path_buf()),
        acceptance_criteria: vec![],
        verification: vec![],
        writable_scope: vec![],
        timeout_secs: Some(60),
        model: None,
        no_change_record: None,
        pane: None,
        isolation: None,
        allow_concurrent_writes: None,
        profile: None,
    }
}

#[test]
fn a_canned_claude_reviews_a_canned_codex_childs_branch_read_only() {
    if !common::script::require_claude_and_codex() {
        return;
    }
    let dir = scratch("review-e2e");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let key = project_root(&repo);
    let project = ProjectDir::new(&state, &key);

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(),
    })
    .expect("the canned provider binds");
    // ---- the work: a claude root delegates a one-file change to a codex child -------------------
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args(["run", "claude-orchestrator", "--prompt"])
        .arg(format!("{ROOT_MARKER}: delegate the review-target task."))
        .args(["--repo", repo.to_str().unwrap()])
        .args(["--state-dir", state.to_str().unwrap()])
        .args([
            "--base-url",
            &server.base_url(),
            "--canned",
            "--timeout",
            "120",
        ])
        .output()
        .expect("marion run completes");
    assert!(
        out.status.success(),
        "marion run failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let intents = |kind: &RecordKind| match kind {
        RecordKind::SpawnIntent(i) => Some(i.clone()),
        _ => None,
    };
    let journal = project.journal();
    let all: Vec<_> = records(&journal).iter().filter_map(intents).collect();
    let root = all.iter().find(|i| i.parent_id.is_none()).expect("a root");
    let child = all
        .iter()
        .find(|i| i.parent_id.is_some())
        .expect("the codex child")
        .agent_id
        .clone();

    struct Stopped(common::Supervisor);
    impl Drop for Stopped {
        fn drop(&mut self) {
            self.0.stop();
        }
    }
    // `marion run` quits the supervisor it used when its root ends, so the review is served by a
    // fresh one, which replays the journal the run left — as `marion review` after a session is.
    let _sup = Stopped(common::Supervisor::start(
        &state,
        &key,
        &std::env::var("PATH").unwrap_or_default(),
        &server.base_url(),
        BOUND,
    ));

    // ---- the review, as `marion review` asks for it ---------------------------------------------
    let paths = paths_for(&state, &repo);
    let mut c = Client::dial(&paths);
    let id = c.send(Call::AgentSpawn(review_params(&child.0, &repo)));
    let (_, outcome) = c.read_to_response(id);
    let Outcome::Result(body) = outcome else {
        panic!("the review was refused: {outcome:?}")
    };
    let MethodResult::AgentSpawn(spawned) = Method::AgentSpawn.decode_result(&body).unwrap() else {
        panic!("wrong result")
    };
    let reviewer = spawned.agent_id.clone();
    let task = spawned.task_id.clone().expect("a reviewer has a contract");
    let delivered = await_contract(&paths, &project, &reviewer, Some(&task), BOUND)
        .expect("the reviewer's end is readable");
    assert!(
        !matches!(delivered, Delivered::StillRunning),
        "the reviewer did not end within {BOUND:?}"
    );

    // ---- placed under the child, and journaled as its review ------------------------------------
    let recs = records(&journal);
    let intent = recs
        .iter()
        .filter_map(intents)
        .find(|i| i.agent_id == reviewer)
        .expect("the reviewer's intent");
    assert_eq!(intent.review_of.as_ref(), Some(&child));
    assert_eq!(
        intent.parent_id.as_ref(),
        Some(&child),
        "under the node it reviews"
    );
    let contract: TaskContract = serde_json::from_slice(
        &std::fs::read(project.agent(&reviewer).contract(&task)).expect("the reviewer's contract"),
    )
    .unwrap();
    let told = || format!("{:#?}", contract.completion);
    // The contract file is written, and the node's stream closed, just before its journal record
    // (`run_spawn_watched`), so a reader woken by the stream's end waits for the record.
    let tally_on_record = || {
        records(&journal).iter().find_map(|k| match k {
            RecordKind::ContractPersisted(p) if p.agent_id == reviewer => Some(p.review),
            _ => None,
        })
    };
    let deadline = std::time::Instant::now() + BOUND;
    let tally = loop {
        if let Some(t) = tally_on_record() {
            break t;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no contract record: {}",
            told()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    .unwrap_or_else(|| panic!("the journal carries the reviewer's tally: {}", told()));
    assert_eq!((tally.findings, tally.blocking), (1, 1));
    assert_eq!(tally.decision, Decision::Block);

    // ---- the contract: a grounded, blocking finding; read-only named; nothing landed ------------
    assert!(
        contract
            .allowed_tools
            .iter()
            .any(|t| t == "read-only:tools-axis"),
        "{:?}",
        contract.allowed_tools
    );
    let comp = contract.completion.as_ref().expect("the reviewer ended");
    let findings = comp
        .findings
        .as_ref()
        .expect("findings on the reviewer's completion");
    assert_eq!(findings.findings.len(), 1, "{findings:?}");
    let f = &findings.findings[0];
    assert_eq!(
        (f.severity, f.file.as_str(), f.grounded),
        (Severity::High, CHILD_FILE, true)
    );
    assert!(
        comp.changed_paths.is_empty() && comp.scope_violations.is_empty(),
        "a refused write changes nothing: {:?} {:?}",
        comp.changed_paths,
        comp.scope_violations
    );
    assert!(
        comp.branch.is_none() && comp.commit.is_none(),
        "a reviewer lands nothing"
    );
    assert!(
        comp.scope_enforced,
        "an empty writable scope is enforced, so any change would have been a violation"
    );
    assert!(
        !repo.join(FORBIDDEN).exists(),
        "nothing reached the operator's tree"
    );

    // ---- the refused attempt is on the reviewer's recorded stream ------------------------------
    let events = std::fs::read_to_string(project.agent(&reviewer).path().join("events.jsonl"))
        .expect("the reviewer's stream");
    assert!(
        events.contains("No such tool available: Write"),
        "the harness's refusal of the write must be recorded"
    );

    // ---- the tree shows it under the child, with its count --------------------------------------
    let tree = c.tree();
    let row = tree
        .iter()
        .find(|n| n.agent_id == reviewer)
        .expect("the reviewer is in the tree");
    assert_eq!(row.review_of.as_ref(), Some(&child));
    assert_eq!(row.review, Some(tally));
    assert_eq!(
        marion_supervisor::tree::review_note(row).as_deref(),
        Some("review: 1 finding, 1 blocking")
    );

    // ---- what `marion review` prints ------------------------------------------------------------
    let lines = marion_supervisor::review::summary_lines(&reviewer, &contract);
    assert_eq!(
        lines[0],
        format!(
            "review {}: 1 finding — One problem in the added file.",
            marion_supervisor::tree::short_id(&reviewer.0)
        )
    );
    assert!(
        lines[1].contains(&format!("High {CHILD_FILE}:1: ")),
        "{lines:?}"
    );

    // ---- plain refusals, through the same call `marion review` makes ---------------------------
    for (target, says) in [
        ("no-such-node", "no node with that id"),
        (root.agent_id.0.as_str(), "it is a root"),
    ] {
        let e = marion_supervisor::review::request(
            &paths,
            &project,
            &AgentId(target.into()),
            "claude",
            None,
            &repo,
            BOUND,
        )
        .expect_err(&format!("a review of {target} must be refused"));
        assert!(
            e.contains(&format!("marion cannot review node {target}: ")) && e.contains(says),
            "{e}"
        );
    }
    drop(server);
}
