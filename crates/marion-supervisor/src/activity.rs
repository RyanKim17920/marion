//! **What a running node has been doing**, as a few bounded lines: its last tool calls and the
//! last line of text it wrote.
//!
//! A parent deciding whether to `steer` a child, and an operator deciding whether to press `s`,
//! both need more than a state word. The node's own `events.jsonl` already carries every frame its
//! harness emitted, verbatim; this reads the tail of that file and hands the frames to
//! [`marion_harness::grammar::recent_activity`] under the row's `activity` rule — the same
//! row-driven reader everything else about a stream uses. **No harness is named here**: a row with
//! no rule (ACP, whose stream is read as code) is said to be unread, never shown as an empty peek,
//! because "did nothing" and "marion cannot tell" call for opposite next moves.
//!
//! Bounded three ways, so a chatty child cannot flood its parent's context: at most
//! [`MAX_CALLS`] calls, each line at most [`LINE_CAP`] characters, and the whole at most
//! [`PEEK_CAP`] bytes. The file is read from its last [`TAIL_BYTES`] only, so a long run costs a
//! peek no more than a short one.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use marion_core::event::{EventLog, Payload};
use marion_core::harness::Harness;
use marion_core::proto::params::ActivityCursor;
use marion_core::proto::result::{ActionKind, ActionLine, ActivityPage};
use marion_harness::adapter::harness_spec;
use marion_harness::grammar::{Activity, RecentActivity, activity_stream, recent_activity};
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
    let Some(rule) = harness_spec(harness)
        .stream
        .and_then(|g| g.activity.as_ref())
    else {
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
        out.push('\n');
        out.push_str(&capped(&format!("- {}", call_line(c))));
    }
    if let Some(t) = &a.text {
        out.push('\n');
        out.push_str(&capped(&format!("- last said: {}", one_line(t))));
    }
    cap_bytes(out, PEEK_CAP)
}

/// The most bytes one [`page`] reads: a first poll of a long run gets the end of it, and each
/// later poll only what was appended since.
pub const PAGE_BYTES: u64 = TAIL_BYTES;

/// A page of what the node whose stream is `events` has been doing, from `cursor`: every call and
/// message in that stretch of the file, one bounded line each, and the byte the next page starts
/// at. Only whole lines are consumed, so a record being written as this reads is left for the next
/// poll rather than read torn. A cursor past the end of the file (a file that was replaced) reads
/// from the start again.
pub fn page(events: &Path, harness: Harness, cursor: ActivityCursor) -> ActivityPage {
    let Some(rule) = harness_spec(harness)
        .stream
        .and_then(|g| g.activity.as_ref())
    else {
        return ActivityPage {
            unread: Some(format!(
                "marion reads no {harness:?} stream by a row, so it cannot say which tools this \
                 node called."
            )),
            ..ActivityPage::default()
        };
    };
    let Ok(mut file) = std::fs::File::open(events) else {
        return ActivityPage::default();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let (mut start, aligned) = match cursor {
        ActivityCursor::Tail => (len.saturating_sub(PAGE_BYTES), len <= PAGE_BYTES),
        ActivityCursor::From(n) if n <= len => (n, true),
        ActivityCursor::From(_) => (0, true),
    };
    let end = len.min(start.saturating_add(PAGE_BYTES));
    let mut bytes = vec![0; (end - start) as usize];
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_exact(&mut bytes).is_err() {
        return ActivityPage {
            from: start,
            next: start,
            ..ActivityPage::default()
        };
    }
    // A tail that starts mid-file starts mid-record: skip to the first whole line.
    if !aligned {
        match bytes.iter().position(|b| *b == b'\n') {
            Some(nl) => {
                bytes.drain(..=nl);
                start += nl as u64 + 1;
            }
            None => {
                return ActivityPage {
                    from: start,
                    next: start,
                    ..ActivityPage::default()
                };
            }
        }
    }
    // Whole lines only: the rest is a record still being written.
    let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    bytes.truncate(whole);
    let mut events_read = Vec::new();
    EventLog::default().extend(&bytes, &mut events_read);
    let (frames, times): (Vec<Value>, Vec<String>) = events_read
        .into_iter()
        .filter_map(|e| match e.payload {
            Payload::Vendor { json, .. } => Some((json, rfc3339(e.ts))),
            _ => None,
        })
        .unzip();
    let lines = activity_stream(rule, &frames)
        .into_iter()
        .map(|i| {
            let (kind, text) = match i.item {
                Activity::Call(c) => (ActionKind::Call, call_line(&c)),
                Activity::Said(t) => (ActionKind::Said, capped(&one_line(&t))),
            };
            ActionLine {
                at: times[i.frame].clone(),
                kind,
                text,
            }
        })
        .collect();
    ActivityPage {
        from: start,
        next: start + whole as u64,
        lines,
        unread: None,
    }
}

/// One call as one bounded line: `name(args)`.
fn call_line(c: &marion_harness::grammar::ToolCall) -> String {
    let args = match &c.args {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    capped(&format!("{}({})", c.name, one_line(&args)))
}

/// A journal or event timestamp as the RFC3339 text it serializes to.
pub(crate) fn rfc3339(ts: marion_core::encoding::SystemTime) -> String {
    serde_json::to_value(ts)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Every harness frame in the last [`TAIL_BYTES`] of `path`, in order. A missing or unreadable
/// file is no frames: a peek is a view and never a reason to fail the answer it rides on.
fn tail_frames(path: &Path) -> Vec<Value> {
    frames_within(path, TAIL_BYTES)
}

/// Every harness frame in `path`, in order: for a reading that must see the whole run, such as a
/// usage fold that sums per-turn counters. Missing or unreadable is no frames, as for a peek.
pub fn all_frames(path: &Path) -> Vec<Value> {
    frames_within(path, u64::MAX)
}

/// The harness frames in the last `limit` bytes of `path`.
fn frames_within(path: &Path, limit: u64) -> Vec<Value> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(limit);
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

    /// A watcher polls with `next` and reads each call once: a second page after more was recorded
    /// holds only the new items, a record still being written is left for the next poll, and a
    /// cursor past a replaced file starts over.
    #[test]
    fn pages_read_each_byte_once_and_never_a_torn_record() {
        use crate::events::{EventSink, EventWriter};
        use std::io::Write;
        let dir = marion_testsupport::scratch("activity-pages");
        let path = dir.join("events.jsonl");
        let agent = marion_core::contract::AgentId("019f-pages".into());
        let stream = include_str!("../../../tests/fixtures/s6/exec-mcp-report.stream.jsonl");
        let lines: Vec<&str> = stream.lines().collect();
        let sink = EventSink::new(
            EventWriter::open_path(&path, &agent).unwrap(),
            Harness::Codex,
            "unused".into(),
        );
        sink.lifecycle(marion_core::event::Lifecycle::Opened);
        let half = lines.len() / 2;
        for line in &lines[..half] {
            sink.record_line(line);
        }
        let first = page(&path, Harness::Codex, ActivityCursor::Tail);
        assert_eq!(first.from, 0);
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(first.next, len, "a whole file is consumed whole");
        for line in &lines[half..] {
            sink.record_line(line);
        }
        let second = page(&path, Harness::Codex, ActivityCursor::From(first.next));
        assert_eq!(second.from, first.next);
        let everything = page(&path, Harness::Codex, ActivityCursor::From(0));
        assert_eq!(
            first.lines.len() + second.lines.len(),
            everything.lines.len(),
            "two pages are the whole, with nothing read twice: {first:?} {second:?}"
        );
        assert!(
            everything
                .lines
                .iter()
                .any(|l| l.kind == ActionKind::Call && l.text.starts_with("report(")),
            "{everything:?}"
        );

        // Half a record on the end: not consumed, so the next poll reads it whole.
        let end = std::fs::metadata(&path).unwrap().len();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"agent_id\":")
            .unwrap();
        let torn = page(&path, Harness::Codex, ActivityCursor::From(end));
        assert_eq!((torn.next, torn.lines.len()), (end, 0), "{torn:?}");

        let replaced = page(&path, Harness::Codex, ActivityCursor::From(u64::MAX));
        assert_eq!(replaced.from, 0);
    }

    #[test]
    fn a_node_with_no_row_is_unread_and_a_node_with_no_file_has_said_nothing() {
        let dir = marion_testsupport::scratch("activity-peek");
        let missing = dir.join("events.jsonl");
        let acp = peek(&missing, Harness::Acp);
        assert!(acp.contains("not shown"), "{acp}");
        let acp_page = page(&missing, Harness::Acp, ActivityCursor::Tail);
        assert!(acp_page.unread.is_some(), "{acp_page:?}");
        let codex_page = page(&missing, Harness::Codex, ActivityCursor::Tail);
        assert_eq!(codex_page, ActivityPage::default());
        let codex = peek(&missing, Harness::Codex);
        assert!(codex.contains("nothing recorded yet"), "{codex}");
    }
}
