//! A minimal HTTP/1.1 server that replays the canned scripts.
//!
//! Deliberately `std`-only and thread-per-connection, like the Python ancestor. The whole surface
//! is "POST a JSON body, get an SSE document back"; an async runtime would be more dependency and
//! more moving parts than the job has. Concurrency still matters, though — Claude Code opens the
//! session-title request *while* the first turn is in flight — hence a thread per connection
//! rather than a serial accept loop.
//!
//! Responses carry an explicit `Content-Length` rather than being streamed, which is what the
//! Python provider did and what both harnesses were observed to accept: the whole scripted turn is
//! known before the first byte, so there is nothing to stream.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use serde_json::Value;

use crate::gate::{Hold, TurnGate};
use crate::reqlog::RequestLog;
use crate::script::{Script, Wire, classify_wire, is_auxiliary, wire_name};

/// Default listen port, matching `spikes/s6`'s `S6_PORT`.
pub const DEFAULT_PORT: u16 = 8099;
/// Environment variable naming the listen port.
pub const PORT_ENV: &str = "MARION_CANNED_PORT";
/// Environment variable naming the request log path.
pub const REQLOG_ENV: &str = "MARION_CANNED_REQLOG";

/// How to start a [`CannedServer`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to bind. Port 0 asks the OS for a free port, which is how tests start one.
    pub addr: SocketAddr,
    /// Where the verbatim request log is appended.
    pub reqlog: PathBuf,
    /// The scripted behaviour to replay.
    pub script: Script,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            reqlog: default_reqlog_path(),
            script: Script::default(),
        }
    }
}

impl Config {
    /// Read `MARION_CANNED_PORT` / `MARION_CANNED_REQLOG`, falling back to the defaults.
    ///
    /// An unparseable port is an error rather than a silent fallback: binding a different port
    /// than the harness config points at fails as a hang, which is the worst failure to diagnose.
    pub fn from_env() -> Result<Self, String> {
        let mut cfg = Self::default();
        if let Ok(p) = std::env::var(PORT_ENV) {
            let port: u16 = p
                .trim()
                .parse()
                .map_err(|_| format!("{PORT_ENV}={p:?} is not a port number"))?;
            cfg.addr.set_port(port);
        }
        if let Ok(path) = std::env::var(REQLOG_ENV) {
            cfg.reqlog = PathBuf::from(path);
        }
        Ok(cfg)
    }
}

fn default_reqlog_path() -> PathBuf {
    std::env::temp_dir().join("marion-canned-requests.jsonl")
}

/// A running canned provider. Dropping it stops the server.
#[derive(Debug)]
pub struct CannedServer {
    addr: SocketAddr,
    reqlog: PathBuf,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl CannedServer {
    /// Bind, start accepting, and return once the port is live.
    ///
    /// Binding happens on the calling thread so that "the server is up" is established before this
    /// returns — a test that spawned a child against a not-yet-bound port would fail flakily.
    pub fn start(config: Config) -> io::Result<Self> {
        Self::start_gated(config, None)
    }

    /// The same, with a [`TurnGate`] holding one wire's turns until a test releases them.
    ///
    /// A separate constructor rather than a fourth [`Config`] field: a gate is a *test's* handle on
    /// this server, not a description of the canned behaviour, and every existing caller builds
    /// `Config` as a literal.
    pub fn start_gated(config: Config, gate: Option<Arc<TurnGate>>) -> io::Result<Self> {
        Self::start_held(config, gate.map(|g| g as Arc<dyn Hold>))
    }

    /// The general form: any [`Hold`], consulted between the log and the answer.
    ///
    /// [`Self::start_gated`] is the [`TurnGate`] special case, kept because every existing caller
    /// spells it that way and because the concrete type is the one a test releases by hand.
    pub fn start_held(config: Config, hold: Option<Arc<dyn Hold>>) -> io::Result<Self> {
        let gate = hold;
        let listener = TcpListener::bind(config.addr)?;
        let addr = listener.local_addr()?;
        let log = Arc::new(RequestLog::create(&config.reqlog)?);
        let script = Arc::new(config.script);
        let stopping = Arc::new(AtomicBool::new(false));

        let accept = {
            let stopping = Arc::clone(&stopping);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (log, script) = (Arc::clone(&log), Arc::clone(&script));
                    let gate = gate.clone();
                    std::thread::spawn(move || {
                        let _ = serve_connection(stream, &log, &script, gate.as_deref());
                    });
                }
            })
        };

        Ok(Self {
            addr,
            reqlog: config.reqlog,
            stopping,
            accept: Some(accept),
        })
    }

    /// The bound address, including the OS-assigned port when the config asked for 0.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Base URL to hand a harness, e.g. `http://127.0.0.1:8099/v1`.
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    /// Path of the request log, for assertions.
    pub fn reqlog_path(&self) -> &std::path::Path {
        &self.reqlog
    }

    /// Everything logged so far, parsed.
    pub fn requests(&self) -> io::Result<Vec<Value>> {
        RequestLog::read(&self.reqlog)
    }
}

impl Drop for CannedServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Unblock `accept()` by knocking on our own door; the loop then sees the flag and exits.
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

/// One parsed HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    }
}

/// Read one request, or `None` at a clean end of stream (a client closing a kept-alive socket).
pub(crate) fn read_request(reader: &mut impl BufRead) -> io::Result<Option<HttpRequest>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty request line",
        ));
    }

    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let mut req = HttpRequest {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    let chunked = req
        .header("transfer-encoding")
        .is_some_and(|v| v.contains("chunked"));
    if chunked {
        req.body = read_chunked(reader)?;
    } else if let Some(len) = req
        .header("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        req.body = vec![0; len];
        reader.read_exact(&mut req.body)?;
    }
    Ok(Some(req))
}

/// Decode `Transfer-Encoding: chunked`.
///
/// The Python original only honoured `Content-Length`; a chunked body there is a hang with no
/// error anywhere, so the twenty lines are cheaper than the incident.
fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            break;
        }
        let size_hex = size_line.trim().split(';').next().unwrap_or("").trim();
        if size_hex.is_empty() {
            continue;
        }
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            // Consume trailers up to the terminating blank line.
            loop {
                let mut t = String::new();
                if reader.read_line(&mut t)? == 0 || t.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        let mut chunk = vec![0; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(body)
}

/// The response to one request, as bytes on the wire.
pub(crate) fn http_response(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         Content-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Decide what to answer, and what to record about it. Pure, so it can be tested without sockets.
pub(crate) fn handle(req: &HttpRequest, script: &Script) -> (String, String, String) {
    if req.method.eq_ignore_ascii_case("GET") {
        return (
            "200 OK".into(),
            "text/plain".into(),
            "marion canned provider\n".into(),
        );
    }
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    match classify_wire(&req.path, &body) {
        Some(wire) => (
            "200 OK".into(),
            content_type_for(wire).into(),
            script.respond(wire, &body),
        ),
        None => (
            "400 Bad Request".into(),
            "application/json".into(),
            serde_json::json!({"error": {
                "type": "marion_canned_unroutable",
                "message": "body has none of `messages` (Anthropic or OpenAI chat), `input` \
                            (Responses) or `contents` (Gemini), and the path names no wire"}})
            .to_string(),
        ),
    }
}

/// Three of the four wires are SSE. Gemini's `:generateContent` is the exception and answers plain
/// JSON (S12) — sending it `text/event-stream` would be a lie about a body that has no frames.
fn content_type_for(wire: Wire) -> &'static str {
    match wire {
        Wire::Gemini { streaming: false } => "application/json",
        _ => "text/event-stream",
    }
}

fn serve_connection(
    stream: TcpStream,
    log: &RequestLog,
    script: &Script,
    gate: Option<&dyn Hold>,
) -> io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let Some(req) = read_request(&mut reader)? else {
            break;
        };
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let classified = classify_wire(&req.path, &body);
        let wire = classified.map(wire_name);
        // Log before answering: if the script panics, the evidence is already on disk.
        if !req.method.eq_ignore_ascii_case("GET") {
            let _ = log.append(&req.method, &req.path, &req.headers, &req.body, wire);
        }

        // **After the log, before the answer.** `gate.rs` argues why that is the only placement
        // that makes the ordering checkable: the held request is already on disk, so a test can
        // read the provider's own account of what it has been asked while it is still holding.
        //
        // **A harness's own auxiliary request is never offered to a hold** ([`is_auxiliary`]): a
        // hold is about a node's turns, and whether a release sends a title request at all is
        // harness drift, so counting one would move "the n-th turn" from build to build.
        let mut answer = None;
        if let Some(gate) = gate
            && !classified.is_some_and(|w| is_auxiliary(w, &body))
        {
            gate.wait_for(wire, &body);
            answer = gate.answer(wire, &req.path, &body);
        }

        let (status, content_type, payload) = match answer {
            Some(a) => (a.status_line(), a.content_type, a.body),
            None => handle(&req, script),
        };
        writer.write_all(&http_response(&status, &content_type, &payload))?;
        writer.flush()?;

        if req
            .header("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"))
        {
            let _ = writer.shutdown(Shutdown::Both);
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;

    fn post(path: &str, body: &str) -> Vec<u8> {
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn a_content_length_body_is_read_exactly() {
        let raw = post("/v1/messages", r#"{"a":1}"#);
        let req = read_request(&mut &raw[..]).unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/v1/messages");
        assert_eq!(req.body, br#"{"a":1}"#);
    }

    #[test]
    fn a_chunked_body_is_decoded_rather_than_hanging() {
        let raw = b"POST /v1/responses HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
                    4\r\n{\"a\"\r\n4\r\n:1}\x20\r\n0\r\n\r\n";
        let req = read_request(&mut &raw[..]).unwrap().unwrap();
        assert_eq!(req.body, b"{\"a\":1} ");
    }

    #[test]
    fn a_closed_connection_reads_as_no_request_not_an_error() {
        assert!(read_request(&mut &b""[..]).unwrap().is_none());
    }

    #[test]
    fn the_sse_response_declares_its_length_and_type() {
        let bytes = http_response("200 OK", "text/event-stream", "event: x\ndata: {}\n\n");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/event-stream\r\n"));
        assert!(
            text.contains("Content-Length: 19\r\n"),
            "both harnesses were driven with an explicit length, not a streamed body"
        );
        assert!(text.ends_with("\r\n\r\nevent: x\ndata: {}\n\n"));
    }

    #[test]
    fn an_unroutable_body_is_a_400_not_a_wrong_script() {
        let req = HttpRequest {
            method: "POST".into(),
            path: "/v1/anything".into(),
            headers: vec![],
            body: b"{}".to_vec(),
        };
        let (status, ct, body) = handle(&req, &Script::default());
        assert!(status.starts_with("400"));
        assert_eq!(ct, "application/json");
        assert!(body.contains("marion_canned_unroutable"));
    }

    #[test]
    fn the_gemini_json_endpoint_is_not_announced_as_an_event_stream() {
        let req = |path: &str| HttpRequest {
            method: "POST".into(),
            path: path.into(),
            headers: vec![],
            body: json!({"contents": [], "tools": [{"functionDeclarations": [{"name": "t"}]}]})
                .to_string()
                .into_bytes(),
        };
        let script = Script::default();
        let (_, ct, body) = handle(&req("/v1beta/models/m:generateContent"), &script);
        assert_eq!(ct, "application/json");
        assert!(!body.contains("data: "));
        let (_, ct, body) = handle(
            &req("/v1beta/models/m:streamGenerateContent?alt=sse"),
            &script,
        );
        assert_eq!(ct, "text/event-stream");
        assert!(body.starts_with("data: "));
    }

    #[test]
    fn all_four_wires_are_served_from_one_port() {
        let script = Script::default();
        let anthropic = HttpRequest {
            method: "POST".into(),
            path: "/v1/messages".into(),
            headers: vec![],
            body: json!({"tools": [{"name": "x"}], "messages": []})
                .to_string()
                .into_bytes(),
        };
        let responses = HttpRequest {
            method: "POST".into(),
            path: "/v1/responses".into(),
            headers: vec![],
            body: json!({"input": []}).to_string().into_bytes(),
        };
        let gemini = HttpRequest {
            method: "POST".into(),
            path: "/v1beta/models/gemini-3.5-flash:streamGenerateContent?alt=sse".into(),
            headers: vec![],
            body: json!({"contents": [{"role": "user", "parts": [{"text": "go"}]}],
                         "tools": [{"functionDeclarations": [{"name": "mcp_marion_report"}]}]})
            .to_string()
            .into_bytes(),
        };
        let openai = HttpRequest {
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            headers: vec![],
            body: json!({"model": "fake-1", "stream": true,
                         "tools": [{"type": "function", "function": {"name": "marion_report"}}],
                         "messages": [{"role": "user", "content": "go"}]})
            .to_string()
            .into_bytes(),
        };
        assert!(handle(&anthropic, &script).2.contains("mcp__marion__spawn"));
        assert!(handle(&responses, &script).2.contains("custom_tool_call"));
        assert!(handle(&gemini, &script).2.contains("mcp_marion_report"));
        assert!(handle(&openai, &script).2.contains("marion_report"));
    }

    /// End to end over a real socket: bind port 0, speak HTTP, read the log back.
    #[test]
    fn a_started_server_answers_both_wires_and_records_them() {
        let reqlog =
            std::env::temp_dir().join(format!("marion-canned-e2e-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&reqlog);
        let server = CannedServer::start(Config {
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            reqlog: reqlog.clone(),
            script: Script::default(),
        })
        .unwrap();
        assert_ne!(
            server.addr().port(),
            0,
            "port 0 must be resolved to a real port for tests"
        );

        let root = speak(
            server.addr(),
            &post(
                "/v1/messages",
                &json!({"tools": [{"name": "mcp__marion__spawn"}], "messages": []}).to_string(),
            ),
        );
        assert!(root.contains("event: content_block_start"));
        assert!(root.contains("mcp__marion__spawn"));

        let child = speak(
            server.addr(),
            &post("/v1/responses", &json!({"input": []}).to_string()),
        );
        assert!(child.contains("response.completed"));

        let log = server.requests().unwrap();
        assert_eq!(
            log.len(),
            2,
            "every request is recorded, including the concurrent ones"
        );
        assert_eq!(log[0]["wire"], "anthropic");
        assert_eq!(log[1]["wire"], "responses");
        assert!(
            log[1]["body"]["input"].is_array(),
            "bodies are recorded parsed and complete"
        );
        let _ = std::fs::remove_file(&reqlog);
    }

    /// The same end-to-end proof for the two child wires S12 and S13 measured: a real socket, a
    /// real reply the harness's own client shape could consume, and a log line naming the wire.
    #[test]
    fn a_started_server_answers_the_gemini_and_openai_wires_and_records_them() {
        let reqlog = std::env::temp_dir().join(format!(
            "marion-canned-e2e-new-wires-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&reqlog);
        let server = CannedServer::start(Config {
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            reqlog: reqlog.clone(),
            script: Script::default(),
        })
        .unwrap();

        let gemini = speak(
            server.addr(),
            &post(
                "/v1beta/models/gemini-3.5-flash:streamGenerateContent?alt=sse",
                &json!({"contents": [{"role": "user", "parts": [{"text": "go"}]}],
                        "tools": [{"functionDeclarations": [{"name": "mcp_marion_report"}]}]})
                .to_string(),
            ),
        );
        assert!(gemini.contains("Content-Type: text/event-stream"));
        assert!(gemini.contains("\r\n\r\ndata: {"));
        assert!(gemini.contains("\"functionCall\""));

        let openai = speak(
            server.addr(),
            &post(
                "/v1/chat/completions",
                &json!({"model": "fake-1", "stream": true,
                        "tools": [{"type": "function", "function": {"name": "marion_report"}}],
                        "messages": [{"role": "user", "content": "go"}]})
                .to_string(),
            ),
        );
        assert!(openai.contains("\"finish_reason\":\"tool_calls\""));
        assert!(openai.ends_with("data: [DONE]\n\n"));

        let probe = speak(
            server.addr(),
            &post(
                "/v1beta/models/gemini-3.1-flash-lite:generateContent",
                &json!({"contents": [{"role": "user", "parts": [{"text": "route"}]}]}).to_string(),
            ),
        );
        assert!(probe.contains("Content-Type: application/json"));
        assert!(!probe.contains("functionCall"));

        let log = server.requests().unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0]["wire"], "gemini");
        assert_eq!(log[1]["wire"], "openai");
        assert_eq!(log[2]["wire"], "gemini");
        assert!(
            log[0]["path"].as_str().unwrap().ends_with("alt=sse"),
            "the query string is evidence of the framing and is kept"
        );
        assert!(
            log[0]["body"]["contents"].is_array() && log[1]["body"]["messages"].is_array(),
            "bodies are recorded parsed and complete on the new wires too"
        );
        let _ = std::fs::remove_file(&reqlog);
    }

    /// **A hold counts node turns, and a request that belongs to no node is not one.** Claude Code
    /// 2.1.276 sends a session-title request on the anthropic wire; 2.1.278 and 2.1.280 do not, in
    /// the same run shape. A gate that counted it held "the third anthropic request" as the root's
    /// closing turn on one build and as nothing at all on the next, so the root finished unheld.
    /// Title first here, then two real turns: `holding_from(2)` must answer the first real turn and
    /// hold the second.
    #[test]
    fn an_auxiliary_request_is_neither_counted_nor_held_by_a_turn_gate() {
        let reqlog = std::env::temp_dir().join(format!(
            "marion-canned-gate-aux-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&reqlog);
        let gate = TurnGate::holding_from("anthropic", 2);
        let server = CannedServer::start_gated(
            Config {
                addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                reqlog: reqlog.clone(),
                script: Script::default(),
            },
            Some(Arc::clone(&gate)),
        )
        .unwrap();
        let addr = server.addr();
        let title = post("/v1/messages", &json!({"messages": []}).to_string());
        let turn = post(
            "/v1/messages",
            &json!({"tools": [{"name": "mcp__marion__spawn"}], "messages": []}).to_string(),
        );

        assert!(
            speak(addr, &title).contains("200"),
            "the title stub is answered"
        );
        let first = {
            let turn = turn.clone();
            std::thread::spawn(move || speak(addr, &turn))
        };
        assert!(
            marion_testsupport::until(|| first.is_finished()),
            "the first real turn is answered: the title request before it was not a turn, so it \
             must not have used up the gate's count (parked: {})",
            gate.parked()
        );
        assert_eq!(gate.parked(), 0);

        let second = std::thread::spawn(move || speak(addr, &turn));
        assert!(
            marion_testsupport::until(|| gate.parked() == 1),
            "the second real turn is the one held"
        );
        gate.release();
        second.join().unwrap();
        let _ = std::fs::remove_file(&reqlog);
    }

    /// A hold that answers a turn itself — a scripted fault — replaces the script's reply on the
    /// wire, status line included, while the request is still logged; a hold that answers nothing
    /// leaves the script's reply alone.
    #[test]
    fn a_hold_that_answers_replaces_the_scripted_reply_with_its_own_status_and_body() {
        #[derive(Debug)]
        struct Fault;
        impl Hold for Fault {
            fn wait_for(&self, _: Option<&str>, _: &Value) {}
            fn answer(
                &self,
                _: Option<&str>,
                _: &str,
                body: &Value,
            ) -> Option<crate::gate::Answer> {
                crate::script::carries(body, "FAULT").then(|| crate::gate::Answer {
                    status: 429,
                    content_type: "application/json".into(),
                    body: r#"{"error":{"type":"rate_limit_error"}}"#.into(),
                })
            }
        }
        let reqlog =
            std::env::temp_dir().join(format!("marion-canned-answer-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&reqlog);
        let server = CannedServer::start_held(
            Config {
                addr: SocketAddr::from(([127, 0, 0, 1], 0)),
                reqlog: reqlog.clone(),
                script: Script::default(),
            },
            Some(Arc::new(Fault)),
        )
        .unwrap();
        let tools = json!([{"name": "mcp__marion__spawn", "input_schema": {}}]);
        let turn = |text: &str| {
            let body = json!({"tools": tools, "messages": [{"role": "user", "content": text}]});
            speak(server.addr(), &post("/v1/messages", &body.to_string()))
        };
        let faulted = turn("FAULT");
        assert!(
            faulted.starts_with("HTTP/1.1 429 Too Many Requests\r\n"),
            "{faulted}"
        );
        assert!(
            faulted.ends_with(r#"{"error":{"type":"rate_limit_error"}}"#),
            "{faulted}"
        );
        let plain = turn("go");
        assert!(plain.starts_with("HTTP/1.1 200 OK\r\n"), "{plain}");
        assert_eq!(
            server.requests().unwrap().len(),
            2,
            "both requests are logged"
        );
        let _ = std::fs::remove_file(&reqlog);
    }

    fn speak(addr: SocketAddr, request: &[u8]) -> String {
        let mut s = TcpStream::connect(addr).expect("server is listening");
        // Ask for close so the read below terminates at EOF instead of on the keep-alive socket.
        let req = String::from_utf8_lossy(request)
            .replace("Host: x\r\n", "Host: x\r\nConnection: close\r\n");
        s.write_all(req.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }
}
