//! **A node over an id-correlated JSON-RPC thread server** — codex's `app-server` (S36,
//! `tests/fixtures/app-server-0.155.1/`), in the vocabulary its row states
//! ([`marion_harness::rpc_channel::RpcChannel`]).
//!
//! The conversation, each step bounded and refused by name rather than falling through:
//!
//! 1. spawn in its own process group ([`crate::rpc::Driver`]) and confirm the pid before one byte
//!    goes out;
//! 2. the handshake, then the row's `initialized` notification;
//! 3. the adapter's opening request — `thread/start`, or `thread/resume` of the node's session —
//!    correlated on that request's own id; the answer names the thread;
//! 4. **the readiness gate**: the first turn waits for the row's startup notification to say
//!    marion's MCP server is ready, because the server does not wait for it (S36 P3: a turn that
//!    starts earlier takes its first request without marion's tools);
//! 5. the prompt as a turn, and every turn after it from the node's inbox: a message queued while
//!    a turn runs is **folded** into it as a steer (P6), and one that arrives at a boundary is the
//!    next turn; a steer that loses the race with the turn's end becomes the next turn instead;
//! 6. server requests answered with the row's answers — approvals declined, never cancelled (P5);
//! 7. on the wall clock's expiry, an **interrupt** of the running turn, repeated without awaiting
//!    its answer (P7: a second interrupt of an interrupted turn is never answered), then marion
//!    kills the turn's processes itself (P7: app-server leaves a running command running), and the
//!    group.
//!
//! The transcript is the server's stdout and nothing else, as on ACP: every frame it wrote, in
//! order, which the row's stream grammar reads — an item that completes after its turn included.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::Value;

use marion_harness::Invocation;
use marion_harness::rpc_channel::{RpcChannel, Startup};

use crate::inbox::{Message, TurnFeed};
use crate::kill::kill_process_tree;
use crate::rpc::{Peer, RpcRun, clip, excerpt};

/// `MessageDelivered.via` for a message folded into a running turn, and for one sent as the next.
pub const VIA_MID_TURN: &str = "app-server:mid-turn";
pub const VIA_NEXT_TURN: &str = "app-server:next-turn";

/// The id on `initialize`. The opening request's is the adapter's, read off the request itself.
const INITIALIZE_ID: u64 = 0;
/// The first id on a turn request; each later request takes the next.
const FIRST_TURN_ID: u64 = 2;

/// A process start and one frame (P2 answered `initialize` at once).
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(30);
/// `thread/start` answered in ~20 ms (P3); `thread/resume` reads a rollout from disk.
const THREAD_BUDGET: Duration = Duration::from_secs(60);
/// How long marion's MCP server is given to report ready. A bridge that has not started in a minute
/// is not starting.
const MCP_READY_BUDGET: Duration = Duration::from_secs(60);
/// How long an interrupted turn is given to say so before the next interrupt, and how many are
/// sent. P7 measured `turn/completed` `interrupted` at once; the repeat is for a server that missed
/// one, and none of them is awaited.
const INTERRUPT_WAIT: Duration = Duration::from_secs(2);
const INTERRUPTS: usize = 3;
/// How much of the server's stderr a refusal carries.
const STDERR_EXCERPT: usize = 2000;

/// One node over a thread server, as plain data.
pub struct AppServerSpec<'a> {
    /// Program, args, env and cwd, compiled by the adapter from the row.
    pub inv: &'a Invocation,
    /// The node's own `TMPDIR` ([`crate::node_tmp`]).
    pub tmpdir: &'a std::path::Path,
    /// The row's vocabulary.
    pub channel: &'static RpcChannel,
    /// The opening request the adapter built (`HarnessAdapter::session_declaration`), whole.
    pub opening: Value,
    /// The MCP server the first turn waits for, or `None` where the launch declares none.
    pub gate: Option<&'a str>,
    pub prompt: &'a str,
    /// The wall clock for the whole session.
    pub bound: Duration,
    /// Called with the pid **before one byte is written to the node**.
    pub on_started: &'a dyn Fn(i32),
    /// Called with every line the server writes, verbatim, while the session runs.
    pub on_line: Option<&'a dyn Fn(&str)>,
    /// The node's inbox, for every turn after the first, or `None` for a one-turn session.
    pub turns: Option<TurnFeed>,
}

/// Why marion has no transcript: each names what marion asked for and did not get, in the server's
/// own words where it has any. A turn that ran and went badly is not here; its frames say so.
#[derive(Debug, thiserror::Error)]
pub enum AppServerError {
    #[error("could not start `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the server's stdin closed while marion was writing `{step}`: {source}")]
    Unwritable {
        step: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the adapter's opening request is not the row's `{open}` or `{resume}`, so there is no \
         thread to correlate an answer against: {request}"
    )]
    Opening {
        open: &'static str,
        resume: &'static str,
        request: String,
    },
    #[error(
        "the server did not answer `{step}` within {waited:?}; it wrote {frames} frames. Its \
         stderr: {stderr}"
    )]
    Silent {
        step: &'static str,
        waited: Duration,
        frames: usize,
        stderr: String,
    },
    /// The server answered with an error, or with an answer that names no thread.
    #[error("the server refused `{step}`: {answer}")]
    Refused { step: &'static str, answer: String },
    /// **The loud refusal §6.1 step 8 requires**: a first turn without marion's tools would end
    /// in plain text with no error anywhere.
    #[error(
        "marion's MCP server `{server}` did not report ready within {waited:?}, so the node would \
         have taken its first turn without marion's tools. The server's stderr: {stderr}"
    )]
    McpNeverReady {
        server: String,
        waited: Duration,
        stderr: String,
    },
    #[error("marion's MCP server `{server}` failed to start: {why}")]
    McpFailed { server: String, why: String },
}

/// What the server told marion about its thread, off its notifications.
struct AppPeer {
    channel: &'static RpcChannel,
    gate: Option<String>,
    startup: Option<Startup>,
    /// Every turn the server has closed.
    closed: Vec<String>,
    /// Processes the server started for a turn and has not reported ended.
    running: Vec<i32>,
}

impl Peer for AppPeer {
    fn answer(&mut self, request: &Value) -> Value {
        self.channel.answer(request)
    }

    fn notified(&mut self, frame: &Value) {
        let c = self.channel;
        if let Some(turn) = c.closes_turn(frame) {
            self.closed.push(turn);
        } else if let Some(pid) = c.process_of(frame) {
            self.running.push(pid);
        } else if let Some(pid) = c.process_ended(frame) {
            self.running.retain(|p| *p != pid);
        } else if let Some(server) = &self.gate
            && self.startup.is_none()
        {
            self.startup = c.startup(frame, server);
        }
    }
}

type Driver<'a> = crate::rpc::Driver<'a, AppPeer>;

/// Drive one node over its thread server and hand back the transcript.
pub fn run_app_server(spec: AppServerSpec<'_>) -> Result<RpcRun, AppServerError> {
    let c = spec.channel;
    // Before the spawn: an opening marion cannot correlate against is a hang discovered late.
    let (opening_id, step) = match (
        spec.opening.get("id").and_then(Value::as_u64),
        c.opened_by(&spec.opening),
    ) {
        (Some(id), Some(None)) => (id, c.open),
        (Some(id), Some(Some(_))) => (id, c.resume),
        _ => {
            return Err(AppServerError::Opening {
                open: c.open,
                resume: c.resume,
                request: spec.opening.to_string(),
            });
        }
    };
    let deadline = Instant::now()
        .checked_add(spec.bound)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
    let peer = AppPeer {
        channel: c,
        gate: spec.gate.map(str::to_string),
        startup: None,
        closed: Vec::new(),
        running: Vec::new(),
    };
    let mut node = Driver::spawn(spec.inv, spec.tmpdir, spec.on_line, peer).map_err(|source| {
        AppServerError::Spawn {
            program: spec.inv.program.clone(),
            source,
        }
    })?;
    (spec.on_started)(node.pid);

    let version = env!("CARGO_PKG_VERSION");
    request(
        &mut node,
        &c.initialize_request(INITIALIZE_ID, version),
        INITIALIZE_ID,
        c.initialize,
        clip(deadline, HANDSHAKE_BUDGET),
    )?;
    if let Some(n) = c.initialized_notification() {
        send(&mut node, &n, "initialized")?;
    }
    let opened = request(
        &mut node,
        &spec.opening,
        opening_id,
        step,
        clip(deadline, THREAD_BUDGET),
    )?;
    let Some(thread) = c.thread_of(&opened) else {
        let _ = node.finish(true);
        return Err(AppServerError::Refused {
            step,
            answer: opened.to_string(),
        });
    };
    if let Some(server) = spec.gate {
        gate(&mut node, server, clip(deadline, MCP_READY_BUDGET))?;
    }

    let mut session = Session {
        c,
        thread,
        next_id: FIRST_TURN_ID,
        deadline,
        turns: spec.turns.as_ref(),
    };
    let finished = session.run(&mut node, spec.prompt)?;
    Ok(node.finish(!finished).0)
}

/// Write one frame, the failure named after its step.
fn send(node: &mut Driver<'_>, frame: &Value, step: &'static str) -> Result<(), AppServerError> {
    node.write(frame).map_err(|source| {
        let _ = node.finish(true);
        AppServerError::Unwritable { step, source }
    })
}

/// Send `frame` and wait for its answer, refusing on silence or on a JSON-RPC error.
fn request(
    node: &mut Driver<'_>,
    frame: &Value,
    id: u64,
    step: &'static str,
    until: Instant,
) -> Result<Value, AppServerError> {
    send(node, frame, step)?;
    let waited = until.saturating_duration_since(Instant::now());
    let Some(line) = node.settle(id, until) else {
        return Err(node.refuse(|end, frames| AppServerError::Silent {
            step,
            waited,
            frames,
            stderr: excerpt(&end.stderr, STDERR_EXCERPT),
        }));
    };
    let answer: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    if answer.get("result").is_none() {
        let _ = node.finish(true);
        return Err(AppServerError::Refused { step, answer: line });
    }
    Ok(answer)
}

/// §6.1 step 8 on this surface: wait for the server to report marion's MCP server ready.
fn gate(node: &mut Driver<'_>, server: &str, until: Instant) -> Result<(), AppServerError> {
    let waited = until.saturating_duration_since(Instant::now());
    match node.settle_until(until, |d| d.peer.startup.take()) {
        Some(Startup::Ready) => Ok(()),
        Some(Startup::Failed(why)) => {
            let _ = node.finish(true);
            Err(AppServerError::McpFailed {
                server: server.to_string(),
                why,
            })
        }
        None => Err(node.refuse(|end, _| AppServerError::McpNeverReady {
            server: server.to_string(),
            waited,
            stderr: excerpt(&end.stderr, STDERR_EXCERPT),
        })),
    }
}

/// The turn loop's state: the thread, the next request id, the wall clock and the inbox.
struct Session<'s> {
    c: &'static RpcChannel,
    thread: String,
    next_id: u64,
    deadline: Instant,
    turns: Option<&'s TurnFeed>,
}

/// A turn request marion is waiting on the answer to, and the message it carries, if any.
struct Pending {
    id: u64,
    msg: Option<Message>,
    steer: bool,
}

impl Session<'_> {
    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Every turn from the prompt to the inbox's seal. `false` when marion cut the session short on
    /// its wall clock.
    fn run(&mut self, node: &mut Driver<'_>, prompt: &str) -> Result<bool, AppServerError> {
        let first = self.id();
        send(
            node,
            &self.c.turn_request(first, &self.thread, prompt),
            self.c.turns.start,
        )?;
        if let Some(feed) = self.turns {
            feed.source.attach_port(node.wake_port());
        }
        let folds = self.turns.is_some_and(TurnFeed::folds);
        // Messages whose steer lost the race with the turn's end: each is the next turn.
        let mut carried: VecDeque<Message> = VecDeque::new();
        let mut pending = vec![Pending {
            id: first,
            msg: None,
            steer: false,
        }];
        // The turn marion last started, once its answer names it.
        let mut current: Option<String> = None;
        loop {
            let mut woken = false;
            let settled = node.settle_until(self.deadline, |d| {
                woken |= d.take_wake();
                // Answers to what marion sent: a start names its turn, a steer the turn it joined.
                for p in std::mem::take(&mut pending) {
                    let Some(line) = answer_to(d, p.id) else {
                        pending.push(p);
                        continue;
                    };
                    let answer: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    let turn = self.c.answered_turn(&answer);
                    match (turn, p.steer) {
                        (Some(t), false) => {
                            current = Some(t);
                            if let (Some(feed), Some(msg)) = (self.turns, &p.msg) {
                                feed.source.delivered(&msg.id, VIA_NEXT_TURN);
                            }
                        }
                        (Some(_), true) => {
                            if let (Some(feed), Some(msg)) = (self.turns, &p.msg) {
                                feed.source.delivered(&msg.id, VIA_MID_TURN);
                            }
                        }
                        // The turn ended before the steer landed: the message is the next turn.
                        (None, true) => carried.extend(p.msg),
                        (None, false) => {
                            if let (Some(feed), Some(msg)) = (self.turns, &p.msg) {
                                feed.source
                                    .dropped(&msg.id, &format!("the server refused it: {line}"));
                            }
                        }
                    }
                }
                let running = current
                    .as_ref()
                    .filter(|t| !d.peer.closed.contains(t))
                    .cloned();
                // A folding row takes a message into the running turn once its id is known.
                if let (Some(turn), Some(feed), true) = (&running, self.turns, folds && woken) {
                    woken = false;
                    while let Some(msg) = feed.source.take_next() {
                        let id = self.id();
                        let text = crate::inbox::render(&msg);
                        let frame = self.c.steer_request(id, &self.thread, turn, &text);
                        match d.write(&frame) {
                            Ok(()) => pending.push(Pending {
                                id,
                                msg: Some(msg),
                                steer: true,
                            }),
                            Err(e) => {
                                feed.source
                                    .dropped(&msg.id, &format!("the server's stdin closed: {e}"));
                            }
                        }
                    }
                }
                // Settled: nothing unanswered, and no turn of marion's still open.
                (pending.is_empty() && running.is_none()).then_some(())
            });
            if settled.is_none() {
                self.interrupt(node, current.as_deref());
                return Ok(false);
            }
            // The boundary: a carried message, else the inbox's next, else a hold or the end.
            let Some(feed) = self.turns else {
                return Ok(true);
            };
            let msg = match carried.pop_front() {
                Some(m) => m,
                None => match feed.source.take_or_seal() {
                    Some(m) => m,
                    // Owed a background child's end: wait for the inbox, bounded by the wall clock.
                    None if feed.source.held() => {
                        if node
                            .settle_until(self.deadline, |d| d.take_wake().then_some(()))
                            .is_none()
                        {
                            return Ok(false);
                        }
                        // Round again with nothing in flight: the boundary takes the message.
                        continue;
                    }
                    None => return Ok(true),
                },
            };
            let id = self.id();
            let text = crate::inbox::render(&msg);
            match node.write(&self.c.turn_request(id, &self.thread, &text)) {
                Ok(()) => pending.push(Pending {
                    id,
                    msg: Some(msg),
                    steer: false,
                }),
                Err(e) => {
                    feed.source
                        .dropped(&msg.id, &format!("the server's stdin closed: {e}"));
                    return Ok(true);
                }
            }
        }
    }

    /// The wall clock expired mid-turn: interrupt it — again, unanswered, if it does not close —
    /// then kill what it started. `turn` is `None` when the server never named one.
    fn interrupt(&mut self, node: &mut Driver<'_>, turn: Option<&str>) {
        if let Some(turn) = turn {
            for _ in 0..INTERRUPTS {
                let id = self.id();
                if node
                    .write(&self.c.interrupt_request(id, &self.thread, turn))
                    .is_err()
                {
                    break;
                }
                let closed = node.settle_until(Instant::now() + INTERRUPT_WAIT, |d| {
                    d.peer.closed.iter().any(|t| t == turn).then_some(())
                });
                if closed.is_some() {
                    break;
                }
            }
        }
        // P7: the interrupted turn's commands keep running — a command that yielded is still
        // running after its item completed — so marion kills every process below the server while
        // the tree is intact, and leaves the server to end its session on stdin EOF (P9). The
        // commands the turn named are killed too, in case one left the tree.
        crate::kill::kill_descendants(node.pid);
        for pid in std::mem::take(&mut node.peer.running) {
            kill_process_tree(pid);
        }
    }
}

/// The answer to `id`, taken out of the driver's index so a later request never sees it.
fn answer_to(node: &mut Driver<'_>, id: u64) -> Option<String> {
    let at = node.responses.iter().position(|(k, _)| *k == id)?;
    Some(node.responses.swap_remove(at).1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use marion_core::contract::AgentId;
    use marion_core::journal::RecordKind;
    use marion_harness::codex::APP;
    use marion_harness::spec::{MidTurn, TurnDelivery};
    use marion_testsupport::scratch;

    use crate::inbox::{BoundInbox, Inboxes, Source};

    /// A fake thread server in the S36 shape, in Python so it can keep time: it logs every frame it
    /// reads with a monotonic stamp, and `MODE` picks what a turn does.
    ///
    /// - `simple`: the turn asks one command approval and completes once it is answered;
    /// - `hold`: the first turn waits for a steer, answers it and completes; later turns as `simple`;
    /// - `sleep`: the turn starts a `sleep 60` it reports as a command and never completes; an
    ///   interrupt is answered once and closes the turn, **leaving the sleep running** (P7);
    /// - `deaf`: as `sleep`, and an interrupt is never answered;
    /// - `late`: as `hold`, but the steer is refused as arriving after the turn (P6's error);
    /// - `never` / `failed`: marion's MCP server never reports ready / reports failed;
    /// - `refuse`: a `thread/resume` is refused as a missing rollout (P8).
    const FAKE: &str = r#"
import json, os, subprocess, sys, time
log = open(os.getenv("LOG"), "a")
mode = os.getenv("MODE") or "simple"
def out(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
def note(k, **v):
    log.write(json.dumps(dict(k=k, t=time.monotonic(), **v)) + "\n"); log.flush()
turn = 0
answered_interrupt = False
for line in sys.stdin:
    m = json.loads(line); note("in", msg=m)
    meth = m.get("method"); i = m.get("id")
    u = "u-%d" % turn
    if meth == "initialize":
        out({"id": i, "result": {"userAgent": "fake"}})
    elif meth in ("thread/start", "thread/resume"):
        if meth == "thread/resume" and mode == "refuse":
            out({"id": i, "error": {"code": -32600, "message": "no rollout found for thread id x"}}); continue
        tid = m["params"].get("threadId", "t-1")
        out({"id": i, "result": {"thread": {"id": tid}}})
        out({"method": "mcpServer/startupStatus/updated", "params": {"threadId": tid, "name": "marion", "status": "starting", "error": None}})
        if mode == "never": continue
        time.sleep(0.3)
        st = "failed" if mode == "failed" else "ready"
        note("startup", status=st)
        out({"method": "mcpServer/startupStatus/updated", "params": {"threadId": tid, "name": "marion", "status": st, "error": "boom" if st == "failed" else None}})
    elif meth == "turn/start":
        turn += 1; u = "u-%d" % turn
        out({"id": i, "result": {"turn": {"id": u, "status": "inProgress"}}})
        out({"method": "turn/started", "params": {"turn": {"id": u}}})
        if mode == "simple" or (mode in ("hold", "late") and turn > 1):
            out({"id": 100 + turn, "method": "item/commandExecution/requestApproval", "params": {"turnId": u}})
        elif mode in ("sleep", "deaf"):
            child = subprocess.Popen(["sleep", "60"]); note("pid", pid=child.pid)
            out({"method": "item/started", "params": {"turnId": u, "item": {"type": "commandExecution", "id": "c1", "processId": str(child.pid), "status": "inProgress"}}})
    elif meth is None and i is not None:
        note("reply", msg=m)
        out({"method": "item/completed", "params": {"turnId": u, "item": {"type": "agentMessage", "id": "m", "text": "done"}}})
        out({"method": "turn/completed", "params": {"turn": {"id": u, "status": "completed"}}})
    elif meth == "turn/steer":
        if mode == "late":
            out({"id": i, "error": {"code": -32600, "message": "no active turn to steer"}})
        else:
            out({"id": i, "result": {"turnId": m["params"]["expectedTurnId"]}})
        out({"method": "turn/completed", "params": {"turn": {"id": m["params"]["expectedTurnId"], "status": "completed"}}})
    elif meth == "turn/interrupt":
        if mode == "deaf" or answered_interrupt: continue
        answered_interrupt = True
        out({"id": i, "result": {}})
        out({"method": "turn/completed", "params": {"turn": {"id": m["params"]["turnId"], "status": "interrupted"}}})
"#;

    struct Bed {
        dir: marion_testsupport::Scratch,
        inv: Invocation,
    }

    impl Bed {
        fn new(tag: &str, mode: &str) -> Bed {
            let dir = scratch(tag);
            let script = dir.join("fake.py");
            std::fs::write(&script, FAKE).unwrap();
            let inv = Invocation {
                program: "python3".into(),
                args: vec![script.to_string_lossy().into_owned()],
                env: vec![
                    ("LOG".into(), dir.join("log.jsonl").to_string_lossy().into()),
                    ("MODE".into(), mode.into()),
                ],
                env_remove: vec![],
                cwd: dir.to_path_buf(),
                model: None,
                session_mode: None,
            };
            Bed { dir, inv }
        }

        fn spec(&self, bound: Duration, turns: Option<TurnFeed>) -> AppServerSpec<'_> {
            AppServerSpec {
                inv: &self.inv,
                tmpdir: &self.dir,
                channel: &APP,
                opening: APP.opening(1, "/wt", None),
                gate: Some("marion"),
                prompt: "go",
                bound,
                on_started: &|_| {},
                on_line: None,
                turns,
            }
        }

        fn log(&self) -> Vec<Value> {
            std::fs::read_to_string(self.dir.join("log.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }

        /// When the server read the first frame with `method`, and that frame.
        fn read(&self, method: &str) -> Option<(f64, Value)> {
            self.log().into_iter().find_map(|e| {
                (e["k"] == "in" && e["msg"]["method"] == method)
                    .then(|| (e["t"].as_f64().unwrap(), e["msg"].clone()))
            })
        }

        fn count(&self, method: &str) -> usize {
            self.log()
                .iter()
                .filter(|e| e["k"] == "in" && e["msg"]["method"] == method)
                .count()
        }

        fn wait_read(&self, method: &str) {
            let until = Instant::now() + Duration::from_secs(10);
            while self.read(method).is_none() {
                assert!(Instant::now() < until, "the server never read {method}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn pid(&self) -> i32 {
            self.log()
                .into_iter()
                .find(|e| e["k"] == "pid")
                .expect("the turn started its command")["pid"]
                .as_i64()
                .unwrap() as i32
        }
    }

    /// **The first turn waits for marion's server to report ready** (P3), and **an approval is
    /// declined, never cancelled** (P5) — the turn then completes and the session ends cleanly.
    #[test]
    fn the_first_turn_waits_for_marions_server_and_an_approval_is_declined() {
        let bed = Bed::new("as-gate", "simple");
        let run = run_app_server(bed.spec(Duration::from_secs(20), None)).expect("a session");
        assert!(!run.exit.timed_out, "{run:?}");
        let ready = bed
            .log()
            .into_iter()
            .find(|e| e["k"] == "startup")
            .expect("the server reported ready")["t"]
            .as_f64()
            .unwrap();
        let (started, turn) = bed.read("turn/start").expect("a turn was started");
        assert!(
            started > ready,
            "turn/start at {started} before ready at {ready}"
        );
        assert_eq!(turn["params"]["input"][0]["text"], "go");
        assert!(
            bed.read("initialized").is_some(),
            "P2's notification was sent"
        );
        let reply = bed
            .log()
            .into_iter()
            .find(|e| e["k"] == "reply")
            .expect("the approval was answered");
        assert_eq!(reply["msg"]["result"]["decision"], "decline");
        assert!(
            run.stdout.contains(r#""status": "completed""#)
                || run.stdout.contains(r#""status":"completed""#),
            "{}",
            run.stdout
        );
        assert_eq!(crate::rpc::turn_exit(run.exit).code, Some(0));
    }

    /// A server whose MCP start fails, or never finishes, refuses the run by name before a turn.
    #[test]
    fn a_server_that_never_readies_marions_tools_is_refused_before_a_turn() {
        let bed = Bed::new("as-failed", "failed");
        let e = run_app_server(bed.spec(Duration::from_secs(20), None)).unwrap_err();
        assert!(
            matches!(&e, AppServerError::McpFailed { why, .. } if why == "boom"),
            "{e}"
        );
        assert!(bed.read("turn/start").is_none());

        let bed = Bed::new("as-never", "never");
        let e = run_app_server(bed.spec(Duration::from_secs(2), None)).unwrap_err();
        assert!(matches!(e, AppServerError::McpNeverReady { .. }), "{e}");
        assert!(bed.read("turn/start").is_none());
    }

    /// A resume the server cannot honour (an ephemeral or unknown thread, P8) is a refusal in its
    /// own words, never a fresh thread under the old id.
    #[test]
    fn a_resume_the_server_refuses_is_a_refusal_in_its_words() {
        let bed = Bed::new("as-refuse", "refuse");
        let e = run_app_server(AppServerSpec {
            opening: APP.opening(1, "/wt", Some("t-old")),
            ..bed.spec(Duration::from_secs(20), None)
        })
        .unwrap_err();
        assert!(
            matches!(&e, AppServerError::Refused { step: "thread/resume", answer } if answer.contains("no rollout found")),
            "{e}"
        );
        assert!(bed.read("turn/start").is_none());
    }

    struct Fed {
        inboxes: Arc<Inboxes>,
        log: Arc<Mutex<Vec<RecordKind>>>,
        agent: AgentId,
        feed: TurnFeed,
    }

    fn delivery(mid_turn: MidTurn) -> TurnDelivery {
        TurnDelivery::TypedTurn {
            mid_turn,
            note: "t",
        }
    }

    fn fed(mid_turn: MidTurn) -> Fed {
        let (inboxes, log) = crate::inbox::tests::recording();
        let inboxes = Arc::new(inboxes);
        let agent = AgentId("as-node".into());
        inboxes.open(&agent);
        let feed = TurnFeed::new(
            Arc::new(BoundInbox::new(Arc::clone(&inboxes), agent.clone())),
            delivery(mid_turn),
        );
        Fed {
            inboxes,
            log,
            agent,
            feed,
        }
    }

    impl Fed {
        fn queue(&self, text: &str) -> String {
            self.inboxes
                .enqueue(
                    &self.agent,
                    delivery(self.feed.mid_turn),
                    Source::Operator,
                    text.into(),
                )
                .expect("open")
        }
        fn delivered(&self) -> Vec<(String, String)> {
            crate::inbox::tests::records(&self.log)
                .into_iter()
                .filter_map(|r| match r {
                    RecordKind::MessageDelivered(d) => Some((d.message_id, d.via)),
                    _ => None,
                })
                .collect()
        }
    }

    /// **A message queued while the turn runs is a steer into it** (P6): `turn/steer` naming the
    /// running turn, journaled `app-server:mid-turn`, and no turn of its own.
    #[test]
    fn a_message_queued_mid_turn_is_steered_into_the_running_turn() {
        let bed = Bed::new("as-steer", "hold");
        let fx = fed(MidTurn::Fold);
        let id = std::thread::scope(|s| {
            let steer = s.spawn(|| {
                bed.wait_read("turn/start");
                fx.queue("also the docs")
            });
            let run = run_app_server(bed.spec(Duration::from_secs(20), Some(fx.feed.clone())))
                .expect("a session");
            assert!(!run.exit.timed_out, "{run:?}");
            steer.join().unwrap()
        });
        let (_, steer) = bed.read("turn/steer").expect("the message was steered");
        assert_eq!(steer["params"]["expectedTurnId"], "u-1");
        assert!(
            steer["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains("also the docs")
        );
        assert_eq!(bed.count("turn/start"), 1, "the steer started no turn");
        assert_eq!(fx.delivered(), [(id, VIA_MID_TURN.to_string())]);
    }

    /// **A steer that loses the race with the turn's end is the next turn instead** — delivered
    /// once, `app-server:next-turn`, never dropped.
    #[test]
    fn a_steer_refused_as_late_becomes_the_next_turn() {
        let bed = Bed::new("as-late", "late");
        let fx = fed(MidTurn::Fold);
        let id = std::thread::scope(|s| {
            let steer = s.spawn(|| {
                bed.wait_read("turn/start");
                fx.queue("one more thing")
            });
            let run = run_app_server(bed.spec(Duration::from_secs(20), Some(fx.feed.clone())))
                .expect("a session");
            assert!(!run.exit.timed_out, "{run:?}");
            steer.join().unwrap()
        });
        assert_eq!(bed.count("turn/steer"), 1);
        assert_eq!(
            bed.count("turn/start"),
            2,
            "the refused steer went out as a turn"
        );
        let second = bed
            .log()
            .into_iter()
            .filter(|e| e["k"] == "in" && e["msg"]["method"] == "turn/start")
            .nth(1)
            .unwrap();
        assert!(second["msg"].to_string().contains("one more thing"));
        assert_eq!(fx.delivered(), [(id, VIA_NEXT_TURN.to_string())]);
    }

    /// **A message held for the boundary is the next turn** of the same process, journaled
    /// `app-server:next-turn` — a queueing feed, so it waits out the running turn.
    #[test]
    fn a_message_at_the_boundary_is_the_next_turn_of_the_same_process() {
        let bed = Bed::new("as-next", "simple");
        let fx = fed(MidTurn::Queue);
        let id = fx.queue("the next thing");
        let run = run_app_server(bed.spec(Duration::from_secs(20), Some(fx.feed.clone())))
            .expect("a session");
        assert!(!run.exit.timed_out, "{run:?}");
        assert_eq!(bed.count("turn/start"), 2);
        assert_eq!(bed.count("turn/steer"), 0);
        assert_eq!(fx.delivered(), [(id, VIA_NEXT_TURN.to_string())]);
    }

    /// **On the wall clock, the turn is interrupted and marion kills what it started** (P7: the
    /// server answers at once and leaves the command running).
    #[test]
    fn an_expired_turn_is_interrupted_and_its_command_killed() {
        let bed = Bed::new("as-interrupt", "sleep");
        let run = run_app_server(bed.spec(Duration::from_secs(3), None)).expect("a session");
        assert!(run.exit.timed_out, "{run:?}");
        assert!(
            !marion_testsupport::alive(bed.pid()),
            "the interrupted turn's command survived"
        );
        let (_, stop) = bed.read("turn/interrupt").expect("an interrupt was sent");
        assert_eq!(
            stop["params"],
            serde_json::json!({"threadId": "t-1", "turnId": "u-1"})
        );
        assert_eq!(bed.count("turn/interrupt"), 1, "answered, so not repeated");
    }

    /// **An interrupt nobody answers is repeated, never awaited**, and the command is killed all
    /// the same.
    #[test]
    fn an_unanswered_interrupt_is_repeated_and_the_command_still_killed() {
        let bed = Bed::new("as-deaf", "deaf");
        let started = Instant::now();
        let run = run_app_server(bed.spec(Duration::from_secs(2), None)).expect("a session");
        assert!(run.exit.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "bounded: {:?}",
            started.elapsed()
        );
        assert_eq!(
            bed.count("turn/interrupt"),
            INTERRUPTS,
            "every interrupt went out"
        );
        assert!(!marion_testsupport::alive(bed.pid()));
    }
}
