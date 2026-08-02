//! Executing one `spawn`: worktree → child → contract (design §6.1).

use std::path::{Path, PathBuf};
use std::process::Command as SysCommand;

use marion_core::contract::*;
use marion_core::encoding::{Duration, SystemTime};
use marion_harness::{ExecSpec, compile_exec, config_toml};

use crate::spawn::{
    SpawnError, build_contract, changed_paths, diff_text, make_worktree, parse_child_stream,
};

pub struct SpawnRequest {
    pub agent_type: String,
    pub prompt: String,
    pub acceptance_criteria: Vec<String>,
    pub writable_scope: Vec<String>,
    pub timeout_secs: u64,
}

pub struct Env {
    /// The repo marion is delegating within.
    pub repo: PathBuf,
    /// Where worktrees, contracts and child configuration live.
    pub state_dir: PathBuf,
    /// Path to this binary, for the child's MCP declaration.
    pub bridge: PathBuf,
    pub base_url: String,
}

/// Run one child to completion and return its contract.
///
/// Blocking, per §9: backgrounding is M2+. The bound is a hard kill, so `spawn` always returns.
pub fn run_spawn(
    env: &Env,
    req: &SpawnRequest,
    task_id: &str,
    requester: &str,
) -> Result<TaskContract, SpawnError> {
    let spawned_at = SystemTime(std::time::SystemTime::now());
    let wt = env.state_dir.join("worktrees").join(task_id);
    let ch = env.state_dir.join("codex-home").join(task_id);
    std::fs::create_dir_all(&ch)?;
    std::fs::create_dir_all(wt.parent().expect("has a parent"))?;

    let base = make_worktree(&env.repo, &wt, &format!("marion/{task_id}"))?;

    // The child's whole configuration, written by marion — control is config-time.
    std::fs::write(
        ch.join("config.toml"),
        config_toml(&env.bridge.to_string_lossy(), &["mcp"], &env.base_url),
    )?;

    let inv = compile_exec(&ExecSpec {
        cwd: wt.clone(),
        codex_home: ch.clone(),
        prompt: req.prompt.clone(),
        output_schema: None,
        output_last_message: None,
    });

    let out = SysCommand::new(&inv.program)
        .args(&inv.args)
        .envs(inv.env.iter().cloned())
        .env("MARION_DUMMY_KEY", "dummy")
        .current_dir(&inv.cwd)
        .stdin(std::process::Stdio::null())
        .output()?;

    let stream = String::from_utf8_lossy(&out.stdout);
    let mut outcome = parse_child_stream(&stream);
    outcome.exit_code = out.status.code();

    // Git is the authority for what changed; the child's file_change items are corroboration.
    let changed = changed_paths(&wt, &base).unwrap_or_default();
    let diff = diff_text(&wt, &base).ok().filter(|d| !d.is_empty());

    let requested: Vec<Glob> = if req.writable_scope.is_empty() {
        vec![Glob("**".into())]
    } else {
        req.writable_scope.iter().cloned().map(Glob).collect()
    };

    Ok(build_contract(
        TaskId(task_id.to_string()),
        AgentId(requester.to_string()),
        RepoIdentity {
            git_common_dir: env.repo.join(".git"),
            head_branch: None,
        },
        base,
        Workspace::Worktree { path: wt, branch: format!("marion/{task_id}") },
        &req.prompt,
        &req.acceptance_criteria,
        &[Glob("**".into())],
        &requested,
        Duration::from_secs(req.timeout_secs),
        spawned_at,
        &outcome,
        changed,
        diff,
        vec![],
    ))
}

/// Remove a task's worktree. Best-effort: a failure here must not mask the contract.
pub fn cleanup(repo: &Path, wt: &Path) {
    let _ = SysCommand::new("git")
        .current_dir(repo)
        .args(["worktree", "remove", "--force", &wt.to_string_lossy()])
        .output();
}
