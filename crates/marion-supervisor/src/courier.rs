//! **The per-child MCP bridge, as a socket client** — §11 item 28 step 5.
//!
//! # What moved, and what the bridge is now
//!
//! Until this, `marion-supervisor mcp` *was* the thing that ran a child: `handle_tool_call`'s
//! `spawn` arm called `run::run_spawn` in its own process, held the child's `Child`, its pipe and
//! its whole lifetime, and wrote its `events.jsonl`. The bridge is a process the **harness** starts
//! and the harness owns — s16 measured what that costs (SIGINT, SIGTERM 100 ms later, SIGKILL
//! ~450 ms after that, pid-targeted, no EOF at all), so every child of every node was a process
//! whose lifetime was bounded by a process marion does not control.
//!
//! Now the bridge dials §2's socket and sends `agent/spawn`. It carries a request and brings back
//! an answer; the node belongs to the supervisor for the whole of its life. Killing the bridge
//! kills a **courier**, which is what this module is named for, and `tests/background_spawn.rs`
//! measures it: the node keeps running and its stream keeps growing after the bridge is gone.
//!
//! # Three decisions, each of which had a cheaper alternative
//!
//! **1. No fallback to `run_spawn`, ever.** A dial that fails is a refusal in the bridge's own
//! voice. The cheap alternative — spawn in-process when the socket is not there — would make a
//! supervisor's absence invisible: the same tool result, the same contract, and a live node no
//! supervisor owned, could kill, or could hand to a re-attaching client. `bin/marion.rs` made this
//! same choice for the client at step 6 ([`crate::run`]'s callers are now the supervisor's own),
//! and a silent fallback is the failure class this repository keeps re-finding. If a node is
//! running at all then a supervisor spawned it, so nothing listening means the supervisor died —
//! which is news, not a condition to route around.
//!
//! **2. The socket path is derived, never configured.** §2: *"the per-child MCP bridge derives this
//! path by the same rule"*. [`crate::socket::socket_paths`] over `<state>` and
//! `git rev-parse --git-common-dir`, exactly as `marion run` and `detach` compute it. A new
//! environment variable would be a second spelling of one fact, and the way a bridge ends up
//! talking to a supervisor for a different project.
//!
//! **3. The synchronous `spawn` is a composition, not a method.** §5.4's `spawn` must return the
//! child's `TaskContract`; `agent/spawn` answers as soon as the process exists, because a call that
//! blocked for the length of a child's run would put a minutes-long request on the JSON-RPC surface
//! — which is exactly what took a whole bridge down when it hung. So the tool call blocks and the
//! wire does not: `agent/spawn` → `node/attach` → read the node's own stream until its terminal
//! bookend → read `contracts/<task_id>.json`. [`marion_proto::Method::ALL`] stays at fifteen.
//!
//! The reader may act on that bookend because `run_spawn` writes the contract *before* it — an
//! ordering that is now explicit and commented there, since without it this module would race one
//! `write(2)` and report a finished child as one that produced nothing.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marion_core::cap::cap_for_return;
use marion_core::contract::{AgentId, TaskContract, TaskId};
use marion_core::event::{Lifecycle, Payload};
use marion_core::paths::ProjectDir;
use marion_proto::params::AgentSpawnParams;
use marion_proto::result::AgentSpawnResult;
use marion_proto::{Call, Frame, Method, MethodResult, Outcome, Request, RequestId};

use crate::spawn::SpawnError;

/// One connection to this project's supervisor, held for exactly one errand.
///
/// **Per errand and not per bridge**, deliberately. A held connection would make the bridge a
/// client the supervisor counts for §5.7's idle-exit predicate for the whole life of the harness
/// session — so a supervisor with one idle root's bridge attached could never reach zero clients
/// and could never leave. A courier dials, asks, and hangs up; `Handle::gone` touches nothing, so
/// hanging up costs the node nothing either.
struct Conn {
    socket: PathBuf,
    stream: UnixStream,
    lines: BufReader<UnixStream>,
    /// **Carried across reads, because a read can time out mid-line.** `read_line` appends what it
    /// got before the deadline and then reports the error; a fresh `String` per attempt would drop
    /// those bytes and the next read would parse the tail of a frame as a whole one.
    pending: String,
    next_id: i64,
}

/// What a read produced, with "nothing yet" distinguished from "nothing ever".
enum Next {
    Frame(Box<Frame>),
    /// The socket's read timeout expired. Not an error: on the waiting path it is how a bound is
    /// enforced without a second thread.
    Expired,
}

impl Conn {
    fn dial(socket: &Path) -> Result<Self, SpawnError> {
        let stream =
            UnixStream::connect(socket).map_err(|e| unreachable(socket, &e.to_string()))?;
        let lines = BufReader::new(stream.try_clone().map_err(|e| {
            unreachable(socket, &format!("its connection could not be split: {e}"))
        })?);
        Ok(Self {
            socket: socket.to_path_buf(),
            stream,
            lines,
            pending: String::new(),
            next_id: 1,
        })
    }

    fn send(&mut self, call: Call) -> Result<i64, SpawnError> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = Frame::Request(Request::new(RequestId::Number(id), call));
        self.stream
            .write_all(frame.to_line().as_bytes())
            .and_then(|()| self.stream.flush())
            .map_err(|e| unreachable(&self.socket, &e.to_string()))?;
        Ok(id)
    }

    /// Bound the next read, or stop rather than read unbounded.
    ///
    /// A timeout that cannot be set is a reason to refuse, not a reason to read without one: an
    /// unbounded read here blocks the bridge's single-threaded dispatch loop, so one child's stall
    /// would be every later frame from every caller going unread.
    fn bound(&self, remaining: Duration) -> Result<(), SpawnError> {
        // Zero means "no timeout" to the kernel, which is the one value this must never set.
        let d = remaining.max(Duration::from_millis(1));
        self.stream.set_read_timeout(Some(d)).map_err(|e| {
            unreachable(
                &self.socket,
                &format!("marion could not bound a read on it: {e}"),
            )
        })
    }

    fn next(&mut self) -> Result<Next, SpawnError> {
        match self.lines.read_line(&mut self.pending) {
            Ok(0) => Err(unreachable(
                &self.socket,
                "it closed the connection while marion was still waiting for this node",
            )),
            Ok(_) => {
                let line = std::mem::take(&mut self.pending);
                Frame::from_line(&line)
                    .map(|f| Next::Frame(Box::new(f)))
                    .map_err(|e| {
                        unreachable(
                            &self.socket,
                            &format!("it sent a frame marion cannot read: {e}"),
                        )
                    })
            }
            Err(e) if timed_out(&e) => Ok(Next::Expired),
            Err(e) => Err(unreachable(&self.socket, &e.to_string())),
        }
    }
}

/// `WouldBlock` on macOS, `TimedOut` on Linux — the same event under two names.
fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn unreachable(socket: &Path, why: &str) -> SpawnError {
    SpawnError::SupervisorUnreachable {
        socket: socket.to_path_buf(),
        why: why.to_string(),
    }
}

/// **Ask this project's supervisor to start a child, and return when the process exists.**
///
/// The answer is `agent/spawn`'s own: an id, a state the supervisor read back off its registry, and
/// the `task_id` that names the contract file this run will be filed under. Nothing here waits for
/// the run.
pub fn spawn(socket: &Path, params: AgentSpawnParams) -> Result<AgentSpawnResult, SpawnError> {
    let mut c = Conn::dial(socket)?;
    // A spawn is answered when the child's process exists (`LAUNCH_BOUND` on the far side), so this
    // is generous by design and is not a bound on the child's run — that is [`await_contract`]'s.
    c.bound(SPAWN_ANSWER_BOUND)?;
    let id = c.send(Call::AgentSpawn(params))?;
    loop {
        match c.next()? {
            // Nothing is subscribed on this connection, so a notification here is the supervisor
            // being chatty rather than news for this caller. Skipped, never treated as an answer.
            Next::Frame(f) => match *f {
                Frame::Response(r) if r.id == RequestId::Number(id) => {
                    return match r.outcome {
                        Outcome::Result(body) => match Method::AgentSpawn.decode_result(&body) {
                            Ok(MethodResult::AgentSpawn(r)) => Ok(r),
                            _ => Err(unreachable(
                                socket,
                                "it answered `agent/spawn` with a result marion cannot read",
                            )),
                        },
                        // **The supervisor's own sentence, carried verbatim.** It already names the
                        // rule and the value — `max_depth`, `max_concurrent_children`, an unknown
                        // agent type, a token this supervisor did not mint — and re-wording it here
                        // would put the bridge's guess in front of the answer.
                        Outcome::Error(e) => Err(SpawnError::SupervisorRefused(e.message)),
                    };
                }
                Frame::Notification(_) => continue,
                other => {
                    return Err(unreachable(
                        socket,
                        &format!(
                            "it sent an unexpected frame in answer to `agent/spawn`: {other:?}"
                        ),
                    ));
                }
            },
            Next::Expired => {
                return Err(unreachable(
                    socket,
                    &format!(
                        "it did not answer `agent/spawn` within {} s; nothing here can say whether \
                         a node was started, so the journal is the record to read",
                        SPAWN_ANSWER_BOUND.as_secs()
                    ),
                ));
            }
        }
    }
}

/// How long the bridge will wait for `agent/spawn` itself to answer.
///
/// The supervisor answers this call when the child's **process exists**, which it bounds on its own
/// side (`handler::LAUNCH_BOUND`) across a worktree, the config documents and a `--version` probe.
/// This is that bound plus room, so the bridge's clock is never the one that decides — a courier
/// that gave up first would report "no answer" about a node that was starting normally.
const SPAWN_ANSWER_BOUND: Duration = Duration::from_secs(300);

/// What a wait on a child produced.
#[derive(Debug)]
pub enum Delivered {
    /// The node reached a terminal state and its contract is this.
    Contract(Box<TaskContract>),
    /// **The bound expired and the node is still going.** Not a failure of the node, not a timeout
    /// on the node, and not a terminal state: the node keeps its own wall clock and its handle
    /// stays valid. What expired is marion's willingness to hold this caller's turn — and, because
    /// the bridge dispatches frames on one thread, every other caller's turn behind it.
    StillRunning,
}

/// **Attach to a node, read its stream until it ends, and hand back the contract it wrote.**
///
/// This is the blocking half of §5.4's synchronous `spawn`, and the whole of `wait`. It is a
/// *reader*: it starts nothing, owns nothing and can be abandoned at any point without touching the
/// node — which is the property that makes the bridge's own death survivable.
///
/// **`node/attach` and not `tree/subscribe`**, though the design's sketch said the latter. A
/// subscription to the tree delivers state transitions for every node in the fleet and would make
/// this caller's answer depend on the registry's view of a node rather than on the node's own
/// stream; the attach delivers exactly one node's `events.jsonl` through one cursor, replay leg
/// first, with no seam between the replay and the live legs (`events.rs`). A child that finished
/// before this call is therefore answered out of the replay, which is the common case for a `wait`.
pub fn await_contract(
    socket: &Path,
    project: &ProjectDir,
    agent_id: &AgentId,
    task_id: &TaskId,
    bound: Duration,
) -> Result<Delivered, SpawnError> {
    let deadline = Instant::now() + bound;
    let mut c = Conn::dial(socket)?;
    c.bound(bound)?;
    let attach = c.send(Call::NodeAttach(marion_proto::params::NodeAttachParams {
        agent_id: agent_id.clone(),
    }))?;
    let mut ended: Option<Lifecycle> = None;
    while ended.is_none() {
        c.bound(deadline.saturating_duration_since(Instant::now()))?;
        match c.next()? {
            Next::Expired => return Ok(Delivered::StillRunning),
            Next::Frame(f) => match *f {
                Frame::Notification(n) => ended = terminal_of(agent_id, n.event),
                Frame::Response(r) if r.id == RequestId::Number(attach) => {
                    if let Outcome::Error(e) = r.outcome {
                        return Err(SpawnError::SupervisorRefused(e.message));
                    }
                    // A node that had already reached a terminal reading is `ReplayOnly` and its
                    // bookend arrived in the replay above, so `ended` is already set and this loop
                    // is over. Anything else is live, and the frames after this response are it.
                }
                other => {
                    return Err(unreachable(
                        socket,
                        &format!("it sent an unexpected frame while a node was running: {other:?}"),
                    ));
                }
            },
        }
    }
    match ended {
        Some(Lifecycle::Aborted { reason }) => Err(SpawnError::NodeAborted(reason)),
        // `Opened` never ends the loop (see [`terminal_of`]), so the only remaining arm is `Exited`.
        _ => read_contract(project, agent_id, task_id).map(|c| Delivered::Contract(Box::new(c))),
    }
}

/// The node's own closing bookend, if this event is one.
///
/// Filtered by `agent_id` because one connection can legitimately carry more: `tree/node-added` and
/// `node/state` are not this reader's news, and neither is another node's stream.
fn terminal_of(agent_id: &AgentId, event: marion_proto::notify::Event) -> Option<Lifecycle> {
    let marion_proto::notify::Event::NodeEvent {
        agent_id: id,
        payload,
        ..
    } = event
    else {
        return None;
    };
    if &id != agent_id {
        return None;
    }
    match serde_json::from_value::<Payload>(payload) {
        Ok(Payload::Lifecycle(l @ (Lifecycle::Exited { .. } | Lifecycle::Aborted { .. }))) => {
            Some(l)
        }
        _ => None,
    }
}

/// **§6.7's contract, read from the file that is the contract.**
///
/// Capped here with the same [`cap_for_return`] `run_spawn` applied when the bridge ran children
/// itself, so what a caller receives is unchanged by the ownership move: the persisted copy stays
/// whole (`worktree_reap.rs` pins that) and the copy that goes into a model's context stays
/// bounded. Two paths to one tool result must not differ in how much of a narrative they carry.
fn read_contract(
    project: &ProjectDir,
    agent_id: &AgentId,
    task_id: &TaskId,
) -> Result<TaskContract, SpawnError> {
    let path = project.agent(agent_id).contract(task_id);
    let bytes = std::fs::read(&path).map_err(|e| SpawnError::NoContract {
        path: path.clone(),
        why: e.to_string(),
    })?;
    serde_json::from_slice::<TaskContract>(&bytes)
        .map(cap_for_return)
        .map_err(|e| SpawnError::NoContract {
            path,
            why: format!("marion wrote it and cannot read it back: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A dial that finds nothing is a refusal naming the socket, and it starts nothing.**
    ///
    /// The in-process backstop for `background_spawn.rs`'s end-to-end
    /// `a_spawn_whose_supervisor_is_not_listening_is_refused_and_starts_nothing`: this one runs in
    /// milliseconds and cannot be satisfied by a timeout. What it pins is that the failure is a
    /// *typed refusal* rather than a fallback — a `run_spawn` restored behind this call would have
    /// to make this function return `Ok` for a path nothing is listening on.
    #[test]
    fn a_spawn_with_no_supervisor_listening_is_refused_and_names_the_socket() {
        let socket = std::path::Path::new("/tmp/marion-no-such-supervisor.sock");
        let e = spawn(
            socket,
            AgentSpawnParams {
                agent_type: "codex-impl".into(),
                prompt: "nothing may be started by this call".into(),
                caller: Some(marion_proto::SpawnCaller {
                    agent_id: AgentId("019f-node".into()),
                    node_token: "tok".into(),
                }),
                repo: None,
                acceptance_criteria: vec![],
                writable_scope: vec![],
                timeout_secs: Some(1),
                model: None,
                no_change_record: None,
            },
        )
        .expect_err("nothing is listening there");
        let msg = e.to_string();
        assert!(
            msg.contains("marion-no-such-supervisor.sock"),
            "the refusal names the socket it could not reach, since that is the whole diagnosis: \
             {msg}"
        );
        assert!(
            msg.contains("supervisor"),
            "and says what was not there: {msg}"
        );
        assert!(
            msg.contains("Nothing was started"),
            "and that nothing happened, which is the claim a fallback would make false: {msg}"
        );
    }

    /// A `wait` whose supervisor is gone is the same refusal, from the other verb. It matters
    /// separately because the two paths dial independently — a `wait` that fell back to reading the
    /// contract file directly would be answering about a node nobody is watching.
    #[test]
    fn a_wait_with_no_supervisor_listening_is_refused_rather_than_read_off_disk() {
        let dir = marion_testsupport::scratch("courier-wait-no-supervisor");
        let project = ProjectDir::new(&dir, std::path::Path::new("/canonical/project"));
        let e = await_contract(
            std::path::Path::new("/tmp/marion-no-such-supervisor.sock"),
            &project,
            &AgentId("019f-node".into()),
            &TaskId("task-1".into()),
            Duration::from_secs(1),
        )
        .expect_err("nothing is listening there");
        assert!(
            matches!(e, SpawnError::SupervisorUnreachable { .. }),
            "a missing supervisor is a missing supervisor, whichever verb asked: {e}"
        );
    }
}
