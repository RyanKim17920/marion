//! Every harness's recent activity — its last tool calls and its last line of text — read through
//! its row's `StreamGrammar::activity` over the streams the harnesses were measured emitting.
//!
//! The expected names and lines are transcribed from the committed fixtures, not computed from
//! them: a reader that followed the wrong pointer would compute the same wrong answer the test did.

use marion_core::Harness;
use marion_harness::adapter::harness_spec;
use marion_harness::grammar::{RecentActivity, recent_activity};
use marion_harness::json_frames;

macro_rules! fixture {
    ($path:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/",
            $path
        ))
    };
}

/// `gemini -o stream-json`, the tool and message lines verbatim from `s12/README.md`.
const GEMINI: &str = r#"{"type":"message","timestamp":"<TS>","role":"user","content":"call the report tool"}
{"type":"tool_use","timestamp":"<TS>","tool_name":"mcp_marion_report","tool_id":"mcp_marion_report__mcp_marion_report_<n>_0","parameters":{"text":"hi"}}
{"type":"tool_result","timestamp":"<TS>","tool_id":"<TOOL-ID-1>","status":"success","output":"MARION_REPORT_OK"}
{"type":"message","timestamp":"<TS>","role":"assistant","content":"DONE_AFTER_TOOL","delta":true}"#;

/// `opencode run --format json`, the tool and text lines verbatim from `s13/README.md`.
const OPENCODE: &str = r#"{"type":"tool_use","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"type":"tool","tool":"marionmcp_report","callID":"call_1","state":{"status":"completed","input":{"text":"hello-from-marion"},"output":"MCP_CALLED {\"text\": \"hello-from-marion\"}","metadata":{"truncated":false},"title":"","time":{"<REDACTED-timestamps>":0}},"id":"<PART-1>","sessionID":"<SESSION-1>","messageID":"<MESSAGE-1>"}}
{"type":"text","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"id":"<PART-2>","messageID":"<MESSAGE-1>","type":"text","text":"CANNED_OK","time":{"start":"<TS>","end":"<TS>"}}}"#;

fn activity_of(h: Harness, stdout: &str) -> Option<RecentActivity> {
    let rule = harness_spec(h).stream?.activity.as_ref()?;
    Some(recent_activity(rule, &json_frames(stdout), 5))
}

fn names(a: &RecentActivity) -> Vec<&str> {
    a.calls.iter().map(|c| c.name.as_str()).collect()
}

#[test]
fn each_harness_reads_its_calls_and_words_from_the_stream_it_was_measured_emitting() {
    let cases: &[(&str, Harness, &str, &[&str], &str)] = &[
        // A patch item, `item.started` then `item.completed` under one id: one call, not two.
        (
            "codex s6 patch",
            Harness::Codex,
            fixture!("s6/exec-codemode-apply-patch.stream.jsonl"),
            &["file_change"],
            "patched",
        ),
        (
            "codex s6 mcp",
            Harness::Codex,
            fixture!("s6/exec-mcp-report.stream.jsonl"),
            &["report"],
            "done",
        ),
        (
            "claude-code s9",
            Harness::ClaudeCode,
            fixture!("s9/can-use-tool-allow.stdout.jsonl"),
            &["mcp__marion__report"],
            "s9: the turn continued after the permission was answered.",
        ),
        // A built-in tool beside marion's: the peek shows every tool, not only marion's verbs.
        (
            "qwen s25",
            Harness::Qwen,
            fixture!("s25/qwen-write-then-report.stdout.jsonl"),
            &["write_file", "mcp__marion__report"],
            "S25-OK",
        ),
        (
            "copilot s24",
            Harness::Copilot,
            fixture!("s24/copilot-write-then-report.stdout.jsonl"),
            &["create", "marion-report"],
            "Reported back through marion. Done.",
        ),
        (
            "goose s26",
            Harness::Goose,
            fixture!("s26/goose-report.stdout.jsonl"),
            &["marion__report"],
            "S26-OK",
        ),
        (
            "cline s27",
            Harness::Cline,
            fixture!("s27/cline-report-ok.stdout.jsonl"),
            &["marion__report"],
            "S27-OK",
        ),
        (
            "gemini s12",
            Harness::Gemini,
            GEMINI,
            &["mcp_marion_report"],
            "DONE_AFTER_TOOL",
        ),
        (
            "opencode s13",
            Harness::OpenCode,
            OPENCODE,
            &["marionmcp_report"],
            "CANNED_OK",
        ),
    ];
    for (what, h, stdout, calls, text) in cases {
        let a = activity_of(*h, stdout).unwrap_or_else(|| panic!("{what}: the row has no rule"));
        assert_eq!(names(&a), *calls, "{what}: {a:?}");
        assert_eq!(a.text.as_deref(), Some(*text), "{what}: {a:?}");
    }
}

/// A call's arguments come back whole — the renderer shortens, the reader does not — and a shell
/// command's argument is the command line itself.
#[test]
fn a_calls_arguments_are_what_the_harness_gave_the_tool() {
    let a = activity_of(Harness::Codex, fixture!("s6/exec-mcp-report.stream.jsonl")).unwrap();
    assert_eq!(
        a.calls[0].args,
        serde_json::json!({"narrative": "s6 probe: reporting via MCP"})
    );
    // `s7`'s shell commands: three item frames under two ids are two calls, each its command line.
    let a = activity_of(Harness::Codex, fixture!("s7/exec-spawn-child.stream.jsonl")).unwrap();
    assert_eq!(
        names(&a),
        ["command_execution", "command_execution"],
        "{a:?}"
    );
    for (c, script) in a.calls.iter().zip(["spawn.sh", "runaway.sh"]) {
        let line = c.args.as_str().expect("a command is its command line");
        assert!(line.contains(script), "{line}");
    }
}

/// ACP's stream is read as code, per agent, not by a row: there is no activity rule, and a caller
/// says so rather than showing an empty peek as if the node had done nothing.
#[test]
fn an_acp_node_has_no_row_to_read_its_activity_by() {
    assert!(activity_of(Harness::Acp, "").is_none());
}
