//! Launching the **root node** (design §9).
//!
//! > *"marion launches the root node itself. The `claude` root is not hand-started: `marion run
//! > <agent-type> --prompt <…>` spawns it through the same §6.1 path as any child, which is what
//! > gives it an `AgentId`, an agent-dir, and a capability token — without which its `spawn` call
//! > cannot be stamped and `TaskContract.requester` has no value. **A root has no `TaskContract`**
//! > […]; `requester` for a top-level `spawn` is the root's `AgentId`."*
//!
//! So this module mints the id, writes the `--mcp-config` declaration that carries that id to the
//! bridge, compiles the root argv through [`marion_harness::compile_headless`], and drives the
//! `stream-json` conversation.
//!
//! # Why the prompt is not written the moment the process starts
//!
//! Measured on 2.1.220: MCP servers declared through `--mcp-config` are connected
//! **asynchronously and non-blockingly** — the debug log says so in as many words — and the first
//! turn is *not* held for them. Against a real endpoint that is invisible, because a model reply
//! takes seconds and the ~70 ms connect has long finished. Against the CannedProvider the reply
//! comes back in microseconds, so the root's first request goes out with `tools: []`, marion's
//! `spawn` is not among the tools, and the turn ends with plain text. **Nothing anywhere reports
//! an error** — the same class of silent failure the provider's shape-dispatch exists to prevent.
//!
//! marion owns both ends of that race, so it closes it rather than sleeping through it. The bridge
//! touches [`READY_FILE_ENV`] once it has answered `tools/list`, which is the actual event that
//! matters ("the harness has marion's tool list"), and the root's prompt is written only after
//! that file appears. A `control_request`/`control_response` round trip then follows, so the
//! prompt is written only after the harness's event loop has demonstrably run at least once since
//! the tool list was flushed to it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as SysCommand, Stdio};
use std::time::{Duration as StdDuration, Instant};

use marion_core::contract::AgentId;
use marion_core::ids::{new_agent_id, uuid_v7};
use marion_core::paths::{AgentDir, ProjectDir};
use marion_harness::{HeadlessSpec, Invocation, compile_headless};
use serde_json::{Value, json};

use crate::run::{entropy, unix_millis};

/// Env var naming the file the bridge touches once it has answered `tools/list`.
pub const READY_FILE_ENV: &str = "MARION_READY_FILE";
/// Env var carrying the root's `AgentId` to the bridge, so a top-level `spawn` can stamp
/// `TaskContract.requester` with it (§9).
pub const AGENT_ID_ENV: &str = "MARION_AGENT_ID";

/// The permission axis for an M1 root (§9).
///
/// `spawn` is the load-bearing one; without it the root's single call is denied. The other three
/// are the only descendant verbs an M1 root can reach — `spawn` blocks and backgrounding is M2+,
/// so its child is terminal by the time the root regains control and §5.4 denies `send`/`cancel`
/// against terminal targets. `report` is rejected on a root. Omitting a *reachable* verb would
/// deny calls that then block until the root's bound expires, which is why the list is stated in
/// full rather than trimmed to what the canned script happens to use.
pub const ROOT_ALLOWED_TOOLS: [&str; 4] = [
    "mcp__marion__spawn",
    "mcp__marion__status",
    "mcp__marion__wait",
    "mcp__marion__list",
];

/// What `marion run` was asked for.
#[derive(Debug, Clone)]
pub struct RootSpec {
    /// Agent type of the root itself, e.g. `claude`.
    pub agent_type: String,
    /// Canonical repository root. The root's cwd, and the repo its children are worktrees of.
    pub repo: PathBuf,
    /// Resolved state directory (`<state>` of §4.3), *not* the per-project subdirectory.
    pub state: PathBuf,
    /// The CannedProvider's base URL, in the `…/v1` form a Codex `model_providers` entry takes.
    pub base_url: String,
    /// Path to the `marion-supervisor` binary the harness will start as the MCP server.
    pub bridge: PathBuf,
    pub model: Option<String>,
}

/// A prepared, not-yet-started root node.
#[derive(Debug, Clone)]
pub struct RootNode {
    pub agent_id: AgentId,
    pub agent_dir: AgentDir,
    pub mcp_config: PathBuf,
    pub ready_file: PathBuf,
    /// The per-run bearer token (§9). Never a real credential: the endpoint is the canned server.
    pub token: String,
    pub invocation: Invocation,
}

/// What the root's run produced.
#[derive(Debug, Default)]
pub struct RootOutcome {
    pub exit_code: Option<i32>,
    /// Every `stream-json` frame the root emitted, parsed.
    pub transcript: Vec<Value>,
    pub stderr: String,
    /// `tool_name` of each permission request marion denied on expiry of the root's bound.
    pub denied_permissions: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "the harness never fetched marion's tool list within {0:?}: no {1} appeared. \
         The root would have taken its first turn without mcp__marion__spawn, so the run is \
         refused rather than allowed to end in plain text with no error anywhere."
    )]
    McpNeverReady(StdDuration, PathBuf),
    #[error("the root exited before answering marion's initialize control request")]
    DiedBeforeInitialize,
}

/// `ANTHROPIC_BASE_URL` from the provider base URL marion carries.
///
/// The two harnesses disagree about the `/v1`: a Codex `model_providers` entry names the full
/// `…/v1`, while Claude Code appends `/v1/messages` to whatever it is given and would otherwise
/// request `/v1/v1/messages`. marion stores the Codex form — it is the one that appears verbatim
/// in a config file — and derives the other.
pub fn anthropic_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// The `--mcp-config` document declaring marion's control MCP.
///
/// The bridge is a *short-lived process the harness starts*, not one marion spawns (§5.4), so
/// everything it needs rides this declaration: which repo, which state dir, which provider, and
/// **which node it is serving**. `MARION_AGENT_ID` is what makes `TaskContract.requester` the
/// root's own `AgentId` rather than a placeholder.
pub fn mcp_config_json(node_env: &McpEnv) -> Value {
    json!({
        "mcpServers": {
            "marion": {
                "type": "stdio",
                "command": node_env.bridge.to_string_lossy(),
                "args": ["mcp"],
                "env": {
                    "MARION_REPO": node_env.repo.to_string_lossy(),
                    "MARION_STATE_DIR": node_env.state.to_string_lossy(),
                    "MARION_BASE_URL": node_env.base_url,
                    AGENT_ID_ENV: node_env.agent_id.0,
                    READY_FILE_ENV: node_env.ready_file.to_string_lossy(),
                }
            }
        }
    })
}

/// The values [`mcp_config_json`] writes into the declaration.
#[derive(Debug, Clone)]
pub struct McpEnv {
    pub bridge: PathBuf,
    pub repo: PathBuf,
    pub state: PathBuf,
    pub base_url: String,
    pub agent_id: AgentId,
    pub ready_file: PathBuf,
}

/// One `stream-json` user turn, in the shape `tests/fixtures/s1/stdin.jsonl` records.
pub fn user_message(prompt: &str) -> String {
    json!({
        "type": "user",
        "session_id": "",
        "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// The `initialize` control request. §5.2: optional, `request_id` client-generated.
pub fn initialize_request(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {"subtype": "initialize", "hooks": {}},
    })
    .to_string()
}

/// Is this frame the `control_response` to `request_id`?
pub fn is_control_response_to(frame: &Value, request_id: &str) -> bool {
    frame.get("type").and_then(Value::as_str) == Some("control_response")
        && frame
            .pointer("/response/request_id")
            .and_then(Value::as_str)
            == Some(request_id)
}

/// The `tool_name` of an inbound `can_use_tool` request, if that is what this frame is.
pub fn can_use_tool_request(frame: &Value) -> Option<(String, String)> {
    if frame.get("type").and_then(Value::as_str) != Some("control_request") {
        return None;
    }
    if frame.pointer("/request/subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return None;
    }
    let request_id = frame.get("request_id").and_then(Value::as_str)?.to_string();
    let tool = frame
        .pointer("/request/tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((request_id, tool))
}

/// marion's answer to a permission it has nobody to ask (§9).
///
/// M1 has no TUI, so a genuine permission request blocks until the root's per-episode `Blocked`
/// bound expires and is then **denied** — the root is not killed, because "expired" and
/// "terminated" are different events for a root.
///
/// **The response half of this channel is designed, not measured** (§5.2's `UNVERIFIED` box, and
/// §9's third owed M1 debt: "a real `can_use_tool` round-trip with a committed fixture"). It
/// cannot fire on M1's acceptance path — every verb the canned root reaches is allowlisted — so
/// nothing M1 claims rests on this frame being right. It is here so the bound has something to
/// bound rather than being a flag that does nothing.
pub fn deny_response(request_id: &str, reason: &str) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {"behavior": "deny", "message": reason},
        },
    })
    .to_string()
}

/// Mint the root's identity, write its configuration, and compile its argv.
pub fn prepare(spec: &RootSpec) -> Result<RootNode, RootError> {
    let agent_id = new_agent_id(unix_millis(), entropy()?);
    let project = ProjectDir::new(&spec.state, &spec.repo);
    let agent_dir = project.agent(&agent_id);
    std::fs::create_dir_all(agent_dir.config_dir())?;

    let mcp_config = agent_dir.config_dir().join("mcp.json");
    // Not one of §4.3's normative files: this is marion's own start-up handshake with a process it
    // did not spawn, so it lives beside the node's state rather than in the layout.
    let ready_file = agent_dir.path().join("mcp-ready");
    let _ = std::fs::remove_file(&ready_file);

    std::fs::write(
        &mcp_config,
        serde_json::to_string_pretty(&mcp_config_json(&McpEnv {
            bridge: spec.bridge.clone(),
            repo: spec.repo.clone(),
            state: spec.state.clone(),
            base_url: spec.base_url.clone(),
            agent_id: agent_id.clone(),
            ready_file: ready_file.clone(),
        }))
        .expect("a Value always serialises"),
    )?;

    let token = per_run_token()?;
    let mut invocation = compile_headless(&HeadlessSpec {
        cwd: spec.repo.clone(),
        model: spec.model.clone(),
        allowed_tools: ROOT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect(),
        mcp_config: mcp_config.clone(),
        base_url: Some(anthropic_base_url(&spec.base_url)),
    });
    // §9: `ANTHROPIC_AUTH_TOKEN=<per-run token>` and `ANTHROPIC_API_KEY=""` — a non-empty key
    // silently wins (§6.4), so it is set to empty rather than left inherited.
    invocation
        .env
        .push(("ANTHROPIC_AUTH_TOKEN".into(), token.clone()));
    invocation
        .env
        .push(("ANTHROPIC_API_KEY".into(), String::new()));

    Ok(RootNode {
        agent_id,
        agent_dir,
        mcp_config,
        ready_file,
        token,
        invocation,
    })
}

/// A token scoped to this run and nothing else. Not a credential — the endpoint is canned — but
/// distinct per run so a request log attributes traffic to one run.
fn per_run_token() -> Result<String, RootError> {
    Ok(format!("marion-run-{}", uuid_v7(unix_millis(), entropy()?)))
}

/// Wait for the bridge's readiness marker.
fn wait_for_ready(path: &Path, timeout: StdDuration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(StdDuration::from_millis(10));
    }
    path.exists()
}

/// Start the root and drive its `stream-json` conversation to the terminal `result` frame.
///
/// `blocked_bound` is the root's node-level timeout (§9): a **per-episode `Blocked`-only** budget,
/// consumed only while marion is holding an answer the root is waiting on. It is deliberately not
/// a wall-clock ceiling — marion offers a root none — so this function has no deadline of its own.
pub fn launch(
    node: &RootNode,
    prompt: &str,
    blocked_bound: StdDuration,
    mcp_ready_timeout: StdDuration,
) -> Result<RootOutcome, RootError> {
    let inv = &node.invocation;
    let mut child = SysCommand::new(&inv.program)
        .args(&inv.args)
        .envs(inv.env.iter().cloned())
        .current_dir(&inv.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });
    let mut lines = BufReader::new(stdout).lines();

    let mut outcome = RootOutcome::default();

    if !wait_for_ready(&node.ready_file, mcp_ready_timeout) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stderr_thread.join();
        return Err(RootError::McpNeverReady(
            mcp_ready_timeout,
            node.ready_file.clone(),
        ));
    }

    // One round trip through the harness's event loop, after the tool list was flushed to it.
    let init_id = format!("marion-init-{}", node.agent_id.0);
    writeln!(stdin, "{}", initialize_request(&init_id))?;
    stdin.flush()?;
    let mut initialized = false;
    for line in lines.by_ref() {
        let line = line?;
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let done = is_control_response_to(&frame, &init_id);
        outcome.transcript.push(frame);
        if done {
            initialized = true;
            break;
        }
    }
    if !initialized {
        let _ = child.wait();
        outcome.stderr = stderr_thread.join().unwrap_or_default();
        return Err(RootError::DiedBeforeInitialize);
    }

    writeln!(stdin, "{}", user_message(prompt))?;
    stdin.flush()?;

    for line in lines.by_ref() {
        let line = line?;
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some((request_id, tool)) = can_use_tool_request(&frame) {
            // Nobody to ask. Consume the episode's budget, then deny and let the root proceed.
            std::thread::sleep(blocked_bound);
            writeln!(
                stdin,
                "{}",
                deny_response(
                    &request_id,
                    "marion: no permission answerer in M1; the root's Blocked bound expired",
                )
            )?;
            stdin.flush()?;
            outcome.denied_permissions.push(tool);
        }
        let terminal = frame.get("type").and_then(Value::as_str) == Some("result");
        outcome.transcript.push(frame);
        if terminal {
            break;
        }
    }

    // Closing stdin is what ends a `--input-format stream-json` session.
    drop(stdin);
    let status = child.wait()?;
    outcome.exit_code = status.code();
    outcome.stderr = stderr_thread.join().unwrap_or_default();
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> McpEnv {
        McpEnv {
            bridge: "/bin/marion-supervisor".into(),
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: "http://127.0.0.1:8099/v1".into(),
            agent_id: AgentId("019f-root".into()),
            ready_file: "/state/x/mcp-ready".into(),
        }
    }

    #[test]
    fn claude_gets_the_base_url_without_the_v1_it_appends_itself() {
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099/v1"),
            "http://127.0.0.1:8099",
            "leaving it on yields a request to /v1/v1/messages"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099/v1/"),
            "http://127.0.0.1:8099"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099"),
            "http://127.0.0.1:8099"
        );
    }

    #[test]
    fn the_declaration_carries_the_root_agent_id_so_requester_is_not_a_placeholder() {
        let v = mcp_config_json(&env());
        assert_eq!(
            v["mcpServers"]["marion"]["env"][AGENT_ID_ENV], "019f-root",
            "§9: requester for a top-level spawn is the root's own AgentId"
        );
        assert_eq!(v["mcpServers"]["marion"]["args"][0], "mcp");
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_BASE_URL"], "http://127.0.0.1:8099/v1",
            "the child's model_providers entry wants the /v1 form"
        );
    }

    #[test]
    fn the_declaration_names_the_readiness_marker_the_prompt_waits_on() {
        let v = mcp_config_json(&env());
        assert_eq!(
            v["mcpServers"]["marion"]["env"][READY_FILE_ENV],
            "/state/x/mcp-ready"
        );
    }

    #[test]
    fn the_root_allowlist_is_every_verb_an_m1_root_can_reach() {
        // Omitting a reachable verb denies a call that then blocks until the root's bound expires.
        assert_eq!(
            ROOT_ALLOWED_TOOLS.to_vec(),
            vec![
                "mcp__marion__spawn",
                "mcp__marion__status",
                "mcp__marion__wait",
                "mcp__marion__list"
            ]
        );
        assert!(
            !ROOT_ALLOWED_TOOLS.contains(&"mcp__marion__report"),
            "report is rejected on a node without a contract, and a root has none"
        );
    }

    #[test]
    fn a_user_turn_matches_the_shape_s1_recorded() {
        let v: Value = serde_json::from_str(&user_message("delegate")).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"][0]["text"], "delegate");
        assert!(v["parent_tool_use_id"].is_null());
    }

    #[test]
    fn the_initialize_round_trip_is_matched_on_the_request_id_not_the_subtype() {
        let frame: Value = serde_json::from_str(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"a","response":{}}}"#,
        )
        .unwrap();
        assert!(is_control_response_to(&frame, "a"));
        assert!(
            !is_control_response_to(&frame, "b"),
            "another node's reply must not satisfy our wait"
        );
        let init: Value = serde_json::from_str(&initialize_request("a")).unwrap();
        assert_eq!(init["request_id"], "a");
        assert_eq!(init["request"]["subtype"], "initialize");
    }

    #[test]
    fn a_permission_ask_is_recognised_by_its_subtype_and_carries_its_request_id() {
        // §5.2's observed ask frame. The top-level request_id is load-bearing: a frame without one
        // could be neither answered nor cancelled.
        let frame: Value = serde_json::from_str(
            r#"{"type":"control_request","request_id":"req_7","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"toolu_1"}}"#,
        )
        .unwrap();
        assert_eq!(
            can_use_tool_request(&frame),
            Some(("req_7".to_string(), "Bash".to_string()))
        );
        let other: Value = serde_json::from_str(
            r#"{"type":"control_request","request_id":"r","request":{"subtype":"interrupt"}}"#,
        )
        .unwrap();
        assert!(can_use_tool_request(&other).is_none());
    }

    #[test]
    fn an_unanswerable_permission_is_denied_and_the_root_is_not_killed() {
        let v: Value = serde_json::from_str(&deny_response("req_7", "bound expired")).unwrap();
        assert_eq!(v["response"]["request_id"], "req_7");
        assert_eq!(
            v["response"]["response"]["behavior"], "deny",
            "§9: on expiry marion denies the pending permission and lets the root proceed"
        );
    }

    #[test]
    fn a_ready_marker_that_never_appears_is_a_refusal_not_a_silent_first_turn() {
        let missing = std::env::temp_dir().join(format!("marion-never-{}", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        assert!(!wait_for_ready(&missing, StdDuration::from_millis(30)));
        // And the error says why, because the symptom is a run that "succeeds" with plain text.
        let e = RootError::McpNeverReady(StdDuration::from_millis(30), missing);
        assert!(e.to_string().contains("without mcp__marion__spawn"));
    }

    #[test]
    fn a_marker_written_after_the_wait_begins_is_still_seen() {
        let path = std::env::temp_dir().join(format!("marion-ready-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = path.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_millis(50));
            std::fs::write(&p, b"").unwrap();
        });
        assert!(wait_for_ready(&path, StdDuration::from_secs(5)));
        h.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
