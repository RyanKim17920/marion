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
//! bookend → read `contracts/<task_id>.json`. [`marion_core::proto::Method::ALL`] stays at fifteen.
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
use marion_core::proto::params::{
    AgentSpawnParams, NodeCancelParams, NodeGetParams, NodeKillParams, NodeSteerParams,
    TreeSubscribeParams,
};
use marion_core::proto::result::{
    AgentSpawnResult, DeliveryResult, NodeCancelResult, NodeGetResult, NodeKillResult,
    TreeSubscribeResult,
};
use marion_core::proto::{Call, Frame, MethodResult, Outcome, Request, RequestId};

use crate::socket::SocketPaths;
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
    fn dial(sock: &SocketPaths) -> Result<Self, SpawnError> {
        let socket = sock.socket();
        let stream = crate::client_auth::dial(sock).map_err(|e| unreachable(socket, &e))?;
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

    /// **One call, its own answer, and nothing else** — the correlation loop every request-shaped
    /// errand in this module runs.
    ///
    /// Written once because all three of its callers had it letter for letter, and the three
    /// clauses that make it correct are each easy to drop when copying: the answer is matched on
    /// **this** request's id, a notification is *skipped* rather than mistaken for the answer, and
    /// an expiry is a refusal rather than a silent `None`. A fourth verb added by copy-paste is how
    /// one of those goes missing.
    ///
    /// `on_expiry` is the caller's, because what an unanswered call leaves behind differs by verb —
    /// an `agent/spawn` may or may not have started a process, while a `node/get` has changed
    /// nothing — and a generic sentence would have to be silent about exactly that.
    fn ask(
        &mut self,
        call: Call,
        bound: Duration,
        on_expiry: &str,
    ) -> Result<MethodResult, SpawnError> {
        let method = call.method();
        self.bound(bound)?;
        let id = self.send(call)?;
        loop {
            match self.next()? {
                Next::Frame(f) => match *f {
                    Frame::Response(r) if r.id == RequestId::Number(id) => {
                        return match r.outcome {
                            Outcome::Result(body) => method.decode_result(&body).map_err(|_| {
                                unreachable(
                                    &self.socket,
                                    &format!(
                                        "it answered `{}` with a result marion cannot read",
                                        method.as_str()
                                    ),
                                )
                            }),
                            // **The supervisor's own sentence, carried verbatim.** It already names
                            // the rule and the value — `max_depth`, an unknown agent type, a node
                            // this project's journal has no record of — and re-wording it here
                            // would put the bridge's guess in front of the answer.
                            Outcome::Error(e) => Err(SpawnError::SupervisorRefused(e.message)),
                        };
                    }
                    // Nothing this module dials is subscribed for its own sake, so a notification
                    // is the supervisor being chatty rather than news for this caller. Skipped,
                    // never treated as an answer — `tree/subscribe` in particular registers the
                    // connection as a subscriber, so its answer can arrive behind a burst of them.
                    Frame::Notification(_) => continue,
                    other => {
                        return Err(unreachable(
                            &self.socket,
                            &format!(
                                "it sent an unexpected frame in answer to `{}`: {other:?}",
                                method.as_str()
                            ),
                        ));
                    }
                },
                Next::Expired => return Err(unreachable(&self.socket, on_expiry)),
            }
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
pub fn spawn(sock: &SocketPaths, params: AgentSpawnParams) -> Result<AgentSpawnResult, SpawnError> {
    // A spawn is answered when the child's process exists (`LAUNCH_BOUND` on the far side), so this
    // is generous by design and is not a bound on the child's run — that is [`await_contract`]'s.
    match Conn::dial(sock)?.ask(
        Call::AgentSpawn(params),
        SPAWN_ANSWER_BOUND,
        &format!(
            "it did not answer `agent/spawn` within {} s; nothing here can say whether a node was \
             started, so the journal is the record to read",
            SPAWN_ANSWER_BOUND.as_secs()
        ),
    )? {
        MethodResult::AgentSpawn(r) => Ok(r),
        _ => Err(unreachable(
            sock.socket(),
            "it answered `agent/spawn` with a result marion cannot read",
        )),
    }
}

/// **What the supervisor currently knows about one node** — §5.4's `status`, on the wire.
///
/// `node/get` is the whole of it: the supervisor projects the node out of the registry it wrote
/// itself and answers one [`NodeSummary`]. Nothing is cached here and nothing is remembered between
/// calls, which is the property `status` exists to have — a caller asks *because* the answer may
/// have changed, and a stale one is worse than no answer.
///
/// A node this project's journal has no record of comes back as the supervisor's own `not_found`
/// sentence through [`SpawnError::SupervisorRefused`], which already names the id and says how
/// current the registry is.
pub fn node_get(sock: &SocketPaths, agent_id: &AgentId) -> Result<NodeGetResult, SpawnError> {
    node_get_with(sock, agent_id, None)
}

/// [`node_get`], with a page of the node's activity stream from `activity` when it is given.
pub fn node_get_with(
    sock: &SocketPaths,
    agent_id: &AgentId,
    activity: Option<marion_core::proto::params::ActivityCursor>,
) -> Result<NodeGetResult, SpawnError> {
    match Conn::dial(sock)?.ask(
        Call::NodeGet(NodeGetParams {
            agent_id: agent_id.clone(),
            activity,
        }),
        READ_ANSWER_BOUND,
        &format!(
            "it did not answer `node/get` within {} s, so marion cannot say what state that node \
             is in. Nothing was changed by asking.",
            READ_ANSWER_BOUND.as_secs()
        ),
    )? {
        MethodResult::NodeGet(r) => Ok(r),
        _ => Err(unreachable(
            sock.socket(),
            "it answered `node/get` with a result marion cannot read",
        )),
    }
}

/// **Queue a message for one node's next turn** — §2's `node/steer`, the call behind `marion
/// steer`, the MCP `steer` tool and the tree's `s`.
///
/// `caller` is `None` for a top-level peer (the operator's CLI, the tree, a `marion mcp`) and the
/// node's own [`marion_core::proto::SpawnCaller`] for a node's bridge; the supervisor decides what
/// either may steer, and a refusal is its sentence, carried verbatim.
///
/// On acceptance one `node/get` follows, for the target's agent type — the one fact the sentence
/// needs that `node/steer`'s answer does not carry. It is asked only *after* the supervisor has
/// authorized the steer, so it tells a caller nothing about a node it may not address; and it is
/// best-effort, because the message is already queued and a failed read must not report it lost.
pub fn steer(
    sock: &SocketPaths,
    agent_id: &AgentId,
    text: &str,
    caller: Option<marion_core::proto::SpawnCaller>,
) -> Result<Steered, SpawnError> {
    let result = match Conn::dial(sock)?.ask(
        Call::NodeSteer(NodeSteerParams {
            agent_id: agent_id.clone(),
            text: text.to_string(),
            caller,
        }),
        READ_ANSWER_BOUND,
        &format!(
            "it did not answer `node/steer` within {} s, so marion cannot say whether the message \
             was queued; the journal's `MessageQueued` records are the answer",
            READ_ANSWER_BOUND.as_secs()
        ),
    )? {
        MethodResult::NodeSteer(r) => r,
        _ => {
            return Err(unreachable(
                sock.socket(),
                "it answered `node/steer` with a result marion cannot read",
            ));
        }
    };
    let agent_type = node_get(sock, agent_id).ok().map(|r| r.node.agent_type);
    Ok(Steered {
        agent_id: agent_id.clone(),
        agent_type,
        result,
    })
}

/// **Tell the supervisor a node read one of its children's end for itself** — §2's
/// `node/collected`, sent by a node's bridge once its `wait` returned the child's end or its
/// `status` found the child finished, so marion spends no turn of the node announcing it. `true`
/// iff a queued announcement was withdrawn.
pub fn collected(
    sock: &SocketPaths,
    child: &AgentId,
    caller: marion_core::proto::SpawnCaller,
) -> Result<bool, SpawnError> {
    match Conn::dial(sock)?.ask(
        Call::NodeCollected(marion_core::proto::params::NodeCollectedParams {
            agent_id: child.clone(),
            caller,
        }),
        READ_ANSWER_BOUND,
        &format!(
            "it did not answer `node/collected` within {} s, so the child's end may still be \
             announced to its parent",
            READ_ANSWER_BOUND.as_secs()
        ),
    )? {
        MethodResult::NodeCollected(r) => Ok(r.withdrawn),
        _ => Err(unreachable(
            sock.socket(),
            "it answered `node/collected` with a result marion cannot read",
        )),
    }
}

/// **End one node, as the operator** — §2's `node/kill`, which §6.7 records as `Cancelled`.
///
/// The supervisor decides whether it may and whether there is anything to signal (a node already
/// ended, or one still spawning with no pid, is refused before anything is signalled); a refusal
/// is its sentence, carried verbatim.
pub fn kill(sock: &SocketPaths, agent_id: &AgentId) -> Result<NodeKillResult, SpawnError> {
    match Conn::dial(sock)?.ask(
        Call::NodeKill(NodeKillParams {
            agent_id: agent_id.clone(),
        }),
        READ_ANSWER_BOUND,
        &format!(
            "it did not answer `node/kill` within {} s, so marion cannot say whether the node was \
             ended; `marion list` shows its state",
            READ_ANSWER_BOUND.as_secs()
        ),
    )? {
        MethodResult::NodeKill(r) => Ok(r),
        _ => Err(unreachable(
            sock.socket(),
            "it answered `node/kill` with a result marion cannot read",
        )),
    }
}

/// **Cancel one node and everything below it, as the operator** — §2's `node/cancel`: each running
/// turn is ended by its row's abort, bottom-up, and whatever outlives its grace is killed. The
/// answer names every node ended and whether it had to be killed. A refusal is the supervisor's
/// sentence, carried verbatim.
pub fn cancel(sock: &SocketPaths, agent_id: &AgentId) -> Result<NodeCancelResult, SpawnError> {
    cancel_as(sock, agent_id, None)
}

/// [`cancel`] as `caller` — a node ending one of its descendants, proved by its token — or as the
/// operator where `caller` is `None`.
pub fn cancel_as(
    sock: &SocketPaths,
    agent_id: &AgentId,
    caller: Option<marion_core::proto::SpawnCaller>,
) -> Result<NodeCancelResult, SpawnError> {
    match Conn::dial(sock)?.ask(
        Call::NodeCancel(NodeCancelParams {
            agent_id: agent_id.clone(),
            caller,
        }),
        CANCEL_ANSWER_BOUND,
        &format!(
            "it did not answer `node/cancel` within {} s, so marion cannot say whether the node \
             was ended; `marion list` shows its state",
            CANCEL_ANSWER_BOUND.as_secs()
        ),
    )? {
        MethodResult::NodeCancel(r) => Ok(r),
        _ => Err(unreachable(
            sock.socket(),
            "it answered `node/cancel` with a result marion cannot read",
        )),
    }
}

/// How long a `node/cancel` may take to answer: it waits out one grace per depth level of the
/// subtree (each at most [`marion_harness::spec::MAX_CANCEL_GRACE_MS`]) plus the time each level's
/// nodes take to commit their partial work. Minutes rather than [`READ_ANSWER_BOUND`]'s seconds,
/// and still a bound: a supervisor that never answers is reported, not waited on for ever.
const CANCEL_ANSWER_BOUND: Duration = Duration::from_secs(300);

/// An accepted steer: the supervisor's answer and the node it is for.
#[derive(Debug, Clone)]
pub struct Steered {
    pub agent_id: AgentId,
    /// `None` when the follow-up `node/get` could not be read.
    pub agent_type: Option<String>,
    pub result: DeliveryResult,
}

impl Steered {
    /// **The one sentence every steer surface prints on acceptance**: the message's id and when the
    /// node takes it. "Queued", never "delivered", while the supervisor says `queued` — a queued
    /// message reaches the model only at a boundary, and which one is the node's row: the
    /// supervisor words it (`DeliveryResult::arrives`), and this sentence carries it verbatim. A
    /// supervisor that predates the field names no row, so the sentence promises no more than a
    /// turn boundary.
    pub fn sentence(&self) -> String {
        let short = crate::tree::short_id(&self.agent_id.0);
        let who = match &self.agent_type {
            Some(t) => format!("{t} {short}"),
            None => short.to_string(),
        };
        let id = self
            .result
            .message_id
            .as_deref()
            .map_or_else(String::new, |m| format!(" as {m}"));
        if self.result.queued {
            let when = self
                .result
                .arrives
                .as_deref()
                .unwrap_or("at its next turn boundary");
            format!("queued{id}; reaches {who} {when}")
        } else {
            format!("delivered{id} to {who}")
        }
    }
}

/// **Every node this project's supervisor is holding** — §5.4's `list`, on the wire.
///
/// `tree/subscribe` is the only one of §2's fifteen methods that *enumerates* nodes, so it is what
/// discovery composes over; a sixteenth method for the snapshot alone would be a second way to ask
/// one question. Its answer carries the snapshot (`nodes`) and the journal position it was read at,
/// and the subscription it also opens dies with the connection this courier hangs up — see
/// [`Conn`]'s own note on why one errand is one connection.
pub fn tree(sock: &SocketPaths) -> Result<TreeSubscribeResult, SpawnError> {
    match Conn::dial(sock)?.ask(
        Call::TreeSubscribe(TreeSubscribeParams {}),
        READ_ANSWER_BOUND,
        &format!(
            "it did not answer `tree/subscribe` within {} s, so marion cannot say which nodes it \
             is holding. Nothing was changed by asking.",
            READ_ANSWER_BOUND.as_secs()
        ),
    )? {
        MethodResult::TreeSubscribe(r) => Ok(r),
        _ => Err(unreachable(
            sock.socket(),
            "it answered `tree/subscribe` with a result marion cannot read",
        )),
    }
}

/// How long a courier waits for a **read** — `node/get`, `tree/subscribe` — to come back.
///
/// Far shorter than [`SPAWN_ANSWER_BOUND`], and the difference is the point: a spawn's answer is
/// gated on a real process starting, while both reads are a registry projection under one lock with
/// no I/O of their own. Seconds here means a supervisor that is wedged is reported as wedged
/// instead of holding a model's turn for five minutes over a question that cannot legitimately take
/// that long.
const READ_ANSWER_BOUND: Duration = Duration::from_secs(30);

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
    /// **The node reached a terminal state and there is no contract**, because §9 gives a *root*
    /// none.
    ///
    /// Not a degraded [`Self::Contract`] and not an error: it is the complete answer for a node of
    /// that kind, carrying the same two facts the contract's own `exit` block would have carried.
    /// Distinguished from [`crate::spawn::SpawnError::NoContract`] — which says marion expected a
    /// file and could not read it — because a root's absence is a property of §9 and a child's is
    /// a fault. Collapsing them would report every successful root as a broken child.
    Ended {
        status: marion_core::contract::ExitStatus,
        exit: Box<marion_core::contract::ProcessExit>,
    },
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
///
/// **`task_id` is an `Option` because a root has none (§9), not because a caller may omit it.** The
/// wait is identical either way — the same attach, the same one cursor, the same bookend — and what
/// the `Option` decides is only whether there is a file to read afterwards. Writing the root's case
/// as a second function would duplicate the correlation loop, which is the duplication `9a6211a`
/// removed from the three couriers and which loses a different clause each time it is copied.
pub fn await_contract(
    sock: &SocketPaths,
    project: &ProjectDir,
    agent_id: &AgentId,
    task_id: Option<&TaskId>,
    bound: Duration,
) -> Result<Delivered, SpawnError> {
    let deadline = Instant::now() + bound;
    let mut c = Conn::dial(sock)?;
    c.bound(bound)?;
    let attach = c.send(Call::NodeAttach(
        marion_core::proto::params::NodeAttachParams {
            agent_id: agent_id.clone(),
            pane_stream: None,
        },
    ))?;
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
                        sock.socket(),
                        &format!("it sent an unexpected frame while a node was running: {other:?}"),
                    ));
                }
            },
        }
    }
    match ended {
        Some(Lifecycle::Aborted { reason }) => Err(SpawnError::NodeAborted(reason)),
        // A root: §9 gives it no contract, so the bookend it just read *is* the whole answer. The
        // two fields come off the node's own stream rather than off the registry, so they are the
        // same observation a child's contract would have recorded.
        Some(Lifecycle::Exited { status, exit }) if task_id.is_none() => Ok(Delivered::Ended {
            status,
            exit: Box::new(exit),
        }),
        // `Opened` never ends the loop (see [`terminal_of`]), so the only remaining arm is `Exited`
        // with a task id.
        _ => read_contract(
            project,
            agent_id,
            task_id.expect("a child names its contract"),
        )
        .map(|c| Delivered::Contract(Box::new(c))),
    }
}

/// What a wait on a race produced.
#[derive(Debug)]
pub enum RaceDelivered {
    /// The race is decided and this is its scoreboard, read from `races/<race_id>.json`.
    Decided(Box<marion_core::race::RaceResult>),
    /// The bound expired with the race still open. Its seats keep running and the handle stays
    /// valid, exactly as [`Delivered::StillRunning`] says of one child.
    StillRunning,
}

/// **Wait for a race to be decided and hand back its scoreboard.**
///
/// A race is decided by the supervisor, which writes `races/<race_id>.json` and then journals
/// `RaceDecided`; folding that record re-announces each seat's summary with its verdict on
/// `tree/subscribe`. So the file is read first (a race decided before this call is answered from
/// disk), then the tree is followed until a seat of this race carries a verdict, and the file is
/// read again. Like [`await_contract`] this is a reader: it starts and stops nothing.
pub fn await_race(
    sock: &SocketPaths,
    project: &ProjectDir,
    race_id: &marion_core::race::RaceId,
    bound: Duration,
) -> Result<RaceDelivered, SpawnError> {
    let read = || crate::race::read_result(project, race_id);
    if let Some(r) = read() {
        return Ok(RaceDelivered::Decided(Box::new(r)));
    }
    let decided_seat = |n: &marion_core::proto::model::NodeSummary| {
        n.race
            .as_ref()
            .is_some_and(|b| &b.race_id == race_id && b.verdict.is_some())
    };
    let deadline = Instant::now() + bound;
    let mut c = Conn::dial(sock)?;
    c.bound(bound)?;
    let sub = c.send(Call::TreeSubscribe(TreeSubscribeParams {}))?;
    loop {
        c.bound(deadline.saturating_duration_since(Instant::now()))?;
        let seen = match c.next()? {
            Next::Expired => return Ok(RaceDelivered::StillRunning),
            Next::Frame(f) => match *f {
                Frame::Response(r) if r.id == RequestId::Number(sub) => match r.outcome {
                    Outcome::Error(e) => return Err(SpawnError::SupervisorRefused(e.message)),
                    Outcome::Result(v) => serde_json::from_value::<TreeSubscribeResult>(v)
                        .is_ok_and(|t| t.nodes.iter().any(decided_seat)),
                },
                Frame::Notification(n) => matches!(
                    n.event,
                    marion_core::proto::notify::Event::NodeAdded { ref node, .. }
                        if decided_seat(node)
                ),
                _ => false,
            },
        };
        if seen && let Some(r) = read() {
            return Ok(RaceDelivered::Decided(Box::new(r)));
        }
    }
}

/// The node's own closing bookend, if this event is one.
///
/// Filtered by `agent_id` because one connection can legitimately carry more: `tree/node-added` and
/// `node/state` are not this reader's news, and neither is another node's stream.
fn terminal_of(agent_id: &AgentId, event: marion_core::proto::notify::Event) -> Option<Lifecycle> {
    let marion_core::proto::notify::Event::NodeEvent {
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
///
/// **The file must be the contract of the task asked about.** The path is keyed on the task id, but
/// what the caller is handed is the file's content, so a contract naming another task — a file
/// copied or left over under the wrong name — is refused rather than reported as this task's.
pub(crate) fn read_contract(
    project: &ProjectDir,
    agent_id: &AgentId,
    task_id: &TaskId,
) -> Result<TaskContract, SpawnError> {
    let path = project.agent(agent_id).contract(task_id);
    let bytes = std::fs::read(&path).map_err(|e| SpawnError::NoContract {
        path: path.clone(),
        why: e.to_string(),
    })?;
    let contract =
        serde_json::from_slice::<TaskContract>(&bytes).map_err(|e| SpawnError::NoContract {
            path: path.clone(),
            why: format!("marion wrote it and cannot read it back: {e}"),
        })?;
    if contract.task_id != *task_id {
        return Err(SpawnError::NoContract {
            path,
            why: format!(
                "it is the contract of task {}, not of task {}",
                contract.task_id.0, task_id.0
            ),
        });
    }
    Ok(cap_for_return(contract))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A contract file is read only as the contract of the task it names.** One written under
    /// this task's name but naming another is refused with both ids; the right one reads back.
    ///
    /// Mutation: drop the `task_id` comparison and the mismatched file is handed back as this
    /// task's contract.
    #[test]
    fn a_contract_naming_another_task_is_refused_rather_than_read_as_this_one() {
        let dir = marion_testsupport::scratch("courier-contract-task");
        let project = ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        let contract = crate::bridge::contract_that_ran(crate::spawn::ChildOutcome {
            exit_code: Some(0),
            ..Default::default()
        });
        let agent_id = AgentId("019fbf94-0000-7000-8000-000000000001".into());
        let write = |task: &TaskId| {
            let path = project.agent(&agent_id).contract(task);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
        };

        write(&contract.task_id);
        let read = read_contract(&project, &agent_id, &contract.task_id).expect("its own contract");
        assert_eq!(read.task_id, contract.task_id);

        let other = TaskId("019fbf94-0000-7000-8000-00000000beef".into());
        write(&other);
        let e = read_contract(&project, &agent_id, &other).expect_err("another task's contract");
        let msg = e.to_string();
        assert!(
            matches!(e, SpawnError::NoContract { .. })
                && msg.contains(&contract.task_id.0)
                && msg.contains(&other.0),
            "the refusal names both tasks: {msg}"
        );
    }

    /// A supervisor's socket paths under a state root with nothing listening.
    fn nowhere(dir: &std::path::Path) -> crate::socket::SocketPaths {
        crate::socket::socket_paths(
            &dir.join("state"),
            std::path::Path::new("/canonical/project"),
            crate::socket::own_uid(),
        )
    }

    /// **A decided race is answered from its file**, before any dial: the socket here does not
    /// exist, so reaching it would be a refusal.
    #[test]
    fn a_decided_race_is_read_from_disk_without_dialling() {
        let dir = marion_testsupport::scratch("courier-race-decided");
        let project = ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        let result = marion_core::race::RaceResult {
            race_id: marion_core::race::RaceId("r-1".into()),
            requester: AgentId("root".into()),
            policy: marion_core::race::RacePolicy::default(),
            winner: None,
            decided_by: marion_core::race::DecidedBy::NoPass,
            seats: vec![],
        };
        crate::race::write_result(&project, &result).unwrap();
        let got = await_race(
            &nowhere(&dir),
            &project,
            &result.race_id,
            Duration::from_secs(1),
        )
        .expect("read from disk, no dial");
        let RaceDelivered::Decided(got) = got else {
            panic!("{got:?}")
        };
        assert_eq!(*got, result);
    }

    /// An open race whose supervisor cannot be reached is that refusal, not a hang or a result.
    #[test]
    fn an_open_race_with_no_supervisor_is_refused() {
        let dir = marion_testsupport::scratch("courier-race-open");
        let project = ProjectDir::new(&dir.join("state"), &dir.join("repo"));
        let got = await_race(
            &nowhere(&dir),
            &project,
            &marion_core::race::RaceId("r-1".into()),
            Duration::from_secs(1),
        );
        assert!(
            matches!(got, Err(SpawnError::SupervisorUnreachable { .. })),
            "{got:?}"
        );
    }

    /// **A dial that finds nothing is a refusal naming the socket, and it starts nothing.**
    ///
    /// The in-process backstop for `background_spawn.rs`'s end-to-end
    /// `a_spawn_whose_supervisor_is_not_listening_is_refused_and_starts_nothing`: this one runs in
    /// milliseconds and cannot be satisfied by a timeout. What it pins is that the failure is a
    /// *typed refusal* rather than a fallback — a `run_spawn` restored behind this call would have
    /// to make this function return `Ok` for a path nothing is listening on.
    #[test]
    fn a_spawn_with_no_supervisor_listening_is_refused_and_names_the_socket() {
        let dir = marion_testsupport::scratch("courier-spawn-no-supervisor");
        let sock = nowhere(&dir);
        let e = spawn(
            &sock,
            AgentSpawnParams {
                wider_children: None,
                budget_tokens: None,
                review_of: None,
                notify_parent: false,
                agent_type: "codex-impl".into(),
                prompt: "nothing may be started by this call".into(),
                native_launch: None,
                caller: Some(marion_core::proto::SpawnCaller {
                    agent_id: AgentId("019f-node".into()),
                    node_token: "tok".into(),
                }),
                repo: None,
                acceptance_criteria: vec![],
                verification: vec![],
                writable_scope: vec![],
                timeout_secs: Some(1),
                model: None,
                no_change_record: None,
                pane: None,
                isolation: None,
                allow_concurrent_writes: None,
                profile: None,
                candidates: vec![],
                race: None,
            },
        )
        .expect_err("nothing is listening there");
        let msg = e.to_string();
        assert!(
            msg.contains(&sock.socket().display().to_string()),
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
            &nowhere(&dir),
            &project,
            &AgentId("019f-node".into()),
            Some(&TaskId("task-1".into())),
            Duration::from_secs(1),
        )
        .expect_err("nothing is listening there");
        assert!(
            matches!(e, SpawnError::SupervisorUnreachable { .. }),
            "a missing supervisor is a missing supervisor, whichever verb asked: {e}"
        );
    }

    /// A steer with nobody listening is the same typed refusal, and names the socket.
    #[test]
    fn a_steer_with_no_supervisor_listening_is_refused_and_names_the_socket() {
        let dir = marion_testsupport::scratch("courier-steer-no-supervisor");
        let sock = nowhere(&dir);
        let e = steer(&sock, &AgentId("019f-node".into()), "use the v2 API", None)
            .expect_err("nothing is listening there");
        assert!(matches!(e, SpawnError::SupervisorUnreachable { .. }), "{e}");
        assert!(
            e.to_string().contains(&sock.socket().display().to_string()),
            "{e}"
        );
    }

    /// **What every steer surface prints on acceptance**: the message's id, and who takes it when.
    /// Worded once here so the CLI, the MCP tool and the tree cannot describe one queueing three
    /// ways; an unread type falls back to the id rather than inventing one. **When is the
    /// supervisor's, read off the node's row**: a live run was promised "its next tool round or
    /// turn" for a node that took messages only between runs, and it arrived a generation late.
    #[test]
    fn an_accepted_steer_names_its_message_the_node_and_when_it_arrives() {
        let relaunch = marion_harness::spec::TurnDelivery::Continuation { note: "" };
        let result = marion_core::proto::result::DeliveryResult {
            delivered_as: marion_core::proto::Delivery::Steer,
            state: marion_core::node::NodeState::Running,
            resumed: false,
            message_id: Some("m-7".into()),
            queued: true,
            arrives: Some(relaunch.arrival().into()),
        };
        let id = AgentId("01a091ba-8ea3-7000-8000-000000000000".into());
        let typed = Steered {
            agent_id: id.clone(),
            agent_type: Some("codex-impl".into()),
            result: result.clone(),
        };
        assert_eq!(
            typed.sentence(),
            format!(
                "queued as m-7; reaches codex-impl 8ea3 {}",
                relaunch.arrival()
            )
        );
        assert!(
            !typed.sentence().contains("tool round"),
            "{}",
            typed.sentence()
        );
        let untyped = Steered {
            agent_id: id.clone(),
            agent_type: None,
            result: result.clone(),
        };
        assert!(
            untyped
                .sentence()
                .starts_with("queued as m-7; reaches 8ea3 when its current run ends"),
            "{}",
            untyped.sentence()
        );
        // A supervisor that predates `arrives` names no row, so the sentence names both boundaries.
        let older = Steered {
            agent_id: id,
            agent_type: Some("codex-impl".into()),
            result: marion_core::proto::result::DeliveryResult {
                arrives: None,
                ..result
            },
        };
        assert_eq!(
            older.sentence(),
            "queued as m-7; reaches codex-impl 8ea3 at its next turn boundary"
        );
    }
}
