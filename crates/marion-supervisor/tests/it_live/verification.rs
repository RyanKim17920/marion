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
//! cargo test -p marion-supervisor --test it_live verification::
//! ```
//!
//! Needs a real `codex` on `PATH`; a runner declared to have no harnesses skips loudly by name.

use std::path::{Path, PathBuf};

use marion_core::contract::{ExitStatus, Isolation, TaskContract, TaskId, Workspace};
use marion_provider::Script;
use marion_supervisor::journal::read_path;
use marion_supervisor::run::{Caller, MAX_VERIFICATION_BYTES, SpawnRequest, run_spawn};
use marion_supervisor::spawn::SpawnError;
use marion_testsupport::{fixture_repo, harness_available, persisted_contract, scratch};

use crate::common;

use common::canned::{CannedFixture as Fixture, canned_fixture};

const CHILD_TIMEOUT_SECS: u64 = 60;
const NARRATIVE: &str = "Edited the worktree and reported back without committing.";
const EDIT: &str = "keep, edited by the canned codex child";

/// The child **modifies a file already in `base_commit`**, so `cat` of it after the run is the
/// child's bytes and nothing else.
const MODIFY: &str = "*** Begin Patch\n*** Update File: src/keep.txt\n@@\n-keep\n\
                      +keep, edited by the canned codex child\n*** End Patch";

fn fixture(root: &Path) -> Fixture {
    canned_fixture(
        root,
        fixture_repo(root),
        Script {
            child_narrative: NARRATIVE.into(),
            child_patch: MODIFY.to_string(),
            ..Script::default()
        },
    )
}

fn request(fx: &Fixture, verification: &[&str]) -> SpawnRequest {
    SpawnRequest {
        budget: None,
        review: None,
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
        profile: None,
        race: None,
        read_only: false,
        workflow: None,
    }
}

fn spawn_one(fx: &Fixture, task_id: &str, verification: &[&str]) -> TaskContract {
    let req = request(fx, verification);
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    run_spawn(&fx.env, &req, &TaskId(task_id.into()), &caller)
        .unwrap_or_else(|e| panic!("the child runs: {e}"))
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
    // A codex child: each line runs inside codex's own workspace sandbox, and the record is the
    // command that actually ran.
    for (cmd, line) in c.verification.iter().zip(["cat src/keep.txt", "exit 3"]) {
        assert_eq!(cmd.program, "codex");
        assert!(
            cmd.args.contains(&"sandbox".to_string())
                && cmd
                    .args
                    .ends_with(&["sh".to_string(), "-c".into(), line.to_string()]),
            "{:?}",
            cmd.args
        );
        assert_eq!(
            cmd.cwd, wt,
            "run in the child's worktree, not the operator's tree"
        );
    }

    assert_eq!(comp.evidence.len(), 2);
    assert_eq!(
        comp.verification_containment,
        Some(marion_core::contract::VerificationContainment::Sandboxed),
        "the contract says where the lines ran"
    );
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

/// **Verification that would not fit in the journal is refused before the node exists.**
///
/// The lines ride on the node's `SpawnIntent`, and a record over the journal's cap is dropped
/// whole — which would lose the node's identity record, not merely its verification. So the spawn
/// is refused by name before the intent is written: no node in the journal, and a sentence the
/// caller can act on. Needs no harness, because nothing is launched.
#[test]
fn oversized_verification_is_refused_by_name_and_writes_no_intent() {
    let _root = scratch("verif-oversized");
    let fx = fixture(&_root);
    let req = SpawnRequest {
        verification: vec!["x".repeat(MAX_VERIFICATION_BYTES)],
        ..request(&fx, &[])
    };
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let e = run_spawn(&fx.env, &req, &TaskId("verif-oversized".into()), &caller)
        .expect_err("an intent that cannot be journaled is refused");
    assert!(
        matches!(
            e,
            SpawnError::VerificationTooLarge { bytes, cap }
                if bytes == MAX_VERIFICATION_BYTES + 4 && cap == MAX_VERIFICATION_BYTES
        ),
        "{e:?}"
    );
    let said = e.to_string();
    assert!(
        said.contains(&MAX_VERIFICATION_BYTES.to_string()) && said.contains("verification"),
        "the sentence names the field and the cap: {said}"
    );
    let replay = read_path(&fx.env.project_dir.journal()).expect("the journal replays");
    assert!(
        replay.nodes().is_empty(),
        "a refused spawn creates no node: {:?}",
        replay.nodes()
    );
}

/// **The spawn's verification lines are on the node's intent**, which is what a restart reads to
/// relaunch a lost child — so a resumed child re-runs what its spawn asked for.
#[test]
fn a_spawns_verification_is_journaled_on_its_intent() {
    if !harness_available("codex") {
        return;
    }
    let _root = scratch("verif-journaled");
    let fx = fixture(&_root);
    spawn_one(&fx, "verif-journaled", &["true"]);
    let replay = read_path(&fx.env.project_dir.journal()).expect("the journal replays");
    let intents: Vec<_> = replay
        .nodes()
        .iter()
        .filter_map(|n| n.intent.as_ref())
        .filter(|i| i.task_id == Some(TaskId("verif-journaled".into())))
        .collect();
    let [intent] = intents.as_slice() else {
        panic!("exactly one intent for the child: {:?}", replay.nodes());
    };
    assert_eq!(
        intent.verification,
        vec!["true".to_string()],
        "the lines a restart must re-run are on the record"
    );
}
