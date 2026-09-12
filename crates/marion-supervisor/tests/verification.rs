//! **§5.4's `verification`, run for real at a child's terminal transition.**
//!
//! A real `codex` child, driven by the in-process [`CannedServer`], edits `src/keep.txt`; marion
//! then runs the parent's verification lines by `sh -c` in the child's worktree, before the reap,
//! and judges the contract by them. Two things are measured, both against the **persisted**
//! contract because that copy is the uncapped record (`cap_for_return` applies to the returned one
//! only):
//!
//! 1. A non-zero exit fails the contract — status `Failed`, the description saying how many of
//!    the commands did not exit 0 — and every outcome lands in `evidence`, the passing one carrying
//!    the child's own edit in its stdout, which is what proves the commands ran in the child's
//!    workspace *after* the child was done with it.
//! 2. A passing verification leaves an `Ok` child `Ok`, with its evidence recorded.
//!
//! Both assert that `changed_paths` is untouched by the verification itself: the commands run
//! after the diff is taken, so a `cargo test` that writes a `target/` cannot show up as the child's
//! work.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test verification
//! ```
//!
//! Needs a real `codex` on `PATH`; a runner declared to have no harnesses skips loudly by name.

use std::path::{Path, PathBuf};

use marion_core::contract::{ExitStatus, Isolation, TaskContract, TaskId, Workspace};
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, harness_available, judge, persisted_contracts, scratch};
use serde_json::Value;

const CHILD_TIMEOUT_SECS: u64 = 60;
const NARRATIVE: &str = "Edited the worktree and reported back without committing.";
const EDIT: &str = "keep, edited by the canned codex child";

/// The child **modifies a file already in `base_commit`**, so `cat` of it after the run is the
/// child's bytes and nothing else.
const MODIFY: &str = "*** Begin Patch\n*** Update File: src/keep.txt\n@@\n-keep\n\
                      +keep, edited by the canned codex child\n*** End Patch";

struct Fixture {
    repo: PathBuf,
    state: PathBuf,
    env: Env,
    /// Held, not dropped: dropping the server closes the port the child talks to.
    _server: CannedServer,
}

fn fixture(root: &Path) -> Fixture {
    let repo = fixture_repo(root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root.join("provider-requests.jsonl"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            child_patch: MODIFY.to_string(),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");

    let env = Env {
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state: state.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    Fixture {
        repo,
        state,
        env,
        _server: server,
    }
}

fn spawn_one(fx: &Fixture, task_id: &str, verification: &[&str]) -> TaskContract {
    let req = SpawnRequest {
        agent_type: "codex-impl".into(),
        prompt: "Edit the file under src/ and report back through marion.".into(),
        repo: fx.repo.clone(),
        acceptance_criteria: vec!["a file under src/ was edited".into()],
        verification: verification.iter().map(|s| s.to_string()).collect(),
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
    };
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    run_spawn(&fx.env, &req, &TaskId(task_id.into()), &caller)
        .unwrap_or_else(|e| panic!("the child runs: {e}"))
}

/// The contract marion wrote to disk — the uncapped copy.
fn persisted_contract(state: &Path, task_id: &str) -> TaskContract {
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

fn worktree_of(c: &TaskContract) -> PathBuf {
    match &c.workspace {
        Workspace::Worktree { path, .. } => path.clone(),
        other => panic!("a codex child runs in a worktree, got {other:?}"),
    }
}

#[test]
fn a_failing_verification_command_fails_the_contract_and_carries_its_evidence() {
    if !harness_available("codex") {
        return;
    }
    let _root = scratch("verif-e2e-failing");
    let fx = fixture(&_root);
    let returned = spawn_one(&fx, "verif-failing", &["cat src/keep.txt", "exit 3"]);
    let wt = worktree_of(&returned);

    let c = persisted_contract(&fx.state, "verif-failing");
    let comp = c.completion.as_ref().expect("a finished run completes");
    assert_eq!(
        comp.status,
        ExitStatus::Failed,
        "`exit 3` did not exit 0, so the child's clean report is not the last word: {}",
        comp.exit.description
    );
    assert!(
        comp.exit
            .description
            .contains("verification: 1 of 2 commands did not exit 0"),
        "the description says which: {}",
        comp.exit.description
    );

    assert_eq!(c.verification.len(), 2, "both lines are on the record");
    for (cmd, line) in c.verification.iter().zip(["cat src/keep.txt", "exit 3"]) {
        assert_eq!(cmd.program, "sh");
        assert_eq!(cmd.args, vec!["-c".to_string(), line.to_string()]);
        assert_eq!(
            cmd.cwd, wt,
            "run in the child's worktree, not the operator's tree"
        );
    }

    assert_eq!(comp.evidence.len(), 2);
    assert_eq!(comp.evidence_omitted, 0, "the persisted copy is whole");
    assert_eq!(comp.evidence[0].exit_code, Some(0));
    assert!(
        comp.evidence[0].stdout.value.contains(EDIT),
        "`cat` ran after the child's edit and in its worktree: {:?}",
        comp.evidence[0].stdout.value
    );
    assert!(!comp.evidence[0].timed_out);
    assert_eq!(comp.evidence[1].exit_code, Some(3));
    assert!(!comp.evidence[1].timed_out);

    assert_eq!(
        comp.changed_paths,
        vec![PathBuf::from("src/keep.txt")],
        "verification runs after the diff is taken, so it cannot pollute the child's changes"
    );
    assert!(
        !wt.exists(),
        "the worktree is still reaped after verification"
    );
}

#[test]
fn a_passing_verification_leaves_an_ok_child_ok_with_its_evidence_recorded() {
    if !harness_available("codex") {
        return;
    }
    let _root = scratch("verif-e2e-passing");
    let fx = fixture(&_root);
    let returned = spawn_one(&fx, "verif-passing", &["cat src/keep.txt"]);
    let wt = worktree_of(&returned);

    let c = persisted_contract(&fx.state, "verif-passing");
    let comp = c.completion.as_ref().expect("a finished run completes");
    assert_eq!(comp.status, ExitStatus::Ok, "{}", comp.exit.description);
    assert!(
        !comp.exit.description.contains("verification"),
        "nothing failed, so nothing is claimed: {}",
        comp.exit.description
    );
    assert_eq!(c.verification.len(), 1);
    assert_eq!(c.verification[0].cwd, wt);
    assert_eq!(comp.evidence.len(), 1);
    assert_eq!(comp.evidence[0].exit_code, Some(0));
    assert!(comp.evidence[0].stdout.value.contains(EDIT));
    assert_eq!(comp.changed_paths, vec![PathBuf::from("src/keep.txt")]);
}
