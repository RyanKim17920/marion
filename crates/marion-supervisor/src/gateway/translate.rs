//! **Anthropic Messages ↔ OpenAI Chat Completions, as pure functions** — the one translation marion's
//! gateway performs today (`marion_core::provider::TRANSLATIONS`).
//!
//! A harness that speaks only Anthropic Messages (Claude Code) sends its request here; the request
//! goes to the provider as a Chat Completions request ([`chat_request`]), and the provider's
//! streamed answer comes back as Anthropic's event stream ([`StreamTranslator`]) or, for a request
//! that did not ask to stream, as one Anthropic message ([`Collected`]). Nothing here touches a
//! socket, a process or a key, so every rule is a unit test.
//!
//! # What is carried
//!
//! Text, tool definitions, tool calls and tool results, images, the system prompt, `tool_choice`,
//! sampling parameters, stop sequences and token usage. What has no counterpart on the other wire
//! is dropped rather than approximated: thinking blocks (Chat Completions has nowhere to put a
//! signature), Anthropic server tools (`web_search_*`, which carry a `type` and no schema), cache
//! control markers and request metadata.
//!
//! # Usage
//!
//! Chat Completions counts cache reads inside `prompt_tokens`; Anthropic counts them beside
//! `input_tokens`. So `input_tokens = prompt_tokens - cached_tokens` and
//! `cache_read_input_tokens = cached_tokens`, and `output_tokens = completion_tokens` (reasoning
//! included, as both wires count it).

use serde_json::{Map, Value, json};

/// The request the provider is sent, from the request the harness sent: `model` is the endpoint's
/// (a harness's background calls may name another), and the upstream always streams with usage, so
/// one reader serves both a streaming and a non-streaming harness request.
pub fn chat_request(anthropic: &Value, model: &str) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = system_text(anthropic.get("system")) {
        messages.push(json!({"role": "system", "content": system}));
    }
    for m in anthropic
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match m.get("role").and_then(Value::as_str) {
            Some("assistant") => messages.push(assistant_message(m.get("content"))),
            _ => messages.extend(user_messages(m.get("content"))),
        }
    }
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), json!(true));
    out.insert("stream_options".into(), json!({"include_usage": true}));
    for (from, to) in [
        ("max_tokens", "max_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stop_sequences", "stop"),
    ] {
        if let Some(v) = anthropic.get(from).filter(|v| !v.is_null()) {
            out.insert(to.into(), v.clone());
        }
    }
    let tools = chat_tools(anthropic.get("tools"));
    if !tools.is_empty() {
        out.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = anthropic.get("tool_choice") {
            if let Some(c) = chat_tool_choice(choice) {
                out.insert("tool_choice".into(), c);
            }
            if choice.get("disable_parallel_tool_use") == Some(&json!(true)) {
                out.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }
    Value::Object(out)
}

/// A string system prompt as it is, or the text blocks of an array one, joined.
fn system_text(system: Option<&Value>) -> Option<String> {
    let text = match system? {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// Anthropic content as a list of blocks: a bare string is one text block.
fn blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    }
}

/// A block's text, for the places Chat Completions takes only text (a tool result).
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// One user turn: its tool results first, each its own `role: tool` message (Chat Completions wants
/// them directly after the assistant message that called), then whatever text and images remain.
fn user_messages(content: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut parts: Vec<Value> = Vec::new();
    for b in blocks(content) {
        match b.get("type").and_then(Value::as_str) {
            Some("tool_result") => {
                let mut text = text_of(b.get("content"));
                if b.get("is_error") == Some(&json!(true)) && !text.starts_with("Error") {
                    text = format!("Error: {text}");
                }
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": b.get("tool_use_id").cloned().unwrap_or(json!("")),
                    "content": text,
                }));
            }
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    parts.push(json!({"type": "text", "text": t}));
                }
            }
            Some("image") => {
                if let Some(url) = image_url(b.get("source")) {
                    parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return out;
    }
    // Text alone travels as one string, which every Chat Completions server takes; an image makes
    // it the array form, which is the only way to carry one.
    let content = if parts.iter().all(|p| p["type"] == "text") {
        json!(
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        Value::Array(parts)
    };
    out.push(json!({"role": "user", "content": content}));
    out
}

fn image_url(source: Option<&Value>) -> Option<String> {
    let s = source?;
    match s.get("type").and_then(Value::as_str)? {
        "base64" => Some(format!(
            "data:{};base64,{}",
            s.get("media_type").and_then(Value::as_str)?,
            s.get("data").and_then(Value::as_str)?
        )),
        "url" => s.get("url").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// One assistant turn: its text as `content`, its `tool_use` blocks as `tool_calls`.
fn assistant_message(content: Option<&Value>) -> Value {
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for b in blocks(content) {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    text.push(t.to_string());
                }
            }
            Some("tool_use") => calls.push(json!({
                "id": b.get("id").cloned().unwrap_or(json!("")),
                "type": "function",
                "function": {
                    "name": b.get("name").cloned().unwrap_or(json!("")),
                    "arguments": b.get("input").unwrap_or(&json!({})).to_string(),
                },
            })),
            _ => {}
        }
    }
    let mut m = Map::new();
    m.insert("role".into(), json!("assistant"));
    m.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text.join("\n"))
        },
    );
    if !calls.is_empty() {
        m.insert("tool_calls".into(), Value::Array(calls));
    }
    Value::Object(m)
}

/// The harness's tools as functions. A server tool (it carries a `type` other than `custom`, and
/// no schema) is the provider's own feature on Anthropic's API and has no counterpart here.
fn chat_tools(tools: Option<&Value>) -> Vec<Value> {
    tools
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| {
            t.get("type")
                .and_then(Value::as_str)
                .is_none_or(|k| k == "custom")
        })
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            let mut f = Map::new();
            f.insert("name".into(), json!(name));
            if let Some(d) = t.get("description") {
                f.insert("description".into(), d.clone());
            }
            f.insert(
                "parameters".into(),
                t.get("input_schema")
                    .cloned()
                    .unwrap_or(json!({"type": "object", "properties": {}})),
            );
            Some(json!({"type": "function", "function": Value::Object(f)}))
        })
        .collect()
}

fn chat_tool_choice(choice: &Value) -> Option<Value> {
    Some(match choice.get("type").and_then(Value::as_str)? {
        "auto" => json!("auto"),
        "any" => json!("required"),
        "none" => json!("none"),
        "tool" => json!({"type": "function", "function": {"name": choice.get("name")?}}),
        _ => return None,
    })
}

// ---- the answer: Chat Completions chunks in, Anthropic events out --------------------------------

/// Token counts in Anthropic's shape, from a Chat Completions `usage` object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Usage {
    fn from_chat(u: &Value) -> Usage {
        let prompt = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cached = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Usage {
            input: prompt.saturating_sub(cached),
            cache_read: cached,
            output: u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        }
    }

    fn json(self) -> Value {
        json!({
            "input_tokens": self.input,
            "cache_read_input_tokens": self.cache_read,
            "cache_creation_input_tokens": 0,
            "output_tokens": self.output,
        })
    }
}

/// Anthropic's `stop_reason` for a Chat Completions `finish_reason`.
fn stop_reason(finish: Option<&str>) -> &'static str {
    match finish {
        Some("length") => "max_tokens",
        Some("tool_calls" | "function_call") => "tool_use",
        _ => "end_turn",
    }
}

/// One Anthropic stream event: its name and its payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: &'static str,
    pub data: Value,
}

impl Event {
    fn new(name: &'static str, data: Value) -> Event {
        Event { name, data }
    }

    /// The event as SSE bytes, the way Anthropic's API frames it.
    pub fn sse(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.name, self.data)
    }
}

/// Which content block is open, if one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Text(usize),
    /// A tool call's block, with the Chat Completions `index` it was opened for.
    Tool(usize, u64),
}

/// **Chat Completions chunks in, Anthropic's event stream out.** Feed each `data:` payload to
/// [`Self::chunk`] in order, then call [`Self::finish`] once; each returns the events to send.
///
/// Anthropic's stream carries one content block at a time — `content_block_start`, its deltas,
/// `content_block_stop` — so a text run and each tool call become consecutive blocks, and the open
/// one is closed before the next starts. Usage arrives on Chat Completions' last chunk and so rides
/// `message_delta`, which is where Anthropic's own clients read final counts from.
#[derive(Debug)]
pub struct StreamTranslator {
    model: String,
    started: bool,
    next_block: usize,
    open: Option<Open>,
    finish: Option<String>,
    usage: Usage,
    finished: bool,
}

impl StreamTranslator {
    pub fn new(model: &str) -> StreamTranslator {
        StreamTranslator {
            model: model.to_string(),
            started: false,
            next_block: 0,
            open: None,
            finish: None,
            usage: Usage::default(),
            finished: false,
        }
    }

    fn start(&mut self, id: Option<&str>, out: &mut Vec<Event>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(Event::new(
            "message_start",
            json!({"type": "message_start", "message": {
                "id": format!("msg_{}", id.unwrap_or("marion_gateway")),
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": Usage::default().json(),
            }}),
        ));
    }

    fn close(&mut self, out: &mut Vec<Event>) {
        if let Some(Open::Text(i) | Open::Tool(i, _)) = self.open.take() {
            out.push(Event::new(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": i}),
            ));
        }
    }

    fn open_block(&mut self, block: Value, out: &mut Vec<Event>) -> usize {
        self.close(out);
        let i = self.next_block;
        self.next_block += 1;
        out.push(Event::new(
            "content_block_start",
            json!({"type": "content_block_start", "index": i, "content_block": block}),
        ));
        i
    }

    /// One `data:` payload of the provider's stream (`[DONE]` included).
    pub fn chunk(&mut self, data: &str) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if data.trim() == "[DONE]" {
            return self.finish();
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return out;
        };
        if let Some(e) = v.get("error") {
            self.start(None, &mut out);
            self.finished = true;
            out.push(stream_error(e));
            return out;
        }
        self.start(v.get("id").and_then(Value::as_str), &mut out);
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Usage::from_chat(u);
        }
        for choice in v
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                let i = match self.open {
                    Some(Open::Text(i)) => i,
                    _ => {
                        let i = self.open_block(json!({"type": "text", "text": ""}), &mut out);
                        self.open = Some(Open::Text(i));
                        i
                    }
                };
                out.push(Event::new(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": i,
                           "delta": {"type": "text_delta", "text": text}}),
                ));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.tool_fragment(call, &mut out);
            }
            if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish = Some(f.to_string());
            }
        }
        out
    }

    /// One `delta.tool_calls[]` fragment. A fragment naming a function opens a new block; one
    /// carrying only argument text continues the block its `index` opened.
    fn tool_fragment(&mut self, call: &Value, out: &mut Vec<Event>) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let name = call.pointer("/function/name").and_then(Value::as_str);
        let block = match (self.open, name) {
            (Some(Open::Tool(i, at)), None) if at == index => i,
            (Some(Open::Tool(i, at)), Some(_)) if at == index && call.get("id").is_none() => i,
            (_, name) => {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("toolu_marion_{}", self.next_block));
                let i = self.open_block(
                    json!({"type": "tool_use", "id": id, "name": name.unwrap_or(""),
                           "input": {}}),
                    out,
                );
                self.open = Some(Open::Tool(i, index));
                i
            }
        };
        if let Some(args) = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .filter(|a| !a.is_empty())
        {
            out.push(Event::new(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": block,
                       "delta": {"type": "input_json_delta", "partial_json": args}}),
            ));
        }
    }

    /// The end of the provider's stream: close the open block, then `message_delta` with the stop
    /// reason and usage, then `message_stop`. Idempotent: `[DONE]` and the end of the body both end
    /// the stream, and whichever comes second adds nothing.
    pub fn finish(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.finished = true;
        self.start(None, &mut out);
        self.close(&mut out);
        out.push(Event::new(
            "message_delta",
            json!({"type": "message_delta",
                   "delta": {"stop_reason": stop_reason(self.finish.as_deref()),
                             "stop_sequence": null},
                   "usage": self.usage.json()}),
        ));
        out.push(Event::new("message_stop", json!({"type": "message_stop"})));
        out
    }
}

/// An error the provider put inside its stream, as Anthropic's `error` event.
fn stream_error(e: &Value) -> Event {
    let message = e
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| e.to_string());
    Event::new(
        "error",
        json!({"type": "error", "error": {"type": "api_error", "message": message}}),
    )
}

/// A provider that answered with one `chat.completion` object rather than a stream, as the chunks
/// that stream would have been — so one translator reads both.
pub fn completion_as_chunks(completion: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let id = completion.get("id").cloned().unwrap_or(Value::Null);
    let choice = completion.pointer("/choices/0").unwrap_or(&Value::Null);
    let msg = choice.get("message").unwrap_or(&Value::Null);
    let mut delta = Map::new();
    if let Some(t) = msg.get("content").filter(|c| c.is_string()) {
        delta.insert("content".into(), t.clone());
    }
    if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
        let calls: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut c = c.clone();
                c["index"] = json!(i);
                c
            })
            .collect();
        delta.insert("tool_calls".into(), Value::Array(calls));
    }
    out.push(
        json!({"id": id, "choices": [{"index": 0, "delta": Value::Object(delta),
               "finish_reason": choice.get("finish_reason").cloned().unwrap_or(Value::Null)}]})
        .to_string(),
    );
    if let Some(u) = completion.get("usage") {
        out.push(json!({"id": id, "choices": [], "usage": u}).to_string());
    }
    out
}

/// **One Anthropic message from the event stream**, for a harness request that did not ask to
/// stream: the same events, folded.
#[derive(Debug, Default)]
pub struct Collected {
    message: Option<Value>,
    content: Vec<Value>,
    /// Argument text of the open tool block, parsed at its stop.
    partial: String,
    error: Option<Value>,
}

impl Collected {
    pub fn push(&mut self, ev: &Event) {
        let d = &ev.data;
        match ev.name {
            "message_start" => self.message = d.get("message").cloned(),
            "content_block_start" => {
                self.content
                    .push(d.get("content_block").cloned().unwrap_or(Value::Null));
                self.partial.clear();
            }
            "content_block_delta" => {
                let Some(last) = self.content.last_mut() else {
                    return;
                };
                if let Some(t) = d.pointer("/delta/text").and_then(Value::as_str) {
                    let so_far = last["text"].as_str().unwrap_or("").to_string();
                    last["text"] = json!(so_far + t);
                }
                if let Some(p) = d.pointer("/delta/partial_json").and_then(Value::as_str) {
                    self.partial.push_str(p);
                }
            }
            "content_block_stop" => {
                if let Some(last) = self.content.last_mut()
                    && last["type"] == "tool_use"
                    && !self.partial.is_empty()
                {
                    last["input"] = serde_json::from_str(&self.partial).unwrap_or(json!({}));
                }
            }
            "message_delta" => {
                if let Some(m) = self.message.as_mut() {
                    m["stop_reason"] = d.pointer("/delta/stop_reason").cloned().unwrap_or_default();
                    m["usage"] = d.get("usage").cloned().unwrap_or_default();
                }
            }
            "error" => self.error = Some(d.clone()),
            _ => {}
        }
    }

    /// The message, or the error the stream ended on.
    pub fn result(self) -> Result<Value, Value> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let mut m = self.message.unwrap_or_else(|| json!({"type": "message"}));
        m["content"] = Value::Array(self.content);
        Ok(m)
    }
}

// ---- errors ------------------------------------------------------------------------------------

/// Anthropic's error `type` for an HTTP status, as its API names them.
pub fn error_type(status: u16) -> &'static str {
    match status {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        s if s >= 500 => "api_error",
        _ => "invalid_request_error",
    }
}

/// An Anthropic error body, so the harness reads the provider's refusal as it reads Anthropic's own
/// — with the status kept, which is what its retry logic and marion's failure classifier key on.
pub fn error_body(status: u16, message: &str) -> Value {
    json!({"type": "error", "error": {"type": error_type(status), "message": message}})
}

/// The words of a provider's error body: `error.message` where it is JSON, else the text itself.
pub fn upstream_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().chars().take(500).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut StreamTranslator, stream: &str) -> Vec<Event> {
        let mut out = Vec::new();
        for line in stream.lines().filter_map(|l| l.strip_prefix("data: ")) {
            out.extend(t.chunk(line));
        }
        out.extend(t.finish());
        out
    }

    fn names(evs: &[Event]) -> Vec<&'static str> {
        evs.iter().map(|e| e.name).collect()
    }

    #[test]
    fn a_request_becomes_chat_with_system_tools_history_and_the_endpoints_model() {
        let req = json!({
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "temperature": 0.2,
            "stop_sequences": ["END"],
            "stream": true,
            "system": [{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}},
                       {"type": "text", "text": "use tools"}],
            "tools": [
                {"name": "mcp__marion__report", "description": "report back",
                 "input_schema": {"type": "object", "properties": {"narrative": {"type": "string"}}}},
                {"type": "web_search_20250305", "name": "web_search", "max_uses": 3}
            ],
            "tool_choice": {"type": "any", "disable_parallel_tool_use": true},
            "messages": [
                {"role": "user", "content": "do it"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hm", "signature": "sig"},
                    {"type": "text", "text": "calling"},
                    {"type": "tool_use", "id": "toolu_1", "name": "mcp__marion__report",
                     "input": {"narrative": "done"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "ok"}]},
                    {"type": "text", "text": "thanks"}]}
            ]
        });
        let chat = chat_request(&req, "endpoint-model-7");
        assert_eq!(chat["model"], "endpoint-model-7");
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["stream_options"]["include_usage"], true);
        assert_eq!(chat["max_tokens"], 1024);
        assert_eq!(chat["temperature"], 0.2);
        assert_eq!(chat["stop"], json!(["END"]));
        assert_eq!(chat["tool_choice"], "required");
        assert_eq!(chat["parallel_tool_calls"], false);
        let tools = chat["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "the server tool has no counterpart");
        assert_eq!(tools[0]["function"]["name"], "mcp__marion__report");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        let m = chat["messages"].as_array().unwrap();
        assert_eq!(
            m[0],
            json!({"role": "system", "content": "be brief\n\nuse tools"})
        );
        assert_eq!(m[1], json!({"role": "user", "content": "do it"}));
        assert_eq!(m[2]["role"], "assistant");
        assert_eq!(m[2]["content"], "calling");
        assert_eq!(m[2]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(
            serde_json::from_str::<Value>(
                m[2]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap()
            )
            .unwrap(),
            json!({"narrative": "done"})
        );
        assert_eq!(
            m[3],
            json!({"role": "tool", "tool_call_id": "toolu_1", "content": "ok"}),
            "the tool result comes straight after the call"
        );
        assert_eq!(m[4], json!({"role": "user", "content": "thanks"}));
        assert_eq!(m.len(), 5);
    }

    #[test]
    fn tool_choice_and_images_and_errors_map_to_their_chat_spelling() {
        assert_eq!(
            chat_tool_choice(&json!({"type": "auto"})),
            Some(json!("auto"))
        );
        assert_eq!(
            chat_tool_choice(&json!({"type": "none"})),
            Some(json!("none"))
        );
        assert_eq!(
            chat_tool_choice(&json!({"type": "tool", "name": "x"})),
            Some(json!({"type": "function", "function": {"name": "x"}}))
        );
        let msgs = user_messages(Some(&json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            {"type": "text", "text": "what is it"},
            {"type": "tool_result", "tool_use_id": "t", "content": "boom", "is_error": true}
        ])));
        assert_eq!(msgs[0]["content"], "Error: boom");
        assert_eq!(
            msgs[1]["content"][0]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        // No tools: no tool_choice either, since a Chat server refuses one without tools.
        let chat = chat_request(
            &json!({"messages": [], "tool_choice": {"type": "auto"}, "tools": []}),
            "m",
        );
        assert!(chat.get("tools").is_none() && chat.get("tool_choice").is_none());
    }

    const TOOL_STREAM: &str = "\
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null}}]}
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"mcp__marion__report\",\"arguments\":\"\"}}]}}]}
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"narrative\\\":\"}}]}}]}
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"done\\\"}\"}}]}}]}
data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}
data: {\"id\":\"c1\",\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"prompt_tokens_details\":{\"cached_tokens\":10},\"completion_tokens\":20}}
data: [DONE]
";

    #[test]
    fn a_streamed_tool_call_becomes_one_tool_use_block_with_its_arguments_rejoined() {
        let mut t = StreamTranslator::new("endpoint-model-7");
        let evs = feed(&mut t, TOOL_STREAM);
        assert_eq!(
            names(&evs),
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(evs[1].data["content_block"]["type"], "tool_use");
        assert_eq!(evs[1].data["content_block"]["id"], "call_1");
        assert_eq!(evs[1].data["content_block"]["name"], "mcp__marion__report");
        assert_eq!(evs[5].data["delta"]["stop_reason"], "tool_use");
        assert_eq!(
            evs[5].data["usage"],
            json!({"input_tokens": 90, "cache_read_input_tokens": 10,
                   "cache_creation_input_tokens": 0, "output_tokens": 20})
        );
        let mut c = Collected::default();
        evs.iter().for_each(|e| c.push(e));
        let m = c.result().unwrap();
        assert_eq!(m["content"][0]["input"], json!({"narrative": "done"}));
        assert_eq!(m["stop_reason"], "tool_use");
        assert_eq!(m["model"], "endpoint-model-7");
    }

    #[test]
    fn text_then_two_tool_calls_become_three_consecutive_blocks() {
        let stream = "\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Look\"}}]}
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ing.\"}}]}
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}]}
data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"g\",\"arguments\":\"{\\\"x\\\":1}\"}}]}}]}
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}
";
        let mut t = StreamTranslator::new("m");
        let evs = feed(&mut t, stream);
        let starts: Vec<&Value> = evs
            .iter()
            .filter(|e| e.name == "content_block_start")
            .map(|e| &e.data)
            .collect();
        assert_eq!(starts.len(), 3);
        assert_eq!(
            starts
                .iter()
                .map(|s| s["index"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        let stops = evs
            .iter()
            .filter(|e| e.name == "content_block_stop")
            .count();
        assert_eq!(stops, 3, "every block is closed before the next opens");
        let mut c = Collected::default();
        evs.iter().for_each(|e| c.push(e));
        let m = c.result().unwrap();
        assert_eq!(m["content"][0]["text"], "Looking.");
        assert_eq!(m["content"][2]["input"], json!({"x": 1}));
    }

    #[test]
    fn finishing_twice_and_a_missing_done_both_end_the_stream_once() {
        let mut t = StreamTranslator::new("m");
        let mut evs = t.chunk("{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}");
        evs.extend(t.chunk("[DONE]"));
        evs.extend(t.finish());
        assert_eq!(evs.iter().filter(|e| e.name == "message_stop").count(), 1);
        let delta = evs.iter().find(|e| e.name == "message_delta").unwrap();
        assert_eq!(delta.data["delta"]["stop_reason"], "end_turn");
        // No chunk at all still yields a well-formed empty message.
        let evs = StreamTranslator::new("m").finish();
        assert_eq!(
            names(&evs),
            ["message_start", "message_delta", "message_stop"]
        );
    }

    #[test]
    fn an_error_inside_the_stream_becomes_anthropics_error_event() {
        let mut t = StreamTranslator::new("m");
        let evs = t.chunk("{\"error\":{\"message\":\"overloaded\",\"code\":503}}");
        assert_eq!(evs.last().unwrap().name, "error");
        assert_eq!(evs.last().unwrap().data["error"]["message"], "overloaded");
        assert!(t.finish().is_empty(), "the error ended the stream");
        let mut c = Collected::default();
        evs.iter().for_each(|e| c.push(e));
        assert!(c.result().is_err());
    }

    #[test]
    fn a_whole_completion_reads_as_the_stream_it_would_have_been() {
        let completion = json!({"id": "x", "choices": [{"index": 0, "finish_reason": "tool_calls",
            "message": {"role": "assistant", "content": "ok", "tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "f", "arguments": "{\"a\":2}"}}]}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}});
        let mut t = StreamTranslator::new("m");
        let mut evs = Vec::new();
        for c in completion_as_chunks(&completion) {
            evs.extend(t.chunk(&c));
        }
        evs.extend(t.finish());
        let mut c = Collected::default();
        evs.iter().for_each(|e| c.push(e));
        let m = c.result().unwrap();
        assert_eq!(m["content"][0]["text"], "ok");
        assert_eq!(m["content"][1]["input"], json!({"a": 2}));
        assert_eq!(m["usage"]["input_tokens"], 5);
        assert_eq!(m["usage"]["output_tokens"], 3);
    }

    #[test]
    fn a_status_keeps_its_meaning_in_anthropics_error_vocabulary() {
        assert_eq!(
            error_body(429, "slow down")["error"]["type"],
            "rate_limit_error"
        );
        assert_eq!(error_type(401), "authentication_error");
        assert_eq!(error_type(503), "api_error");
        assert_eq!(error_type(400), "invalid_request_error");
        assert_eq!(
            upstream_message("{\"error\":{\"message\":\"no such model\"}}"),
            "no such model"
        );
        assert_eq!(upstream_message("gateway timeout\n"), "gateway timeout");
    }
}
