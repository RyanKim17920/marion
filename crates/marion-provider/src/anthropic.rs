//! Anthropic Messages SSE, for a Claude Code root.
//!
//! Only the frames M1's root needs: a `tool_use` block naming marion's `spawn`, and a plain text
//! reply. Shapes follow the s1 fixtures.

use serde_json::{Value, json};

fn sse(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// A turn whose only content is one `tool_use` block.
pub fn tool_use_turn(tool_name: &str, tool_use_id: &str, input: &Value) -> String {
    let mut out = String::new();
    out.push_str(&sse(
        "message_start",
        &json!({"type":"message_start","message":{
            "id":"msg_canned_1","type":"message","role":"assistant","model":"canned",
            "content":[],"stop_reason":null,
            "usage":{"input_tokens":0,"output_tokens":0}}}),
    ));
    out.push_str(&sse(
        "content_block_start",
        &json!({"type":"content_block_start","index":0,"content_block":{
            "type":"tool_use","id":tool_use_id,"name":tool_name,"input":{}}}),
    ));
    out.push_str(&sse(
        "content_block_delta",
        &json!({"type":"content_block_delta","index":0,"delta":{
            "type":"input_json_delta","partial_json":input.to_string()}}),
    ));
    out.push_str(&sse(
        "content_block_stop",
        &json!({"type":"content_block_stop","index":0}),
    ));
    out.push_str(&sse(
        "message_delta",
        &json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},
                "usage":{"output_tokens":0}}),
    ));
    out.push_str(&sse("message_stop", &json!({"type":"message_stop"})));
    out
}

/// A turn whose only content is text.
pub fn text_turn(text: &str) -> String {
    let mut out = String::new();
    out.push_str(&sse(
        "message_start",
        &json!({"type":"message_start","message":{
            "id":"msg_canned_2","type":"message","role":"assistant","model":"canned",
            "content":[],"stop_reason":null,
            "usage":{"input_tokens":0,"output_tokens":0}}}),
    ));
    out.push_str(&sse(
        "content_block_start",
        &json!({"type":"content_block_start","index":0,
                "content_block":{"type":"text","text":""}}),
    ));
    out.push_str(&sse(
        "content_block_delta",
        &json!({"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":text}}),
    ));
    out.push_str(&sse(
        "content_block_stop",
        &json!({"type":"content_block_stop","index":0}),
    ));
    out.push_str(&sse(
        "message_delta",
        &json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
                "usage":{"output_tokens":0}}),
    ));
    out.push_str(&sse("message_stop", &json!({"type":"message_stop"})));
    out
}

/// The fixed stub for Claude Code's concurrent session-title request.
pub fn session_title_stub() -> String {
    text_turn("{\"title\":\"marion m1\"}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_use_turn_carries_the_tool_name_and_id() {
        let s = tool_use_turn(
            "mcp__marion__spawn",
            "toolu_1",
            &json!({"agent_type": "codex-impl"}),
        );
        assert!(s.contains("\"name\":\"mcp__marion__spawn\""));
        assert!(s.contains("\"id\":\"toolu_1\""));
        assert!(s.contains("\"stop_reason\":\"tool_use\""));
        // every SSE frame is event+data separated by a blank line
        assert_eq!(s.matches("event: ").count(), s.matches("data: ").count());
    }

    #[test]
    fn text_turn_ends_the_turn() {
        assert!(text_turn("done").contains("\"stop_reason\":\"end_turn\""));
    }
}
