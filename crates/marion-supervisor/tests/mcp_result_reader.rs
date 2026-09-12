//! `common::mcp_result`'s reader, exercised on the shapes it exists to tell apart.
//!
//! In its own binary rather than as a `#[cfg(test)]` module of `common/`: a module there is
//! compiled into every suite that declares `mod common`, and its tests would then be counted five
//! times over in every admission's per-suite tallies.

mod common;
use common::mcp_result::tool_result_text;
use serde_json::{Value, json};

fn request_with(content: Value) -> Value {
    json!({ "body": { "messages": [
        { "role": "system", "content": "<total_tokens>15000000 tokens left</total_tokens>" },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": content }
        ] }
    ] } })
}

#[test]
fn a_block_list_is_joined_in_order() {
    let r = request_with(
        json!([{ "type": "text", "text": "{\"a\":" }, { "type": "text", "text": "1}" }]),
    );
    assert_eq!(
        tool_result_text(&r, "toolu_1").as_deref(),
        Some("{\"a\":1}")
    );
}

/// The 2.1.268 shape: the budget reminder is a trailing block inside the result.
#[test]
fn the_harness_reminder_block_is_dropped_wherever_it_sits() {
    let r = request_with(json!([
        { "type": "text", "text": "  <system-reminder>\nlead\n</system-reminder>" },
        { "type": "text", "text": "{\"a\":1}\n" },
        { "type": "text", "text": "<system-reminder>\n<total_tokens>1 tokens left</total_tokens>\n</system-reminder>" }
    ]));
    assert_eq!(
        tool_result_text(&r, "toolu_1").as_deref(),
        Some("{\"a\":1}\n")
    );
}

/// A result that talks *about* the tag is marion's own text and stays.
#[test]
fn a_result_mentioning_the_tag_is_kept() {
    let r = request_with(json!([{ "type": "text", "text": "saw <system-reminder> in the log" }]));
    assert_eq!(
        tool_result_text(&r, "toolu_1").as_deref(),
        Some("saw <system-reminder> in the log")
    );
}

#[test]
fn a_flattened_string_result_is_returned_as_is() {
    let r = request_with(json!("done"));
    assert_eq!(tool_result_text(&r, "toolu_1").as_deref(), Some("done"));
}

#[test]
fn another_call_id_and_a_bare_first_turn_find_nothing() {
    let r = request_with(json!([{ "type": "text", "text": "x" }]));
    assert_eq!(tool_result_text(&r, "toolu_2"), None);
    let first_turn = json!({ "body": { "messages": [{ "role": "user", "content": "hi" }] } });
    assert_eq!(tool_result_text(&first_turn, "toolu_1"), None);
}
