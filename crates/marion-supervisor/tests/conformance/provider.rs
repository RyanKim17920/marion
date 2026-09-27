//! The probe's provider: marion's `CannedServer` with a [`Hold`] in front that scripts each probe's
//! turns itself — S36's hold/script proxy, moved in-process so a hold, a release and a fault are
//! events rather than sleeps.
//!
//! **A script is keyed by a marker in the prompt**, as `NodeScript` is: the request's *last* marker
//! picks the script, and the script's step is the first of its call ids not yet in the body. That
//! keeps every answer a function of the request, so a harness that retries, sends a title request
//! or folds a mid-turn message is answered consistently without the probe counting anything. A
//! request carrying no marker (a harness's own side request) gets a one-word text turn.
//!
//! On top of the script the hold can **park** one step (the mid-turn, interrupt and lifecycle
//! probes), **fault** every request with a status (the error probe) and **count tokens** into
//! every answer (the usage probe — canned itself reports zeros).

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use marion_provider::{
    Answer, CannedServer, Config, Hold, Script, anthropic, gemini, openai, responses,
};
use serde_json::{Value, json};

use crate::report::Log;

/// One scripted call: marion's verb, spelled for the wire when the request arrives.
#[derive(Debug, Clone)]
pub struct Call {
    /// The model-facing name on every wire but Responses.
    pub spelled: String,
    /// The bare verb: the Responses dispatch form puts the server in `namespace` (S6, §11 item 12),
    /// a fact about the wire rather than about any harness.
    pub verb: String,
    pub args: Value,
}

/// One turn's script: calls in order, then a closing text.
#[derive(Debug, Clone)]
pub struct Turn {
    pub marker: String,
    pub calls: Vec<Call>,
    pub final_text: String,
}

impl Turn {
    fn call_id(&self, i: usize, wire: &str) -> String {
        let m = self.marker.to_ascii_lowercase();
        match wire {
            "anthropic" => format!("toolu_{m}_{i:02}"),
            _ => format!("call_{m}_{i:02}"),
        }
    }
}

/// What the provider saw of one request.
#[derive(Debug, Clone)]
pub struct Seen {
    pub idx: usize,
    pub t: f64,
    pub wire: Option<String>,
    /// The turn the request was answered for, and the step it was at (`calls.len()` = the text).
    pub turn: Option<(String, usize)>,
    /// Whether marion's `report`, in the harness's spelling, appears anywhere in the request.
    pub marion_tools: bool,
    /// Whether the request offers the model any tool at all — a side request (a title, a router
    /// probe) offers none.
    pub offers_tools: bool,
    pub body: Value,
}

#[derive(Debug, Default)]
struct State {
    turns: Vec<Turn>,
    seen: Vec<Seen>,
    /// Park the first request at this (marker, step) until [`ProbeHold::release`].
    park: Option<(String, usize)>,
    parked: bool,
    released: bool,
    fault: Option<u16>,
    usage: bool,
    /// Requests answered with a counted usage, for the numbers the probe expects back.
    usage_sent: Vec<(u64, u64, u64)>,
    stopping: bool,
}

#[derive(Debug)]
pub struct ProbeHold {
    state: Mutex<State>,
    cv: Condvar,
    log: Arc<Log>,
    /// marion's `report` as this harness spells it — the needle for [`Seen::marion_tools`].
    needle: String,
}

impl ProbeHold {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_turn(&self, turn: Turn) {
        self.lock().turns.push(turn);
    }

    pub fn park(&self, marker: &str, step: usize) {
        let mut s = self.lock();
        s.park = Some((marker.to_string(), step));
        s.parked = false;
        s.released = false;
    }

    pub fn release(&self) {
        let mut s = self.lock();
        s.released = true;
        self.cv.notify_all();
    }

    pub fn fault(&self, status: Option<u16>) {
        self.lock().fault = status;
    }

    pub fn count_usage(&self) {
        self.lock().usage = true;
    }

    pub fn usage_sent(&self) -> Vec<(u64, u64, u64)> {
        self.lock().usage_sent.clone()
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.lock().seen.clone()
    }

    /// Wait until `pred` holds over what the provider has seen, or the bound passes.
    pub fn wait(&self, bound: Duration, mut pred: impl FnMut(&[Seen], bool) -> bool) -> bool {
        let deadline = Instant::now() + bound;
        let mut s = self.lock();
        loop {
            if pred(&s.seen, s.parked && !s.released) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            s = self
                .cv
                .wait_timeout(s, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Whether a parked request is being held right now.
    pub fn wait_parked(&self, bound: Duration) -> bool {
        self.wait(bound, |_, parked| parked)
    }

    fn stop(&self) {
        let mut s = self.lock();
        s.stopping = true;
        s.released = true;
        self.cv.notify_all();
    }
}

/// The turn a request belongs to: the known marker whose last occurrence sits latest in the body.
fn pick(turns: &[Turn], text: &str) -> Option<Turn> {
    turns
        .iter()
        .filter_map(|t| text.rfind(&t.marker).map(|at| (at, t)))
        .max_by_key(|(at, _)| *at)
        .map(|(_, t)| t.clone())
}

fn step_of(turn: &Turn, wire: &str, text: &str) -> usize {
    if wire == "gemini" {
        // Gemini's calls carry no id: the step is the function responses since the turn's prompt.
        let from = text.rfind(&turn.marker).unwrap_or(0);
        return text[from..]
            .matches("\"functionResponse\"")
            .count()
            .min(turn.calls.len());
    }
    (0..turn.calls.len())
        .find(|i| !text.contains(&turn.call_id(*i, wire)))
        .unwrap_or(turn.calls.len())
}

impl Hold for ProbeHold {
    fn wait_for(&self, wire: Option<&str>, body: &Value) {
        let text = body.to_string();
        let mut s = self.lock();
        let turn = pick(&s.turns, &text);
        let step = turn
            .as_ref()
            .map(|t| (t.marker.clone(), step_of(t, wire.unwrap_or(""), &text)));
        let idx = s.seen.len() + 1;
        let seen = Seen {
            idx,
            t: self.log.secs(),
            wire: wire.map(str::to_string),
            turn: step.clone(),
            marion_tools: text.contains(&self.needle),
            offers_tools: body["tools"].as_array().is_some_and(|t| !t.is_empty()),
            body: body.clone(),
        };
        self.log.w(
            "prov",
            &json!({"req": idx, "wire": wire, "turn": step, "marion_tools": seen.marion_tools,
                    "offers_tools": seen.offers_tools,
                    "user_texts": user_texts(body)}),
        );
        s.seen.push(seen);
        self.cv.notify_all();
        if step.is_some() && s.park == step && !s.parked {
            s.parked = true;
            self.cv.notify_all();
            self.log.note(format!("holding provider request {idx}"));
            while !s.released && !s.stopping {
                s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            s.park = None;
            s.parked = false;
            self.log.note(format!("released provider request {idx}"));
        }
    }

    fn answer(&self, wire: Option<&str>, path: &str, body: &Value) -> Option<Answer> {
        let wire = wire?;
        let text = body.to_string();
        let mut s = self.lock();
        if let Some(status) = s.fault {
            let answer = fault(wire, status);
            self.log
                .w("prov", &json!({"resp": "fault", "status": status}));
            return Some(answer);
        }
        let streaming = path.contains(":streamGenerateContent") || path.contains("alt=sse");
        let content_type = if wire == "gemini" && !streaming {
            "application/json"
        } else {
            "text/event-stream"
        };
        let reply = match pick(&s.turns, &text) {
            Some(turn) => {
                let step = step_of(&turn, wire, &text);
                match turn.calls.get(step) {
                    Some(call) => call_turn(wire, streaming, call, &turn.call_id(step, wire)),
                    None => text_turn(wire, streaming, &turn.final_text),
                }
            }
            None => text_turn(wire, streaming, "ok"),
        };
        let reply = if s.usage {
            let k = s.usage_sent.len() as u64 + 1;
            let counts = (100 * k + 11, 7 * k + 3, 5 * k);
            s.usage_sent.push(counts);
            with_usage(wire, &reply, counts)
        } else {
            reply
        };
        self.log.w("prov", &json!({"resp": "script", "wire": wire}));
        Some(Answer {
            status: 200,
            content_type: content_type.into(),
            body: reply,
        })
    }
}

fn call_turn(wire: &str, streaming: bool, call: &Call, id: &str) -> String {
    match wire {
        "anthropic" => anthropic::tool_use_turn(&call.spelled, id, &call.args),
        "responses" => responses::mcp_call(&call.verb, &call.args, id),
        "gemini" => gemini::function_call_turn(&call.spelled, &call.args, streaming),
        _ => openai::tool_call_turn(&call.spelled, id, &call.args),
    }
}

fn text_turn(wire: &str, streaming: bool, text: &str) -> String {
    match wire {
        "anthropic" => anthropic::text_turn(text),
        "responses" => responses::final_message(text),
        "gemini" => gemini::text_turn(text, streaming),
        _ => openai::text_turn(text),
    }
}

/// Every user-authored text in a request, whatever the wire — the history a resume or a fold
/// carries is read off this.
pub fn user_texts(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let items = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array);
    for m in items.into_iter().flatten() {
        if m.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        match m.get("content") {
            Some(Value::String(s)) => out.push(s.clone()),
            Some(Value::Array(parts)) => {
                for p in parts {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        out.push(t.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    for c in body
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if c.get("role").and_then(Value::as_str) == Some("user") {
            for p in c["parts"].as_array().into_iter().flatten() {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    out.push(t.to_string());
                }
            }
        }
    }
    out
}

/// Rewrite each SSE `data:` line's JSON with `f`, leaving every other line as it was.
fn rewrite_sse(sse: &str, mut f: impl FnMut(&mut Value) -> Option<String>) -> String {
    let mut out = String::new();
    for line in sse.split_inclusive('\n') {
        let Some(data) = line.strip_prefix("data: ") else {
            out.push_str(line);
            continue;
        };
        match serde_json::from_str::<Value>(data.trim_end()) {
            Ok(mut v) => {
                let extra = f(&mut v);
                out.push_str(&format!("data: {v}\n"));
                if let Some(extra) = extra {
                    out.push_str(&extra);
                }
            }
            Err(_) => out.push_str(line),
        }
    }
    out
}

/// Put counted usage into an answer, where each wire states it: Anthropic's `message_start` and
/// `message_delta`, Responses' `response.completed`, OpenAI chat's finishing chunk.
fn with_usage(wire: &str, sse: &str, (input, output, cached): (u64, u64, u64)) -> String {
    let gemini_usage = json!({"promptTokenCount": input + cached, "candidatesTokenCount": output,
        "cachedContentTokenCount": cached, "totalTokenCount": input + cached + output});
    if wire == "gemini" && !sse.starts_with("data: ") {
        // Non-streaming Gemini: one JSON body.
        return match serde_json::from_str::<Value>(sse) {
            Ok(mut v) => {
                v["usageMetadata"] = gemini_usage;
                v.to_string()
            }
            Err(_) => sse.to_string(),
        };
    }
    rewrite_sse(sse, |v| {
        match wire {
            "anthropic" => match v["type"].as_str() {
                Some("message_start") => {
                    v["message"]["usage"] = json!({"input_tokens": input, "output_tokens": 1,
                        "cache_read_input_tokens": cached, "cache_creation_input_tokens": 0});
                }
                Some("message_delta") => v["usage"] = json!({"output_tokens": output}),
                _ => {}
            },
            "gemini" => v["usageMetadata"] = gemini_usage.clone(),
            "responses" => {
                if v["type"] == "response.completed" {
                    v["response"]["usage"] = json!({"input_tokens": input + cached,
                        "input_tokens_details": {"cached_tokens": cached},
                        "output_tokens": output, "output_tokens_details": {"reasoning_tokens": 0},
                        "total_tokens": input + cached + output});
                }
            }
            _ => {
                let finishing = v["choices"][0]["finish_reason"].is_string();
                if finishing {
                    v["usage"] = json!({"prompt_tokens": input + cached, "completion_tokens": output,
                        "total_tokens": input + cached + output,
                        "prompt_tokens_details": {"cached_tokens": cached}});
                }
            }
        }
        None
    })
}

/// A provider error in each wire's own error shape.
fn fault(wire: &str, status: u16) -> Answer {
    // Each vendor's own words for a bad key, so a harness that echoes the provider's message
    // echoes what it would echo in the field.
    let (anthropic_type, openai_type, openai_code, gemini_status, message) = match status {
        401 => (
            "authentication_error",
            "invalid_request_error",
            json!("invalid_api_key"),
            "UNAUTHENTICATED",
            match wire {
                "anthropic" => "invalid x-api-key",
                "gemini" => "API key not valid. Please pass a valid API key.",
                _ => "Incorrect API key provided: dummy.",
            },
        ),
        429 => (
            "rate_limit_error",
            "rate_limit_exceeded",
            json!("rate_limit_exceeded"),
            "RESOURCE_EXHAUSTED",
            "rate limit exceeded",
        ),
        529 => (
            "overloaded_error",
            "server_error",
            Value::Null,
            "UNAVAILABLE",
            "Overloaded",
        ),
        _ => (
            "api_error",
            "server_error",
            Value::Null,
            "INTERNAL",
            "internal server error",
        ),
    };
    let body = match wire {
        "anthropic" => {
            json!({"type": "error", "error": {"type": anthropic_type, "message": message}})
        }
        "gemini" => json!({"error": {"code": status, "message": message, "status": gemini_status}}),
        _ => json!({"error": {"message": message, "type": openai_type, "code": openai_code}}),
    };
    Answer {
        status,
        content_type: "application/json".into(),
        body: body.to_string(),
    }
}

/// One probe's provider: a canned server on a fresh port with its hold.
pub struct Provider {
    pub server: CannedServer,
    pub hold: Arc<ProbeHold>,
}

impl Provider {
    pub fn start(dir: &std::path::Path, log: Arc<Log>, needle: String) -> Self {
        let hold = Arc::new(ProbeHold {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            log,
            needle,
        });
        let server = CannedServer::start_held(
            Config {
                addr: ([127, 0, 0, 1], 0).into(),
                reqlog: dir.join("reqlog.jsonl"),
                script: Script::default(),
            },
            Some(Arc::clone(&hold) as Arc<dyn Hold>),
        )
        .expect("the probe's provider binds a loopback port");
        Self { server, hold }
    }

    pub fn base_url(&self) -> String {
        self.server.base_url()
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        // A parked request must not outlive its probe: releasing it lets the connection thread end.
        self.hold.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(marker: &str, calls: usize) -> Turn {
        let call = Call {
            spelled: "mcp__marion__report".into(),
            verb: "report".into(),
            args: json!({}),
        };
        Turn {
            marker: marker.into(),
            calls: vec![call; calls],
            final_text: "done".into(),
        }
    }

    #[test]
    fn the_latest_marker_in_a_request_picks_its_turn_and_the_first_unseen_call_is_the_step() {
        let turns = [turn("ONE", 1), turn("TWO", 0)];
        let body = r#"{"messages":["ONE go","call_one_00 done","TWO go"]}"#;
        let picked = pick(&turns, body).unwrap();
        assert_eq!(picked.marker, "TWO", "the folded message is the latest");
        let one = pick(&turns, r#"{"messages":["ONE go"]}"#).unwrap();
        assert_eq!(step_of(&one, "openai", r#"["ONE go"]"#), 0);
        assert_eq!(step_of(&one, "openai", r#"["ONE go","call_one_00"]"#), 1);
        assert_eq!(
            step_of(&one, "anthropic", r#"["ONE go","toolu_one_00"]"#),
            1
        );
        assert!(pick(&turns, r#"{"messages":["generate a title"]}"#).is_none());
        // Gemini carries no call ids: its step is the function responses after the prompt.
        let gem = r#"["old","functionResponse","ONE go",{"functionResponse":{}}]"#;
        assert_eq!(step_of(&one, "gemini", gem), 1);
        assert_eq!(step_of(&one, "gemini", r#"["ONE go"]"#), 0);
    }

    #[test]
    fn counted_usage_lands_where_each_wire_states_it() {
        let a = with_usage("anthropic", &anthropic::text_turn("x"), (111, 10, 5));
        assert!(
            a.contains(r#""input_tokens":111"#) && a.contains(r#""output_tokens":10"#),
            "{a}"
        );
        let r = with_usage("responses", &responses::final_message("x"), (111, 10, 5));
        assert!(r.contains(r#""total_tokens":126"#), "{r}");
        let o = with_usage("openai", &openai::text_turn("x"), (111, 10, 5));
        assert!(
            o.contains(r#""prompt_tokens":116"#) && o.ends_with("data: [DONE]\n\n"),
            "{o}"
        );
    }

    #[test]
    fn a_fault_speaks_the_wire_s_own_error_shape() {
        let a = fault("anthropic", 429);
        assert_eq!(a.status, 429);
        assert!(a.body.contains("rate_limit_error"));
        assert!(fault("gemini", 401).body.contains("UNAUTHENTICATED"));
        assert!(fault("openai", 500).body.contains("server_error"));
    }
}
