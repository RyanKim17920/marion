//! **What a running node has been doing**, as a few bounded lines: its last tool calls and the
//! last line of text it wrote.
//!
//! A parent deciding whether to `steer` a child, and an operator deciding whether to press `s`,
//! both need more than a state word. The node's own `events.jsonl` already carries every frame its
//! harness emitted, verbatim; this reads the tail of that file and hands the frames to
//! [`marion_harness::grammar::recent_activity`] under the adapter's activity rule — a row's, or
//! ACP's protocol-wide `session/update` one — the same reader everything else about a stream uses.
//! **No harness is named here**: a harness with no rule is said to be unread, never shown as an
//! empty peek, because "did nothing" and "marion cannot tell" call for opposite next moves.
//!
//! Bounded three ways, so a chatty child cannot flood its parent's context: at most
//! [`MAX_CALLS`] calls, each line at most [`LINE_CAP`] characters, and the whole at most
//! [`PEEK_CAP`] bytes. The file is read from its last [`TAIL_BYTES`] only, so a long run costs a
//! peek no more than a short one.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use marion_core::event::{EventLog, Payload};
use marion_core::harness::Harness;
use marion_harness::adapter::adapter_for;
use marion_harness::grammar::{RecentActivity, recent_activity};
use serde_json::Value;

/// The most tool calls a peek shows.
pub const MAX_CALLS: usize = 5;
/// The most characters one line of a peek carries, ellipsis included.
pub const LINE_CAP: usize = 160;
/// The most bytes a whole peek carries.
pub const PEEK_CAP: usize = 2048;
/// How much of the end of `events.jsonl` is read. Four times the largest single event
/// (`marion_core::event::MAX_EVENT_BYTES`), so the last few frames are always whole.
const TAIL_BYTES: u64 = 1024 * 1024;

/// The peek at a node of `harness` whose stream is `events`, as lines ready to print.
pub fn peek(events: &Path, harness: Harness) -> String {
    let Some(rule) = adapter_for(harness).ok().and_then(|a| a.activity()) else {
        return format!(
            "Recent activity: not shown — marion reads no {harness:?} stream by a row, so it \
             cannot say which tools this node called."
        );
    };
    let frames = tail_frames(events);
    if frames.is_empty() {
        return "Recent activity: nothing recorded yet — the node has not said anything marion \
                could read."
            .to_string();
    }
    render(&recent_activity(rule, &frames, MAX_CALLS))
}

/// [`peek`]'s text for an activity already read: the calls oldest first, then the last words.
pub fn render(a: &RecentActivity) -> String {
    let mut out = String::from("Recent activity (oldest first):");
    if a.calls.is_empty() {
        out.push_str("\n- no tool calls yet");
    }
    for c in &a.calls {
        let args = match &c.args {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.push('\n');
        out.push_str(&capped(&format!("- {}({})", c.name, one_line(&args))));
    }
    if let Some(t) = &a.text {
        out.push('\n');
        out.push_str(&capped(&format!("- last said: {}", one_line(t))));
    }
    cap_bytes(out, PEEK_CAP)
}

/// Every harness frame in the last [`TAIL_BYTES`] of `path`, in order. A missing or unreadable
/// file is no frames: a peek is a view and never a reason to fail the answer it rides on.
fn tail_frames(path: &Path) -> Vec<Value> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL_BYTES);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    // Started mid-file: the first line is a fragment of an event, not an event.
    if start > 0 {
        match bytes.iter().position(|b| *b == b'\n') {
            Some(nl) => {
                bytes.drain(..=nl);
            }
            None => return Vec::new(),
        }
    }
    let mut events = Vec::new();
    EventLog::default().extend(&bytes, &mut events);
    events
        .into_iter()
        .filter_map(|e| match e.payload {
            Payload::Vendor { json, .. } => Some(json),
            _ => None,
        })
        .collect()
}

/// Newlines and tabs as spaces, so one call is one line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `s`, or its first [`LINE_CAP`] − 1 characters and an ellipsis.
fn capped(s: &str) -> String {
    if s.chars().count() <= LINE_CAP {
        return s.to_string();
    }
    let mut out: String = s.chars().take(LINE_CAP - 1).collect();
    out.push('…');
    out
}

/// `s`, or its longest whole-character prefix under `cap` bytes with an ellipsis.
fn cap_bytes(s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    let mut i = cap - '…'.len_utf8();
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    format!("{}…", &s[..i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_harness::grammar::ToolCall;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            name: name.into(),
            args,
        }
    }

    #[test]
    fn a_peek_is_one_line_per_call_then_the_last_words() {
        let a = RecentActivity {
            calls: vec![
                call("command_execution", Value::String("cargo\ntest".into())),
                call("report", serde_json::json!({"narrative": "done"})),
            ],
            text: Some("all green".into()),
        };
        assert_eq!(
            render(&a),
            "Recent activity (oldest first):\n- command_execution(cargo test)\n\
             - report({\"narrative\":\"done\"})\n- last said: all green"
        );
        let quiet = render(&RecentActivity::default());
        assert!(quiet.contains("no tool calls yet"), "{quiet}");
    }

    #[test]
    fn every_line_and_the_whole_peek_stay_inside_their_bounds() {
        let huge = "é".repeat(5000);
        let a = RecentActivity {
            calls: (0..MAX_CALLS)
                .map(|i| call(&format!("t{i}"), Value::String(huge.clone())))
                .collect(),
            text: Some(huge.clone()),
        };
        let out = render(&a);
        assert!(out.len() <= PEEK_CAP, "{} bytes", out.len());
        for line in out.lines().skip(1) {
            assert!(line.chars().count() <= LINE_CAP, "{line}");
            assert!(line.ends_with('…'), "a shortened line says so: {line}");
        }
        assert_eq!(out.lines().count(), 1 + MAX_CALLS + 1);
        // The byte cap on its own, where a caller passes more than it allows.
        let long = cap_bytes("x".repeat(PEEK_CAP * 2), PEEK_CAP);
        assert!(long.len() <= PEEK_CAP && long.ends_with('…'));
    }

    /// The file as a live launch-only child leaves it: codex's s6 stream recorded line by line,
    /// with the bookend marion writes first, read back through the row.
    #[test]
    fn a_peek_reads_the_frames_a_running_nodes_stream_recorded() {
        use crate::events::{EventSink, EventWriter};
        let dir = marion_testsupport::scratch("activity-file");
        let path = dir.join("events.jsonl");
        let agent = marion_core::contract::AgentId("019f-peek".into());
        {
            let s = EventSink::new(
                EventWriter::open_path(&path, &agent).unwrap(),
                Harness::Codex,
                "unused".into(),
            );
            s.lifecycle(marion_core::event::Lifecycle::Opened);
            for line in
                include_str!("../../../tests/fixtures/s6/exec-mcp-report.stream.jsonl").lines()
            {
                s.record_line(line);
            }
        }
        let out = peek(&path, Harness::Codex);
        assert_eq!(
            out,
            "Recent activity (oldest first):\n\
             - report({\"narrative\":\"s6 probe: reporting via MCP\"})\n- last said: done"
        );
    }

    /// **An ACP node is read by the protocol's `session/update` frames**, recorded live since the
    /// ACP child path gained its line seam: `opencode acp`'s s21 turn, its call's input taken from
    /// the `in_progress` update and its words joined from token-sized chunks.
    #[test]
    fn a_peek_reads_an_acp_nodes_session_updates() {
        use crate::events::{EventSink, EventWriter};
        let dir = marion_testsupport::scratch("activity-acp");
        let path = dir.join("events.jsonl");
        let agent = marion_core::contract::AgentId("019f-acp-peek".into());
        {
            let s = EventSink::new(
                EventWriter::open_path(&path, &agent).unwrap(),
                Harness::Acp,
                "unused".into(),
            );
            s.lifecycle(marion_core::event::Lifecycle::Opened);
            for line in
                include_str!("../../../tests/fixtures/s21/opencode-acp-session.jsonl").lines()
            {
                s.record_line(line);
            }
        }
        assert_eq!(
            peek(&path, Harness::Acp),
            "Recent activity (oldest first):\n\
             - marion_report({\"narrative\":\"hello from acp\"})\n- last said: Reported."
        );
    }

    #[test]
    fn a_node_with_no_file_has_said_nothing() {
        let dir = marion_testsupport::scratch("activity-peek");
        let missing = dir.join("events.jsonl");
        for h in [Harness::Acp, Harness::Codex] {
            let out = peek(&missing, h);
            assert!(out.contains("nothing recorded yet"), "{h:?}: {out}");
        }
    }
}
