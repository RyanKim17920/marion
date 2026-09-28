//! The gateway against marion's canned provider standing in for an OpenAI Chat Completions server,
//! driven over real sockets and a real `curl`.

use std::io::{Read, Write};
use std::net::TcpStream;

use marion_provider::reqlog::fingerprint;
use marion_provider::{CannedServer, Config, KeyRefusal, Script};
use marion_testsupport::scratch;
use serde_json::{Value, json};

use super::*;

const KEY: &str = "sk-gateway-test-key";
const MODEL: &str = "gateway-model-3";
const TOOL: &str = "mcp__marion__report";

fn canned(tag: &str, script: Script) -> (marion_testsupport::Scratch, CannedServer) {
    let dir = scratch(&format!("gateway-{tag}"));
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script,
    })
    .expect("the canned provider binds");
    (dir, server)
}

fn report_script() -> Script {
    Script {
        openai_report_tool: TOOL.into(),
        openai_report_args: json!({"narrative": "done through the gateway"}),
        openai_final_text: "Reported.".into(),
        ..Script::default()
    }
}

fn gateway_for(base_url: &str) -> Gateway {
    Gateway::start(
        Wire::AnthropicMessages,
        Upstream {
            provider: "canned-chat".into(),
            wire: Wire::OpenAiChat,
            base_url: base_url.into(),
            model: MODEL.into(),
            key: Some(Secret::new(KEY)),
            key_header: KeyHeader::Bearer,
        },
    )
    .expect("the gateway starts")
}

/// One request over a fresh connection; the status and the body, de-chunked.
fn post(gw: &Gateway, credential: Option<&str>, body: &Value) -> (u16, String) {
    let addr = gw.base_url().trim_start_matches("http://").to_string();
    let mut s = TcpStream::connect(&addr).unwrap();
    let body = body.to_string();
    let auth = credential
        .map(|c| format!("Authorization: Bearer {c}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(rest)
    } else {
        rest.to_string()
    };
    (status, body)
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    loop {
        let (size, rest) = s.split_once("\r\n").unwrap();
        let n = usize::from_str_radix(size, 16).unwrap();
        if n == 0 {
            return out;
        }
        out.push_str(&rest[..n]);
        s = &rest[n + 2..];
    }
}

fn anthropic_turn(stream: bool, history: Vec<Value>) -> Value {
    let mut messages = vec![json!({"role": "user", "content": "Report back through marion."})];
    messages.extend(history);
    json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 512,
        "stream": stream,
        "system": [{"type": "text", "text": "You are a marion node."}],
        "tools": [{"name": TOOL, "description": "report",
                   "input_schema": {"type": "object", "properties": {"narrative": {"type": "string"}}}}],
        "messages": messages,
    })
}

/// The events of an SSE body, as `(name, data)`.
fn events(sse: &str) -> Vec<(String, Value)> {
    sse.split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .map(|f| {
            let name = f
                .lines()
                .find_map(|l| l.strip_prefix("event: "))
                .unwrap()
                .to_string();
            let data = f.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
            (name, serde_json::from_str(data).unwrap())
        })
        .collect()
}

#[test]
fn every_translation_the_core_table_lists_is_one_the_gateway_performs_and_no_other() {
    for h in Wire::ALL {
        for p in Wire::ALL {
            assert_eq!(
                translates(h, p),
                marion_core::provider::translates(h, p),
                "{h} -> {p}"
            );
        }
    }
    let err = Gateway::start(
        Wire::OpenAiResponses,
        Upstream {
            provider: "x".into(),
            wire: Wire::OpenAiChat,
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "m".into(),
            key: None,
            key_header: KeyHeader::Bearer,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("does not translate"), "{err}");
}

#[test]
fn a_request_without_the_nodes_bearer_is_refused_and_never_reaches_the_provider() {
    let (_d, server) = canned("unauth", report_script());
    let gw = gateway_for(&server.base_url());
    for credential in [None, Some("marion-gw-not-it"), Some(KEY)] {
        let (status, body) = post(&gw, credential, &anthropic_turn(true, vec![]));
        assert_eq!(status, 401, "{credential:?}: {body}");
        assert!(body.contains("authentication_error"), "{body}");
    }
    assert!(server.requests().unwrap().is_empty());
}

/// **A streamed tool-calling turn, both ways**: Anthropic in, Chat Completions to the provider with
/// the stored key and the endpoint's model, the provider's streamed `tool_calls` back as one
/// `tool_use` block with its arguments rejoined, and — with the tool's result in the history — the
/// provider's final text back as text, with the usage carried.
#[test]
fn a_streamed_tool_calling_turn_is_translated_both_ways() {
    let (_d, server) = canned("stream", report_script());
    let gw = gateway_for(&server.base_url());
    let bearer = gw.bearer().expose().to_string();
    let (status, body) = post(&gw, Some(&bearer), &anthropic_turn(true, vec![]));
    assert_eq!(status, 200, "{body}");
    let evs = events(&body);
    assert_eq!(evs.first().unwrap().0, "message_start");
    assert_eq!(evs.last().unwrap().0, "message_stop");
    let start = evs
        .iter()
        .find(|(n, _)| n == "content_block_start")
        .unwrap();
    assert_eq!(start.1["content_block"]["type"], "tool_use");
    assert_eq!(start.1["content_block"]["name"], TOOL);
    let args: String = evs
        .iter()
        .filter_map(|(_, d)| d.pointer("/delta/partial_json").and_then(Value::as_str))
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&args).unwrap(),
        json!({"narrative": "done through the gateway"})
    );
    let delta = evs.iter().find(|(n, _)| n == "message_delta").unwrap();
    assert_eq!(delta.1["delta"]["stop_reason"], "tool_use");
    let usage = &delta.1["usage"];
    let u = marion_provider::USAGE;
    assert_eq!(usage["input_tokens"], u.input - u.cached);
    assert_eq!(usage["cache_read_input_tokens"], u.cached);
    assert_eq!(usage["output_tokens"], u.output);

    // The provider saw Chat Completions, the stored key, the endpoint's model.
    let reqs = server.requests().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["wire"], "openai");
    assert_eq!(reqs[0]["body"]["model"], MODEL);
    assert_eq!(reqs[0]["body"]["stream"], true);
    assert_eq!(
        reqs[0]["credentials"]["authorization"],
        fingerprint(&format!("Bearer {KEY}"))
    );
    assert!(
        !reqs[0].to_string().contains(&bearer),
        "the bearer stays with the harness"
    );

    // Turn two: the tool's result in the history; the provider finishes in text.
    let id = start.1["content_block"]["id"].as_str().unwrap();
    let history = vec![
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": TOOL,
               "input": {"narrative": "done through the gateway"}}]}),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id,
               "content": "reported"}]}),
    ];
    let (status, body) = post(&gw, Some(&bearer), &anthropic_turn(true, history));
    assert_eq!(status, 200, "{body}");
    let text: String = events(&body)
        .iter()
        .filter_map(|(_, d)| d.pointer("/delta/text").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    assert_eq!(text, "Reported.");
    let reqs = server.requests().unwrap();
    let m = reqs[1]["body"]["messages"].as_array().unwrap();
    assert_eq!(m[m.len() - 1]["role"], "tool");
    assert_eq!(m[m.len() - 1]["tool_call_id"], id);
}

#[test]
fn a_request_that_does_not_stream_gets_one_message() {
    let (_d, server) = canned("whole", report_script());
    let gw = gateway_for(&server.base_url());
    let bearer = gw.bearer().expose().to_string();
    let (status, body) = post(&gw, Some(&bearer), &anthropic_turn(false, vec![]));
    assert_eq!(status, 200, "{body}");
    let m: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(m["type"], "message");
    assert_eq!(m["content"][0]["type"], "tool_use");
    assert_eq!(
        m["content"][0]["input"]["narrative"],
        "done through the gateway"
    );
    assert_eq!(m["stop_reason"], "tool_use");
}

/// **A provider's refusal keeps its status**, in Anthropic's error vocabulary — what the harness's
/// retry logic and marion's failure classifier both key on — and never carries the key.
#[test]
fn a_rate_limited_key_answers_429_as_anthropics_rate_limit_error() {
    let (_d, server) = canned(
        "429",
        Script {
            refusals: vec![KeyRefusal {
                key: KEY.into(),
                status: 429,
            }],
            ..report_script()
        },
    );
    let gw = gateway_for(&server.base_url());
    let bearer = gw.bearer().expose().to_string();
    let (status, body) = post(&gw, Some(&bearer), &anthropic_turn(true, vec![]));
    assert_eq!(status, 429, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"]["type"], "rate_limit_error");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("429"),
        "{v}"
    );
    assert!(!body.contains(KEY));
}

#[test]
fn an_unreachable_provider_is_a_502_naming_it() {
    // Port 9 (discard) is closed on a test machine: curl's connection is refused.
    let gw = gateway_for("http://127.0.0.1:9/v1");
    let bearer = gw.bearer().expose().to_string();
    let (status, body) = post(&gw, Some(&bearer), &anthropic_turn(true, vec![]));
    assert_eq!(status, 502, "{body}");
    assert!(
        body.contains("canned-chat") && body.contains("api_error"),
        "{body}"
    );
    assert!(!body.contains(KEY));
}

/// **The gateway ends with its handle**: when the drop returns, every thread it ran — the accept
/// loop and each connection, one of them mid-request here — has let go of its state, so nothing of
/// it is left running. (The port itself is not asserted: a concurrent test may be handed it.)
#[test]
fn a_dropped_gateway_is_gone() {
    let before = started();
    let gw = gateway_for("http://127.0.0.1:9/v1");
    assert!(started() > before);
    assert!(live() >= 1);
    let addr = gw.base_url().trim_start_matches("http://").to_string();
    // A connection that has sent half a request: its thread is blocked reading when the drop comes.
    let mut half = TcpStream::connect(&addr).expect("it listens while held");
    half.write_all(b"POST /v1/messages HTTP/1.1\r\n").unwrap();
    let state = Arc::downgrade(&gw.shared);
    drop(gw);
    assert!(
        state.upgrade().is_none(),
        "a thread of the dropped gateway still holds its state"
    );
    let mut rest = Vec::new();
    let _ = half.read_to_end(&mut rest);
}

#[test]
fn the_bearer_is_random_per_gateway_and_redacted_in_debug() {
    let a = gateway_for("http://127.0.0.1:9/v1");
    let b = gateway_for("http://127.0.0.1:9/v1");
    assert_ne!(a.bearer(), b.bearer());
    assert!(a.bearer().expose().len() > 64);
    let printed = format!("{a:?} {:?}", a.bearer());
    assert!(!printed.contains(a.bearer().expose()), "{printed}");
    assert!(a.base_url().starts_with("http://127.0.0.1:"));
}
