//! Every harness's token usage, read through the one public seam a supervisor would call —
//! `adapter_for_type(..).usage(frames)` — over the streams the harnesses were measured emitting.
//!
//! The expected values are transcribed from the committed fixtures, not computed from them: a
//! reader that followed the wrong pointer would compute the same wrong number the test did.

use marion_core::{Harness, TokenUsage};
use marion_harness::adapter::{adapter_for, adapter_for_type};
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

fn tokens(input: u64, output: u64, cache_read: u64, cache_write: u64) -> TokenUsage {
    TokenUsage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

fn usage_of(h: Harness, acp_agent: Option<&str>, stdout: &str) -> Option<TokenUsage> {
    adapter_for_type(h, acp_agent)
        .expect("a registered adapter")
        .usage(&json_frames(stdout))
}

/// `gemini -o stream-json`'s terminal frame, verbatim from `s12/README.md`. **Unmeasured beyond
/// that one line**: its `cached` is zero, so whether `input_tokens` counts cached tokens is not
/// shown by any capture; the row assumes it does.
const GEMINI_RESULT: &str = r#"{"type":"result","timestamp":"<TS>","status":"success","stats":{"total_tokens":16,"input_tokens":10,"output_tokens":6,"cached":0,"input":10,"duration_ms":47,"tool_calls":1,"models":{"<REDACTED-machine-specific>":{}}}}"#;

/// `opencode run --format json`'s `step_finish`, verbatim from `s13/README.md`.
const OPENCODE_STEP_FINISH: &str = r#"{"type":"step_finish","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"reason":"stop","type":"step-finish","tokens":{"input":0,"output":0,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0}}"#;

#[test]
fn each_harness_reads_its_usage_from_the_stream_it_was_measured_emitting() {
    let cases: &[(&str, Harness, &str, Option<TokenUsage>)] = &[
        // `turn.completed` counts cache reads inside `input_tokens`: 14997 of which 11008 cached.
        (
            "codex s4",
            Harness::Codex,
            fixture!("s4/codex/stream-none.jsonl"),
            Some(tokens(3989, 5, 11008, 0)),
        ),
        (
            "claude-code s4",
            Harness::ClaudeCode,
            fixture!("s4/claude-code/stream-none.jsonl"),
            Some(tokens(13375, 21, 0, 0)),
        ),
        // A canned provider reports zeros; that is a report of zero, not an absence.
        (
            "qwen s25",
            Harness::Qwen,
            fixture!("s25/qwen-write-then-report.stdout.jsonl"),
            Some(TokenUsage::default()),
        ),
        (
            "goose s26",
            Harness::Goose,
            fixture!("s26/goose-report.stdout.jsonl"),
            Some(tokens(20, 10, 0, 0)),
        ),
        (
            "cline s27",
            Harness::Cline,
            fixture!("s27/cline-report-ok.stdout.jsonl"),
            Some(TokenUsage::default()),
        ),
        // copilot's `result.usage` counts premium requests and durations, never tokens.
        (
            "copilot s24",
            Harness::Copilot,
            fixture!("s24/copilot-write-then-report.stdout.jsonl"),
            None,
        ),
        (
            "gemini s12",
            Harness::Gemini,
            GEMINI_RESULT,
            Some(tokens(10, 6, 0, 0)),
        ),
        (
            "opencode s13",
            Harness::OpenCode,
            OPENCODE_STEP_FINISH,
            Some(TokenUsage::default()),
        ),
    ];
    for (name, h, stdout, want) in cases {
        assert_eq!(usage_of(*h, None, stdout), *want, "{name}");
    }
}

#[test]
fn a_stream_that_never_reached_its_usage_frame_reports_no_usage() {
    // The first frame of each measured stream alone: the run was killed before it said anything
    // about spend, and zero would be a claim it did not make.
    for h in Harness::ALL.into_iter().filter(|h| *h != Harness::Acp) {
        let init = r#"{"type":"system","subtype":"init","session_id":"s"}"#;
        assert_eq!(usage_of(h, None, init), None, "{h}");
    }
}

#[test]
fn per_step_harnesses_sum_their_steps_and_whole_run_harnesses_take_the_last_total() {
    // opencode emits one `step_finish` per model step, in the README's shape.
    let steps = [
        r#"{"type":"step_finish","sessionID":"s","part":{"tokens":{"input":100,"output":7,"reasoning":0,"cache":{"write":50,"read":0}}}}"#,
        r#"{"type":"step_finish","sessionID":"s","part":{"tokens":{"input":20,"output":3,"reasoning":0,"cache":{"write":0,"read":150}}}}"#,
    ]
    .join("\n");
    assert_eq!(
        usage_of(Harness::OpenCode, None, &steps),
        Some(tokens(120, 10, 150, 50))
    );
    // goose's `complete` is a whole-run total: a second one supersedes the first.
    let completes = [
        r#"{"type":"complete","total_tokens":30,"input_tokens":20,"output_tokens":10,"cache_read_input_tokens":0,"cache_write_input_tokens":0}"#,
        r#"{"type":"complete","total_tokens":60,"input_tokens":40,"output_tokens":20,"cache_read_input_tokens":0,"cache_write_input_tokens":0}"#,
    ]
    .join("\n");
    assert_eq!(
        usage_of(Harness::Goose, None, &completes),
        Some(tokens(40, 20, 0, 0))
    );
}

/// The harnesses whose stream grammar reads no usage, each with why. A row leaves this list by
/// gaining a rule, never by being deleted from it silently.
const NO_USAGE: &[(Harness, &str)] = &[(
    Harness::Copilot,
    "`-p --output-format json`'s result.usage carries premium requests and durations, no tokens (s24)",
)];

#[test]
fn every_stream_grammar_states_where_its_usage_is_or_why_it_has_none() {
    for h in Harness::ALL {
        let adapter = adapter_for(h).expect("a registered adapter");
        let Some(g) = adapter.spec().stream else {
            continue;
        };
        let exempt = NO_USAGE.iter().find(|(x, _)| *x == h);
        match (&g.usage, exempt) {
            (Some(_), None) => {}
            (None, Some((_, why))) => assert!(!why.trim().is_empty(), "{h}"),
            (Some(_), Some(_)) => panic!("{h} reads usage and is still listed as having none"),
            (None, None) => panic!("{h} has a stream grammar that states nothing about usage"),
        }
    }
}

#[test]
fn every_acp_agent_reads_its_usage_from_the_protocols_prompt_response() {
    // ACP's `session/prompt` response carries `result.usage` in the protocol's own shape, the same
    // for every agent; each transcript's `totalTokens` is the four counters' sum, so `inputTokens`
    // excludes the cache. The `usage_update` notifications around it are context occupancy, not
    // spend, and must not be read.
    let cases: &[(&str, Option<&str>, &str, TokenUsage)] = &[
        (
            "opencode acp s21",
            Some("opencode"),
            fixture!("s21/opencode-acp-session.jsonl"),
            tokens(113, 4, 14464, 0),
        ),
        (
            "claude-agent-acp s22",
            Some("claude-acp"),
            fixture!("s22/claude-agent-acp-session.jsonl"),
            tokens(6, 139, 82749, 19170),
        ),
        (
            "codex-acp s22",
            Some("codex-acp"),
            fixture!("s22/codex-acp-session.jsonl"),
            tokens(905, 4, 28416, 0),
        ),
        (
            "opencode acp canned s23",
            Some("opencode"),
            fixture!("s23/opencode-acp-canned-session.jsonl"),
            TokenUsage::default(),
        ),
        // An agent marion has no refinement row for is read by the protocol alone.
        (
            "generic agent over s22",
            Some("some-agent --acp"),
            fixture!("s22/claude-agent-acp-session.jsonl"),
            tokens(6, 139, 82749, 19170),
        ),
    ];
    for (name, agent, stdout, want) in cases {
        assert_eq!(usage_of(Harness::Acp, *agent, stdout), Some(*want), "{name}");
    }
    // The protocol row, bound to no agent, reads the same shape.
    let unbound = adapter_for(Harness::Acp).expect("the protocol row");
    assert_eq!(
        unbound.usage(&json_frames(fixture!("s21/opencode-acp-session.jsonl"))),
        Some(tokens(113, 4, 14464, 0))
    );
}

#[test]
fn an_acp_session_sums_its_turns_and_ignores_context_occupancy() {
    let turns = [
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"usage_update","used":23739,"size":258400}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn","usage":{"inputTokens":10,"outputTokens":1,"cachedReadTokens":100,"totalTokens":111}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","usage":{"inputTokens":20,"outputTokens":2,"cachedWriteTokens":5,"totalTokens":27}}}"#,
    ]
    .join("\n");
    assert_eq!(
        usage_of(Harness::Acp, Some("opencode"), &turns),
        Some(tokens(30, 3, 100, 5))
    );
    // A session that only ever reported occupancy, and a prompt response with no usage at all,
    // made no claim about spend.
    let silent = [
        r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"usage_update","used":23739,"size":258400}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}"#,
    ]
    .join("\n");
    assert_eq!(usage_of(Harness::Acp, Some("opencode"), &silent), None);
}
