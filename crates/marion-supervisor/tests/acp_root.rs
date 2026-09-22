//! **An ACP agent as a headless root.**
//!
//! `acp_child.rs` proves an ACP agent runs as a child; this proves the other half — `marion run
//! acp:<command>` puts one at the top of a tree, watched live, and its `spawn` reaches the
//! supervisor as a depth-0 node's. The agent is `tests/fixtures/acp/fake_acp_agent.py` in its
//! delegating mode (`spawn:<agent type>|<child prompt>`), so the whole run is hermetic: no model, no
//! network, no credential, and the child is the same fake in its reporting mode.
//!
//! Through the real `marion run` binary, so the supervisor that owns the root, the socket the frames
//! cross and the renderer that prints them are the shipped ones.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use marion_core::contract::ExitStatus;
use marion_core::node::NodeState;
use marion_core::paths::ProjectDir;
use marion_core::registry::{Replay, ReplayedNode};
use marion_supervisor::run::run_bounded;
use marion_testsupport::{fixture_repo, on_path, scratch};

/// A bound that exists only to fail: the fake answers in milliseconds.
const RUN_BOUND: Duration = Duration::from_secs(120);

fn fake() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/acp/fake_acp_agent.py"
    )
    .to_string()
}

fn journal_nodes(state: &Path, repo: &Path) -> Vec<ReplayedNode> {
    let project = ProjectDir::new(state, &marion_supervisor::socket::project_root(repo));
    let bytes = std::fs::read(project.journal()).unwrap_or_default();
    let mut replay = Replay::default();
    replay.extend(&bytes);
    replay.nodes().to_vec()
}

#[test]
fn an_acp_root_delegates_to_a_child_and_is_watched_live() {
    if !on_path("python3") {
        eprintln!("skipped: `python3` is not installed, so the fake ACP agent cannot run");
        return;
    }
    let dir = scratch("acp-root");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let agent_type = format!("acp:python3 {}", fake());

    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                &agent_type,
                "--prompt",
                &format!("spawn:{agent_type}|write the file"),
                "--repo",
                &repo.to_string_lossy(),
                "--state-dir",
                &state.to_string_lossy(),
                "--timeout",
                "90",
            ])
            .current_dir(&dir),
        RUN_BOUND,
    )
    .expect("marion run starts");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.timed_out, "marion run hung\nstderr:\n{stderr}");
    assert_eq!(
        out.code,
        Some(0),
        "the ACP root ran and delegated\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // The journal: a depth-0 root with no parent, confirmed and exited Ok, and a child under it.
    let nodes = journal_nodes(&state, &repo);
    let root = nodes
        .iter()
        .find(|n| n.depth() == Some(0))
        .unwrap_or_else(|| panic!("no root in the journal: {nodes:#?}"));
    assert_eq!(root.parent_id(), None, "a root has no parent");
    assert!(root.spawn_confirmed, "the root's `Spawned` was journalled");
    assert!(root.pid.is_some(), "and it names a process");
    assert_eq!(root.state, NodeState::Exited(ExitStatus::Ok), "{root:#?}");

    let child = nodes
        .iter()
        .find(|n| n.parent_id() == Some(&root.agent_id))
        .unwrap_or_else(|| panic!("the root's spawn made no child: {nodes:#?}"));
    assert_eq!(child.depth(), Some(1));
    let contract = child
        .contracts
        .first()
        .unwrap_or_else(|| panic!("the child has no contract: {child:#?}"));
    assert_eq!(contract.requester, root.agent_id, "the root asked for it");
    assert_eq!(contract.status, Some(ExitStatus::Ok), "{child:#?}");

    // The root's own record of what it said: live `session/update` frames, not a capture.
    let project = ProjectDir::new(&state, &marion_supervisor::socket::project_root(&repo));
    let events = std::fs::read_to_string(project.agent(&root.agent_id).events())
        .expect("the root has an events.jsonl");
    assert!(
        events.contains("session/update") && events.contains("marion/spawn"),
        "the root's frames are recorded as they arrived:\n{events}"
    );

    // stdout is the machine surface: the root's frames, verbatim.
    assert!(
        stdout
            .lines()
            .any(|l| l.contains("\"stopReason\":\"end_turn\"")),
        "the transcript reaches stdout whole:\n{stdout}"
    );
    // And stderr is what the operator saw: the chunks as one run of prose, the tool call by name.
    assert!(
        stderr.contains("Delegating to a child."),
        "the two message chunks render as one run of text:\n{stderr}"
    );
    assert!(
        stderr.lines().any(|l| l.contains("marion/spawn")),
        "the tool call renders by its title:\n{stderr}"
    );
    assert!(
        !stderr.contains("no `type` field"),
        "an ACP frame is rendered by its shape, not reported as an unknown one:\n{stderr}"
    );
}
