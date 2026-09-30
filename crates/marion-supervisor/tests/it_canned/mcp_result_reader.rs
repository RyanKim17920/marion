//! `common::mcp_result`'s reader, exercised on the shapes it exists to tell apart.
//!
//! In its own binary rather than as a `#[cfg(test)]` module of `common/`: a module there is
//! compiled into every suite that declares `mod common`, and its tests would then be counted five
//! times over in every admission's per-suite tallies.

use crate::common;
use common::mcp_result::{codex_call_output_text, tool_result_text};
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

fn codex_body(output: Value) -> Value {
    json!({ "input": [
        { "type": "function_call", "call_id": "call_1", "name": "spawn", "namespace": "mcp__marion" },
        { "type": "function_call_output", "call_id": "call_1", "output": output }
    ] })
}

/// codex 0.146.0 – 0.147.0: one string, the block list as JSON after the label.
#[test]
fn codex_string_output_is_unwrapped_and_its_blocks_joined() {
    let blocks = json!([{ "type": "text", "text": "{\"a\":" }, { "type": "text", "text": "1}" }]);
    let body = codex_body(json!(format!("Wall time: 0.9 seconds\nOutput:\n{blocks}")));
    assert_eq!(
        codex_call_output_text(&body, "call_1").as_deref(),
        Some("{\"a\":1}")
    );
}

/// codex 0.155.1: the wrapper is its own leading block and marion's blocks follow verbatim.
#[test]
fn codex_block_list_output_drops_the_wrapper_block_and_joins_the_rest() {
    let body = codex_body(json!([
        { "type": "input_text", "text": "Wall time: 0.9356 seconds\nOutput:" },
        { "type": "input_text", "text": "{\"a\":" },
        { "type": "input_text", "text": "1}" }
    ]));
    assert_eq!(
        codex_call_output_text(&body, "call_1").as_deref(),
        Some("{\"a\":1}")
    );
}

#[test]
fn codex_output_for_another_call_or_none_at_all_finds_nothing() {
    let body = codex_body(json!("Wall time: 1 seconds\nOutput:\n[]"));
    assert_eq!(codex_call_output_text(&body, "call_2"), None);
    assert_eq!(
        codex_call_output_text(&json!({ "input": [] }), "call_1"),
        None
    );
}

#[test]
#[should_panic(expected = "does not open with its `Wall time")]
fn a_codex_block_list_without_the_wrapper_is_refused_loudly() {
    let body = codex_body(json!([{ "type": "input_text", "text": "{\"a\":1}" }]));
    codex_call_output_text(&body, "call_1");
}

#[test]
#[should_panic(expected = "something other than")]
fn a_codex_string_without_the_label_is_refused_loudly() {
    codex_call_output_text(&codex_body(json!("{\"a\":1}")), "call_1");
}
