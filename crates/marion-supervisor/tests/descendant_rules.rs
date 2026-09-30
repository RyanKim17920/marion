//! **A descendant cannot rewrite the rules its own tree runs under.**
//!
//! A child works in a worktree marion made, which is a copy of the tree it was spawned from and
//! includes that tree's `.marion/agents.toml`. A child that edits the copy must change nothing for
//! the nodes it spawns: they resolve through the table the root started with, which marion keeps
//! in each node's agent directory, outside every worktree.
//!
//! No harness runs here: every case is refused, or accepted, before any process exists.

use std::path::Path;

use marion_core::contract::{AgentId, Isolation, TaskId};
use marion_supervisor::run::{AGENT_TYPES_FILE, Caller, SpawnRequest, run_spawn};
use marion_supervisor::spawn::SpawnError;
use marion_supervisor::types_snapshot::TypesSnapshot;
use marion_testsupport::{fixture_repo, scratch};

mod common;

use common::canned::canned_env;

/// A row that names a tool its harness cannot grant, so a spawn that reaches it is refused at
/// compile: a witness that the row was resolved, with no process started.
const ESCAPE_ROW: &str = r#"
[[agent]]
name = "escape"
harness = "codex"
description = "a row only the child's worktree defines"
tools = ["read"]
"#;

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

fn write_row(tree: &Path, row: &str) {
    let file = tree.join(AGENT_TYPES_FILE);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, row).unwrap();
}

/// A child whose worktree defines a type its root's tree never had spawns nothing of that type:
/// the name is unknown to the table its tree started with.
#[test]
fn a_type_a_child_added_to_its_worktree_is_unknown_to_its_spawns() {
    let dir = scratch("descendant-rules-added");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    let env = canned_env(&state, &repo, None);
    let child = AgentId("child-1".into());
    let child_dir = env.project_dir.agent(&child);
    let child_wt = child_dir.worktree();
    std::fs::create_dir_all(&child_wt).unwrap();
    // The root's tree had no table; the child's worktree now has one.
    TypesSnapshot::take(&repo, Some("claude"))
        .unwrap()
        .write(child_dir.path())
        .unwrap();
    write_row(&child_wt, ESCAPE_ROW);

    let caller = Caller {
        depth: 1,
        ..Caller::root(
            "child-1",
            marion_core::agent_type::builtin("claude").unwrap(),
        )
    };
    let err = run_spawn(
        &env,
        &request(&child_wt, "escape"),
        &TaskId("t-escape".into()),
        &caller,
    )
    .expect_err("a type only the child's worktree defines must not launch");
    assert!(
        matches!(&err, SpawnError::UnknownAgentType(name) if name == "escape"),
        "{err}"
    );
}

/// A child with no record of its tree's table is refused rather than read from its worktree.
#[test]
fn a_child_with_no_recorded_table_is_not_read_from_its_worktree() {
    let dir = scratch("descendant-rules-missing");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    let env = canned_env(&state, &repo, None);
    let child = AgentId("child-2".into());
    let child_wt = env.project_dir.agent(&child).worktree();
    std::fs::create_dir_all(&child_wt).unwrap();
    write_row(&child_wt, ESCAPE_ROW);

    let caller = Caller {
        depth: 1,
        ..Caller::root(
            "child-2",
            marion_core::agent_type::builtin("claude").unwrap(),
        )
    };
    let err = run_spawn(
        &env,
        &request(&child_wt, "escape"),
        &TaskId("t-missing".into()),
        &caller,
    )
    .expect_err("no recorded table, and the worktree is not read");
    assert!(
        err.to_string()
            .contains("will not read them from the worktree"),
        "{err}"
    );
}
