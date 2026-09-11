//! **A client of the supervisor: one connection, requests out, notifications in.**
//!
//! Shared by `client_run.rs`, `node_attach.rs` and `pane_attach.rs`, which each used to carry a
//! copy. The point of the type is that **nothing here reads a file**: every event a test sees
//! through it arrived over this socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use marion_core::contract::AgentId;
use marion_core::proto::notify::Event as Note;
use marion_core::proto::{Call, Frame, Method, MethodResult, Outcome, Request, RequestId};
use marion_supervisor::socket::{SocketPaths, socket_paths};

use super::getuid;

/// How long a single socket read may block before it is a failure. Never a verdict: the suites
/// that dial through this assert over identities, ordinals, counts, payloads and liveness
/// readings — never over elapsed time — and no failure may be repaired by widening this.
pub const BOUND: Duration = Duration::from_secs(180);

/// The supervisor's socket for `repo` under `state`, keyed the way marion keys it: on the
/// **canonical** project root (`socket::project_root`), not the path the test happened to spell.
pub fn paths_for(state: &Path, repo: &Path) -> SocketPaths {
    // SAFETY: reads the calling process's real uid and cannot fail.
    socket_paths(
        state,
        &marion_supervisor::socket::project_root(repo),
        unsafe { getuid() },
    )
}

/// One client, on one connection.
pub struct Client {
    sock: UnixStream,
    lines: BufReader<UnixStream>,
    next_id: i64,
}

impl Client {
    pub fn dial(paths: &SocketPaths) -> Client {
        let sock = UnixStream::connect(paths.socket()).expect("the supervisor is listening");
        sock.set_read_timeout(Some(BOUND)).unwrap();
        let lines = BufReader::new(sock.try_clone().unwrap());
        Client {
            sock,
            lines,
            next_id: 1,
        }
    }

    pub fn send(&mut self, call: Call) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let f = Frame::Request(Request::new(RequestId::Number(id), call));
        self.sock.write_all(f.to_line().as_bytes()).unwrap();
        self.sock.flush().unwrap();
        id
    }

    pub fn next_frame(&mut self) -> Frame {
        let mut line = String::new();
        let n = self.lines.read_line(&mut line).expect("a frame arrives");
        assert!(n > 0, "the supervisor closed the connection");
        Frame::from_line(&line).expect("the supervisor emits well-formed frames")
    }

    /// Read up to the response for `id`, returning the notifications that arrived first.
    ///
    /// The replay leg arrives **before** the answer, by construction — the handler sends it inside
    /// the call — so this ordering is itself part of what is asserted: the `ReplayPoint` in the
    /// answer is a statement about what has already been delivered.
    pub fn read_to_response(&mut self, id: i64) -> (Vec<Note>, Outcome) {
        let mut notes = Vec::new();
        loop {
            match self.next_frame() {
                Frame::Notification(n) => notes.push(n.event),
                Frame::Response(r) => {
                    assert_eq!(r.id, RequestId::Number(id), "answers are correlated");
                    return (notes, r.outcome);
                }
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    pub fn tree(&mut self) -> Vec<marion_core::proto::NodeSummary> {
        self.tree_at().nodes
    }

    /// The snapshot **and the read point it is as of**, which the criterion-2 test needs together:
    /// §7.3.3's seam is that a snapshot without its read point cannot be compared to anything, and
    /// `TreeSubscribeResult` carries the two because the supervisor builds them under one lock from
    /// one read (`handler::subscribe`). Splitting them here would re-introduce the race the
    /// protocol removed.
    pub fn tree_at(&mut self) -> marion_core::proto::result::TreeSubscribeResult {
        let id = self.send(Call::TreeSubscribe(
            marion_core::proto::params::TreeSubscribeParams {},
        ));
        let (_, outcome) = self.read_to_response(id);
        let Outcome::Result(body) = outcome else {
            panic!("tree/subscribe was refused: {outcome:?}")
        };
        let MethodResult::TreeSubscribe(s) = Method::TreeSubscribe.decode_result(&body).unwrap()
        else {
            panic!("wrong result")
        };
        s
    }

    /// `node/attach` **leases the write half** (§7.3.1). A suite whose real client holds it —
    /// `pane_attach.rs` — must never call this: it would be competing with the process under test
    /// for the keyboard.
    pub fn attach(
        &mut self,
        agent: &AgentId,
    ) -> (Vec<Note>, marion_core::proto::result::NodeAttachResult) {
        let id = self.send(Call::NodeAttach(
            marion_core::proto::params::NodeAttachParams {
                agent_id: agent.clone(),
                pane_stream: None,
            },
        ));
        let (notes, outcome) = self.read_to_response(id);
        let Outcome::Result(body) = outcome else {
            panic!("node/attach was refused: {outcome:?}")
        };
        let MethodResult::NodeAttach(r) = Method::NodeAttach.decode_result(&body).unwrap() else {
            panic!("wrong result")
        };
        (notes, r)
    }

    /// **Tighten this client's socket read timeout**, for the one wait whose expiry is itself the
    /// defect report.
    ///
    /// [`BOUND`] is three minutes, which is right for a wait that should normally succeed and whose
    /// failure means something is wrong somewhere unknown. It is wrong for clause (ii) of §9's
    /// criterion 4: a supervisor whose attach serves replay only never sends the event, so the
    /// wait's *expiry* is the finding, and at three minutes that finding arrives as a timeout
    /// instead of as a sentence. The whole test runs in about two seconds, so a budget an order of
    /// magnitude above that discriminates without being a race.
    pub fn read_bound(&mut self, d: Duration) {
        self.sock.set_read_timeout(Some(d)).unwrap();
    }

    /// [`Self::wait_for_event`], reporting an expiry **as the assertion it is**.
    ///
    /// `wait_for_event` panics with "a frame arrives" when the socket times out, which is true and
    /// tells the reader nothing. Here the absence of the event is the whole result, so it is said
    /// out loud.
    pub fn expect_event(
        &mut self,
        why: &str,
        want: impl FnMut(&Note) -> bool,
    ) -> (Vec<Note>, Note) {
        let r =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.wait_for_event(want)));
        match r {
            Ok(v) => v,
            Err(_) => panic!("{why}"),
        }
    }

    /// Read notifications until one satisfies `want`, or the read bound expires.
    ///
    /// **This is the only way a suite learns what a node said after an attach**, and it is a
    /// blocking read on the socket — there is no fallback to the file, so a supervisor that stops
    /// sending is a failure here rather than a slower success.
    pub fn wait_for_event(&mut self, mut want: impl FnMut(&Note) -> bool) -> (Vec<Note>, Note) {
        let mut seen = Vec::new();
        loop {
            match self.next_frame() {
                Frame::Notification(n) if want(&n.event) => return (seen, n.event),
                Frame::Notification(n) => seen.push(n.event),
                other => panic!("unexpected frame while following a node: {other:?}"),
            }
        }
    }

    /// §11 item 28 step 6's `agent/spawn` with **no caller** — a client creating a root.
    ///
    /// The criterion-4 test needs the tree to be created by a client whose departure it controls,
    /// which `marion run` is not: that process leaves when its root's turn is over, and the whole
    /// criterion is about a client leaving while a node is still mid-turn.
    pub fn spawn_root(&mut self, repo: &Path, prompt: &str, timeout_secs: u64) -> AgentId {
        let id = self.send(Call::AgentSpawn(
            marion_core::proto::params::AgentSpawnParams {
                agent_type: "claude".into(),
                prompt: prompt.into(),
                native_launch: None,
                caller: None,
                repo: Some(repo.to_path_buf()),
                acceptance_criteria: vec![],
                writable_scope: vec![],
                timeout_secs: Some(timeout_secs),
                model: None,
                no_change_record: None,
                pane: None,
                // A root: `isolation` and `allow_concurrent_writes` are child-only and refused
                // beside `caller: None` (§6.6, §9).
                isolation: None,
                allow_concurrent_writes: None,
            },
        ));
        let (_, outcome) = self.read_to_response(id);
        let Outcome::Result(body) = outcome else {
            panic!("agent/spawn was refused: {outcome:?}")
        };
        let MethodResult::AgentSpawn(r) = Method::AgentSpawn.decode_result(&body).unwrap() else {
            panic!("wrong result")
        };
        r.agent_id
    }

    /// §7.3.2's voluntary departure. Sent so that a supervisor whose only other client was killed
    /// does not wait out §5.7's full grace after the suite has finished with it — the same call
    /// `marion run` makes on its way out.
    pub fn quit(&mut self) -> marion_core::proto::QuitOutcome {
        let id = self.send(Call::SessionQuit(
            marion_core::proto::params::SessionQuitParams {
                disposition: marion_core::proto::QuitDisposition::DetachAll,
            },
        ));
        let (_, outcome) = self.read_to_response(id);
        let Outcome::Result(body) = outcome else {
            panic!("session/quit was refused: {outcome:?}")
        };
        let MethodResult::SessionQuit(r) = Method::SessionQuit.decode_result(&body).unwrap() else {
            panic!("wrong result")
        };
        r.outcome
    }
}
