//! **marion's translating gateway**: a harness that speaks one wire, pointed at a provider that serves
//! another. Endpoint resolution routes a node here only when the two differ and the pair is one
//! marion translates (`marion_core::provider::TRANSLATIONS`); a provider serving the harness's own
//! wire is reached directly, and an operator's own gateway (LiteLLM, a claude-code-router-style
//! proxy) is simply a provider in `providers.toml` with the wire it speaks.
//!
//! # Lifetime: one per node, and nothing when idle
//!
//! A gateway is started for one node's attempt ([`crate::endpoint::open`]) and stopped when its
//! handle drops — which the node's launch holds for exactly as long as the node runs. Between
//! requests it is one thread blocked in `accept(2)`: no timer, no poll, no wakeup. Each request is
//! one connection thread and one `curl` for the provider (`upstream`), both gone when the answer is.
//!
//! # What it trusts
//!
//! It binds `127.0.0.1` on a port the kernel picks, and answers only a request presenting its
//! per-run bearer — 32 random bytes minted at start, handed to the harness in place of the key and
//! compared in constant time. The provider's key never reaches the harness: it stays in this
//! process and goes to curl on a pipe (`upstream`). Neither is on any argv, and the gateway writes
//! nothing to disk and logs nothing.

mod http;
pub mod translate;
mod upstream;

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use marion_core::provider::{KeyHeader, Wire};
use serde_json::Value;

use crate::credentials::Secret;
use translate::{Collected, Event, StreamTranslator};

/// Connections served at once. A harness opens a handful (a turn plus a background title call);
/// the bound is what keeps a local process that is not the node from pinning threads.
const MAX_CONNS: usize = 16;
/// How long a stopping gateway waits for its connection threads to see their sockets closed.
const DRAIN: Duration = Duration::from_secs(5);
/// How much of a provider's error body is read, for its message.
const ERROR_BODY: u64 = 64 * 1024;

/// Gateways running in this process, and gateways ever started — what a test reads to prove a
/// node's gateway ended with it.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static STARTED: AtomicUsize = AtomicUsize::new(0);

/// Gateways running in this process now.
pub fn live() -> usize {
    LIVE.load(Ordering::SeqCst)
}

/// Gateways started in this process so far.
pub fn started() -> usize {
    STARTED.load(Ordering::SeqCst)
}

/// Where the gateway sends what it translates: the provider, its wire and base, the model every
/// request is for, and how it authenticates.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub provider: String,
    pub wire: Wire,
    pub base_url: String,
    pub model: String,
    pub key: Option<Secret>,
    pub key_header: KeyHeader,
}

/// The translations this gateway performs: one arm per [`marion_core::provider::TRANSLATIONS`]
/// pair, checked against that table by a sweep test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Translation {
    /// Anthropic Messages from the harness, Chat Completions to the provider ([`translate`]).
    AnthropicToChat,
}

impl Translation {
    fn of(harness: Wire, provider: Wire) -> Option<Translation> {
        match (harness, provider) {
            (Wire::AnthropicMessages, Wire::OpenAiChat) => Some(Translation::AnthropicToChat),
            _ => None,
        }
    }
}

struct Conn {
    stream: TcpStream,
    /// The request's curl while it runs, so a stopping gateway can end it.
    child: Mutex<Option<Child>>,
}

struct Shared {
    translation: Translation,
    upstream: Upstream,
    bearer: Secret,
    stopping: AtomicBool,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    drained: Condvar,
    /// The connection threads not yet known to be finished, joined by a stopping gateway once its
    /// connections have drained — so its drop returns with none of them left.
    workers: Mutex<Vec<JoinHandle<()>>>,
}

/// A running gateway. Dropping it stops it: the listener closes, every in-flight connection is
/// shut and its curl killed, and the drop returns once the connection threads have ended (bounded).
pub struct Gateway {
    addr: SocketAddr,
    harness_wire: Wire,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("addr", &self.addr)
            .field("harness_wire", &self.harness_wire)
            .finish_non_exhaustive()
    }
}

/// Whether marion's gateway translates a harness speaking `harness` to a provider serving
/// `provider` — the gateway's own answer, which the sweep test holds to the core table.
pub fn translates(harness: Wire, provider: Wire) -> bool {
    Translation::of(harness, provider).is_some()
}

/// 32 random bytes, hex: the harness's credential for this gateway alone.
fn mint_bearer() -> io::Result<Secret> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(Secret::new(format!(
        "marion-gw-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )))
}

impl Gateway {
    /// Start a gateway serving `harness_wire` in front of `upstream`.
    pub fn start(harness_wire: Wire, upstream: Upstream) -> io::Result<Gateway> {
        let translation = Translation::of(harness_wire, upstream.wire).ok_or_else(|| {
            io::Error::other(format!(
                "marion's gateway does not translate {harness_wire} to {}",
                upstream.wire
            ))
        })?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            translation,
            upstream,
            bearer: mint_bearer()?,
            stopping: AtomicBool::new(false),
            conns: Mutex::new(HashMap::new()),
            drained: Condvar::new(),
            workers: Mutex::new(Vec::new()),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("marion-gateway".into())
                .spawn(move || accept_loop(&listener, &shared))?
        };
        LIVE.fetch_add(1, Ordering::SeqCst);
        STARTED.fetch_add(1, Ordering::SeqCst);
        Ok(Gateway {
            addr,
            harness_wire,
            shared,
            accept: Some(accept),
        })
    }

    /// The base URL the harness is handed, in the spelling its wire's clients take: `…/v1` for the
    /// two OpenAI wires, the root for the others (their SDKs add the version themselves).
    pub fn base_url(&self) -> String {
        match self.harness_wire {
            Wire::OpenAiChat | Wire::OpenAiResponses => format!("http://{}/v1", self.addr),
            _ => format!("http://{}", self.addr),
        }
    }

    /// The credential the harness presents to this gateway, in place of the provider's key.
    pub fn bearer(&self) -> &Secret {
        &self.shared.bearer
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        // `accept(2)` returns for this connection, and the loop sees `stopping`.
        let _ = TcpStream::connect(self.addr);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
        let mut conns = self.shared.conns.lock().unwrap_or_else(|p| p.into_inner());
        for c in conns.values() {
            let _ = c.stream.shutdown(Shutdown::Both);
            if let Some(child) = c.child.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                let _ = child.kill();
            }
        }
        let deadline = Instant::now() + DRAIN;
        while !conns.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            conns = match self.shared.drained.wait_timeout(conns, left) {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
        }
        let drained = conns.is_empty();
        drop(conns);
        // Each has removed itself and has nothing left but to return; one still serving after the
        // bound is left to finish on its own rather than waited for without end.
        if drained {
            let workers = std::mem::take(
                &mut *self
                    .shared
                    .workers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()),
            );
            for w in workers {
                let _ = w.join();
            }
        }
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    let mut next = 0u64;
    for stream in listener.incoming() {
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        // The permit is taken here, before any thread exists, so the bound bounds threads.
        let mut conns = shared.conns.lock().unwrap_or_else(|p| p.into_inner());
        if conns.len() >= MAX_CONNS {
            drop(conns);
            let body = translate::error_body(503, "marion gateway: too many connections");
            let _ = http::respond(
                &mut &stream,
                503,
                "application/json",
                &[],
                body.to_string().as_bytes(),
            );
            continue;
        }
        let Ok(clone) = stream.try_clone() else {
            continue;
        };
        next += 1;
        let id = next;
        let conn = Arc::new(Conn {
            stream: clone,
            child: Mutex::new(None),
        });
        conns.insert(id, Arc::clone(&conn));
        drop(conns);
        let worker = {
            let shared = Arc::clone(shared);
            std::thread::Builder::new()
                .name("marion-gateway-conn".into())
                .spawn(move || {
                    serve(&shared, &conn, &stream);
                    let mut conns = shared.conns.lock().unwrap_or_else(|p| p.into_inner());
                    conns.remove(&id);
                    shared.drained.notify_all();
                })
        };
        match worker {
            Ok(handle) => {
                let mut workers = shared.workers.lock().unwrap_or_else(|p| p.into_inner());
                workers.retain(|w| !w.is_finished());
                workers.push(handle);
            }
            Err(_) => {
                shared
                    .conns
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
            }
        }
    }
}

/// Whether the request presents this gateway's bearer, as `Authorization: Bearer` or `x-api-key`.
fn presents(req: &http::Request, bearer: &Secret) -> bool {
    let offered = req
        .header("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| req.header("x-api-key"));
    offered.is_some_and(|v| Secret::new(v.trim()) == *bearer)
}

fn error_response(mut w: &TcpStream, status: u16, message: &str) {
    let body = translate::error_body(status, message).to_string();
    let _ = http::respond(&mut w, status, "application/json", &[], body.as_bytes());
}

fn serve(shared: &Shared, conn: &Conn, stream: &TcpStream) {
    // No read timeout: a connection that never sends holds one of `MAX_CONNS` permits until the
    // gateway stops, which shuts it — and a timer would be a wakeup an idle node does not need.
    let w = stream;
    let req = match http::read_request(&mut BufReader::new(stream)) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            return error_response(w, 400, &format!("marion gateway: {e}"));
        }
        Err(_) => return,
    };
    if !presents(&req, &shared.bearer) {
        return error_response(
            w,
            401,
            "marion gateway: this request does not carry the node's gateway credential",
        );
    }
    let method = req.method.to_ascii_uppercase();
    if method == "GET" || method == "HEAD" {
        let _ = http::respond(&mut { w }, 200, "text/plain", &[], b"");
        return;
    }
    let is_messages = req.path.ends_with("/messages");
    match shared.translation {
        Translation::AnthropicToChat if method == "POST" && is_messages => {
            messages(shared, conn, &req, w);
        }
        Translation::AnthropicToChat => error_response(
            w,
            404,
            &format!("marion gateway: {} {} is not served", method, req.path),
        ),
    }
}

/// `POST /v1/messages`: translate, send, and relay the provider's answer back as Anthropic's.
fn messages(shared: &Shared, conn: &Conn, req: &http::Request, w: &TcpStream) {
    let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
        return error_response(w, 400, "marion gateway: the request body is not JSON");
    };
    let up = &shared.upstream;
    let chat = translate::chat_request(&body, &up.model);
    let url = format!("{}/chat/completions", up.base_url.trim_end_matches('/'));
    let auth = upstream::Auth {
        key: up.key.clone(),
        header: up.key_header,
    };
    let (child, answer) = match upstream::post(&url, &auth, chat.to_string().as_bytes()) {
        Ok(x) => x,
        Err(e) => return error_response(w, 502, &format!("marion gateway: cannot run curl: {e}")),
    };
    *conn.child.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
    if shared.stopping.load(Ordering::SeqCst) {
        reap(conn);
        return;
    }
    let wants_stream = body.get("stream") == Some(&Value::Bool(true));
    match answer {
        Err(words) => error_response(
            w,
            502,
            &format!(
                "marion gateway could not reach provider `{}`: {}",
                up.provider,
                scrub(up, &words)
            ),
        ),
        Ok(a) if a.status >= 400 => {
            let mut text = String::new();
            let _ = a.body.take(ERROR_BODY).read_to_string(&mut text);
            let message = format!(
                "provider `{}` answered {}: {}",
                up.provider,
                a.status,
                scrub(up, &translate::upstream_message(&text))
            );
            let body = translate::error_body(a.status, &message).to_string();
            let retry: Vec<(&str, &str)> = a
                .retry_after
                .as_deref()
                .map(|r| ("retry-after", r))
                .into_iter()
                .collect();
            let _ = http::respond(
                &mut { w },
                a.status,
                "application/json",
                &retry,
                body.as_bytes(),
            );
        }
        Ok(a) => {
            let _ = relay(a, wants_stream, &up.model, w);
        }
    }
    reap(conn);
}

/// The request's curl, killed if it is still running and reaped either way.
fn reap(conn: &Conn) {
    if let Some(mut c) = conn.child.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// A provider's words with its key removed, should it ever echo one.
fn scrub(up: &Upstream, text: &str) -> String {
    match &up.key {
        Some(k) => crate::endpoint::redact(text, k.expose()),
        None => text.to_string(),
    }
}

/// Where translated events go: a streaming harness request gets each batch as SSE at once, a
/// non-streaming one gets the events folded into one message at the end.
enum Sink<'a> {
    Stream(Option<http::Chunked<&'a TcpStream>>, &'a TcpStream),
    Whole(Collected),
}

impl Sink<'_> {
    fn send(&mut self, events: Vec<Event>) -> io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        match self {
            Sink::Stream(chunked, stream) => {
                if chunked.is_none() {
                    *chunked = Some(http::Chunked::start(*stream, "text/event-stream")?);
                }
                let text: String = events.iter().map(Event::sse).collect();
                chunked
                    .as_mut()
                    .expect("started above")
                    .send(text.as_bytes())
            }
            Sink::Whole(c) => {
                events.iter().for_each(|e| c.push(e));
                Ok(())
            }
        }
    }
}

/// A 2xx answer, translated: read as SSE where the provider streamed, as one completion where it
/// did not, and sent on as the harness asked.
fn relay(a: upstream::Answer, wants_stream: bool, model: &str, w: &TcpStream) -> io::Result<()> {
    let mut t = StreamTranslator::new(model);
    let mut sink = if wants_stream {
        Sink::Stream(None, w)
    } else {
        Sink::Whole(Collected::default())
    };
    let mut body = a.body;
    if a.content_type.contains("event-stream") {
        let mut data = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            if body.read_line(&mut line)? == 0 {
                break;
            }
            let l = line.trim_end_matches(['\r', '\n']);
            if l.is_empty() {
                if !data.is_empty() {
                    sink.send(t.chunk(&data))?;
                    data.clear();
                }
            } else if let Some(d) = l.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(d.trim_start());
            }
        }
        if !data.is_empty() {
            sink.send(t.chunk(&data))?;
        }
    } else {
        let mut text = String::new();
        body.take(http::MAX_BODY as u64).read_to_string(&mut text)?;
        match serde_json::from_str::<Value>(&text) {
            Ok(completion) => {
                for c in translate::completion_as_chunks(&completion) {
                    sink.send(t.chunk(&c))?;
                }
            }
            Err(_) => {
                error_response(
                    w,
                    502,
                    "marion gateway: the provider's answer was neither a stream nor JSON",
                );
                return Ok(());
            }
        }
    }
    sink.send(t.finish())?;
    match sink {
        Sink::Stream(Some(chunked), _) => chunked.end(),
        Sink::Stream(None, _) => Ok(()),
        Sink::Whole(c) => match c.result() {
            Ok(m) => http::respond(
                &mut { w },
                200,
                "application/json",
                &[],
                m.to_string().as_bytes(),
            ),
            Err(e) => http::respond(
                &mut { w },
                502,
                "application/json",
                &[],
                e.to_string().as_bytes(),
            ),
        },
    }
}

#[cfg(test)]
mod tests;
