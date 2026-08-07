//! **`marion run` is a client, and M2's acceptance criterion 1** (§9, §11 item 28 step 6).
//!
//! > *"A TUI crash cannot kill running agents: agents keep running, and a new client shows the full
//! > tree."*
//!
//! Until step 6 this criterion was unmeetable and `MILESTONES.md` said so: `marion run` held the
//! root's `Child`, its pipe and its whole turn, so killing it killed the root. Now the supervisor
//! owns the root and `marion run` is a socket client that spawns, attaches and renders — so the
//! criterion is a `kill -9` away from being a measurement, and this file is that measurement.
//!
//! # What is asserted, and why each clause is here
//!
//! The kill is of the **client**, and the assertions afterwards are §9's criterion-4 shape applied
//! to it, because "a new client shows the full tree" is worth nothing if the tree it shows is a
//! corpse:
//!
//! * **(i) the same supervisor** — by pid *and* `ps` start time, so a reissued pid cannot satisfy
//!   it. `restart.rs` is emphatic that a bare pid is not an identity.
//! * **(ii) events emitted after the new client attached** — §6.1 step 8's *MUST NOT substitute a
//!   sleep for an observation* binds a test as hard as it binds a launcher, so the node's next turn
//!   is **held** at the provider until the attach has happened, and the ordering is proved from the
//!   provider's own request log rather than from this file's say-so.
//! * **(iii) contiguity across the detached window** — one file's own ordinals, read by one cursor:
//!   a gap means an event was dropped, a repeat means the replay and subscribe legs overlapped.
//! * **liveness, and then growth.** A pid in the process table can be a zombie, so liveness is S15's
//!   three-valued `procid` reading (`marion_testsupport::liveness`) and never `kill(pid, 0)`. And a
//!   live process that has stopped working would satisfy even that, so the load-bearing assertion is
//!   that the root's `events.jsonl` **grew after the kill**.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test client_run
//! ```
//!
//! Needs real `claude` and `codex` on `PATH` and does **not** skip when they are missing, for the
//! reason `journal_wiring.rs` gives. Every model call is served by the CannedServer: no paid tokens.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_proto::notify::Event as Note;
use marion_proto::{Call, Frame, Method, MethodResult, Outcome, Request, RequestId};
use marion_provider::script::classify_root;
use marion_provider::{
    CannedServer, Config, RequestKind, RootScript, RootStep, RootTurn, Script, TurnGate,
    classify_anthropic,
};
use marion_supervisor::socket::{SocketPaths, read_identity, socket_paths};
use marion_testsupport::{Liveness, fixture_repo, liveness, scratch, sweep};
use serde_json::json;

unsafe extern "C" {
    fn getuid() -> u32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// How long anything in this file may take before it is a failure. Never a verdict: every assertion
/// below is over an identity, an ordinal, a count, a payload or a liveness reading — never over
/// elapsed time — and no failure here may be repaired by widening this.
const BOUND: Duration = Duration::from_secs(180);

const ROOT_MARKER: &str = "MARION-CLIENT-RUN-ROOT-TURN-b41c";
/// Appears in the **root's** closing turn and nowhere else, so an event carrying it can only have
/// come from the provider request the test released after the new client attached.
const ROOT_FINAL_MARKER: &str = "MARION-CLIENT-RUN-AFTER-RELEASE-9d3e";
const NARRATIVE: &str = "Wrote the marker under src/ and reported back.";
const CHILD_FILE: &str = "src/client-run-marker.txt";
const CHILD_TIMEOUT_SECS: u64 = 120;
const ROOT_BLOCKED_SECS: &str = "5";
/// The child's wire, and the one whose turn is held. Counted per wire because arrival order across
/// the port is a race by construction — see `marion_provider::gate`.
const CHILD_WIRE: &str = "responses";
/// The root's wire. A claude root speaks Anthropic messages.
const ROOT_WIRE: &str = "anthropic";

fn script() -> Script {
    let claude = marion_harness::adapter_for(marion_core::harness::Harness::ClaudeCode)
        .expect("claude has an adapter");
    Script {
        root: Some(RootScript {
            marker: ROOT_MARKER.into(),
            turn: RootTurn {
                tool: claude.marion_tool_name("spawn"),
                args: json!({
                    "agent_type": "codex-impl",
                    "prompt": "Add the marker file under src/ and report back.",
                    "acceptance_criteria": ["a file exists under src/ containing the marker"],
                    "writable_scope": ["src/**"],
                    "timeout_secs": CHILD_TIMEOUT_SECS,
                }),
                final_text: ROOT_FINAL_MARKER.into(),
            },
        }),
        child_narrative: NARRATIVE.into(),
        child_patch: format!(
            "*** Begin Patch\n*** Add File: {CHILD_FILE}\n+marion client-run marker\n*** End Patch"
        ),
        child_final_text: json!({"narrative": "the child is done", "result_commits": []})
            .to_string(),
        ..Script::default()
    }
}

// ------------------------------------------------------------------------------------------
// The run under test, and the cleanup that outlives a failing assertion.
// ------------------------------------------------------------------------------------------

/// A `marion run` in flight, plus everything that must be undone whether it finishes or not.
///
/// **The gate is released first** on drop — a held provider turn is a node parked for ever, and
/// tearing the run down without releasing would leave the provider's connection thread blocked on a
/// condvar for the life of the test binary.
struct Run {
    child: Option<Child>,
    gate: Arc<TurnGate>,
    needle: String,
}

impl Run {
    fn pid(&self) -> i32 {
        self.child.as_ref().expect("still held").id() as i32
    }

    /// SIGKILL the client and wait for it to be gone. **Uncatchable, and that is the point**: §7.3.1
    /// distinguishes a client that *said* it was leaving from one that vanished, and a crash is the
    /// second. Waited on rather than merely signalled, so every assertion after this is about a
    /// process that has actually stopped.
    fn kill_client(&mut self) {
        let mut c = self.child.take().expect("killed once");
        // SAFETY: `kill` on the pid of a child this process spawned and has not reaped.
        unsafe { kill(c.id() as i32, 9) };
        let _ = c.wait();
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.gate.release();
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // **Waited on, not merely signalled.** See `marion_testsupport::sweep`: the `Scratch` guard
        // removes this tree the moment this returns, and a process that has been SIGKILLed but has
        // not died yet can still land the write it was already inside.
        sweep(&self.needle);
    }
}

fn start_run(dir: &Path, repo: &Path, state: &Path, base_url: &str, gate: &Arc<TurnGate>) -> Run {
    let child = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "claude",
            "--prompt",
            &format!("{ROOT_MARKER}: delegate the marker-file task to a child."),
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--base-url",
            base_url,
            "--canned",
            "--timeout",
            ROOT_BLOCKED_SECS,
        ])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("marion run starts");
    Run {
        child: Some(child),
        gate: Arc::clone(gate),
        needle: dir.display().to_string(),
    }
}

// ------------------------------------------------------------------------------------------
// A client of the supervisor: one connection, requests out, notifications in.
// ------------------------------------------------------------------------------------------

/// One client, on one connection. The point of the type is that **nothing here reads a file**: every
/// event a test sees through it arrived over this socket.
struct Client {
    sock: UnixStream,
    lines: BufReader<UnixStream>,
    next_id: i64,
}

impl Client {
    fn dial(paths: &SocketPaths) -> Client {
        let sock = UnixStream::connect(paths.socket()).expect("the supervisor is listening");
        sock.set_read_timeout(Some(BOUND)).unwrap();
        let lines = BufReader::new(sock.try_clone().unwrap());
        Client {
            sock,
            lines,
            next_id: 1,
        }
    }

    fn send(&mut self, call: Call) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let f = Frame::Request(Request::new(RequestId::Number(id), call));
        self.sock.write_all(f.to_line().as_bytes()).unwrap();
        self.sock.flush().unwrap();
        id
    }

    fn next_frame(&mut self) -> Frame {
        let mut line = String::new();
        let n = self.lines.read_line(&mut line).expect("a frame arrives");
        assert!(n > 0, "the supervisor closed the connection");
        Frame::from_line(&line).expect("the supervisor emits well-formed frames")
    }

    fn read_to_response(&mut self, id: i64) -> (Vec<Note>, Outcome) {
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

    fn tree(&mut self) -> Vec<marion_proto::NodeSummary> {
        let id = self.send(Call::TreeSubscribe(
            marion_proto::params::TreeSubscribeParams {},
        ));
        let (_, outcome) = self.read_to_response(id);
        let Outcome::Result(body) = outcome else {
            panic!("tree/subscribe was refused: {outcome:?}")
        };
        let MethodResult::TreeSubscribe(s) = Method::TreeSubscribe.decode_result(&body).unwrap()
        else {
            panic!("wrong result")
        };
        s.nodes
    }

    fn attach(&mut self, agent: &AgentId) -> (Vec<Note>, marion_proto::result::NodeAttachResult) {
        let id = self.send(Call::NodeAttach(marion_proto::params::NodeAttachParams {
            agent_id: agent.clone(),
        }));
        let (notes, outcome) = self.read_to_response(id);
        let Outcome::Result(body) = outcome else {
            panic!("node/attach was refused: {outcome:?}")
        };
        let MethodResult::NodeAttach(r) = Method::NodeAttach.decode_result(&body).unwrap() else {
            panic!("wrong result")
        };
        (notes, r)
    }

    /// Read notifications until one satisfies `want`. **A blocking read on the socket**, with no
    /// fallback to the file: a supervisor that stops sending is a failure here rather than a slower
    /// success.
    fn wait_for_event(&mut self, mut want: impl FnMut(&Note) -> bool) -> (Vec<Note>, Note) {
        let mut seen = Vec::new();
        loop {
            match self.next_frame() {
                Frame::Notification(n) if want(&n.event) => return (seen, n.event),
                Frame::Notification(n) => seen.push(n.event),
                other => panic!("unexpected frame while following a node: {other:?}"),
            }
        }
    }

    /// §7.3.2's voluntary departure. Sent so that a supervisor whose only other client was killed
    /// does not wait out §5.7's full grace after the suite has finished with it — the same call
    /// `marion run` makes on its way out.
    fn quit(&mut self) {
        let id = self.send(Call::SessionQuit(marion_proto::params::SessionQuitParams {
            disposition: marion_proto::QuitDisposition::DetachAll,
        }));
        let (_, _outcome) = self.read_to_response(id);
    }
}

/// `(agent_seq, payload)` for the `node/event` notifications about one node, in arrival order.
fn stream(notes: &[Note], about: &AgentId) -> Vec<(u64, serde_json::Value)> {
    notes
        .iter()
        .filter_map(|n| match n {
            Note::NodeEvent {
                agent_id,
                agent_seq,
                payload,
                ..
            } if agent_id == about => Some((*agent_seq, payload.clone())),
            _ => None,
        })
        .collect()
}

/// §9's *"contiguous across the detached window"*, as an assertion rather than a hope.
///
/// Legitimate here in a way §4.2 forbids generally — that section refuses inferred ordering across
/// *sources* — because these are one file's own ordinals, assigned by that file's single writer and
/// read by one cursor.
fn assert_contiguous_from(seqs: &[u64], first: u64, what: &str) {
    assert!(!seqs.is_empty(), "{what}: nothing arrived");
    let want: Vec<u64> = (first..first + seqs.len() as u64).collect();
    assert_eq!(seqs, want.as_slice(), "{what}: not contiguous, or repeated");
}

// ------------------------------------------------------------------------------------------
// Facts about the supervisor, the node and the provider, read from outside.
// ------------------------------------------------------------------------------------------

/// A supervisor's identity as something a *recycled pid* cannot forge. `ps`'s `lstart` is the start
/// time of the process **currently** holding that pid.
fn supervisor_identity(paths: &SocketPaths) -> (i32, String) {
    let pid = read_identity(paths)
        .expect("a serving supervisor publishes its identity")
        .pid;
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    let started = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!started.is_empty(), "pid {pid} is not in the process table");
    (pid, started)
}

/// How many times the provider has been asked for one step of the **root's own** script — its
/// account of what it was asked, not the test's account of what it expected.
///
/// Filtered four ways, and every filter is load-bearing rather than tidy:
///
/// * **the wire**, because the child speaks a different one;
/// * **[`RequestKind::ScriptedTurn`]**, because Claude Code issues a *concurrent* session-title
///   request on the same wire that belongs to no node at all. A bare per-wire count therefore
///   measures a race: it is 1 or 2 at the same point in the same run depending on whether that
///   request has landed, and an assertion over it is not an assertion over anything the node did.
/// * **the marker**, because it is the same discriminator `Script::respond` itself uses to decide
///   whose turn a request is ([`RootScript::marker`]) — so this reads the provider's log by the
///   provider's own rule rather than by a second one invented here;
/// * **the step**, because *which* turn was asked for is the whole question. `RootStep::Finish` is
///   the root's closing turn, the one that produces [`ROOT_FINAL_MARKER`], and *"it had not been
///   asked for at attach time"* is the checkable fact that stands in for a sleep.
fn root_turns_asked(server: &CannedServer, step: RootStep) -> usize {
    server
        .requests()
        .expect("the request log is readable")
        .iter()
        .filter(|r| r.get("wire").and_then(serde_json::Value::as_str) == Some(ROOT_WIRE))
        .filter_map(|r| r.get("body"))
        .filter(|b| classify_anthropic(b) == RequestKind::ScriptedTurn)
        .filter(|b| b.to_string().contains(ROOT_MARKER))
        .filter(|b| classify_root(b) == step)
        .count()
}

fn until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + BOUND;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

fn project(state: &Path, repo: &Path) -> marion_core::paths::ProjectDir {
    marion_core::paths::ProjectDir::new(state, &marion_supervisor::socket::project_root(repo))
}

fn paths_for(state: &Path, repo: &Path) -> SocketPaths {
    // SAFETY: reads the calling process's real uid and cannot fail.
    socket_paths(
        state,
        &marion_supervisor::socket::project_root(repo),
        unsafe { getuid() },
    )
}

/// **The journal, read from a second process** — which is the whole point of every assertion that
/// uses it. Nothing here shares memory with the supervisor.
fn journal_nodes(state: &Path, repo: &Path) -> Vec<marion_core::registry::ReplayedNode> {
    let bytes = std::fs::read(project(state, repo).journal()).unwrap_or_default();
    let mut replay = marion_core::registry::Replay::default();
    replay.extend(&bytes);
    replay.nodes().to_vec()
}

fn the_root(nodes: &[marion_proto::NodeSummary]) -> AgentId {
    let mut roots: Vec<&marion_proto::NodeSummary> =
        nodes.iter().filter(|n| n.depth == 0).collect();
    assert_eq!(roots.len(), 1, "this run has exactly one root: {nodes:?}");
    roots.pop().unwrap().agent_id.clone()
}

fn events_len(state: &Path, repo: &Path, node: &AgentId) -> u64 {
    std::fs::metadata(project(state, repo).agent(node).events())
        .map(|m| m.len())
        .unwrap_or(0)
}

// ------------------------------------------------------------------------------------------
// T5 — M2 acceptance criterion 1
// ------------------------------------------------------------------------------------------

/// **M2 criterion 1: SIGKILL the client, and the agents keep running.**
#[test]
fn a_killed_client_leaves_its_agents_running_and_a_new_client_sees_the_whole_tree() {
    let dir = scratch("client-run-kill");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // Hold the child's **second** turn, which parks the whole tree: the child is waiting on the
    // provider, and the root is waiting on the child's `spawn` to return. Everything below happens
    // while both nodes are alive and neither can finish without this test's permission.
    let gate = TurnGate::holding_from(CHILD_WIRE, 2);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: script(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");

    let mut run = start_run(&dir, &repo, &state, &server.base_url(), &gate);
    let client_pid = run.pid();
    let paths = paths_for(&state, &repo);

    assert!(
        until(|| gate.parked() == 1),
        "the child's second turn is held, so the tree is parked with its question asked"
    );

    // ---- (i) the supervisor, before ---------------------------------------------------------
    let before = supervisor_identity(&paths);
    assert_ne!(
        before.0, client_pid,
        "the supervisor is a **different process** from the client — without that the rest of this \
         test is measuring nothing"
    );

    // The root's own pid, read out of the journal by a second process. This is the number the whole
    // criterion turns on: it is what the supervisor journaled at `command.spawn()`, and it must
    // outlive the client.
    let root_pid = until_root_pid(&state, &repo);
    assert_eq!(
        liveness(root_pid),
        Liveness::Alive,
        "the premise: the root is running before anything is killed"
    );

    // ---- the client is killed, uncatchably ---------------------------------------------------
    run.kill_client();
    assert_eq!(
        liveness(client_pid),
        Liveness::Gone,
        "the client is really gone, so nothing below can be crediting it"
    );
    let at_kill = events_len(&state, &repo, &root_from_journal(&state, &repo));
    let root = root_from_journal(&state, &repo);

    // ---- the agents kept running -------------------------------------------------------------
    assert_eq!(
        liveness(root_pid),
        Liveness::Alive,
        "**M2 criterion 1**: a client's crash must not reach the node's process. Liveness is S15's \
         three-valued reading and never `kill(pid, 0)`, which reports a zombie as alive"
    );
    assert_eq!(
        supervisor_identity(&paths),
        before,
        "the supervisor outlived the client that started it — by pid *and* start time, so a \
         reissued pid cannot satisfy it"
    );

    // ---- a new client shows the full tree ----------------------------------------------------
    let mut b = Client::dial(&paths);
    let nodes = b.tree();
    assert_eq!(
        nodes.len(),
        2,
        "*the full tree*: the root the dead client asked for, and the child it spawned — {nodes:?}"
    );
    assert_eq!(
        the_root(&nodes),
        root,
        "and the root is the one still running"
    );

    let closing_turns_at_attach = root_turns_asked(&server, RootStep::Finish);
    let (replay, attached) = b.attach(&root);
    let replayed = stream(&replay, &root);

    // **(iii) contiguity across the window nobody was watching.** Everything the root said while
    // its own client was dead is here, once each, in order, from the stream's first ordinal.
    let replay_seqs: Vec<u64> = replayed.iter().map(|(s, _)| *s).collect();
    assert_contiguous_from(&replay_seqs, 0, "B's replay of the root's detached window");
    let point = attached.mode.replay_point().records;
    assert_eq!(
        point,
        replayed.len() as u64,
        "the read point counts exactly what has already been delivered"
    );
    assert!(
        attached.mode.is_live(),
        "the root has not exited: {:?}",
        attached.mode
    );
    assert!(
        !replayed
            .iter()
            .any(|(_, p)| p.to_string().contains(ROOT_FINAL_MARKER)),
        "the root's closing turn has not happened — the provider has not been asked for it"
    );
    assert_eq!(
        closing_turns_at_attach, 0,
        "at attach time the provider had **not** been asked for the root's closing turn, so the \
         event asserted on below cannot already exist anywhere — this is the checkable fact §6.1 \
         step 8 requires in place of a sleep"
    );

    // ---- (ii) release, and hear an event that did not exist at attach time -------------------
    gate.release();
    let (between, caused) = b.wait_for_event(|n| match n {
        Note::NodeEvent {
            agent_id, payload, ..
        } => agent_id == &root && payload.to_string().contains(ROOT_FINAL_MARKER),
        _ => false,
    });
    let Note::NodeEvent { agent_seq, .. } = caused else {
        unreachable!()
    };
    assert!(
        agent_seq >= point,
        "an event caused after the attach carries an ordinal past the read point ({agent_seq} < \
         {point})"
    );
    assert!(
        root_turns_asked(&server, RootStep::Finish) > closing_turns_at_attach,
        "the event was caused by a provider request the root made **after** the attach, and the \
         provider's own log is what says so"
    );

    let mut live_seqs: Vec<u64> = stream(&between, &root).iter().map(|(s, _)| *s).collect();
    live_seqs.push(agent_seq);
    assert_contiguous_from(&live_seqs, point, "B's live leg after the replay");

    // ---- **grew after the kill**: liveness, not mere presence in the process table -----------
    assert!(
        events_len(&state, &repo, &root) > at_kill,
        "a zombie is in the process table too. The root's own `events.jsonl` was {at_kill} bytes \
         when its client died and must be longer now — that is the difference between a node that \
         survived and a node that is merely still listed"
    );

    assert_eq!(
        supervisor_identity(&paths),
        before,
        "one supervisor, across the client's whole life and death"
    );

    // The run's own client is dead, so nobody would otherwise say the fleet is finished; this one
    // does, and §5.7's grace is waived for a client that announced itself.
    assert!(
        until(|| journal_nodes(&state, &repo)
            .iter()
            .all(|n| n.state.is_exited())),
        "both nodes reach a terminal record without the dead client: {:?}",
        journal_nodes(&state, &repo)
            .iter()
            .map(|n| (n.agent_id.clone(), n.state))
            .collect::<Vec<_>>()
    );
    b.quit();
    drop(b);
    drop(server);
}

/// The root's pid, from the journal, once the supervisor has written it.
fn until_root_pid(state: &Path, repo: &Path) -> i32 {
    let mut found = None;
    until(|| {
        found = journal_nodes(state, repo)
            .into_iter()
            .find(|n| n.depth() == Some(0))
            .and_then(|n| n.pid);
        found.is_some()
    });
    found.expect("the supervisor journals the root's `Spawned` with a pid at `command.spawn()`")
}

fn root_from_journal(state: &Path, repo: &Path) -> AgentId {
    journal_nodes(state, repo)
        .into_iter()
        .find(|n| n.depth() == Some(0))
        .expect("the run's root is in the journal")
        .agent_id
}

// ------------------------------------------------------------------------------------------
// The root's `Spawned` names a live process
// ------------------------------------------------------------------------------------------

/// **§11 item 28 step 1's rule, applied to the node it had left out.**
///
/// A root's `Spawned` used to be written after the whole turn returned, carrying `pid: None`, on
/// the ground that nothing outside `marion run` could act on the number. The supervisor owning the
/// root is that something. So the record now names a process, and it is written at the instant the
/// process exists rather than at the instant it stops existing — which is what the second half of
/// this test measures: the pid is read **while the root is still running**, from a second process,
/// so a record appended after the run could not be there to read.
///
/// Liveness is `marion_testsupport::liveness` — S15's three-valued `procid` — and never
/// `kill(pid, 0)`, which reports a zombie as alive and would let this pass over a corpse.
#[test]
fn the_roots_spawned_record_names_a_live_process_while_the_root_is_still_running() {
    let dir = scratch("client-run-pid");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let gate = TurnGate::holding_from(CHILD_WIRE, 2);
    let server = CannedServer::start_gated(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script: script(),
        },
        Some(Arc::clone(&gate)),
    )
    .expect("the canned provider binds");

    let run = start_run(&dir, &repo, &state, &server.base_url(), &gate);
    assert!(
        until(|| gate.parked() == 1),
        "the tree is parked, so the root is unambiguously mid-run"
    );

    let nodes = journal_nodes(&state, &repo);
    let root = nodes
        .iter()
        .find(|n| n.depth() == Some(0))
        .expect("the root is in the journal");
    let pid = root.pid.expect(
        "**the whole assertion**: a root's `Spawned` carries the pid of the process marion started. \
         `None` here is the pre-step-6 record, and it is unusable — §6.7's kill has no target and \
         §7.2 cannot tell a node that exists from one that never did",
    );
    assert_eq!(
        liveness(pid),
        Liveness::Alive,
        "the pid the journal names is a process that exists **now**, read from a second process \
         while the run is in flight — so the record cannot have been appended after the run"
    );
    assert_ne!(
        pid,
        std::process::id() as i32,
        "…and it is not this test's own pid, which a `std::process::id()` mutation would write"
    );
    assert!(
        !root.state.is_exited(),
        "the node whose pid this is has not exited: {:?}",
        root.state
    );

    gate.release();
    let mut c = Client::dial(&paths_for(&state, &repo));
    assert!(
        until(|| journal_nodes(&state, &repo)
            .iter()
            .all(|n| n.state.is_exited())),
        "the run finishes"
    );
    c.quit();
    drop(c);
    drop(run);
    drop(server);
}

// ------------------------------------------------------------------------------------------
// A supervisor that cannot start is a refusal
// ------------------------------------------------------------------------------------------

/// A state directory nothing may create anything inside, restored on drop so the scratch guard can
/// remove it.
struct Sealed(PathBuf);

impl Sealed {
    fn new(path: PathBuf) -> Sealed {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&path).expect("the state dir exists before it is sealed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("sealing the state dir");
        Sealed(path)
    }
}

impl Drop for Sealed {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// **A run whose supervisor cannot start is refused, and journals nothing.**
///
/// This is the test that replaces the one pinning the old behaviour. `bin/marion.rs` used to say, in
/// as many words, *"a supervisor that will not start is reported, not fatal — today … the day
/// `spawn` goes over the socket, this must become a refusal, and the test that pins the current
/// behaviour is named so that whoever changes it has to say so."* This is that change, said out
/// loud: the run now **is** the socket, so a supervisor marion cannot reach is not a lost view, it
/// is a lost run.
///
/// **The absence of a fallback is the assertion, not a detail.** A build that quietly drove the
/// root in this process on failure would exit 0 here and look identical to a healthy run — while
/// leaving a live node no supervisor owned, could kill, or could hand to a re-attaching client.
/// That is why the journal is checked too: a fallback would have written one.
///
/// The supervisor is prevented from starting by sealing the state directory rather than by hiding
/// the binary, because that failure is the operator's real one (a `<state>` they cannot write) and
/// it exercises the same path a full disk would.
#[test]
fn a_run_whose_supervisor_cannot_start_is_refused_and_journals_nothing() {
    let dir = scratch("client-run-nosup");
    let repo = fixture_repo(&dir);
    // Short and outside the scratch tree's sealed part: `socket.rs` overruns `sun_path` under
    // macOS's `/private/var/folders/…`, and a socket path that fell back to `/tmp` would be one the
    // supervisor *could* bind.
    let state = PathBuf::from(format!("/tmp/mnosup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    let sealed = Sealed::new(state.clone());

    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args([
            "run",
            "claude",
            "--prompt",
            "this must never launch",
            "--repo",
            &repo.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "--canned",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--timeout",
            "5",
        ])
        .current_dir(&dir)
        .output()
        .expect("marion run starts");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a run with no supervisor did something and called it success\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("this project's supervisor is what runs the root"),
        "the refusal must be in marion's own voice and name what failed:\n{stderr}"
    );
    assert!(
        stderr.contains("no in-process fallback"),
        "and it must say why there is nothing to fall back to:\n{stderr}"
    );
    assert!(
        !stderr.contains("root ") || !stderr.contains(" in /"),
        "nothing may have announced a root:\n{stderr}"
    );

    // **Nothing was journaled**, which is the half a silent fallback would fail. The whole state
    // tree is unwritable, so the check is that marion did not create one somewhere else either.
    drop(sealed);
    let entries: Vec<_> = std::fs::read_dir(&state)
        .expect("the state dir is readable once unsealed")
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "a refused run wrote into <state>: {entries:?}"
    );
    let _ = std::fs::remove_dir_all(&state);
}
