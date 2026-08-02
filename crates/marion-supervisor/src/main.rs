//! `marion-supervisor` — the supervisor binary.
//!
//! `mcp` and `doctor` are **subcommands of this binary**, not separate ones (§10). The
//! user-facing `marion` command forwards `doctor` here, since the supervisor owns the registry
//! the check reads.

use std::io::{BufRead, Read, Write};

use marion_core::ids::{RAND_BYTES, new_task_id as core_new_task_id};
use marion_core::paths::{ProjectDir, state_dir};
use marion_supervisor::root::{AGENT_ID_ENV, READY_FILE_ENV};
use marion_supervisor::{bridge, run};

fn usage() -> ! {
    eprintln!("usage: marion-supervisor <mcp|doctor>");
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("mcp") => run_bridge(),
        Some("doctor") => {
            println!("marion doctor: no adapters registered yet (M1 in progress)");
        }
        _ => usage(),
    }
}

/// `spawn` blocks for the child's whole run and returns the completed contract; `report` stages
/// the child's payload, which the parent's own `spawn` then returns.
fn handle_tool_call(
    id: &serde_json::Value,
    name: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
    match name {
        "report" => {
            // Staged, not delivered: the contract is written at the node's terminal transition.
            bridge::tool_result(id, "report recorded", false)
        }
        "spawn" => {
            let Ok(env) = spawn_env() else {
                return bridge::tool_result(id, "marion: MARION_REPO is not set", true);
            };
            let req = run::SpawnRequest {
                agent_type: args["agent_type"]
                    .as_str()
                    .unwrap_or("codex-impl")
                    .to_string(),
                prompt: args["prompt"].as_str().unwrap_or_default().to_string(),
                acceptance_criteria: args["acceptance_criteria"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                writable_scope: args["writable_scope"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                timeout_secs: args["timeout_secs"].as_u64().unwrap_or(900),
            };
            let Ok(task_id) = new_task_id() else {
                return bridge::tool_result(id, "marion: could not generate task id", true);
            };
            match run::run_spawn(&env, &req, &task_id, &requester()) {
                Ok(contract) => {
                    let json = serde_json::to_string_pretty(&contract)
                        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
                    bridge::tool_result(id, &json, false)
                }
                Err(e) => bridge::tool_result(id, &format!("marion: spawn failed: {e}"), true),
            }
        }
        other => bridge::tool_result(id, &format!("marion: no tool {other}"), true),
    }
}

/// The node this bridge instance is serving, which becomes `TaskContract.requester`.
///
/// §9: *"`requester` for a top-level `spawn` is the root's `AgentId`."* marion is not this
/// process's parent — the harness is — so the id rides the server declaration marion wrote
/// (`root::mcp_config_json`) rather than an inherited fd. The literal fallback is what a
/// hand-started bridge gets: honest about being unattributed rather than inventing a uuid that
/// names no agent-dir.
fn requester() -> String {
    std::env::var(AGENT_ID_ENV).unwrap_or_else(|_| "unattributed-root".into())
}

/// Tell marion the harness now has our tool list.
///
/// The harness connects `--mcp-config` servers **asynchronously and non-blockingly** (measured on
/// 2.1.220), so without this the root's first turn can go out before `mcp__marion__spawn` exists
/// and end in plain text with no error anywhere. See `root`'s module docs.
fn signal_ready() {
    if let Ok(path) = std::env::var(READY_FILE_ENV) {
        let _ = std::fs::write(path, b"ready\n");
    }
}

fn spawn_env() -> Result<run::Env, ()> {
    let repo = std::path::PathBuf::from(std::env::var("MARION_REPO").map_err(|_| ())?);
    let repo = repo.canonicalize().map_err(|_| ())?;
    let legacy = std::env::var("MARION_STATE").ok();
    let documented = std::env::var("MARION_STATE_DIR").ok();
    let explicit = documented.as_deref().or(legacy.as_deref());
    let state = state_dir(
        explicit,
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
    .ok_or(())?;
    Ok(run::Env {
        project_dir: ProjectDir::new(&state, &repo),
        repo,
        bridge: std::env::current_exe().unwrap_or_else(|_| "marion-supervisor".into()),
        base_url: std::env::var("MARION_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8099/v1".into()),
    })
}

fn new_task_id() -> std::io::Result<marion_core::contract::TaskId> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut entropy = [0; RAND_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
    Ok(core_new_task_id(ms, entropy))
}

/// Serve MCP over stdio until the harness closes our stdin.
fn run_bridge() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(req) = bridge::parse(line) else {
            continue;
        };
        let mut answered_tools_list = false;
        let reply = match req {
            bridge::Request::Initialize { id } => Some(bridge::initialize_result(&id)),
            bridge::Request::ToolsList { id } => {
                answered_tools_list = true;
                Some(bridge::tools_list_result(&id))
            }
            bridge::Request::ToolsCall {
                id,
                name,
                arguments,
            } => Some(handle_tool_call(&id, &name, &arguments)),
            bridge::Request::Notification => None,
            bridge::Request::Unknown { id, method } => Some(bridge::method_not_found(&id, &method)),
        };
        if let Some(r) = reply {
            let _ = writeln!(stdout, "{r}");
            let _ = stdout.flush();
        }
        // After the flush, never before: the marker means "the harness has been sent the list".
        if answered_tools_list {
            signal_ready();
        }
    }
}

#[cfg(test)]
mod main_tests {
    use super::*;

    #[test]
    fn task_ids_minted_back_to_back_use_entropy_and_do_not_collide() {
        assert_ne!(new_task_id().unwrap(), new_task_id().unwrap());
    }

    #[test]
    fn documented_state_precedence_is_resolved_beneath_the_project_hash() {
        let root = std::path::Path::new("/canonical/project");
        let state = state_dir(Some("/explicit"), Some("/xdg"), Some("/home")).unwrap();
        let project = ProjectDir::new(&state, root);
        assert_eq!(
            project.path().parent(),
            Some(std::path::Path::new("/explicit"))
        );
        assert_eq!(
            project.path().file_name().unwrap().to_string_lossy().len(),
            12
        );
    }
}
