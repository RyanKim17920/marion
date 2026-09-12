//! **A user-defined agent type runs a real child, end to end, at zero cost.**
//!
//! `harness_matrix.rs` proves every built-in row; this file proves the one row that is not
//! built in: a `[[agent]]` in the tree's `.marion/agents.toml`, resolved by `run_spawn` from the
//! tree the spawn is against. One cell, on codex, because codex is the harness whose headless
//! surface carries the prompt on argv — so the provider's request log is a direct witness of the
//! prompt the child was actually given, prefix included.
//!
//! Two cases, one file:
//! 1. the row resolves: the contract names the harness the row declared, the journal's
//!    `SpawnIntent` names the row's own name, and the provider saw the prompt;
//! 2. the file is gone: the same name is an unknown type, refused before anything is journaled.
//!
//! The first case drives a REAL `codex` and skips loudly by name on a runner that declared it has
//! no harness binaries ([`marion_testsupport::harness_available`]); the second needs no binary and
//! never skips.

use std::path::PathBuf;

use marion_core::contract::{ExitStatus, Isolation, TaskContract, TaskId};
use marion_core::harness::Harness;
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{AGENT_TYPES_FILE, Caller, Env, SpawnRequest, run_spawn};
use marion_supervisor::spawn::SpawnError;
use marion_testsupport::{
    carries, fixture_repo, git, harness_available, kill_hard, scratch, survivors,
};

const CHILD_TIMEOUT_SECS: u64 = 60;
const NARRATIVE: &str = "Reviewed the tree and reported back.";
const PROMPT: &str = "Review src/keep.txt and report back through marion.";
const PREFIX: &str = "You are a code reviewer. Do not modify files.\n\n";

const AGENTS_TOML: &str = r#"
[[agent]]
name = "reviewer"
harness = "codex"
model = "gpt-5.6-sol"
description = "Reviews a diff and reports findings; never edits."
prompt_prefix = "You are a code reviewer. Do not modify files.\n\n"
"#;

/// A fixture repo whose tree root carries a committed `.marion/agents.toml` defining `reviewer`.
///
/// The row states a `model` for the same reason the matrix's codex cell asks for one: `codex exec`
/// records none (the contract's `child.model` is `None` regardless), but codex 0.153.4 under its
/// *default* model took a code-mode first turn that raced the bridge's tool registration and
/// ended `unsupported call` three runs in four. An explicit model is the measured stable path,
/// and here it is also the witness that a row's `model` reaches the launch.
fn repo_with_reviewer(root: &std::path::Path) -> PathBuf {
    let repo = fixture_repo(root);
    let file = repo.join(AGENT_TYPES_FILE);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, AGENTS_TOML).unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=marion@example.invalid",
            "-c",
            "user.name=marion",
            "commit",
            "-qm",
            "agent types",
        ],
    );
    repo
}

fn env_for(state: &std::path::Path, repo: &std::path::Path, base_url: Option<String>) -> Env {
    Env {
        project_dir: ProjectDir::new(state, repo),
        project_root: repo.to_path_buf(),
        state: state.to_path_buf(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url,
        auth: marion_harness::Auth::Canned,
    }
}

fn request(repo: &std::path::Path) -> SpawnRequest {
    SpawnRequest {
        agent_type: "reviewer".into(),
        prompt: PROMPT.into(),
        repo: repo.to_path_buf(),
        acceptance_criteria: vec!["a report arrived".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        // `None`: the row's own `model` is the default, exactly as a built-in's would be.
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
    }
}

fn root_caller() -> Caller {
    Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    )
}

/// The one `SpawnIntent` the project's journal holds, as (agent_type, harness).
fn journaled_intents(project: &ProjectDir) -> Vec<(String, Harness)> {
    let bytes = std::fs::read(project.journal()).unwrap_or_default();
    marion_core::registry::replay(&bytes)
        .nodes()
        .iter()
        .filter_map(|n| n.intent.as_ref())
        .map(|i| (i.agent_type.clone(), i.harness))
        .collect()
}

#[test]
fn a_user_defined_reviewer_runs_on_codex_and_is_journaled_under_its_own_name() {
    if !harness_available("codex") {
        return;
    }
    let root = scratch("user-types-reviewer");
    let repo = repo_with_reviewer(&root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root.join("provider-requests.jsonl"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");
    let env = env_for(&state, &repo, Some(server.base_url()));

    let contract = run_spawn(
        &env,
        &request(&repo),
        &TaskId("reviewer".into()),
        &root_caller(),
    )
    .map_err(|e| e.to_string());
    let requests = server.requests().unwrap_or_default();
    let intents = journaled_intents(&env.project_dir);

    drop(server);
    let leaked = survivors(&root.to_string_lossy());
    for (pid, _) in &leaked {
        kill_hard(*pid);
    }
    assert!(leaked.is_empty(), "processes outlived the run: {leaked:?}");

    let contract: TaskContract = contract.unwrap_or_else(|e| panic!("run_spawn failed: {e}"));
    assert_eq!(
        contract.child.harness,
        Harness::Codex,
        "the contract names the harness the row declared"
    );
    let comp = contract
        .completion
        .as_ref()
        .expect("a finished run has a completion");
    assert_eq!(
        comp.status,
        ExitStatus::Ok,
        "exit: {}",
        comp.exit.description
    );
    assert_eq!(
        intents,
        vec![("reviewer".to_string(), Harness::Codex)],
        "the journal names the row's own name, so a resume re-resolves the same row"
    );
    let prefixed = format!("{PREFIX}{PROMPT}");
    assert!(
        requests.iter().any(|r| carries(r, &prefixed)),
        "the provider saw the prompt the child was given, prefix first; requests: {}",
        requests.len()
    );
    assert_eq!(
        contract.instructions.value, prefixed,
        "and the contract records that prompt, not the request's"
    );
}

#[test]
fn a_reviewer_whose_file_is_gone_is_an_unknown_type_and_nothing_is_journaled() {
    let root = scratch("user-types-gone");
    let repo = repo_with_reviewer(&root);
    std::fs::remove_file(repo.join(AGENT_TYPES_FILE)).unwrap();
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = env_for(&state, &repo, Some("http://127.0.0.1:9/v1".into()));

    let err = run_spawn(
        &env,
        &request(&repo),
        &TaskId("gone".into()),
        &root_caller(),
    )
    .expect_err("no file defines `reviewer`");
    assert!(
        matches!(&err, SpawnError::UnknownAgentType(t) if t == "reviewer"),
        "{err:?}"
    );
    assert!(journaled_intents(&env.project_dir).is_empty());
    assert!(
        !env.project_dir.journal().exists(),
        "nothing was journaled: the refusal precedes the intent"
    );
}
