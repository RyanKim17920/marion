//! **One real agy child, end to end, on the operator's own login.**
//!
//! agy has no canned provider route, so nothing about its row can be proven hermetically past the
//! committed s32 fixtures: the `--add-dir` root, the operator's `permissions.allow` rule, the
//! working-directory preamble and the `--conversation`-free fresh launch only meet a real model
//! here. A root asks for one `agy-impl` child through the real `run_spawn`; the child must write a
//! file in its own worktree and call marion's `report`, and the parent's verification line reads
//! the file back — so an `Ok` here means the write landed where the node works, the report was
//! approved rather than auto-denied, and marion's own check agreed.
//!
//! # Running it
//!
//! ```sh
//! MARION_LIVE_AGY=1 cargo test -p marion-supervisor --test agy_live
//! ```
//!
//! **Spends the operator's own agy quota** (one short turn on the row's cheapest model), which is
//! why it is gated on an explicit variable and skips loudly without it. It never starts a sign-in
//! and never relocates `HOME`: agy's login lives in the keychain under the real profile. It needs
//! the operator's grant, `"permissions": {"allow": ["mcp(marion/*)"]}` in
//! `~/.gemini/antigravity-cli/settings.json`, which `marion doctor` reports; without it the
//! child's `report` is auto-denied and the run fails by name.

use std::path::{Path, PathBuf};

use marion_core::contract::{ExitStatus, Isolation, TaskContract, TaskId, Workspace};
use marion_core::harness::Harness;
use marion_core::paths::ProjectDir;
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, harness_available, judge, persisted_contracts, scratch};
use serde_json::Value;

/// The one variable that lets this file spend anything.
const GATE: &str = "MARION_LIVE_AGY";
/// A bound that exists only to fail: one short agy turn has taken well under a minute.
const CHILD_TIMEOUT_SECS: u64 = 240;
const FILE: &str = "agy-live.txt";
const CONTENT: &str = "written by a live agy child";

fn gated() -> bool {
    if std::env::var(GATE).as_deref() == Ok("1") {
        return true;
    }
    eprintln!(
        "SKIPPED agy_live: set {GATE}=1 to run one real agy child on the operator's own login \
         (spends agy quota)"
    );
    false
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

#[test]
fn a_live_agy_child_writes_a_file_in_its_worktree_reports_and_passes_verification() {
    if !gated() || !harness_available("agy") {
        return;
    }
    let root = scratch("agy-live");
    let repo = fixture_repo(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = Env {
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state: state.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: None,
        auth: marion_harness::Auth::Inherited,
    };
    let req = SpawnRequest {
        agent_type: "agy-impl".into(),
        prompt: format!(
            "Create a file named {FILE} in your working directory whose entire content is the \
             single line: {CONTENT}\nThen call marion's report tool with a one-sentence summary."
        ),
        repo: repo.clone(),
        acceptance_criteria: vec![format!("{FILE} holds the line `{CONTENT}`")],
        verification: vec![format!("grep -qx '{CONTENT}' {FILE} && cat {FILE}")],
        writable_scope: vec![FILE.into()],
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
    let task = "agy-live";
    let returned = run_spawn(&env, &req, &TaskId(task.into()), &caller)
        .unwrap_or_else(|e| panic!("the agy child runs: {e}"));
    assert_eq!(returned.child.harness, Harness::Antigravity);
    let wt = match &returned.workspace {
        Workspace::Worktree { path, .. } => path.clone(),
        other => panic!("an agy-impl child runs in a worktree, got {other:?}"),
    };

    let c = persisted_contract(&state, task);
    let comp = c.completion.as_ref().expect("a finished run completes");
    assert_eq!(
        comp.status,
        ExitStatus::Ok,
        "the child wrote, reported and passed verification: {}\nnarrative: {:?}",
        comp.exit.description,
        comp.narrative.as_ref().map(|n| &n.value)
    );
    assert!(
        !comp.narrative_synthesized,
        "the narrative is the child's own `report`, not one marion made up for an unreported run"
    );
    assert_eq!(c.child.version, "1.2.8", "the pinned release ran");
    assert!(
        c.allowed_tools.iter().any(|t| t == "mode:accept-edits"),
        "a write declaration compiles --mode accept-edits: {:?}",
        c.allowed_tools
    );

    assert_eq!(c.verification.len(), 1);
    assert_eq!(
        c.verification[0].cwd, wt,
        "verification ran in the child's worktree"
    );
    assert_eq!(comp.evidence.len(), 1);
    assert_eq!(
        comp.evidence[0].exit_code,
        Some(0),
        "{:?}",
        comp.evidence[0]
    );
    assert!(
        comp.evidence[0].stdout.value.contains(CONTENT),
        "the file the child wrote, read back by marion: {:?}",
        comp.evidence[0].stdout.value
    );
    assert_eq!(
        comp.changed_paths,
        vec![PathBuf::from(FILE)],
        "the write landed in the worktree, not in marion's root"
    );
    assert!(
        comp.scope_violations.is_empty(),
        "{:?}",
        comp.scope_violations
    );

    // Where the work landed: marion committed it onto the child's branch before the reap.
    let branch = comp
        .branch
        .as_deref()
        .expect("the child's work landed on a branch");
    let shown = std::process::Command::new("git")
        .args(["show", &format!("{branch}:{FILE}")])
        .current_dir(&repo)
        .output()
        .expect("git runs");
    assert!(
        String::from_utf8_lossy(&shown.stdout).contains(CONTENT),
        "the branch holds the child's file: {}",
        String::from_utf8_lossy(&shown.stderr)
    );
    eprintln!(
        "agy live child: {} ({}), branch {branch}, narrative {:?}",
        comp.exit.description,
        c.child.model.as_deref().unwrap_or("default model"),
        comp.narrative.as_ref().map(|n| &n.value)
    );
}
