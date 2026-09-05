//! **`events.jsonl` written by a real run** — §7.3.3's replay leg, end to end through the real
//! `marion` binary with a real `claude` root and a real `codex` child.
//!
//! `child_stream.rs` proved a person *watching* learns about a child while it runs. That view is
//! ephemeral: it exists only in the terminal of whoever was there. §7.3.3's re-attach is about the
//! person who was **not** there, and the hardest case it names is a node *"spawned, ran and
//! terminated entirely within the detached window"* — a node with no live channel left to
//! re-subscribe to, whose whole existence must be reconstructible from disk.
//!
//! The journal already recorded *that* such a child existed and how it ended. It records not one
//! word of what the child **said**. That is what this asserts is now on disk, for both node kinds.
//!
//! # The split this file used to pin, and why there is no longer one
//!
//! It read: *"a root recorded by `root::launch_watched` in `marion run`'s process, and a child
//! recorded by `run::run_spawn` inside the **bridge's** process, which is the only process that
//! ever has a child's frames."* That was §7.3.3's own claim and it was true when it was written.
//! §11 item 28 steps 5 and 6 made every clause of it false: `marion run` is a socket client that
//! renders what it is sent, the per-child bridge is a courier that dials `agent/spawn`, and **one
//! process holds both node kinds' streams** — the supervisor, which §2 and §5.7 make exactly one
//! per project and the socket lock enforces.
//!
//! So the property is stronger than the one it replaced, and the test's name says which: two node
//! kinds, two recording paths inside one process (`root::launch_owned`'s duplex stream and
//! `run::run_spawn`'s post-hoc capture of a `LaunchOnly` child), one project directory, and every
//! stream bookended at both ends. The old shape could not have asserted the last part, because a
//! child's stream lived or died with a process the harness owned.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test child_events
//! ```
//!
//! It needs real `claude` and `codex` on `PATH` and does **not** skip when they are missing, for
//! the reason `journal_wiring.rs` gives. Every model call is served by the CannedServer: **no paid
//! tokens.**

use std::process::Command;
use std::time::Duration;

use marion_core::event::{Event, Lifecycle, Payload};
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::events::EventReader;
use marion_testsupport::{fixture_repo, scratch};

mod common;
use common::script::{Delegation, claude_delegates_to_codex};

const RUN_BOUND: Duration = Duration::from_secs(300);
const ROOT_BLOCKED_SECS: &str = "5";
const CHILD_TIMEOUT_SECS: u64 = 60;
const ROOT_MARKER: &str = "MARION-CHILD-EVENTS-ROOT-TURN-8d24";
const NARRATIVE: &str = "Wrote the marker under src/ and reported back.";
const CHILD_FILE: &str = "src/child-events-marker.txt";

fn script() -> Script {
    claude_delegates_to_codex(Delegation {
        root_marker: ROOT_MARKER,
        root_final_text: "The child completed the task and reported back.",
        child_narrative: NARRATIVE,
        child_file: CHILD_FILE,
        child_file_line: "marion child-events marker",
        child_final_narrative: NARRATIVE,
        child_timeout_secs: CHILD_TIMEOUT_SECS,
    })
}

/// Every `agents/<id>/` directory the run produced, newest layout first.
fn agent_dirs(state: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(state) else {
        return out;
    };
    for p in projects.flatten() {
        let agents = p.path().join("agents");
        if let Ok(entries) = std::fs::read_dir(&agents) {
            for e in entries.flatten() {
                out.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn replay(dir: &std::path::Path) -> (EventReader, Vec<Event>) {
    EventReader::open_path(&dir.join("events.jsonl")).expect("a readable stream")
}

fn kinds(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|e| match &e.payload {
            Payload::Lifecycle(Lifecycle::Opened) => "opened".into(),
            Payload::Lifecycle(Lifecycle::Exited { status, .. }) => format!("exited:{status:?}"),
            Payload::Lifecycle(Lifecycle::Aborted { .. }) => "aborted".into(),
            Payload::Vendor { key, .. } => format!("frame:{key}"),
            Payload::Raw(_) => "raw".into(),
            Payload::Withheld { key, .. } => format!("withheld:{key}"),
            Payload::Oversized { .. } => "oversized".into(),
            Payload::Normalized(n) => match *n {},
        })
        .collect()
}

#[test]
fn a_real_run_leaves_both_node_kinds_replayable_from_streams_one_supervisor_wrote() {
    let dir = scratch("child-events");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(),
    })
    .expect("the canned provider binds");

    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
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
            &server.base_url(),
            "--canned",
            "--timeout",
            ROOT_BLOCKED_SECS,
        ])
        .current_dir(&*dir)
        .output()
        .expect("marion run completes");
    drop(server);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "marion run exited {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    let _ = RUN_BOUND;

    let dirs = agent_dirs(&state);
    assert_eq!(
        dirs.len(),
        2,
        "one root and one child were expected: {dirs:?}"
    );
    // **One project, therefore one supervisor** (§2 keys a supervisor on the project and the socket
    // lock makes it one; §5.7's start race is what settles it). Two agent directories under one
    // `<state>/<project-hash>/agents/` is the checkable half of the claim above: whichever process
    // wrote these two streams, it was not two of them in the way it used to be — the root's client
    // has exited by now and no bridge outlives its harness.
    assert_eq!(
        dirs[0].parent(),
        dirs[1].parent(),
        "both nodes' streams belong to one project's directory: {dirs:?}"
    );

    // ---- every node that ran has a stream, and it is bounded at both ends ----------------------
    let mut recorded = Vec::new();
    for d in &dirs {
        let (reader, events) = replay(d);
        assert!(
            reader.ever_written(),
            "{} ran and nothing recorded its stream",
            d.display()
        );
        let k = kinds(&events);
        assert_eq!(
            k.first().map(String::as_str),
            Some("opened"),
            "a stream must begin with its opening bookend: {k:?} in {}",
            d.display()
        );
        assert!(
            k.last().is_some_and(|l| l.starts_with("exited:")),
            "a node that finished must say so, or a replay cannot tell it from one cut mid-turn: \
             {k:?} in {}",
            d.display()
        );
        // §4.2/§7.3.3: the seam is stated in ordinals, so they must be contiguous from 0.
        assert_eq!(
            events.iter().map(|e| e.agent_seq).collect::<Vec<_>>(),
            (0..events.len() as u64).collect::<Vec<_>>(),
            "in {}",
            d.display()
        );
        assert!(
            reader.gaps().is_empty(),
            "{:?} in {}",
            reader.gaps(),
            d.display()
        );
        assert_eq!(reader.read_point().records, events.len() as u64);
        recorded.push((d.clone(), k));
    }

    // ---- the child's own words, written by the bridge's process and by nothing else ------------
    //
    // This is the case §7.3.3 cannot answer any other way. `codex exec --json` is a `LaunchOnly`
    // harness, so these frames were recovered from its capture after it exited — the only route
    // that path has — and the root's were recorded live off its duplex stream.
    let (child_dir, child_kinds) = recorded
        .iter()
        .find(|(_, k)| k.iter().any(|s| s.starts_with("frame:thread.started")))
        .expect("the codex child's own frames must be on disk somewhere");
    assert!(
        child_kinds
            .iter()
            .any(|s| s == "frame:item.completed" || s.starts_with("frame:turn.")),
        "the child's stream is bookends and nothing else: {child_kinds:?}"
    );
    let (_, child_events) = replay(child_dir);
    let said = serde_json::to_string(&child_events).unwrap();
    assert!(
        said.contains(NARRATIVE) || said.contains("apply_patch") || said.contains(CHILD_FILE),
        "the child's stream is on disk but says nothing about the work it did: {child_kinds:?}"
    );

    // ---- NC: "nobody recorded this node" is distinguishable from "this node said nothing" ------
    //
    // Both are an empty replay and an identical read point. Only `ever_written` separates them, and
    // a real state tree is where that has to hold — a node marion never recorded must never present
    // as a node that ran in silence.
    let (absent, absent_events) = EventReader::open_path(
        &dirs[0]
            .parent()
            .unwrap()
            .join("a-node-that-never-ran/events.jsonl"),
    )
    .expect("a missing stream is not an error");
    assert!(absent_events.is_empty());
    assert!(
        !absent.ever_written(),
        "a node nothing ever recorded must not read as one that produced no events"
    );
    let (present, _) = replay(&dirs[0]);
    assert_ne!(absent.ever_written(), present.ever_written());
    assert_eq!(
        absent.read_point().records,
        0,
        "and the read point genuinely cannot tell them apart, which is why the flag exists"
    );
}
