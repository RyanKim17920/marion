//! OpenAI Responses SSE, for a `codex exec` child.
//!
//! Shapes are taken from the **S6 fixtures** (`tests/fixtures/s6/`), which were captured by
//! driving a real `codex exec` 0.146.0 against this provider's Python ancestor.
//!
//! S6's load-bearing finding: codex 0.146.0 runs *code mode*, so a real model reaches an MCP tool
//! by writing `await tools.mcp__marion__report({…})` inside a `custom` `exec` tool. **marion's
//! canned scripts do not have to synthesise JavaScript** — a plain `function_call` item carrying
//! `namespace: "mcp__marion"` is executed end to end, which is what `report_call` emits.

use serde_json::{Value, json};

fn sse(data: &Value) -> String {
    let ty = data.get("type").and_then(Value::as_str).unwrap_or("message");
    format!("event: {ty}\ndata: {data}\n\n")
}

fn envelope(item: Value, id: &str) -> String {
    let mut out = String::new();
    out.push_str(&sse(&json!({"type":"response.created","response":{"id":id}})));
    out.push_str(&sse(&json!({"type":"response.output_item.done","item":item})));
    out.push_str(&sse(&json!({"type":"response.completed",
                              "response":{"id":id,"output":[item]}})));
    out
}

/// A call to marion's `report`, in the namespace form codex dispatches on.
///
/// Verified end to end in S6: codex spawned the MCP server, issued `tools/call`, and surfaced the
/// server's result as an `mcp_tool_call` item.
pub fn report_call(narrative: &str, call_id: &str) -> String {
    envelope(
        json!({
            "type": "function_call",
            "id": format!("fc_{call_id}"),
            "call_id": call_id,
            "name": "report",
            "namespace": "mcp__marion",
            "arguments": json!({"narrative": narrative}).to_string(),
        }),
        "resp_report",
    )
}

/// An `exec` custom-tool call whose JavaScript edits a file via `apply_patch`.
///
/// S6 recovered the encoding this repo previously listed as unknown: **`tools.apply_patch` takes a
/// string**, not `{input: …}`. Passing an object fails with
/// `tool `apply_patch` expects a string input`, and the run silently does nothing.
pub fn apply_patch_call(patch: &str, call_id: &str) -> String {
    let js = format!(
        "const r = await tools.apply_patch({});\ntext(JSON.stringify(r));",
        serde_json::to_string(patch).expect("string encodes")
    );
    envelope(
        json!({
            "type": "custom_tool_call",
            "id": format!("ct_{call_id}"),
            "call_id": call_id,
            "name": "exec",
            "input": js,
        }),
        "resp_patch",
    )
}

/// A plain final assistant message. With `--output-schema`, this is what
/// `--output-last-message` receives verbatim (S6 question 3).
pub fn final_message(text: &str) -> String {
    envelope(
        json!({
            "type": "message",
            "id": "msg_final",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }),
        "resp_final",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_call_uses_the_namespace_form_not_the_flat_name() {
        let s = report_call("did it", "call_1");
        assert!(s.contains("\"namespace\":\"mcp__marion\""));
        assert!(s.contains("\"name\":\"report\""));
        assert!(
            !s.contains("\"name\":\"mcp__marion__report\""),
            "the flat string in a function_call is what codex rejects as unsupported"
        );
    }

    #[test]
    fn apply_patch_passes_a_string_not_an_object() {
        let s = apply_patch_call("*** Begin Patch\n*** End Patch", "call_2");
        assert!(s.contains("tools.apply_patch(\\\""), "S6: an object input fails at runtime");
        assert!(!s.contains("apply_patch({input"));
    }

    #[test]
    fn final_message_round_trips_a_schema_document() {
        let doc = "{\"narrative\":\"x\",\"result_commits\":[]}";
        assert!(final_message(doc).contains("output_text"));
    }
}
