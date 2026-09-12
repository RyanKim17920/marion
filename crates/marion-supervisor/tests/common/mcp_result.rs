//! **The text of an MCP tool result as the model was shown it**, read off a recorded provider
//! request. Shared by `m1_hop.rs`, `native_facade_spawn.rs` and `cross_product.rs`, which each used
//! to carry a copy — and each broke on the same day, when the shape below moved.
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
