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

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_proto::notify::Event as Note;
use marion_provider::{CannedServer, Config, Script, TurnGate};
use marion_supervisor::socket::{SocketPaths, read_identity};
use marion_testsupport::{fixture_repo, scratch, sweep};

mod common;
use common::client::{Client, paths_for};
use common::run::start_run;
use common::script::{Delegation, claude_delegates_to_codex};

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
    claude_delegates_to_codex(Delegation {
        root_marker: ROOT_MARKER,
        root_final_text: "The child completed the task and reported back.",
        child_narrative: NARRATIVE,
        child_file: CHILD_FILE,
        child_file_line: "marion node-attach marker",
        child_final_narrative: FINAL_MARKER,
        child_timeout_secs: CHILD_TIMEOUT_SECS,
    })
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
            auth: marion_harness::Auth::Canned,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
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
        sweep(&self.needle);
    }
}

// ------------------------------------------------------------------------------------------
// T3
// ------------------------------------------------------------------------------------------

/// **M2 criterion 4, all three clauses, on a node that is still running.**
#[test]
fn a_re_attaching_client_replays_the_detached_window_and_then_hears_what_the_node_says_next() {
    common::script::require_claude_and_codex();
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

    let mut run = start_run(
        &dir,
        &repo,
        &state,
        &server.base_url(),
        &gate,
        ROOT_MARKER,
        ROOT_BLOCKED_SECS,
    );
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
    common::script::require_claude_and_codex();
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

    let mut run = start_run(
        &dir,
        &repo,
        &state,
        &server.base_url(),
        &gate,
        ROOT_MARKER,
        ROOT_BLOCKED_SECS,
    );
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
