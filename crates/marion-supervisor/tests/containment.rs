//! **A node cannot escape its sandbox by delegating.**
//!
//! A codex implementer runs inside codex's own `workspace-write` sandbox: its writes and commands
//! stop at its workspace. If it could spawn a claude implementer — a harness with no sandbox, whose
//! shell runs as the operator — the child would do everything the parent was contained from doing.
//! So a caller may spawn only a type at least as contained as its own, and the refusal is by name,
//! before any side effect.
//!
//! No harness runs here: the refused spawn stops before the intent, and the allowed one is carried
//! only as far as the worktree question on a directory that is not a repository.

use std::path::Path;

use marion_core::contract::{AgentId, Isolation, TaskId};
use marion_core::journal::RecordKind;
use marion_harness::authority::Axis;
use marion_supervisor::run::{Caller, SpawnRequest, run_spawn};
use marion_supervisor::spawn::SpawnError;
use marion_supervisor::types_snapshot::TypesSnapshot;
use marion_testsupport::scratch;

mod common;

use common::canned::canned_env;

fn request(repo: &Path, agent_type: &str) -> SpawnRequest {
    SpawnRequest {
        race: None,
        review: None,
        agent_type: agent_type.into(),
        prompt: "go".into(),
        repo: repo.to_path_buf(),
        acceptance_criteria: vec![],
        writable_scope: vec![],
        timeout_secs: 30,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        verification: vec![],
        profile: None,
        budget: None,
        read_only: false,
        workflow: None,
    }
}

fn spawn_from(caller_type: &str, child_type: &str) -> Result<(), SpawnError> {
    spawn_opted(caller_type, child_type, false).0
}

/// A spawn from a caller whose tree carries (or not) the operator's containment opt-in, and the
/// journal it left behind.
fn spawn_opted(
    caller_type: &str,
    child_type: &str,
    opt_in: bool,
) -> (Result<(), SpawnError>, Vec<RecordKind>) {
    spawn_in(caller_type, child_type, opt_in, false)
}

/// [`spawn_opted`], from a root whose session the operator started (or not) in a read-only mode.
fn spawn_in(
    caller_type: &str,
    child_type: &str,
    opt_in: bool,
    read_only_session: bool,
) -> (Result<(), SpawnError>, Vec<RecordKind>) {
    let dir = scratch(&format!(
        "containment-{caller_type}-{child_type}-{opt_in}-{read_only_session}"
    ));
    // Not a repository: an allowed spawn stops at the worktree question, before any process.
    let tree = dir.join("tree");
    std::fs::create_dir_all(&tree).unwrap();
    let env = canned_env(&dir.join("state"), &tree, None);
    // The caller's recorded table, carrying the opt-in exactly as a root started with it would.
    TypesSnapshot::take(&tree, Some(caller_type))
        .unwrap()
        .allowing_wider_children(opt_in)
        .with_read_only_session(read_only_session)
        .write(env.project_dir.agent(&AgentId("root".into())).path())
        .unwrap();
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin(caller_type).unwrap(),
    );
    let result = run_spawn(
        &env,
        &request(&tree, child_type),
        &TaskId("t-1".into()),
        &caller,
    )
    .map(|_| ());
    (result, common::journal::records(&env.project_dir.journal()))
}

/// A sandboxed codex implementer asking for a claude implementer is refused by name.
#[test]
fn a_sandboxed_node_cannot_spawn_an_uncontained_one() {
    let err = spawn_from("codex", "claude").expect_err("the escape must be refused");
    assert!(
        matches!(&err, SpawnError::WiderThanParent(r) if r.axes == [Axis::Containment]),
        "refused for containment, not for something later: {err}"
    );
    let said = err.to_string();
    assert!(
        said.starts_with(
            "codex runs sandboxed here; claude has no sandbox marion can apply, so a codex agent \
             can't start it."
        ),
        "{said}"
    );
    assert!(
        said.contains("allow_wider_children = true")
            && said.contains("marion run --allow-wider-children"),
        "the refusal says how to allow it: {said}"
    );
}

/// **With the operator's opt-in the same spawn passes the gate, and is journaled**: one
/// `WiderDelegation` naming the child and both types, beside the child's intent.
#[test]
fn the_operators_opt_in_lets_the_spawn_through_and_journals_it() {
    let (result, kinds) = spawn_opted("codex", "claude", true);
    let err = result.expect_err("no repository, so no worktree");
    assert!(matches!(err, SpawnError::NotAGitRepo { .. }), "{err}");
    let delegated: Vec<_> = kinds
        .iter()
        .filter_map(|k| match k {
            RecordKind::WiderDelegation(d) => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(delegated.len(), 1, "{kinds:?}");
    assert_eq!(
        (
            delegated[0].caller_type.as_str(),
            delegated[0].child_type.as_str()
        ),
        ("codex", "claude")
    );
    assert_eq!(delegated[0].axes, ["containment"]);
    // An allowed spawn that needed no opt-in leaves no such record.
    let (_, kinds) = spawn_opted("codex", "codex", true);
    assert!(
        !kinds
            .iter()
            .any(|k| matches!(k, RecordKind::WiderDelegation(_))),
        "{kinds:?}"
    );
}

/// A node may spawn a type holding no more than itself on every axis. Each gets past the gate,
/// to the worktree question.
#[test]
fn a_node_may_spawn_types_at_least_as_contained_as_itself() {
    for (caller, child) in [
        ("codex", "codex"),
        ("codex", "claude-orchestrator"),
        ("claude-orchestrator", "claude-orchestrator"),
        // A built-in planner delegates writes: starting an implementer is its job.
        ("claude-orchestrator", "codex"),
        ("claude", "claude"),
        ("claude", "codex"),
    ] {
        let err = spawn_from(caller, child).expect_err("no repository, so no worktree");
        assert!(
            matches!(err, SpawnError::NotAGitRepo { .. }),
            "{caller} -> {child} passed the containment gate: {err}"
        );
    }
}

/// **A session started in plan mode is a read-only non-delegator**, even on claude's implementer
/// type: it may start read-only agents and nothing that writes.
#[test]
fn a_plan_mode_session_starts_only_read_only_agents() {
    let (result, _) = spawn_in("claude", "codex", false, true);
    let err = result.expect_err("a planning session cannot hand out writes");
    assert!(
        matches!(&err, SpawnError::WiderThanParent(r) if r.axes == [Axis::ReadOnly]),
        "{err}"
    );
    let (result, _) = spawn_in("claude", "claude-orchestrator", false, true);
    assert!(
        matches!(result, Err(SpawnError::NotAGitRepo { .. })),
        "a read-only child passes the gate: {result:?}"
    );
}
