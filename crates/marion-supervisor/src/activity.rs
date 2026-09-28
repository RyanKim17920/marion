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
use marion_core::proto::params::ActivityCursor;
use marion_core::proto::result::{ActionKind, ActionLine, ActivityPage};
use marion_harness::adapter::adapter_for;
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
    // The adapter's reading, as the peek asks it: a row's grammar, or ACP's protocol frames.
    let Some(rule) = adapter_for(harness).ok().and_then(|a| a.activity()) else {
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
                Activity::Call(c) => (ActionKind::Call, brief(&c)),
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

/// Argument keys that name a shell command, most telling first.
const COMMAND_KEYS: &[&str] = &["command", "cmd", "script"];
/// Argument keys that name a file or a place.
const PATH_KEYS: &[&str] = &[
    "file_path",
    "path",
    "file",
    "filename",
    "notebook_path",
    "target_file",
    "absolute_path",
    "dir_path",
    "directory",
];
/// Argument keys that say what is searched for or fetched.
const PATTERN_KEYS: &[&str] = &["pattern", "query", "regex", "glob", "url"];
/// Argument keys that carry words the agent wrote: quoted, as speech.
const MESSAGE_KEYS: &[&str] = &[
    "narrative",
    "message",
    "text",
    "prompt",
    "description",
    "summary",
    "content",
];

/// One call as a person reads it, one bounded line: `$ cargo test` for a command, `~ src/a.rs`
/// for a file change, and otherwise the tool's verb and its most telling argument —
/// `report "tests pass"`, `Read src/lib.rs`, `Grep fn main`. Worded from the row's
/// [`CallShape`] and the argument's key, so no harness is named here.
pub fn brief(c: &marion_harness::grammar::ToolCall) -> String {
    use marion_harness::grammar::CallShape;
    // Some harnesses deliver the argument object as a JSON string.
    let parsed;
    let args = match &c.args {
        Value::String(s) if s.trim_start().starts_with('{') => {
            parsed = serde_json::from_str::<Value>(s).unwrap_or_else(|_| c.args.clone());
            &parsed
        }
        other => other,
    };
    let line = match c.shape {
        CallShape::Command => format!("$ {}", command_text(args)),
        CallShape::Files => files_text(args),
        CallShape::Tool => tool_text(verb(&c.name), args),
    };
    capped(&one_line(&line))
}

/// The tool's own name without an MCP server prefix (`mcp__marion__report` is `report`).
fn verb(name: &str) -> &str {
    name.rsplit("__").next().unwrap_or(name)
}

fn tool_text(verb: &str, args: &Value) -> String {
    let Value::Object(map) = args else {
        return match args {
            Value::String(s) if !s.trim().is_empty() => format!("{verb} {s}"),
            _ => verb.to_string(),
        };
    };
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| map.get(*k).map(string_of).filter(|s| !s.trim().is_empty()))
    };
    if let Some(cmd) = pick(COMMAND_KEYS) {
        return format!("$ {}", unwrap_shell(&cmd));
    }
    if let Some(p) = pick(PATTERN_KEYS).or_else(|| pick(PATH_KEYS)) {
        return format!("{verb} {p}");
    }
    if let Some(m) = pick(MESSAGE_KEYS) {
        return format!("{verb} \"{}\"", one_line(&m));
    }
    match map
        .values()
        .find_map(|v| v.as_str().filter(|s| !s.trim().is_empty()))
    {
        Some(first) => format!("{verb} {first}"),
        None => verb.to_string(),
    }
}

/// A string as itself, an array of strings as words, anything else as nothing.
fn string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// The command a `Command`-shaped call ran, a shell's `-c` wrapper taken off.
fn command_text(args: &Value) -> String {
    match args {
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match words.as_slice() {
                [sh, flag, script] if is_shell(sh) && is_dash_c(flag) => script.to_string(),
                _ => words.join(" "),
            }
        }
        other => unwrap_shell(&string_of(other)),
    }
}

/// `bash -lc 'cargo test'` as `cargo test`: the wrapper says nothing about what ran.
fn unwrap_shell(cmd: &str) -> String {
    let mut it = cmd.trim().splitn(3, ' ');
    match (it.next(), it.next(), it.next()) {
        (Some(sh), Some(flag), Some(rest)) if is_shell(sh) && is_dash_c(flag) => {
            let rest = rest.trim();
            for q in ['\'', '"'] {
                if let Some(inner) = rest.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
                    return inner.to_string();
                }
            }
            rest.to_string()
        }
        _ => cmd.trim().to_string(),
    }
}

fn is_shell(word: &str) -> bool {
    let name = word.rsplit('/').next().unwrap_or(word);
    matches!(name, "sh" | "bash" | "zsh" | "dash" | "fish")
}

fn is_dash_c(flag: &str) -> bool {
    flag.starts_with('-') && !flag.starts_with("--") && flag.ends_with('c')
}

/// The files a `Files`-shaped call changed: the first, and how many more.
fn files_text(args: &Value) -> String {
    let paths: Vec<String> = match args {
        Value::Array(items) => items
            .iter()
            .filter_map(|i| match i {
                Value::String(s) => Some(s.clone()),
                Value::Object(m) => m.get("path").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect(),
        Value::Object(m) => m.keys().cloned().collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    };
    match paths.as_slice() {
        [] => "~ files".to_string(),
        [one] => format!("~ {one}"),
        [first, rest @ ..] => format!("~ {first} +{} more", rest.len()),
    }
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
    use marion_harness::grammar::{CallShape, ToolCall};

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            name: name.into(),
            args,
            shape: CallShape::Tool,
        }
    }

    fn shaped(shape: CallShape, name: &str, args: Value) -> ToolCall {
        ToolCall {
            shape,
            ..call(name, args)
        }
    }

    /// A call as a person reads it: the verb and the one argument that says what it is about,
    /// never the argument object as JSON. Worded from the row's shape and the argument's key, so
    /// no harness is named.
    #[test]
    fn a_call_reads_as_its_verb_and_most_telling_argument() {
        use serde_json::json;
        let cases = [
            (
                call(
                    "report",
                    json!({"narrative": "s6 probe: reporting via MCP"}),
                ),
                "report \"s6 probe: reporting via MCP\"",
            ),
            (
                call("mcp__marion__report", json!({"narrative": "done"})),
                "report \"done\"",
            ),
            (
                call("Bash", json!({"command": "cargo test", "timeout": 5})),
                "$ cargo test",
            ),
            (
                shaped(
                    CallShape::Command,
                    "command_execution",
                    json!("/bin/zsh -lc 'cargo test -q'"),
                ),
                "$ cargo test -q",
            ),
            (
                shaped(
                    CallShape::Command,
                    "command_execution",
                    json!(["bash", "-lc", "ls src"]),
                ),
                "$ ls src",
            ),
            (
                shaped(
                    CallShape::Files,
                    "file_change",
                    json!([{"path": "src/limits/bucket.rs", "kind": "update"}, {"path": "b.rs"}]),
                ),
                "~ src/limits/bucket.rs +1 more",
            ),
            (
                shaped(CallShape::Files, "file_change", json!([{"path": "a.rs"}])),
                "~ a.rs",
            ),
            (
                call("Read", json!({"file_path": "src/lib.rs", "limit": 40})),
                "Read src/lib.rs",
            ),
            (
                call("Grep", json!({"pattern": "fn main", "path": "src"})),
                "Grep fn main",
            ),
            (
                call("todo", json!({"count": 3, "label": "tidy"})),
                "todo tidy",
            ),
            (call("ping", json!({})), "ping"),
            (call("ping", Value::Null), "ping"),
            // Arguments some harnesses deliver as a JSON string are read as the object they are.
            (call("edit", json!("{\"path\":\"x.rs\"}")), "edit x.rs"),
            (
                call("say", json!("line one\nline two")),
                "say line one line two",
            ),
        ];
        for (c, want) in cases {
            assert_eq!(brief(&c), want, "{c:?}");
        }
        let long = brief(&call("say", json!({"message": "x".repeat(500)})));
        assert!(
            long.chars().count() <= LINE_CAP && long.ends_with('…'),
            "{long}"
        );
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

    /// The file as a live codex node leaves it: its app-server stream (S36 P4) recorded line by
    /// line, with the bookend marion writes first, read back through the row.
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
            for line in marion_testsupport::app_server_capture("p4-items.jsonl").lines() {
                s.record_line(line);
            }
        }
        let out = peek(&path, Harness::Codex);
        // P4's four items in order — two `report`s (direct, then from code-mode JS), the shell
        // command, the patch — and the completed agent message.
        assert_eq!(
            out,
            "Recent activity (oldest first):\n\
             - report({\"narrative\":\"p4 report\"})\n\
             - commandExecution(/bin/zsh -lc 'echo hello-p4')\n\
             - fileChange([{\"diff\":\"p4\\n\",\"kind\":{\"type\":\"add\"},\"path\":\"<SCRATCH>/runs/p4/repo/src/p4.txt\"}])\n\
             - report({\"narrative\":\"p4 via js\"})\n\
             - last said: p4 final message"
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
        let stream = marion_testsupport::app_server_capture("p4-items.jsonl");
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
                .any(|l| l.kind == ActionKind::Call && l.text.starts_with("report ")),
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
            assert_eq!(
                page(&missing, h, ActivityCursor::Tail),
                ActivityPage::default(),
                "{h:?}"
            );
        }
    }
}
