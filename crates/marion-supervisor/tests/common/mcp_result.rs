//! **The text of an MCP tool result as the model was shown it**, read off a recorded provider
//! request. Shared by `m1_hop.rs`, `native_facade_spawn.rs`, `cross_product.rs` and (for codex)
//! `m4_fan_in.rs`, which each used to carry a copy, and which broke together when a shape below
//! moved.
//!
//! # What a harness does to a result before the model sees it
//!
//! marion answers an MCP call with a block list, and the contract a root is handed is the `text` of
//! those blocks joined. What reaches the wire is not only that:
//!
//! * **Claude Code 2.1.220** sends an MCP tool result as the block list, verbatim, under
//!   `tool_result.content`; a plain-string `content` is what it sends for its own built-in tools.
//! * **2.1.263, on 2026-09-09** (`MILESTONES.md`, "Request-shape drift a pin cannot catch"), the
//!   same binary started adding `role: "system"` messages with plain-string `content` — the
//!   environment block, the `<total_tokens>` budget — to `messages`. They carry no result, and a
//!   reader that stopped at the first non-array content found nothing.
//! * **2.1.268, on 2026-09-11**, the budget moved again: every `tool_result.content` list now
//!   ends with one more `text` block, `<system-reminder>\n<total_tokens>…</total_tokens>\n
//!   </system-reminder>`, *after* marion's own blocks — and the environment, model, date and
//!   memory reminders ride as leading `text` blocks of the first `user` message instead of as
//!   `system` messages. Joined blindly, the contract JSON gains trailing characters and no longer
//!   deserializes.
//!
//! So this reader keeps only the blocks marion sent: a `text` block that is a `<system-reminder>`
//! is the harness talking to its model, not the tool result, and is dropped wherever it sits in
//! the list. The `system` messages of the 2.1.263 shape are skipped by the same rule that always
//! skipped a plain-string `content`: it cannot hold a `tool_result` block.
//!
//! # The same question on the Responses wire (codex)
//!
//! codex answers its own `function_call` with a `function_call_output` item quoting the `call_id`,
//! and it wraps the MCP result before the model sees it:
//!
//! * **codex 0.146.0 – 0.147.0** send `output` as one **string**,
//!   `Wall time: <n> seconds\nOutput:\n<block list as JSON>`.
//! * **codex 0.155.1, 2026-09-22** sends `output` as a **list** of `input_text` blocks: the first is
//!   the wrapper alone, `Wall time: <n> seconds\nOutput:`, and each MCP `text` block follows as
//!   its own `input_text` block, verbatim. A reader that took `output` as a string found nothing,
//!   and every codex-root `cross_product` cell failed at once.
//!
//! [`codex_output_text`] reads both, and panics naming what it found when neither matches, so a
//! third shape fails loudly rather than reading as "no result yet".

use serde_json::Value;

/// The opening tag of a block the harness injected for its own model. Matched at the start of the
/// block's text, after leading whitespace, so a tool result that merely *mentions* the tag is kept.
const HARNESS_REMINDER_TAG: &str = "<system-reminder>";

/// An MCP `content` payload — a block list, or the bare string a harness may flatten it to — as the
/// text marion sent, with the harness's own `<system-reminder>` blocks left out.
pub fn mcp_blocks_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|text| !text.trim_start().starts_with(HARNESS_REMINDER_TAG))
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

/// The text of the `tool_result` the root sent back for `tool_use_id`, from one recorded request
/// on the Anthropic Messages wire. `None` means "this request does not carry the result", which is
/// the ordinary state of the root's first turn.
pub fn tool_result_text(request: &Value, tool_use_id: &str) -> Option<String> {
    for message in request.pointer("/body/messages")?.as_array()? {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            if block.get("tool_use_id").and_then(Value::as_str) != Some(tool_use_id) {
                continue;
            }
            return mcp_blocks_text(block.get("content")?);
        }
    }
    None
}

/// The wrapper codex puts ahead of an MCP result, up to and including the label. The string form
/// continues with a newline and the block list as JSON; the list form ends its first block here.
const CODEX_OUTPUT_LABEL: &str = "\nOutput:";

/// The text of a codex `function_call_output.output` as marion sent it, from either shape the
/// module doc names. `None` only for a value that is neither a string nor a list.
pub fn codex_output_text(output: &Value) -> Option<String> {
    match output {
        Value::String(raw) => {
            let (_, blocks) = raw
                .split_once(&format!("{CODEX_OUTPUT_LABEL}\n"))
                .unwrap_or_else(|| {
                    panic!(
                        "codex framed its mcp result as something other than \
                         `…\\nOutput:\\n<blocks>`, so the result cannot be read:\n{raw}"
                    )
                });
            let parsed: Value = serde_json::from_str(blocks)
                .unwrap_or_else(|e| panic!("codex's `Output:` section is not JSON: {e}\n{blocks}"));
            mcp_blocks_text(&parsed)
        }
        Value::Array(blocks) => {
            let wrapper = blocks
                .first()
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str);
            if !wrapper
                .is_some_and(|w| w.starts_with("Wall time:") && w.ends_with(CODEX_OUTPUT_LABEL))
            {
                panic!(
                    "codex's block-list output does not open with its `Wall time: …\\nOutput:` \
                     block, so the result cannot be read:\n{output}"
                );
            }
            mcp_blocks_text(&Value::Array(blocks[1..].to_vec()))
        }
        _ => None,
    }
}

/// The text of the `function_call_output` codex sent back for `call_id`, from one Responses
/// request body. `None` means "this request does not carry the result".
pub fn codex_call_output_text(body: &Value, call_id: &str) -> Option<String> {
    let item = body.get("input")?.as_array()?.iter().find(|i| {
        i.get("type").and_then(Value::as_str) == Some("function_call_output")
            && i.get("call_id").and_then(Value::as_str) == Some(call_id)
    })?;
    codex_output_text(item.get("output")?)
}

/// **The contract a `spawn` or `wait` result carries**: the JSON beneath marion's one summary
/// line (`bridge::spawn_text`), or the whole text where there is no such line.
pub fn contract_json(result_text: &str) -> &str {
    match result_text.split_once("\n\n") {
        Some((line, rest)) if line.starts_with("marion: ") => rest,
        _ => result_text,
    }
}
