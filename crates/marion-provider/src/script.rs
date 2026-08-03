//! The scripts, and the *shape* predicates that choose between their steps.
//!
//! Every decision in this module is a function of the request body alone. That is not a stylistic
//! preference: see the module docs on [`crate`] — Claude Code issues a session-title request
//! concurrently with the first real turn, so "which request is this" is a race, and the Python
//! ancestor of this provider (`spikes/s6/canned_provider.py`) got it wrong by counting turns.
//! Nothing here counts anything.

use serde_json::{Value, json};

use crate::{RequestKind, anthropic, classify_anthropic, responses};

/// `call_id` of the child's `apply_patch` step. Its later echo in `input` is how the provider
/// recognises that the patch step has already happened.
pub const PATCH_CALL_ID: &str = "call_marion_patch_1";
/// `call_id` of the child's `report` step, used the same way.
pub const REPORT_CALL_ID: &str = "call_marion_report_1";
/// `tool_use.id` of the root's single tool call. The root's `tool_result` quotes it back, and — as
/// S9 records — so does the `can_use_tool` frame's `tool_use_id`, which is how a permission ask is
/// tied to the call that provoked it.
pub const ROOT_TOOL_USE_ID: &str = "toolu_marion_spawn_1";

/// Which wire format a request is speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// Anthropic Messages, spoken by the Claude Code root.
    Anthropic,
    /// OpenAI Responses, spoken by the `codex exec` child.
    Responses,
}

/// Tell the two wires apart.
///
/// The body is the primary evidence and the path only a tiebreak: both harnesses let the operator
/// choose the base URL, so a path is configuration, whereas `messages` vs `input` is the protocol.
pub fn classify_wire(path: &str, body: &Value) -> Option<Wire> {
    if body.get("messages").and_then(Value::as_array).is_some() {
        return Some(Wire::Anthropic);
    }
    if body.get("input").is_some() {
        return Some(Wire::Responses);
    }
    if path.contains("/messages") {
        return Some(Wire::Anthropic);
    }
    if path.contains("/responses") {
        return Some(Wire::Responses);
    }
    None
}

/// Where the root is in its two-step script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootStep {
    /// Nothing has been delegated yet: call `spawn`.
    Delegate,
    /// The conversation already carries the result of that call: say something and stop.
    Finish,
}

/// Where the child is in its three-step script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStep {
    /// No edit yet: emit the `exec`/`apply_patch` call.
    ApplyPatch,
    /// The patch is in the transcript: return through marion's `report`.
    Report,
    /// Both have happened: emit the final message.
    Finish,
}

/// Decide the root's step from the conversation it sent us.
///
/// The signal is a `tool_result` block anywhere in `messages`. Claude Code replays the whole
/// conversation on every turn, so once `spawn` has returned, its result is present on this and
/// every later request — which makes the predicate stable rather than edge-triggered.
pub fn classify_root(body: &Value) -> RootStep {
    let has_result = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("content").and_then(Value::as_array))
        .flatten()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
    if has_result {
        RootStep::Finish
    } else {
        RootStep::Delegate
    }
}

/// Decide the child's step from the `input` array it sent us.
///
/// Only *call* and *call output* items count as evidence. A substring scan would be wrong: under
/// code mode codex ships its tool catalogue inside `input` as an `additional_tools` developer
/// message (S6), so the words `exec` and `report` appear on turn one already.
pub fn classify_child(body: &Value) -> ChildStep {
    let input: Vec<&Value> = body
        .get("input")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    if input
        .iter()
        .any(|it| is_echo_of(it, REPORT_CALL_ID) || is_report_call(it))
    {
        ChildStep::Finish
    } else if input
        .iter()
        .any(|it| is_echo_of(it, PATCH_CALL_ID) || is_custom_tool_item(it))
    {
        ChildStep::Report
    } else {
        ChildStep::ApplyPatch
    }
}

/// Our own `call_id`, quoted back to us on a later turn — the most precise possible evidence,
/// since this provider authored the id.
fn is_echo_of(item: &Value, call_id: &str) -> bool {
    item.get("call_id").and_then(Value::as_str) == Some(call_id)
}

fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// A call to marion's `report` under either spelling S6 found: the `{name, namespace}` form codex
/// dispatches on, or the `mcp_tool_call` item it surfaces the result as.
fn is_report_call(item: &Value) -> bool {
    match item_type(item) {
        "function_call" => item.get("name").and_then(Value::as_str) == Some("report"),
        "mcp_tool_call" => item.get("tool").and_then(Value::as_str) == Some("report"),
        _ => false,
    }
}

fn is_custom_tool_item(item: &Value) -> bool {
    matches!(
        item_type(item),
        "custom_tool_call" | "custom_tool_call_output"
    )
}

/// The canned behaviour of one M1 run, as data.
///
/// Held as a struct rather than hard-coded constants so an integration test can point the child's
/// patch at a path that actually exists in its own fixture repo.
#[derive(Debug, Clone)]
pub struct Script {
    /// The tool the root calls on its first turn, in the harness's own spelling.
    ///
    /// Parameterised so a test can aim the root at a verb that is **not** in
    /// `ROOT_ALLOWED_TOOLS` — which is the only way to provoke a real inbound `can_use_tool`
    /// frame from the CLI (S9, design §11 item 14). The M1 hop leaves it at `spawn`.
    pub root_tool: String,
    /// Arguments the root passes to [`Script::root_tool`].
    pub root_tool_input: Value,
    /// The root's closing text turn.
    pub root_final_text: String,
    /// The patch the child feeds to `tools.apply_patch` (a string — S6).
    pub child_patch: String,
    /// The `narrative` the child passes to marion's `report`.
    pub child_narrative: String,
    /// The child's final assistant message. Under `--output-schema` this is delivered verbatim to
    /// `--output-last-message`, so the default is a schema document, not prose.
    pub child_final_text: String,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            root_tool: "mcp__marion__spawn".to_string(),
            root_tool_input: json!({
                "agent_type": "codex-impl",
                "prompt": "Add the M1 marker file under src/ and report back.",
                "acceptance_criteria": ["a file exists under src/ containing the M1 marker"],
                "writable_scope": ["src/**"],
            }),
            root_final_text: "The child completed the task and reported back. M1 hop done."
                .to_string(),
            child_patch: "*** Begin Patch\n*** Add File: src/marion_m1.txt\n\
                          +marion M1: written by the canned codex child\n*** End Patch"
                .to_string(),
            child_narrative: "Added the M1 marker file under src/.".to_string(),
            child_final_text:
                json!({"narrative": "Added the M1 marker file under src/.", "result_commits": []})
                    .to_string(),
        }
    }
}

impl Script {
    /// Produce the SSE body for one request, on whichever wire it arrived.
    pub fn respond(&self, wire: Wire, body: &Value) -> String {
        match wire {
            Wire::Anthropic => self.respond_anthropic(body),
            Wire::Responses => self.respond_responses(body),
        }
    }

    fn respond_anthropic(&self, body: &Value) -> String {
        match classify_anthropic(body) {
            RequestKind::SessionTitle => anthropic::session_title_stub(),
            RequestKind::ScriptedTurn => match classify_root(body) {
                RootStep::Delegate => anthropic::tool_use_turn(
                    &self.root_tool,
                    ROOT_TOOL_USE_ID,
                    &self.root_tool_input,
                ),
                RootStep::Finish => anthropic::text_turn(&self.root_final_text),
            },
        }
    }

    fn respond_responses(&self, body: &Value) -> String {
        match classify_child(body) {
            ChildStep::ApplyPatch => responses::apply_patch_call(&self.child_patch, PATCH_CALL_ID),
            ChildStep::Report => responses::report_call(&self.child_narrative, REPORT_CALL_ID),
            ChildStep::Finish => responses::final_message(&self.child_final_text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_turn() -> Value {
        json!({
            "model": "claude",
            "tools": [{"name": "mcp__marion__spawn"}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "delegate"}]}]
        })
    }

    fn after_spawn_returned() -> Value {
        let mut b = user_turn();
        b["messages"].as_array_mut().unwrap().push(json!({
            "role": "assistant",
            "content": [{"type": "tool_use", "id": ROOT_TOOL_USE_ID,
                         "name": "mcp__marion__spawn", "input": {}}]
        }));
        b["messages"].as_array_mut().unwrap().push(json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": ROOT_TOOL_USE_ID,
                         "content": "{\"state\":\"ok\"}"}]
        }));
        b
    }

    #[test]
    fn the_root_delegates_before_anything_has_come_back() {
        assert_eq!(classify_root(&user_turn()), RootStep::Delegate);
    }

    #[test]
    fn the_root_stops_once_the_transcript_carries_the_spawn_result() {
        assert_eq!(classify_root(&after_spawn_returned()), RootStep::Finish);
    }

    #[test]
    fn root_steps_are_a_function_of_the_body_not_of_arrival_order() {
        // Replay the *second* turn first. A counting provider would answer it with `spawn`,
        // spawn a second child, and never end the run.
        let s = Script::default();
        let second = s.respond(Wire::Anthropic, &after_spawn_returned());
        let first = s.respond(Wire::Anthropic, &user_turn());
        assert!(
            second.contains("end_turn"),
            "the later turn must still end the run"
        );
        assert!(
            first.contains("mcp__marion__spawn"),
            "the earlier turn must still delegate"
        );
    }

    #[test]
    fn the_concurrent_title_request_never_consumes_the_spawn_turn() {
        // The exact failure the crate docs describe: title request racing turn one.
        let title = json!({
            "model": "claude", "tools": [],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "output_config": {"format": {"type": "json_schema"}}
        });
        let out = Script::default().respond(Wire::Anthropic, &title);
        assert!(!out.contains("mcp__marion__spawn"));
        assert!(
            out.contains("marion m1"),
            "it gets the fixed title stub instead"
        );
    }

    fn child_input(items: Value) -> Value {
        json!({"model": "gpt-5.6-sol", "input": items})
    }

    #[test]
    fn the_child_patches_first() {
        let developer_catalogue = json!([{
            "type": "message", "role": "developer",
            "content": [{"type": "input_text",
                         "text": "additional_tools: exec, mcp__marion__report"}]
        }]);
        assert_eq!(
            classify_child(&child_input(developer_catalogue)),
            ChildStep::ApplyPatch,
            "the tool catalogue names exec and report on turn one; naming is not evidence of use"
        );
    }

    #[test]
    fn the_child_reports_once_the_patch_call_is_in_the_transcript() {
        let items = json!([
            {"type": "custom_tool_call", "call_id": PATCH_CALL_ID, "name": "exec", "input": "…"},
            {"type": "custom_tool_call_output", "call_id": PATCH_CALL_ID, "output": "ok"},
        ]);
        assert_eq!(classify_child(&child_input(items)), ChildStep::Report);
    }

    #[test]
    fn the_child_finishes_once_report_has_been_called() {
        let items = json!([
            {"type": "custom_tool_call", "call_id": PATCH_CALL_ID, "name": "exec", "input": "…"},
            {"type": "function_call", "call_id": REPORT_CALL_ID, "name": "report",
             "namespace": "mcp__marion", "arguments": "{}"},
            {"type": "function_call_output", "call_id": REPORT_CALL_ID, "output": "ok"},
        ]);
        assert_eq!(classify_child(&child_input(items)), ChildStep::Finish);
    }

    #[test]
    fn a_report_surfaced_as_an_mcp_tool_call_item_also_counts() {
        // S6 shows codex echoing the MCP result as an `mcp_tool_call` item rather than a
        // function_call/output pair; missing that spelling would loop the child forever.
        let items = json!([
            {"type": "custom_tool_call", "call_id": PATCH_CALL_ID, "name": "exec"},
            {"type": "mcp_tool_call", "server": "marion", "tool": "report", "result": "ok"},
        ]);
        assert_eq!(classify_child(&child_input(items)), ChildStep::Finish);
    }

    #[test]
    fn the_child_script_emits_a_string_patch_then_report_then_a_schema_document() {
        let s = Script::default();
        assert!(
            s.respond(Wire::Responses, &child_input(json!([])))
                .contains("tools.apply_patch(\\\"")
        );
        let items = json!([{"type": "custom_tool_call", "call_id": PATCH_CALL_ID}]);
        assert!(
            s.respond(Wire::Responses, &child_input(items))
                .contains("\"namespace\":\"mcp__marion\"")
        );
        let done = json!([{"type": "mcp_tool_call", "tool": "report"}]);
        assert!(
            s.respond(Wire::Responses, &child_input(done))
                .contains("result_commits")
        );
    }

    #[test]
    fn wires_are_told_apart_by_body_first_then_path() {
        assert_eq!(
            classify_wire("/x", &json!({"messages": []})),
            Some(Wire::Anthropic)
        );
        assert_eq!(
            classify_wire("/x", &json!({"input": []})),
            Some(Wire::Responses)
        );
        assert_eq!(
            classify_wire("/v1/messages", &json!({})),
            Some(Wire::Anthropic)
        );
        assert_eq!(
            classify_wire("/v1/responses", &json!({})),
            Some(Wire::Responses)
        );
        assert_eq!(classify_wire("/health", &json!({})), None);
    }
}
