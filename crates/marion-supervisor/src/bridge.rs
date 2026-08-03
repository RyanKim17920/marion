//! The stdio MCP bridge (design §5.4, §6.1 step 8).
//!
//! marion does **not** spawn this process — the harness does, from a server declaration marion
//! wrote at config-injection time. That is why the per-node token rides the declaration rather
//! than an inherited fd or the bridge's stdin: marion is not the parent, and stdin is already the
//! JSON-RPC transport.
//!
//! Frame shapes follow `tests/fixtures/s6/mcp-server-frames.jsonl`, captured from a real
//! `codex exec` 0.146.0.
//!
//! **The bridge must be idempotent across repeated startup**: S6 observed codex issuing *two* full
//! `initialize` + `tools/list` sequences for a single `exec` run.

use serde_json::{Value, json};

/// Protocol version we answer `initialize` with. Codex 0.146.0 offers `2025-06-18`; MCP requires
/// the server to reply with a version it supports, not to echo the client's.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A decoded inbound request.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Initialize {
        id: Value,
    },
    ToolsList {
        id: Value,
    },
    ToolsCall {
        id: Value,
        name: String,
        arguments: Value,
    },
    /// Notifications carry no id and expect no reply.
    Notification,
    /// Anything else with an id: answered with method-not-found rather than ignored, so a client
    /// waiting on it is never left hanging.
    Unknown {
        id: Value,
        method: String,
    },
}

pub fn parse(line: &str) -> Option<Request> {
    let v: Value = serde_json::from_str(line).ok()?;
    let method = v.get("method").and_then(Value::as_str)?;
    let id = v.get("id").cloned();
    Some(match (method, id) {
        ("initialize", Some(id)) => Request::Initialize { id },
        ("tools/list", Some(id)) => Request::ToolsList { id },
        ("tools/call", Some(id)) => Request::ToolsCall {
            id,
            name: v
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: v
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
        },
        (m, _) if m.starts_with("notifications/") => Request::Notification,
        (m, Some(id)) => Request::Unknown {
            id,
            method: m.to_string(),
        },
        (_, None) => Request::Notification,
    })
}

/// marion's tool surface. `spawn` is child-creating; `report` is the child's return path.
///
/// Names here are the **MCP tool names**. The surface spelling differs per harness — Claude Code
/// prefixes to `mcp__marion__spawn`, and Codex exposes the same tool to the model as the
/// JavaScript identifier `tools.mcp__marion__report` under code mode (S6) — but neither spelling
/// belongs in this declaration.
///
/// # Why this is a constant and not a function of the node
///
/// The node's depth *is* available here — it rides the same `env` block as `MARION_AGENT_ID`
/// (`marion_harness::DEPTH_ENV`) — so a bridge serving a node at its type's `max_depth` could
/// simply not offer `spawn`, and one serving a root could not offer `report`. That was considered
/// and **rejected**, on three grounds:
///
/// 1. **§3.1 wants a refusal that names the bound.** A `spawn` past `max_depth` "is refused with a
///    spawn error, never silently clamped". A verb that is merely *absent* carries no bound, no
///    value and no sentence: the node cannot tell "marion has no `spawn`" from "I may not spawn",
///    and nothing anywhere reports why. That is the §12 silent-failure shape this codebase keeps
///    legislating against, traded for the loud refusal `run::run_spawn` now returns.
/// 2. **`tools/list` is answered once, at startup; the gate is evaluated per call.** Availability
///    would be a snapshot and permission a live decision, so the two could only ever agree or lag
///    — a second source of truth for a rule that must exist in the first place anyway. §9 warns
///    about exactly that kind of drift.
/// 3. **§3.1 and §9 already sanction the two axes differing.** §9 notes the root's allowlist being
///    wider than the declared surface is "not a bug". A uniform declaration plus a per-call gate is
///    a shape the design states, not a gap in it.
///
/// **`report` on a root is the one inconsistency left standing, knowingly.** §7.6 says a root has
/// no contract and cannot `report`, yet `main::handle_tool_call` answers `report recorded` to a
/// root today. Closing it means the bridge answering a root's `report` with an error — and that
/// answer is *inside* `tests/fixtures/s9/can-use-tool-allow.stdout.jsonl`, a committed recording
/// of a real 2.1.220 run that `permission_round_trip` replays and asserts still matches. Correcting
/// the behaviour therefore requires re-recording that fixture, which this change does not touch.
/// It is a real defect, and it is written down here rather than papered over.
pub fn tools() -> Value {
    json!([
        {
            "name": "spawn",
            "description": "Delegate a task to a child agent. Blocks until the child reaches a \
                            terminal state, then returns the completed task contract.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent_type": {"type": "string"},
                    // Optional: omitted, the agent type's own default is used (§3.1). Named in
                    // marion's vocabulary — the adapter maps it to the harness's spelling.
                    "model": {"type": "string"},
                    "prompt": {"type": "string"},
                    "acceptance_criteria": {"type": "array", "items": {"type": "string"}},
                    "verification": {"type": "array", "items": {"type": "string"}},
                    "writable_scope": {"type": "array", "items": {"type": "string"}},
                    "name": {"type": "string"},
                    "isolation": {"type": "string", "enum": ["worktree", "shared-cwd", "remote"]},
                    "timeout_secs": {"type": "integer"},
                    "allow_concurrent_writes": {"type": "boolean"},
                    "background": {"type": "boolean"}
                },
                "required": ["agent_type", "prompt", "acceptance_criteria"]
            }
        },
        {
            "name": "report",
            "description": "Return your result to marion. Call this exactly once when the task \
                            is done. Your final assistant message is NOT the return value.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "narrative": {"type": "string"},
                    "result_commits": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["narrative"]
            }
        }
    ])
}

pub fn initialize_result(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "marion", "version": env!("CARGO_PKG_VERSION")}}})
}

pub fn tools_list_result(id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools()}})
}

pub fn tool_result(id: &Value, text: &str, is_error: bool) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {
        "content": [{"type": "text", "text": text}], "isError": is_error}})
}

pub fn method_not_found(id: &Value, method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id,
           "error": {"code": -32601, "message": format!("no method {method}")}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_frames_a_real_codex_sends() {
        // Verbatim from tests/fixtures/s6/mcp-server-frames.jsonl.
        let init = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"codex-mcp-client","title":"Codex","version":"0.146.0"}}}"#;
        assert!(matches!(parse(init), Some(Request::Initialize { .. })));

        let listed = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"progressToken":0}}}"#;
        assert!(matches!(parse(listed), Some(Request::ToolsList { .. })));

        let called = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"report","arguments":{"narrative":"s6 probe"}}}"#;
        match parse(called) {
            Some(Request::ToolsCall {
                name, arguments, ..
            }) => {
                assert_eq!(name, "report");
                assert_eq!(arguments["narrative"], "s6 probe");
            }
            other => panic!("expected a tools/call, got {other:?}"),
        }

        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert_eq!(parse(note), Some(Request::Notification));
    }

    #[test]
    fn an_unknown_method_with_an_id_is_answered_not_ignored() {
        let f = r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#;
        match parse(f) {
            Some(Request::Unknown { method, .. }) => assert_eq!(method, "resources/list"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn tool_declarations_use_bare_names_not_harness_spellings() {
        let t = tools();
        let names: Vec<&str> = t
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["spawn", "report"]);
        let s = t.to_string();
        assert!(
            !s.contains("mcp__marion"),
            "the prefix is applied by the harness; baking it in would double it"
        );
    }

    #[test]
    fn report_says_the_final_message_is_not_the_return_value() {
        let t = tools();
        let desc = t[1]["description"].as_str().unwrap();
        assert!(
            desc.contains("NOT the return value"),
            "this is the confusion 7.6 exists for"
        );
    }

    #[test]
    fn initialize_answers_with_a_version_we_support() {
        let v = initialize_result(&json!(0));
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "marion");
    }

    #[test]
    fn handling_is_idempotent_across_repeated_startup() {
        // S6: codex issues two full initialize + tools/list sequences per exec run.
        let a = initialize_result(&json!(0));
        let b = initialize_result(&json!(0));
        assert_eq!(a, b);
        assert_eq!(tools_list_result(&json!(1)), tools_list_result(&json!(1)));
    }
}
