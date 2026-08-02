//! `marion-supervisor` — the supervisor binary.
//!
//! `mcp` and `doctor` are **subcommands of this binary**, not separate ones (§10). The
//! user-facing `marion` command forwards `doctor` here, since the supervisor owns the registry
//! the check reads.

use std::io::{BufRead, Write};

mod bridge;
mod run;
mod spawn;

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
fn handle_tool_call(id: &serde_json::Value, name: &str, args: &serde_json::Value) -> serde_json::Value {
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
                agent_type: args["agent_type"].as_str().unwrap_or("codex-impl").to_string(),
                prompt: args["prompt"].as_str().unwrap_or_default().to_string(),
                acceptance_criteria: args["acceptance_criteria"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                writable_scope: args["writable_scope"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                timeout_secs: args["timeout_secs"].as_u64().unwrap_or(900),
            };
            let task_id = new_task_id();
            match run::run_spawn(&env, &req, &task_id, "root") {
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

fn spawn_env() -> Result<run::Env, ()> {
    let repo = std::env::var("MARION_REPO").map_err(|_| ())?;
    let state = std::env::var("MARION_STATE")
        .unwrap_or_else(|_| format!("{repo}/.marion"));
    Ok(run::Env {
        repo: repo.into(),
        state_dir: state.into(),
        bridge: std::env::current_exe().unwrap_or_else(|_| "marion-supervisor".into()),
        base_url: std::env::var("MARION_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8099/v1".into()),
    })
}

/// UUIDv7-shaped id: a 48-bit millisecond timestamp then random-ish tail, lowercase hyphenated,
/// so it stays filesystem-safe as a directory component (4.3).
fn new_task_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let n = std::process::id() as u64;
    format!(
        "{:08x}-{:04x}-7{:03x}-8{:03x}-{:012x}",
        (ms >> 16) as u32,
        (ms & 0xffff) as u16,
        (n & 0xfff) as u16,
        ((n >> 12) & 0xfff) as u16,
        ms & 0xffff_ffff_ffff
    )
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
        let Some(req) = bridge::parse(line) else { continue };
        let reply = match req {
            bridge::Request::Initialize { id } => Some(bridge::initialize_result(&id)),
            bridge::Request::ToolsList { id } => Some(bridge::tools_list_result(&id)),
            bridge::Request::ToolsCall { id, name, arguments } => {
                Some(handle_tool_call(&id, &name, &arguments))
            }
            bridge::Request::Notification => None,
            bridge::Request::Unknown { id, method } => Some(bridge::method_not_found(&id, &method)),
        };
        if let Some(r) = reply {
            let _ = writeln!(stdout, "{r}");
            let _ = stdout.flush();
        }
    }
}
