//! **M2's acceptance criterion 4** — §9's re-attach, against a real run, over the real socket.
//!
//! > *"A client attaches to a running session, detaches, and re-attaches. The new client sees the
//! > same supervisor process, receives events emitted after it attached, and the node's event
//! > stream is contiguous across the detached window."*
//!
//! Three clauses, and §9 names the middle one the load-bearing half: it is the only assertion in
//! that section a **replay-only** implementation fails. So the whole design of this file is about
//! making that clause impossible to satisfy by accident.
//!
//! # What "after it attached" is made to mean here
//!
//! An event that merely *arrives* after an attach proves nothing — a full-file replay delivers
//! every event after the attach. The assertion has to be over an event whose **existence** was
//! caused afterwards, and that means the test has to be the cause.
//!
//! It is. The child node is scripted against the canned provider and its second turn is **held**
//! (`marion_provider::TurnGate`), so the node is parked with its question asked and unanswered.
//! The client attaches while it is parked. Only then is the turn released, and only then does the
//! node ask its **third** question — which is the one that produces the event asserted on.
//!
//! And the ordering is not asserted by the test about itself. §6.1 step 8's *MUST NOT substitute a
//! sleep for an observation* binds a test as hard as it binds a launcher, so the evidence is the
//! provider's own request log, written by a third party before the hold: at the moment of the
//! attach that log contains **two** requests on the child's wire, and the event asserted on is
//! caused by the **third**. *"The provider had not been asked at attach time"* is a fact in a file.
//!
//! # And it is asserted on the connection, not on the file
//!
//! Every event this file asserts about is read as a `node/event` **notification off the socket the
//! client already had** — never by re-reading `events.jsonl`, never by calling `node/attach` again.
//! A helper that "received" by polling the file would pass every other assertion here, which is why
//! the third required mutation is `Outbound::send` going quiet.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test node_attach
//! ```
//!
//! Needs real `claude` and `codex` on `PATH` and does **not** skip when they are missing, for the
//! reason `journal_wiring.rs` gives. Every model call is served by the CannedServer: no paid tokens.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_proto::notify::Event as Note;
use marion_proto::{Call, Frame, Method, MethodResult, Outcome, Request, RequestId};
use marion_provider::{CannedServer, Config, RootScript, RootTurn, Script, TurnGate};
use marion_supervisor::socket::{SocketPaths, read_identity, socket_paths};
use marion_testsupport::{fixture_repo, kill_hard, scratch, survivors};
use serde_json::json;

unsafe extern "C" {
    fn getuid() -> u32;
}

/// How long anything in this file may take before it is a failure. Never a verdict: every
/// assertion below is over an identity, an ordinal, a count or a payload — never over elapsed
/// time — and no failure here may be repaired by widening this.
const BOUND: Duration = Duration::from_secs(180);

const ROOT_MARKER: &str = "MARION-NODE-ATTACH-ROOT-TURN-51ac";
/// Appears in the child's **closing** turn and nowhere else, so an event carrying it can only have
/// come from the provider request the test released.
const FINAL_MARKER: &str = "MARION-ATTACH-AFTER-RELEASE-6f2b";
const NARRATIVE: &str = "Wrote the marker under src/ and reported back.";
const CHILD_FILE: &str = "src/node-attach-marker.txt";
const CHILD_TIMEOUT_SECS: u64 = 120;
const ROOT_BLOCKED_SECS: &str = "5";
/// The child's wire. Counted per wire because arrival order across the port is a race by
/// construction — see `marion_provider::gate`.
const CHILD_WIRE: &str = "responses";

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
                final_text: "The child completed the task and reported back.".into(),
            },
        }),
        child_narrative: NARRATIVE.into(),
        child_patch: format!(
            "*** Begin Patch\n*** Add File: {CHILD_FILE}\n+marion node-attach marker\n*** End Patch"
        ),
        child_final_text: json!({"narrative": FINAL_MARKER, "result_commits": []}).to_string(),
        ..Script::default()
    }
}

// ------------------------------------------------------------------------------------------
// The run under test, and the cleanup that outlives a failing assertion.
// ------------------------------------------------------------------------------------------

/// A `marion run` in flight, plus everything that must be undone whether it finishes or not.
///
/// A guard rather than trailing statements, for `marion_testsupport::Scratch`'s reason: a failing
/// assertion unwinds straight past cleanup, so the runs that leak are exactly the runs that failed.
/// **The gate is released first** — a held provider turn is a node parked for ever, and killing the
/// run without releasing would leave the provider's connection thread blocked on a condvar for the
/// life of the test binary.
struct Run {
    child: Option<Child>,
    gate: Arc<TurnGate>,
    needle: String,
}

impl Run {
    fn wait(&mut self) -> std::process::ExitStatus {
        self.gate.release();
        let mut c = self.child.take().expect("waited once");
        let deadline = Instant::now() + BOUND;
        loop {
            match c.try_wait().expect("try_wait") {
                Some(s) => return s,
                None if Instant::now() >= deadline => {
                    let _ = c.kill();
                    let _ = c.wait();
                    panic!("`marion run` did not finish within {BOUND:?}");
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.gate.release();
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        for (pid, _) in survivors(&self.needle) {
            kill_hard(pid);
        }
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

/// One client, on one connection. The point of the type is that **nothing here reads a file**:
/// every event a test sees arrived through this socket.
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

    /// Read up to the response for `id`, returning the notifications that arrived first.
    ///
    /// The replay leg arrives **before** the answer, by construction — the handler sends it inside
    /// the call — so this ordering is itself part of what is asserted: the `ReplayPoint` in the
    /// answer is a statement about what has already been delivered.
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

    /// Read notifications until one satisfies `want`, or the read bound expires.
    ///
    /// **This is the only way this file learns what a node said after an attach**, and it is a
    /// blocking read on the socket — there is no fallback to the file, so a supervisor that stops
    /// sending is a failure here rather than a slower success.
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
}

/// `(agent_seq, payload)` for the `node/event` notifications in a run, in arrival order.
///
/// A notification that is **not** a `node/event` is dropped rather than tolerated silently only
/// where the caller asked for the node's stream: `tree/node-added` and `node/state` legitimately
/// interleave, and confusing them with events is how a contiguity assertion becomes vacuous.
fn stream(notes: &[Note]) -> Vec<(u64, serde_json::Value)> {
    notes
        .iter()
        .filter_map(|n| match n {
            Note::NodeEvent {
                agent_seq, payload, ..
            } => Some((*agent_seq, payload.clone())),
            _ => None,
        })
        .collect()
}

/// §9's *"contiguous across the detached window"*, as an assertion rather than a hope.
///
/// Legitimate here in a way §4.2 forbids generally — that section refuses inferred ordering across
/// *sources* — because these are one file's own ordinals, assigned by that file's single writer and
/// read by one cursor. A gap means an event was dropped between the file and the client; a repeat
/// means the replay and the subscribe legs overlapped, which is the seam §7.3.3 is about.
fn assert_contiguous_from(seqs: &[u64], first: u64, what: &str) {
    assert!(!seqs.is_empty(), "{what}: nothing arrived");
    let want: Vec<u64> = (first..first + seqs.len() as u64).collect();
    assert_eq!(seqs, want.as_slice(), "{what}: not contiguous, or repeated");
}

// ------------------------------------------------------------------------------------------
// Facts about the supervisor and the provider, read from outside.
// ------------------------------------------------------------------------------------------

/// A supervisor's identity as something a *recycled pid* cannot forge.
///
/// The pid alone is not an identity — `restart.rs` says so in as many words, and it is the reason
/// §9's clause (i) is worth asserting at all. `ps`'s `lstart` is the start time of the process
/// **currently** holding that pid, so a supervisor that died and whose number was reissued differs
/// here even where the number does not. `spikes/s15/procid.py` is where this comparison comes from.
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

/// How many requests the provider has been asked on one wire — **its** account, not the test's.
fn asked_on(server: &CannedServer, wire: &str) -> usize {
    server
        .requests()
        .expect("the request log is readable")
        .iter()
        .filter(|r| r.get("wire").and_then(serde_json::Value::as_str) == Some(wire))
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

fn paths_for(state: &Path, repo: &Path) -> SocketPaths {
    // SAFETY: reads the calling process's real uid and cannot fail.
    socket_paths(
        state,
        &marion_supervisor::socket::project_root(repo),
        unsafe { getuid() },
    )
}

fn the_child(nodes: &[marion_proto::NodeSummary]) -> AgentId {
    let mut kids: Vec<&marion_proto::NodeSummary> = nodes.iter().filter(|n| n.depth == 1).collect();
    assert_eq!(
        kids.len(),
        1,
        "this run spawns exactly one child: {nodes:?}"
    );
    kids.pop().unwrap().agent_id.clone()
}

/// A supervisor this test started, and the sweep that ends it.
///
/// `detach::ensure_supervisor` is the production route — the same one `marion run` takes — so this
/// is not a special test topology; it is the second client §5.7 exists for, arriving after the
/// first one left. The returned connection is held for the whole test, because a supervisor with
/// zero clients and an empty tree is one §5.7 permits to leave, and a fixture that raced its own
/// subject would fail as a mystery.
struct Supervisor {
    _held: marion_supervisor::detach::Ensured,
    needle: String,
}

impl Supervisor {
    fn start(state: &Path, repo: &Path) -> Supervisor {
        let paths = paths_for(state, repo);
        let launch = marion_supervisor::detach::Launch {
            program: std::path::PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
            state_dir: state.to_path_buf(),
            project_root: marion_supervisor::socket::project_root(repo),
            idle_grace: Duration::from_millis(300),
        };
        let held = marion_supervisor::detach::ensure_supervisor(&paths, &launch)
            .expect("a supervisor starts over this project");
        Supervisor {
            _held: held,
            needle: state.display().to_string(),
        }
    }
}

impl Drop for Supervisor {
    /// Leave nothing behind whatever the assertions did. Every stage of the launch carries the
    /// state directory in its argv, so the needle names exactly the processes this test started.
    fn drop(&mut self) {
        for (pid, _) in survivors(&self.needle) {
            kill_hard(pid);
        }
    }
}

// ------------------------------------------------------------------------------------------
// T3
// ------------------------------------------------------------------------------------------

/// **M2 criterion 4, all three clauses, on a node that is still running.**
#[test]
fn a_re_attaching_client_replays_the_detached_window_and_then_hears_what_the_node_says_next() {
    let dir = scratch("attach-live");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // Hold the child's **second** turn: its first is answered, so the node runs, produces events
    // with nobody attached, and then parks with a question the provider has not answered.
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
    let paths = paths_for(&state, &repo);

    assert!(
        until(|| gate.parked() == 1),
        "the child's second turn is held, so the node is parked with its question asked"
    );

    // ---- (i) the same supervisor, and not merely the same number --------------------------
    let before = supervisor_identity(&paths);

    // ---- the first client attaches and leaves, so the next one is genuinely a *re*-attach --
    let child = {
        let mut a = Client::dial(&paths);
        let child = the_child(&a.tree());
        let (replay, got) = a.attach(&child);
        assert!(
            got.mode.is_live(),
            "the node has not exited: {:?}",
            got.mode
        );
        assert!(
            !stream(&replay).is_empty(),
            "the node had already spoken before anybody attached"
        );
        child
        // `a` is dropped here: §7.3.1's departure, which must touch nothing.
    };

    // ---- the new client attaches -----------------------------------------------------------
    let mut b = Client::dial(&paths);
    let asked_at_attach = asked_on(&server, CHILD_WIRE);
    let (replay, attached) = b.attach(&child);
    let replayed = stream(&replay);

    // **(iii) contiguity across the detached window.** Everything the node said while nobody was
    // listening is here, once each, in order, starting at the stream's first ordinal.
    let replay_seqs: Vec<u64> = replayed.iter().map(|(s, _)| *s).collect();
    assert_contiguous_from(&replay_seqs, 0, "B's replay of the detached window");
    let point = attached.mode.replay_point().records;
    assert_eq!(
        point,
        replayed.len() as u64,
        "the read point counts exactly what has already been delivered"
    );
    assert!(attached.mode.is_live(), "{:?}", attached.mode);
    assert!(
        !replayed
            .iter()
            .any(|(_, p)| p.to_string().contains(FINAL_MARKER)),
        "the closing turn has not happened yet — the provider has not been asked for it"
    );

    // **The ordering, from the provider's own log and not from this test's say-so.** Two requests
    // on the child's wire: the one that was answered, and the one being held. The third — the one
    // whose answer produces the event asserted on below — has not been asked for.
    assert_eq!(
        asked_at_attach, 2,
        "at attach time the provider had been asked twice on {CHILD_WIRE} and no more"
    );

    // ---- (ii) release, and hear an event that did not exist at attach time -----------------
    gate.release();
    let (between, caused) = b.wait_for_event(|n| match n {
        Note::NodeEvent { payload, .. } => payload.to_string().contains(FINAL_MARKER),
        _ => false,
    });
    let Note::NodeEvent { agent_seq, .. } = caused else {
        unreachable!()
    };
    assert!(
        agent_seq >= point,
        "an event caused after the attach carries an ordinal past the read point ({agent_seq} < {point})"
    );
    assert!(
        asked_on(&server, CHILD_WIRE) > asked_at_attach,
        "the event was caused by a provider request made after the attach"
    );

    // The live leg continues the replay's own ordinals: still no gap and still nothing twice.
    let mut live_seqs: Vec<u64> = stream(&between).iter().map(|(s, _)| *s).collect();
    live_seqs.push(agent_seq);
    assert_contiguous_from(&live_seqs, point, "B's live leg after the replay");

    // ---- (i), again: the same process served both attaches ---------------------------------
    assert_eq!(
        supervisor_identity(&paths),
        before,
        "the same supervisor process, by pid *and* start time, so a reissued pid cannot satisfy it"
    );

    let status = run.wait();
    assert!(status.success(), "the run itself succeeded: {status:?}");
    drop(server);
}

/// **§9's last sentence**: the hardest re-attach case — a node that was *"spawned, ran and
/// terminated entirely within the detached window"*, with no live channel left to re-subscribe to.
///
/// What separates this from a node cut mid-turn is one record: the `Lifecycle::Exited` bookend.
/// `events.rs` is emphatic that a stream holding `Opened` and frames but no terminal event says the
/// stream stopped mid-turn — so a replay of a finished node that lost its bookend would report the
/// wrong thing about it, and the mode would be the only hint, which is a claim from the journal
/// rather than from the node's own stream.
///
/// **The supervisor here is a different process from the run's, and that is not a weakening.**
/// `marion run` ends with an explicit `session/quit` (`bin/marion.rs`: §5.7's grace bridges between
/// clients that did *not* say they were leaving, and a run did), so its supervisor is gone before
/// this attach happens — §11 item 28's step 6 is what changes that, and it is deliberately not this
/// step. What is left is the case §7.3.3 actually cares about: nothing of this node survives except
/// its `events.jsonl` and the journal, and a supervisor that never saw it run must be able to hand
/// a client its whole life from disk.
#[test]
fn a_node_that_lived_and_died_while_nobody_watched_replays_with_its_terminal_bookend() {
    let dir = scratch("attach-dead");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // No hold: the whole run happens before any client of this test exists.
    let gate = TurnGate::holding_from(CHILD_WIRE, u64::MAX);
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
    let status = run.wait();
    assert!(status.success(), "the run itself succeeded: {status:?}");
    drop(server);

    let paths = paths_for(&state, &repo);
    let _sup = Supervisor::start(&state, &repo);
    assert!(
        until(|| read_identity(&paths).is_some()),
        "a supervisor booted over this project's journal is serving"
    );
    let identity = supervisor_identity(&paths);

    let mut c = Client::dial(&paths);
    let child = the_child(&c.tree());
    let (replay, attached) = c.attach(&child);
    let replayed = stream(&replay);

    assert!(
        !attached.mode.is_live(),
        "a node that finished while detached has no channel to re-subscribe to: {:?}",
        attached.mode
    );
    let seqs: Vec<u64> = replayed.iter().map(|(s, _)| *s).collect();
    assert_contiguous_from(&seqs, 0, "the whole life of a node nobody watched");
    assert_eq!(attached.mode.replay_point().records, replayed.len() as u64);

    let kinds: Vec<String> = replayed.iter().map(|(_, p)| p.to_string()).collect();
    assert!(
        kinds.first().is_some_and(|k| k.contains("\"Opened\"")),
        "the stream opens with the bookend that says marion began recording: {:?}",
        kinds.first()
    );
    assert!(
        kinds.last().is_some_and(|k| k.contains("Exited")),
        "**the bookend is the whole point**: without it this node is indistinguishable from one \
         cut mid-turn. Last payload was {:?}",
        kinds.last()
    );
    assert!(
        kinds.iter().any(|k| k.contains(FINAL_MARKER)),
        "everything the node ever said is in the replay, closing turn included"
    );
    assert_eq!(
        supervisor_identity(&paths),
        identity,
        "one supervisor, before and after"
    );
}
