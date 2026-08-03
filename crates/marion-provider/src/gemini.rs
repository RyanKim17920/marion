//! Gemini `generateContent` replies, for a `gemini` child.
//!
//! Shapes are taken from the **S12 fixture** (`tests/fixtures/s12/README.md`), captured by driving a
//! real gemini CLI 0.53.0 at a fake endpoint on loopback via `GOOGLE_GEMINI_BASE_URL` — which the
//! CLI accepts over plain HTTP precisely because it is loopback, so this server needs no TLS.
//!
//! **Two framings, one payload.** `:streamGenerateContent?alt=sse` wants SSE frames and
//! `:generateContent` wants the same object as plain JSON. The SSE framing here is `data:` only,
//! with **no `event:` line** — the google-genai SDK does not name its events, unlike the Anthropic
//! wire. Which framing to use is a property of the *path*, not of the body, which is why
//! [`crate::Wire::Gemini`] carries the flag rather than these functions guessing.
//!
//! Nothing in this module looks at the model id. S12 measured `-m gemini-2.5-flash` arriving on the
//! wire as `gemini-3.5-flash` — an internal remap whose trigger is unknown — so an equality test
//! against a requested id would fail for reasons no log would explain. Not matching at all is the
//! strongest form of "match on substrings, never on exact ids".

use serde_json::{Value, json};

/// One `GenerateContentResponse`, framed for whichever endpoint asked for it.
fn frame(response: Value, streaming: bool) -> String {
    if streaming {
        format!("data: {response}\n\n")
    } else {
        response.to_string()
    }
}

/// The single-candidate envelope both endpoints return.
///
/// `finishReason` is `STOP` even when the candidate is a `functionCall`: the real API signals a
/// tool call by the part, not by the finish reason.
fn response(parts: Value) -> Value {
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": {
            "promptTokenCount": 0, "candidatesTokenCount": 0, "totalTokenCount": 0,
        },
        "modelVersion": "fake-1",
    })
}

/// A turn whose only part is a `functionCall`.
///
/// `name` is the model-facing spelling, which on this harness is `mcp_<server>_<tool>` — S12:
/// gemini joins the MCP server alias and the tool name with **single** underscores, so marion's
/// `report` is `mcp_marion_report` and **not** Claude Code's `mcp__marion__report`. The same
/// fixture records why the alias itself must not contain `_`: the policy engine mis-parses the
/// fully-qualified name and fails silently.
pub fn function_call_turn(name: &str, args: &Value, streaming: bool) -> String {
    frame(
        response(json!([{"functionCall": {"name": name, "args": args}}])),
        streaming,
    )
}

/// A turn whose only part is text.
pub fn text_turn(text: &str, streaming: bool) -> String {
    frame(response(json!([{"text": text}])), streaming)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_streaming_form_is_sse_with_no_event_line() {
        let s = text_turn("hi", true);
        assert!(s.starts_with("data: {"));
        assert!(s.ends_with("\n\n"));
        assert!(
            !s.contains("event: "),
            "the google-genai SDK does not name its events; an event line is Anthropic's shape"
        );
    }

    #[test]
    fn the_non_streaming_form_is_a_bare_json_document() {
        let s = text_turn("hi", false);
        assert!(!s.contains("data: "));
        let v: Value = serde_json::from_str(&s).expect(":generateContent answers plain JSON");
        assert_eq!(v["candidates"][0]["content"]["parts"][0]["text"], "hi");
        assert_eq!(v["candidates"][0]["finishReason"], "STOP");
    }

    #[test]
    fn a_function_call_is_a_part_not_a_finish_reason() {
        let s = function_call_turn("mcp_marion_report", &json!({"narrative": "x"}), true);
        let v: Value = serde_json::from_str(s.trim_start_matches("data: ").trim()).unwrap();
        let part = &v["candidates"][0]["content"]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "mcp_marion_report");
        assert_eq!(part["functionCall"]["args"]["narrative"], "x");
        assert_eq!(
            v["candidates"][0]["finishReason"], "STOP",
            "the tool call is signalled by the part, not by the finish reason"
        );
    }

    #[test]
    fn the_tool_name_uses_single_underscores() {
        let s = function_call_turn("mcp_marion_report", &json!({}), true);
        assert!(
            !s.contains("mcp__marion__report"),
            "the doubled spelling is Claude Code's; gemini would never dispatch it"
        );
    }
}
