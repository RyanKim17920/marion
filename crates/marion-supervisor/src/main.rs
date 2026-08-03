//! `marion-supervisor` — the supervisor binary.
//!
//! `mcp` and `doctor` are **subcommands of this binary**, not separate ones (§10). The
//! user-facing `marion` command forwards `doctor` here, since the supervisor owns the registry
//! the check reads.

use std::io::{BufRead, Read, Write};

use marion_core::ids::{RAND_BYTES, new_task_id as core_new_task_id};
use marion_core::paths::{ProjectDir, state_dir};
use marion_supervisor::root::{AGENT_ID_ENV, AGENT_TYPE_ENV, DEPTH_ENV, READY_FILE_ENV};
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
            // §6.1 step 2's gates read the caller's agent type and depth, and this bridge is the
            // only place that knows which node it is serving. A bridge that was not told cannot
            // evaluate them, so it refuses rather than spawning ungated — which is exactly the
            // hazard this path exists to close, and the same shape as `spawn_env`'s refusal above.
            let caller = match caller(&requester()) {
                Ok(c) => c,
                Err(e) => return bridge::tool_result(id, &e, true),
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
                // Absent is not empty: `None` falls back to the agent type's own `model` key
                // (§3.1), which is what makes a `gemini` or `opencode` spawn launchable without
                // the parent having to know which harness needs a model and in what spelling.
                model: args["model"].as_str().map(str::to_string),
            };
            let Ok(task_id) = new_task_id() else {
                return bridge::tool_result(id, "marion: could not generate task id", true);
            };
            match run::run_spawn(&env, &req, &task_id, &caller) {
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

/// The caller of this `spawn`, rebuilt from the declaration marion wrote (§6.1 step 2).
///
/// The whole of the gate's input arrives through the per-server MCP `env` block, for the reason
/// [`requester`] gives about the id: marion is not this process's parent. `agent_type` names the
/// type whose `max_depth` / `max_concurrent_children` the gate reads, and `depth` says where in the
/// tree this node sits, with the root at 0.
///
/// **Absent or unresolvable is a refusal, not a default**, and the two candidate defaults are both
/// wrong in the same direction: assuming depth 0 makes every node look like a root, and assuming
/// `DEFAULT_MAX_DEPTH` makes the per-type key a constant nothing reads. Either would restore
/// exactly the unbounded recursion this is here to stop, silently. All four adapters emit both
/// keys — `marion_harness::adapter`'s own test sweeps `Harness::ALL` — so the only caller that can
/// land here is a hand-started bridge, which is the one that most needs to be told.
fn caller(agent_id: &str) -> Result<run::Caller, String> {
    caller_from(
        agent_id,
        std::env::var(AGENT_TYPE_ENV).ok(),
        std::env::var(DEPTH_ENV).ok(),
    )
}

/// The pure half, so the resolution is testable without an environment.
fn caller_from(
    agent_id: &str,
    agent_type: Option<String>,
    depth: Option<String>,
) -> Result<run::Caller, String> {
    let name = agent_type.ok_or_else(|| {
        format!(
            "marion: {AGENT_TYPE_ENV} is not set, so this bridge does not know which agent type is \
             calling and cannot read its max_depth (§6.1 step 2). Refusing rather than spawning \
             ungated."
        )
    })?;
    let agent_type = marion_core::agent_type::builtin(&name).ok_or_else(|| {
        format!(
            "marion: {AGENT_TYPE_ENV}={name:?} names no known agent type, so its spawn gates \
                 cannot be read. Refusing rather than spawning ungated."
        )
    })?;
    let raw = depth.ok_or_else(|| {
        format!(
            "marion: {DEPTH_ENV} is not set, so this bridge does not know how deep in the tree it \
             is and cannot enforce max_depth (§6.1 step 2). Refusing rather than spawning ungated."
        )
    })?;
    let depth: u32 = raw.trim().parse().map_err(|_| {
        format!(
            "marion: {DEPTH_ENV}={raw:?} is not a depth. Refusing rather than spawning ungated."
        )
    })?;
    Ok(run::Caller {
        agent_id: agent_id.to_string(),
        agent_type,
        depth,
    })
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
        base_url: Some(
            std::env::var("MARION_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8099/v1".into()),
        ),
        // The bridge serves a node marion did not start, and the only channel it has is the
        // per-server `env` block that node's config carries. Nothing in that block says "live" yet,
        // so a child spawned through the bridge stays canned — carrying live down a hop is part 2's
        // change, and inferring it here from an absent variable would be a guess.
        auth: marion_harness::Auth::Canned,
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

    /// The gate's inputs come off the declaration marion wrote, and both are load-bearing: without
    /// the type there is no `max_depth` to read, and without the depth there is nothing to compare
    /// it against.
    #[test]
    fn the_callers_type_and_depth_are_read_off_the_declaration() {
        let c = caller_from("019f-node", Some("codex".into()), Some("2".into()))
            .expect("a declaration carrying both resolves");
        assert_eq!(c.agent_id, "019f-node");
        assert_eq!(
            c.agent_type.name, "codex-impl",
            "the alias resolves to the one definition, not a second one"
        );
        assert_eq!(c.depth, 2);
        assert_eq!(
            c.agent_type.max_depth, 3,
            "which is the bound the gate reads"
        );
    }

    /// **A bridge that was not told is a refusal, not a default.** Both plausible defaults restore
    /// the unbounded recursion this exists to stop: depth 0 makes every node look like a root, and
    /// a constant `max_depth` makes the per-type key a number nothing reads. The message has to say
    /// which piece is missing, because the operator's fix differs.
    #[test]
    fn a_bridge_that_was_not_told_which_node_it_serves_refuses_rather_than_spawning_ungated() {
        for (agent_type, depth, expected) in [
            (None, Some("0".to_string()), "MARION_AGENT_TYPE is not set"),
            (Some("claude".to_string()), None, "MARION_DEPTH is not set"),
            (
                Some("not-a-type".to_string()),
                Some("0".to_string()),
                "names no known agent type",
            ),
            (
                Some("claude".to_string()),
                Some("deep".to_string()),
                "is not a depth",
            ),
        ] {
            let e = caller_from("019f-node", agent_type.clone(), depth.clone())
                .expect_err("an unevaluable gate must refuse");
            assert!(e.contains(expected), "expected {expected:?} in: {e}");
            assert!(
                e.contains("ungated"),
                "the refusal must say what it is protecting against: {e}"
            );
        }
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
