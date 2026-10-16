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

/// **How long a hello waits for its answer.** A supervisor answers one at once; silence this long is
/// a socket nobody behind it will answer — an older listener a starting supervisor has not replaced
/// yet — and a client blocked on it would wait for ever.
#[cfg(not(test))]
pub const HELLO_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
pub const HELLO_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// Why a hello did not go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelloError {
    /// Nothing answered within [`HELLO_WAIT`]: whatever holds the socket is not answering.
    Unanswered(String),
    /// A refusal, or a stream that could not carry the exchange.
    Failed(String),
}

impl std::fmt::Display for HelloError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HelloError::Unanswered(m) | HelloError::Failed(m) => f.write_str(m),
        }
    }
}

/// **Say who `stream` speaks for, and read the supervisor's one answer**, within [`HELLO_WAIT`].
/// `Err` is the supervisor's refusal, a stream that could not carry the exchange, or silence, as a
/// sentence.
pub fn hello(stream: &mut UnixStream, identity: &Identity) -> Result<(), HelloError> {
    // Before a byte of the key leaves: the hello names the operator's key or a node's token.
    served_by_us(crate::serve::peer_of(stream)).map_err(HelloError::Failed)?;
    // The connection outlives its hello, and keeps whatever bound its owner gave its reads.
    let before = stream
        .read_timeout()
        .map_err(|e| HelloError::Failed(format!("reading the connection's read bound: {e}")))?;
    stream
        .set_read_timeout(Some(before.map_or(HELLO_WAIT, |b| b.min(HELLO_WAIT))))
        .map_err(|e| HelloError::Failed(format!("bounding marion's hello: {e}")))?;
    let answered = exchange(stream, identity);
    stream
        .set_read_timeout(before)
        .map_err(|e| HelloError::Failed(format!("restoring the connection's read bound: {e}")))?;
    answered
}

fn exchange(stream: &mut UnixStream, identity: &Identity) -> Result<(), HelloError> {
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
        .map_err(|e| {
            HelloError::Failed(format!("the supervisor did not take marion's hello: {e}"))
        })?;
    let line = read_line(stream)?;
    match Frame::from_line(&line) {
        Ok(Frame::Response(r)) if r.id == RequestId::Text(HELLO_ID.into()) => match r.outcome {
            Outcome::Result(_) => Ok(()),
            Outcome::Error(e) => Err(HelloError::Failed(format!(
                "the supervisor refused this connection: {}",
                e.message
            ))),
        },
        Ok(other) => Err(HelloError::Failed(format!(
            "the supervisor answered marion's hello with something else: {other:?}"
        ))),
        Err(e) => Err(HelloError::Failed(format!(
            "the supervisor's answer to marion's hello did not parse: {e}"
        ))),
    }
}

/// **Whether the process on the other end of a dialed socket is this user's**, as the kernel
/// reports it — the check a client makes before it sends a key. The socket's directory is the
/// operator's own, or the audited `/tmp` fallback, but a name in a directory is not proof of who
/// bound it, and a listener of another uid would be handed the operator's capability. An unread
/// peer is refused as a foreign one is.
pub(crate) fn served_by_us(peer: crate::serve::Peer) -> Result<(), String> {
    let ours = crate::socket::own_uid();
    match peer {
        crate::serve::Peer::Uid(uid) if uid == ours => Ok(()),
        crate::serve::Peer::Uid(uid) => Err(format!(
            "the socket is served by uid {uid}, not by this user (uid {ours}); marion sends its \
             key to no one else's process"
        )),
        crate::serve::Peer::Unknown => {
            Err("the kernel would not say who serves this socket, so marion sends it no key".into())
        }
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
    crate::socket::check_fallback_dir(paths).map_err(|e| e.to_string())?;
    let stream = UnixStream::connect(paths.socket()).map_err(|e| dialling(paths, &e))?;
    hello_on(stream, paths)
}

/// **[`dial`], or `None` when the supervisor is leaving**: nothing listens on the socket, and its
/// lock goes within [`crate::socket::LEAVING`] ([`crate::socket::left_within`]). For a caller that
/// found the lock held, for whom a supervisor that has just gone is nobody serving. Only a socket
/// with no listener waits; a supervisor that answers and refuses the hello is refused at once.
pub fn dial_unless_leaving(paths: &SocketPaths) -> Result<Option<UnixStream>, String> {
    crate::socket::check_fallback_dir(paths).map_err(|e| e.to_string())?;
    let stream = match UnixStream::connect(paths.socket()) {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) && crate::socket::left_within(paths, crate::socket::LEAVING) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(dialling(paths, &e)),
    };
    hello_on(stream, paths).map(Some)
}

fn dialling(paths: &SocketPaths, e: &std::io::Error) -> String {
    format!("dialling {}: {e}", paths.socket().display())
}

fn hello_on(mut stream: UnixStream, paths: &SocketPaths) -> Result<UnixStream, String> {
    hello(&mut stream, &identity_for(paths)?).map_err(|e| e.to_string())?;
    Ok(stream)
}

/// One line, read a byte at a time so nothing past it is taken off the stream.
fn read_line(stream: &mut UnixStream) -> Result<String, HelloError> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(HelloError::Failed(format!(
                    "the supervisor closed the connection before answering; it closes one past \
                     its limit of {} clients at once",
                    crate::serve::MAX_CLIENTS
                )));
            }
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(HelloError::Unanswered(format!(
                    "nothing answered marion's hello within {} ms",
                    HELLO_WAIT.as_millis()
                )));
            }
            Err(e) => {
                return Err(HelloError::Failed(format!(
                    "reading the supervisor's answer to marion's hello: {e}"
                )));
            }
        }
        if line.len() > 64 * 1024 {
            return Err(HelloError::Failed(
                "the supervisor's answer to marion's hello was not one line".into(),
            ));
        }
    }
    String::from_utf8(line)
        .map_err(|e| HelloError::Failed(format!("the supervisor's answer was not text: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A key goes only to a socket this user's process serves**: another uid, or a peer the
    /// kernel would not name, is refused before the hello is written.
    #[test]
    fn a_key_goes_only_to_a_socket_this_user_serves() {
        use crate::serve::Peer;
        let ours = crate::socket::own_uid();
        assert_eq!(served_by_us(Peer::Uid(ours)), Ok(()));
        let foreign = served_by_us(Peer::Uid(ours.wrapping_add(1))).unwrap_err();
        assert!(foreign.contains("not by this user"), "{foreign}");
        assert!(served_by_us(Peer::Unknown).is_err());
    }

    /// **A socket nobody answers on is reported as silence, not waited on for ever** — the listener
    /// a starting supervisor has not yet replaced — and the connection keeps no bound after it.
    #[test]
    fn a_hello_nobody_answers_is_unanswered_within_its_wait() {
        let (mut ours, _silent) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        let e = hello(
            &mut ours,
            &Identity::Operator(marion_core::secret::Secret::new("k")),
        )
        .expect_err("nobody answered");
        assert!(matches!(e, HelloError::Unanswered(_)), "{e}");
        assert!(
            started.elapsed() < HELLO_WAIT * 10,
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            ours.read_timeout().unwrap(),
            None,
            "the bound is the hello's alone"
        );
    }
}
