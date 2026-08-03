//! OpenAI Chat Completions SSE, for an `opencode` child.
//!
//! Shapes are taken from the **S13 fixture** (`tests/fixtures/s13/README.md`), captured by driving a
//! real opencode 1.17.3 at a canned local endpoint through `@ai-sdk/openai-compatible`, which ships
//! inside the opencode binary. S13 confirmed on the wire: `POST /v1/chat/completions`,
//! `Authorization: Bearer …`, `stream: true`, the standard `tools[].function` request schema, and
//! standard `chat.completion.chunk` deltas **including streamed `tool_calls`**.
//!
//! **Tool calls are streamed, and this module streams them.** A real OpenAI-compatible client
//! reassembles a call from `delta.tool_calls[]` fragments keyed by `index`: the first fragment
//! carries `id`/`type`/`function.name`, later ones carry only `function.arguments` text. Emitting
//! the whole call in one delta would work with a lenient client and silently fail with a strict
//! one, so [`tool_call_turn`] deliberately splits its arguments across two fragments — if a
//! consumer cannot reassemble, it fails here, in a unit test, rather than inside a harness.
//!
//! **Two spellings, one tool.** The name on *this* wire is the model-facing one, which opencode
//! builds as `<serverName>_<toolName>` — so with the alias `marion` it is `marion_report`. The
//! JSON-RPC `tools/call` opencode then makes to marion's MCP server carries the **unprefixed**
//! `report` (S13, verified live). That is the MCP layer, a different protocol on a different
//! transport; nothing in this module should ever be handed the unprefixed name.

use serde_json::{Value, json};

/// `id` shared by every chunk of one canned completion, as a real stream does.
const COMPLETION_ID: &str = "chatcmpl_canned_1";

fn chunk(choice: Value) -> String {
    let payload = json!({
        "id": COMPLETION_ID,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "canned",
        "choices": [choice],
    });
    format!("data: {payload}\n\n")
}

fn delta(delta: Value) -> String {
    chunk(json!({"index": 0, "delta": delta, "finish_reason": null}))
}

/// The last chunk plus the `[DONE]` sentinel that ends an OpenAI stream.
fn stop(finish_reason: &str) -> String {
    let mut out = chunk(json!({"index": 0, "delta": {}, "finish_reason": finish_reason}));
    out.push_str("data: [DONE]\n\n");
    out
}

/// A turn whose only content is one streamed `tool_call`.
pub fn tool_call_turn(name: &str, call_id: &str, args: &Value) -> String {
    let mut out = String::new();
    out.push_str(&delta(json!({"role": "assistant", "content": null})));
    out.push_str(&delta(json!({"tool_calls": [{
        "index": 0,
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": ""},
    }]})));
    for fragment in halves(&args.to_string()) {
        out.push_str(&delta(json!({"tool_calls": [{
            "index": 0,
            "function": {"arguments": fragment},
        }]})));
    }
    out.push_str(&stop("tool_calls"));
    out
}

/// A turn whose only content is text.
pub fn text_turn(text: &str) -> String {
    let mut out = String::new();
    out.push_str(&delta(json!({"role": "assistant", "content": ""})));
    out.push_str(&delta(json!({"content": text})));
    out.push_str(&stop("stop"));
    out
}

/// Split at a char boundary, so the fragments are two and the JSON is never cut mid-codepoint.
fn halves(s: &str) -> [&str; 2] {
    let mid = s
        .char_indices()
        .nth(s.chars().count() / 2)
        .map_or(s.len(), |(i, _)| i);
    [&s[..mid], &s[mid..]]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reassemble a stream the way a real client does, so the tests assert the *result* of the
    /// protocol rather than the bytes of one chosen chunking.
    fn reassemble(stream: &str) -> (String, Option<(String, String, String)>, Option<String>) {
        let mut text = String::new();
        let mut call: Option<(String, String, String)> = None;
        let mut finish = None;
        for line in stream.lines().filter_map(|l| l.strip_prefix("data: ")) {
            if line == "[DONE]" {
                break;
            }
            let v: Value = serde_json::from_str(line).expect("every chunk is JSON");
            assert_eq!(v["object"], "chat.completion.chunk");
            let choice = &v["choices"][0];
            if let Some(t) = choice["delta"]["content"].as_str() {
                text.push_str(t);
            }
            for tc in choice["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                assert_eq!(tc["index"], 0, "fragments are keyed by index");
                let entry = call.get_or_insert_with(Default::default);
                if let Some(id) = tc["id"].as_str() {
                    entry.0 = id.to_string();
                }
                if let Some(n) = tc["function"]["name"].as_str() {
                    entry.1 = n.to_string();
                }
                if let Some(a) = tc["function"]["arguments"].as_str() {
                    entry.2.push_str(a);
                }
            }
            if let Some(r) = choice["finish_reason"].as_str() {
                finish = Some(r.to_string());
            }
        }
        (text, call, finish)
    }

    #[test]
    fn a_tool_call_reassembles_from_its_fragments() {
        let s = tool_call_turn("marion_report", "call_1", &json!({"narrative": "did it"}));
        let (_, call, finish) = reassemble(&s);
        let (id, name, args) = call.expect("a client must find a tool call here");
        assert_eq!(id, "call_1");
        assert_eq!(name, "marion_report");
        let parsed: Value = serde_json::from_str(&args).expect("the fragments rejoin into JSON");
        assert_eq!(parsed["narrative"], "did it");
        assert_eq!(finish.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn the_arguments_really_arrive_in_more_than_one_fragment() {
        // A single-delta call works with a lenient client and fails with a strict one; splitting
        // here is what proves marion's stream is reassembled rather than merely accepted.
        let s = tool_call_turn("marion_report", "call_1", &json!({"narrative": "did it"}));
        let fragments = s.matches("\"arguments\"").count();
        assert!(fragments >= 3, "one opener plus at least two fragments");
    }

    #[test]
    fn a_text_turn_reassembles_and_stops() {
        let (text, call, finish) = reassemble(&text_turn("all done"));
        assert_eq!(text, "all done");
        assert!(call.is_none());
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[test]
    fn every_stream_ends_with_the_done_sentinel() {
        assert!(text_turn("x").ends_with("data: [DONE]\n\n"));
        assert!(tool_call_turn("t", "c", &json!({})).ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn the_model_facing_name_is_singly_prefixed_not_the_mcp_layers_bare_name() {
        let s = tool_call_turn("marion_report", "call_1", &json!({}));
        assert!(s.contains("\"name\":\"marion_report\""));
        assert!(
            !s.contains("\"name\":\"report\""),
            "the bare name belongs to the JSON-RPC tools/call, not to this wire"
        );
    }

    #[test]
    fn splitting_never_cuts_a_codepoint() {
        let [a, b] = halves("héllo wörld");
        assert_eq!(format!("{a}{b}"), "héllo wörld");
    }
}
