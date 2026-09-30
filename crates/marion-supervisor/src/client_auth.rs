//! **Who a connection to the supervisor speaks for**, said once, first, as `session/hello`.
//!
//! The supervisor's socket is private to the operator's uid, and every process of that uid can
//! `connect(2)` to it — including a child's own shell. So the uid alone cannot mean "the
//! operator": a connection states who it is before its first call, and the supervisor authorizes
//! every later call against that.
//!
//! * **The operator** presents the capability in the state root's `operator.key`
//!   ([`crate::operator_key`]). The CLI, the home screen and a top-level `marion mcp` read it; marion
//!   never hands its path or its bytes to a node.
//! * **A node** presents its own `agent_id` and node token, the pair its bridge already holds; it may
//!   then act about itself and the nodes below it, and nothing else.
//!
//! The hello is written and its one answer read **byte by byte**, before any buffered reader is
//! put on the stream, so no reader later misses bytes a first one had read ahead.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;

use marion_core::proto::params::SessionHelloParams;
use marion_core::proto::{Call, Frame, Outcome, Request, RequestId, SpawnCaller};

use crate::socket::SocketPaths;

/// Whom this process's connections speak for, where one place sets it for the whole process: a
/// node's bridge (its own identity) or a top-level `marion mcp` (the operator of one state root).
#[derive(Debug, Clone)]
pub enum Identity {
    Operator(marion_core::secret::Secret),
    Node(SpawnCaller),
}

static IDENTITY: Mutex<Option<Result<Identity, String>>> = Mutex::new(None);

/// Set whom this process's connections speak for, in place of the operator. `Err` is a process
/// that must speak for someone and could not say who — a node's bridge missing its token — and
/// makes every dial fail with that sentence rather than fall back to the operator.
pub fn set_identity(identity: Result<Identity, String>) {
    *IDENTITY.lock().unwrap_or_else(|e| e.into_inner()) = Some(identity);
}

/// The identity [`set_identity`] recorded, if any.
fn identity() -> Option<Result<Identity, String>> {
    IDENTITY.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The operator's identity for the state root `state`: its capability, created there on first use.
pub fn operator(state: &Path) -> Result<Identity, String> {
    crate::operator_key::ensure(state)
        .map(Identity::Operator)
        .map_err(|e| {
            format!(
                "marion could not read or create its operator key under {}: {e}",
                state.display()
            )
        })
}

/// The id a hello goes out under: text, so it can never collide with a client's numbered calls.
const HELLO_ID: &str = "hello";

/// **Say who `stream` speaks for, and read the supervisor's one answer.** `Err` is the
/// supervisor's refusal, or a stream that could not carry the exchange, as a sentence.
pub fn hello(stream: &mut UnixStream, identity: &Identity) -> Result<(), String> {
    let params = match identity {
        Identity::Operator(key) => SessionHelloParams {
            operator: Some(key.clone()),
            node: None,
        },
        Identity::Node(caller) => SessionHelloParams {
            operator: None,
            node: Some(caller.clone()),
        },
    };
    let frame = Frame::Request(Request::new(
        RequestId::Text(HELLO_ID.into()),
        Call::SessionHello(params),
    ));
    stream
        .write_all(frame.to_line().as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| format!("the supervisor did not take marion's hello: {e}"))?;
    let line = read_line(stream)?;
    match Frame::from_line(&line) {
        Ok(Frame::Response(r)) if r.id == RequestId::Text(HELLO_ID.into()) => match r.outcome {
            Outcome::Result(_) => Ok(()),
            Outcome::Error(e) => Err(format!(
                "the supervisor refused this connection: {}",
                e.message
            )),
        },
        Ok(other) => Err(format!(
            "the supervisor answered marion's hello with something else: {other:?}"
        )),
        Err(e) => Err(format!(
            "the supervisor's answer to marion's hello did not parse: {e}"
        )),
    }
}

/// Whom a connection to `paths` speaks for: the identity this process set, where it set one (a
/// node's bridge), else the operator of `paths`' state root.
pub fn identity_for(paths: &SocketPaths) -> Result<Identity, String> {
    identity().unwrap_or_else(|| operator(paths.state()))
}

/// **Dial `paths`' supervisor and say who the connection speaks for** — the one way a client
/// opens a connection it will make calls on.
pub fn dial(paths: &SocketPaths) -> Result<UnixStream, String> {
    let mut stream = UnixStream::connect(paths.socket())
        .map_err(|e| format!("dialling {}: {e}", paths.socket().display()))?;
    hello(&mut stream, &identity_for(paths)?)?;
    Ok(stream)
}

/// One line, read a byte at a time so nothing past it is taken off the stream.
fn read_line(stream: &mut UnixStream) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err("the supervisor closed the connection before answering".into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(format!(
                    "reading the supervisor's answer to marion's hello: {e}"
                ));
            }
        }
        if line.len() > 64 * 1024 {
            return Err("the supervisor's answer to marion's hello was not one line".into());
        }
    }
    String::from_utf8(line).map_err(|e| format!("the supervisor's answer was not text: {e}"))
}
