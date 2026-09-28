//! **OpenAI Chat Completions → Anthropic Messages, as pure functions** — the gateway's second pair:
//! a harness that speaks Chat Completions (opencode, goose, cline, qwen) in front of a provider that
//! serves only Anthropic Messages. The mirror of [`super::translate`], with the same rules: nothing
//! here touches a socket, a process or a key.
//!
//! The request becomes an Anthropic request ([`messages_request`]): system and developer messages
//! join into `system`, consecutive same-role turns merge (Anthropic wants them alternating), tool
//! results become `tool_result` blocks of a user turn, and `max_tokens` — optional on Chat,
//! required on Anthropic — defaults to [`DEFAULT_MAX_TOKENS`]. The provider's event stream comes back
//! as `chat.completion.chunk`s ([`ChunkTranslator`]), or folded into one `chat.completion`
//! ([`Completion`]) for a request that did not stream.

use serde_json::{Map, Value, json};

/// `max_tokens` where the harness named none: Anthropic refuses a request without one.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// The request the provider is sent. `model` is the endpoint's; the upstream always streams.
pub fn messages_request(chat: &Value, model: &str) -> Value {
    let mut system = Vec::new();
    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    for m in chat
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let (role, blocks) = match role {
            "system" | "developer" => {
                let text = text_of(m.get("content"));
                if !text.is_empty() {
                    system.push(text);
                }
                continue;
            }
            "assistant" => ("assistant", assistant_blocks(m)),
            "tool" => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").cloned().unwrap_or(json!("")),
                    "content": text_of(m.get("content")),
                })],
            ),
            _ => ("user", user_blocks(m.get("content"))),
        };
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((r, b)) if r == role => b.extend(blocks),
            _ => turns.push((role.to_string(), blocks)),
        }
    }
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert(
        "max_tokens".into(),
        chat.get("max_completion_tokens")
            .or_else(|| chat.get("max_tokens"))
            .filter(|v| v.is_u64())
            .cloned()
            .unwrap_or(json!(DEFAULT_MAX_TOKENS)),
    );
    if !system.is_empty() {
        out.insert("system".into(), json!(system.join("\n\n")));
    }
    out.insert(
        "messages".into(),
        Value::Array(
            turns
                .into_iter()
                .map(|(role, content)| json!({"role": role, "content": content}))
                .collect(),
        ),
    );
    out.insert("stream".into(), json!(true));
    for key in ["temperature", "top_p"] {
        if let Some(v) = chat.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    match chat.get("stop") {
        Some(Value::String(s)) => {
            out.insert("stop_sequences".into(), json!([s]));
        }
        Some(Value::Array(a)) if !a.is_empty() => {
            out.insert("stop_sequences".into(), Value::Array(a.clone()));
        }
        _ => {}
    }
    let tools = anthropic_tools(chat.get("tools"));
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        let mut choice = chat.get("tool_choice").and_then(anthropic_tool_choice);
        if chat.get("parallel_tool_calls") == Some(&json!(false)) {
            let c = choice.get_or_insert_with(|| json!({"type": "auto"}));
            c["disable_parallel_tool_use"] = json!(true);
        }
        if let Some(c) = choice {
            out.insert("tool_choice".into(), c);
        }
    }
    Value::Object(out)
}

/// A message's text: a string as it is, the text parts of an array joined.
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn user_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|t| json!({"type": "text", "text": t})),
                Some("image_url") => image_block(p.pointer("/image_url/url")?.as_str()?),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// An `image_url` as Anthropic's image source: a `data:` URL as base64, any other as a URL.
fn image_block(url: &str) -> Option<Value> {
    let source = match url.strip_prefix("data:") {
        Some(rest) => {
            let (media_type, data) = rest.split_once(";base64,")?;
            json!({"type": "base64", "media_type": media_type, "data": data})
        }
        None => json!({"type": "url", "url": url}),
    };
    Some(json!({"type": "image", "source": source}))
}

fn assistant_blocks(m: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let text = text_of(m.get("content"));
    if !text.is_empty() {
        out.push(json!({"type": "text", "text": text}));
    }
    for call in m
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let args = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        out.push(json!({
            "type": "tool_use",
            "id": call.get("id").cloned().unwrap_or(json!("")),
            "name": call.pointer("/function/name").cloned().unwrap_or(json!("")),
            "input": serde_json::from_str::<Value>(args).unwrap_or(json!({})),
        }));
    }
    out
}

fn anthropic_tools(tools: Option<&Value>) -> Vec<Value> {
    tools
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let f = t.get("function")?;
            let mut out = Map::new();
            out.insert("name".into(), f.get("name")?.clone());
            if let Some(d) = f.get("description") {
                out.insert("description".into(), d.clone());
            }
            out.insert(
                "input_schema".into(),
                f.get("parameters")
                    .cloned()
                    .unwrap_or(json!({"type": "object", "properties": {}})),
            );
            Some(Value::Object(out))
        })
        .collect()
}

fn anthropic_tool_choice(choice: &Value) -> Option<Value> {
    Some(match choice {
        Value::String(s) => match s.as_str() {
            "auto" => json!({"type": "auto"}),
            "required" => json!({"type": "any"}),
            "none" => json!({"type": "none"}),
            _ => return None,
        },
        Value::Object(_) => json!({"type": "tool", "name": choice.pointer("/function/name")?}),
        _ => return None,
    })
}

// ---- the answer: Anthropic events in, Chat Completions chunks out --------------------------------

/// Chat Completions' `finish_reason` for an Anthropic `stop_reason`.
fn finish_reason(stop: Option<&str>) -> &'static str {
    match stop {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}

/// **Anthropic events in, `chat.completion.chunk`s out.** Feed each event's `data:` payload to
/// [`Self::event`] in order, then [`Self::finish`] once; each returns the chunks to send, the last
/// of them followed by a choiceless usage chunk (what `stream_options.include_usage` asks for, and
/// harmless where it was not asked) — the `[DONE]` sentinel is the caller's.
#[derive(Debug)]
pub struct ChunkTranslator {
    model: String,
    id: String,
    /// Chat's `tool_calls[].index` for each Anthropic tool block, by block index.
    tools: Vec<(u64, usize)>,
    /// Prompt tokens from `message_start` (input plus cache reads) and cache reads alone.
    prompt: u64,
    cached: u64,
    completion: u64,
    stop: Option<String>,
    finished: bool,
}

impl ChunkTranslator {
    pub fn new(model: &str) -> ChunkTranslator {
        ChunkTranslator {
            model: model.to_string(),
            id: "chatcmpl-marion-gateway".into(),
            tools: Vec::new(),
            prompt: 0,
            cached: 0,
            completion: 0,
            stop: None,
            finished: false,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    }

    fn usage(&self) -> Value {
        json!({
            "prompt_tokens": self.prompt,
            "prompt_tokens_details": {"cached_tokens": self.cached},
            "completion_tokens": self.completion,
            "total_tokens": self.prompt + self.completion,
        })
    }

    /// One event's `data:` payload of the provider's stream.
    pub fn event(&mut self, data: &str) -> Vec<Value> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return out;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(id) = v.pointer("/message/id").and_then(Value::as_str) {
                    self.id = format!("chatcmpl-{id}");
                }
                let u = v.pointer("/message/usage").unwrap_or(&Value::Null);
                self.take_usage(u);
                out.push(self.chunk(json!({"role": "assistant", "content": ""}), None));
            }
            Some("content_block_start") => {
                let block = v.get("content_block").unwrap_or(&Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let call = self.tools.len();
                    self.tools.push((index, call));
                    out.push(self.chunk(
                        json!({"tool_calls": [{"index": call,
                            "id": block.get("id").cloned().unwrap_or(json!("")),
                            "type": "function",
                            "function": {"name": block.get("name").cloned().unwrap_or(json!("")),
                                         "arguments": ""}}]}),
                        None,
                    ));
                }
            }
            Some("content_block_delta") => {
                let delta = v.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                            out.push(self.chunk(json!({"content": t}), None));
                        }
                    }
                    Some("input_json_delta") => {
                        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                        if let (Some((_, call)), Some(p)) = (
                            self.tools.iter().find(|(i, _)| *i == index),
                            delta.get("partial_json").and_then(Value::as_str),
                        ) {
                            out.push(self.chunk(
                                json!({"tool_calls": [{"index": call,
                                    "function": {"arguments": p}}]}),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(s) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(s.to_string());
                }
                if let Some(u) = v.get("usage") {
                    self.take_usage(u);
                }
            }
            Some("message_stop") => out.extend(self.finish()),
            Some("error") => {
                self.finished = true;
                let message = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("the provider's stream ended in an error");
                out.push(json!({"error": {"message": message, "type": "api_error"}}));
            }
            _ => {}
        }
        out
    }

    /// Anthropic reports input on `message_start` and output on `message_delta`, each where it
    /// knows it; a later non-zero reading replaces an earlier one.
    fn take_usage(&mut self, u: &Value) {
        let n = |k: &str| u.get(k).and_then(Value::as_u64);
        let (input, read, write) = (
            n("input_tokens"),
            n("cache_read_input_tokens"),
            n("cache_creation_input_tokens"),
        );
        if input.is_some() || read.is_some() {
            let prompt = input.unwrap_or(0) + read.unwrap_or(0) + write.unwrap_or(0);
            if prompt > 0 {
                self.prompt = prompt;
                self.cached = read.unwrap_or(0);
            }
        }
        if let Some(o) = n("output_tokens").filter(|o| *o > 0) {
            self.completion = o;
        }
    }

    /// The end: the finishing chunk, then the usage chunk. Idempotent.
    pub fn finish(&mut self) -> Vec<Value> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut last = self.chunk(json!({}), Some(finish_reason(self.stop.as_deref())));
        let mut usage = last.clone();
        usage["choices"] = json!([]);
        usage["usage"] = self.usage();
        last["usage"] = Value::Null;
        vec![last, usage]
    }
}

/// A provider that answered one whole Anthropic message, as the events its stream would have been.
pub fn message_as_events(message: &Value) -> Vec<String> {
    let mut out = vec![json!({"type": "message_start", "message": message}).to_string()];
    for (i, block) in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                out.push(
                    json!({"type": "content_block_delta", "index": i,
                           "delta": {"type": "text_delta", "text": block.get("text")}})
                    .to_string(),
                );
            }
            Some("tool_use") => {
                let mut start = block.clone();
                start["input"] = json!({});
                out.push(
                    json!({"type": "content_block_start", "index": i, "content_block": start})
                        .to_string(),
                );
                out.push(
                    json!({"type": "content_block_delta", "index": i,
                           "delta": {"type": "input_json_delta",
                                     "partial_json": block.get("input").unwrap_or(&json!({})).to_string()}})
                    .to_string(),
                );
            }
            _ => {}
        }
    }
    out.push(
        json!({"type": "message_delta",
               "delta": {"stop_reason": message.get("stop_reason")},
               "usage": message.get("usage")})
        .to_string(),
    );
    out.push(json!({"type": "message_stop"}).to_string());
    out
}

/// **One `chat.completion` from the chunk stream**, for a harness request that did not stream.
#[derive(Debug, Default)]
pub struct Completion {
    id: Option<Value>,
    model: Option<Value>,
    text: String,
    /// `(id, name, arguments)` per tool call, by Chat index.
    calls: Vec<(Value, Value, String)>,
    finish: Option<Value>,
    usage: Option<Value>,
    error: Option<Value>,
}

impl Completion {
    pub fn push(&mut self, chunk: &Value) {
        if let Some(e) = chunk.get("error") {
            self.error = Some(json!({"error": e}));
            return;
        }
        self.id.get_or_insert_with(|| chunk["id"].clone());
        self.model.get_or_insert_with(|| chunk["model"].clone());
        if let Some(u) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(u.clone());
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return;
        };
        if let Some(t) = choice.pointer("/delta/content").and_then(Value::as_str) {
            self.text.push_str(t);
        }
        for call in choice
            .pointer("/delta/tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let i = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            while self.calls.len() <= i {
                self.calls.push((Value::Null, Value::Null, String::new()));
            }
            let slot = &mut self.calls[i];
            if let Some(id) = call.get("id") {
                slot.0 = id.clone();
            }
            if let Some(n) = call.pointer("/function/name") {
                slot.1 = n.clone();
            }
            if let Some(a) = call.pointer("/function/arguments").and_then(Value::as_str) {
                slot.2.push_str(a);
            }
        }
        if let Some(f) = choice.get("finish_reason").filter(|f| !f.is_null()) {
            self.finish = Some(f.clone());
        }
    }

    /// The completion, or the error the stream ended on.
    pub fn result(self) -> Result<Value, Value> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        message.insert(
            "content".into(),
            if self.text.is_empty() {
                Value::Null
            } else {
                json!(self.text)
            },
        );
        if !self.calls.is_empty() {
            message.insert(
                "tool_calls".into(),
                Value::Array(
                    self.calls
                        .into_iter()
                        .map(|(id, name, args)| {
                            json!({"id": id, "type": "function",
                                   "function": {"name": name, "arguments": args}})
                        })
                        .collect(),
                ),
            );
        }
        Ok(json!({
            "id": self.id.unwrap_or(Value::Null),
            "object": "chat.completion",
            "created": 0,
            "model": self.model.unwrap_or(Value::Null),
            "choices": [{"index": 0, "message": Value::Object(message),
                         "finish_reason": self.finish.unwrap_or(json!("stop"))}],
            "usage": self.usage.unwrap_or(Value::Null),
        }))
    }
}

/// A Chat Completions error body, as OpenAI-compatible clients read one, with the status's meaning.
pub fn error_body(status: u16, message: &str) -> Value {
    let kind = match status {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_exceeded",
        s if s >= 500 => "server_error",
        _ => "invalid_request_error",
    };
    json!({"error": {"message": message, "type": kind, "code": status}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_request_becomes_anthropic_with_merged_turns_and_tool_results() {
        let chat = json!({
            "model": "whatever",
            "stream": true,
            "temperature": 0.1,
            "stop": "END",
            "parallel_tool_calls": false,
            "tools": [{"type": "function", "function": {"name": "marion_report",
                "description": "report", "parameters": {"type": "object"}}}],
            "tool_choice": "required",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "developer", "content": [{"type": "text", "text": "use tools"}]},
                {"role": "user", "content": "do it"},
                {"role": "user", "content": [{"type": "text", "text": "now"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "marion_report", "arguments": "{\"narrative\":\"x\"}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
                {"role": "user", "content": "thanks"}
            ]
        });
        let a = messages_request(&chat, "claude-endpoint-1");
        assert_eq!(a["model"], "claude-endpoint-1");
        assert_eq!(a["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(a["system"], "be brief\n\nuse tools");
        assert_eq!(a["stop_sequences"], json!(["END"]));
        assert_eq!(
            a["tool_choice"],
            json!({"type": "any", "disable_parallel_tool_use": true})
        );
        assert_eq!(a["tools"][0]["input_schema"]["type"], "object");
        let m = a["messages"].as_array().unwrap();
        assert_eq!(m.len(), 3, "user, assistant, user: {m:#?}");
        assert_eq!(
            m[0]["content"].as_array().unwrap().len(),
            3,
            "two user turns merged"
        );
        assert_eq!(m[0]["content"][2]["source"]["media_type"], "image/png");
        assert_eq!(m[1]["content"][0]["type"], "tool_use");
        assert_eq!(m[1]["content"][0]["input"], json!({"narrative": "x"}));
        assert_eq!(m[2]["content"][0]["type"], "tool_result");
        assert_eq!(m[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(
            m[2]["content"][1],
            json!({"type": "text", "text": "thanks"})
        );
        // A stated max_tokens is kept.
        let a = messages_request(&json!({"messages": [], "max_tokens": 77}), "m");
        assert_eq!(a["max_tokens"], 77);
    }

    fn events(stream: &str) -> Vec<String> {
        stream
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(str::to_string)
            .collect()
    }

    const TOOL_STREAM: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":90,"cache_read_input_tokens":10,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"marion_report","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"narrative\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"done\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}

event: message_stop
data: {"type":"message_stop"}
"#;

    #[test]
    fn a_streamed_tool_use_becomes_chat_tool_call_fragments_with_usage() {
        let mut t = ChunkTranslator::new("claude-endpoint-1");
        let mut chunks = Vec::new();
        for e in events(TOOL_STREAM) {
            chunks.extend(t.event(&e));
        }
        chunks.extend(t.finish());
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[0]["id"], "chatcmpl-msg_1");
        let mut c = Completion::default();
        chunks.iter().for_each(|ch| c.push(ch));
        let done = c.result().unwrap();
        let call = &done["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "toolu_1");
        assert_eq!(call["function"]["name"], "marion_report");
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
            json!({"narrative": "done"})
        );
        assert_eq!(done["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(done["usage"]["prompt_tokens"], 100);
        assert_eq!(done["usage"]["prompt_tokens_details"]["cached_tokens"], 10);
        assert_eq!(done["usage"]["completion_tokens"], 20);
        assert_eq!(
            chunks.iter().filter(|c| c["choices"] == json!([])).count(),
            1,
            "one usage chunk, once"
        );
    }

    #[test]
    fn a_whole_message_reads_as_its_stream_and_errors_end_it() {
        let msg = json!({"id": "m", "type": "message", "role": "assistant", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 3, "output_tokens": 2}});
        let mut t = ChunkTranslator::new("m");
        let mut c = Completion::default();
        for e in message_as_events(&msg) {
            t.event(&e).iter().for_each(|ch| c.push(ch));
        }
        t.finish().iter().for_each(|ch| c.push(ch));
        let done = c.result().unwrap();
        assert_eq!(done["choices"][0]["message"]["content"], "hi");
        assert_eq!(done["choices"][0]["finish_reason"], "stop");
        assert_eq!(done["usage"]["completion_tokens"], 2);

        let mut t = ChunkTranslator::new("m");
        let out =
            t.event(r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#);
        assert_eq!(out[0]["error"]["message"], "busy");
        assert!(t.finish().is_empty());
        assert_eq!(error_body(429, "slow")["error"]["code"], 429);
    }
}
