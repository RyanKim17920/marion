//! **A node the operator asks for directly is a contracted node, not a root.**
//!
//! One run of real binaries against marion's canned provider. With no parent node, a spawn that
//! asks for a worktree (`isolation: "worktree"`, which is what `marion mcp` sends for a writer) gets
//! what a child gets — its own worktree on a `marion/` branch, verification, a contract with usage
//! on it — at the top of the tree, with the operator as its requester. A race with no parent seats
//! such nodes. Both are driven over the socket, and then end to end through the real `marion mcp`
//! binary the way an MCP client drives it.
//!
//! What is asserted: the work lands on the node's own branch and never in the operator's checkout;
//! verification ran and passed; the contract names the operator as requester; the node sits at the
//! top of the tree one level below a root, with a contract, so it is not a root; a top-level race is
//! decided from such seats.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use marion_core::contract::{ExitStatus, Isolation};
use marion_core::journal::RecordKind;
use marion_core::paths::ProjectDir;
use marion_core::proto::notify::Event as Note;
use marion_core::proto::params::{AgentSpawnParams, NodeResumeParams};
use marion_core::proto::{Call, Method, MethodResult, Outcome};
use marion_core::race::{DecidedBy, SeatVerdict};
use marion_provider::{CannedServer, Config, NodeScript, Script, ScriptedCall};
use marion_supervisor::socket::project_root;
use marion_testsupport::{fixture_repo, persisted_contract, scratch};
use serde_json::{Value, json};

mod common;
use common::client::{Client, paths_for};
use common::journal::records;

const BOUND: Duration = Duration::from_secs(240);
const MARKER: &str = "MARION-TOP-LEVEL-CONTRACTED-5c1e";
const MODELS: [&str; 2] = ["top-race-model-one", "top-race-model-two"];
const DONE_FILE: &str = "ok";

/// A claude node that writes `file` and reports, keyed on `marker` in its request.
fn writer(marker: &str, prefix: &str, file: &str) -> NodeScript {
    let claude = marion_harness::adapter_for(marion_core::harness::Harness::ClaudeCode)
        .expect("claude has an adapter");
    NodeScript {
        marker: marker.into(),
        call_prefix: prefix.into(),
        turns: vec![
            ScriptedCall::new("Write", json!({"file_path": file, "content": "the work\n"})),
            ScriptedCall::new(
                claude.marion_tool_name("report"),
                json!({"narrative": format!("wrote {file}")}),
            ),
        ],
        final_text: "Done.".into(),
    }
}

fn script() -> Script {
    Script {
        nodes: vec![
            writer(MARKER, "one", DONE_FILE),
            // Seat 2 of the race does the work; seat 1 writes the wrong file.
            writer(MODELS[0], "seat1", "wrong.txt"),
            writer(MODELS[1], "seat2", DONE_FILE),
        ],
        ..Script::default()
    }
}

/// The canned provider, a supervisor and a repository; `None` where claude is not installed.
struct Bed {
    repo: std::path::PathBuf,
    state: std::path::PathBuf,
    project: ProjectDir,
    // Dropped in order: the supervisor stops before the provider goes, and the directory last.
    sup: Option<common::Supervisor>,
    server: CannedServer,
    _dir: marion_testsupport::Scratch,
}

impl Drop for Bed {
    fn drop(&mut self) {
        if let Some(mut s) = self.sup.take() {
            s.stop();
        }
    }
}

fn bed(tag: &str, supervisor: bool) -> Option<Bed> {
    if !marion_testsupport::harness_available("claude") {
        return None;
    }
    let dir = scratch(tag);
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let key = project_root(&repo);
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(),
    })
    .expect("the canned provider binds");
    let sup = supervisor.then(|| {
        common::Supervisor::start(
            &state,
            &key,
            &std::env::var("PATH").unwrap_or_default(),
            &server.base_url(),
            BOUND,
        )
    });
    Some(Bed {
        project: ProjectDir::new(&state, &key),
        repo,
        state,
        sup,
        server,
        _dir: dir,
    })
}

fn operator_params(repo: &std::path::Path) -> AgentSpawnParams {
    AgentSpawnParams {
        wider_children: None,
        review_of: None,
        notify_parent: false,
        agent_type: "claude".into(),
        prompt: format!("{MARKER}: create the file the task needs, then report."),
        native_launch: None,
        caller: None,
        repo: Some(repo.to_path_buf()),
        acceptance_criteria: vec![],
        verification: vec![format!("test -f {DONE_FILE}")],
        writable_scope: vec![],
        timeout_secs: Some(120),
        model: None,
        no_change_record: None,
        pane: None,
        isolation: Some(Isolation::Worktree),
        allow_concurrent_writes: None,
        profile: None,
        candidates: vec![],
        race: None,
        budget_tokens: None,
    }
}

/// The refs under `marion/` in `repo`.
fn task_branches(repo: &std::path::Path) -> Vec<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["branch", "--list", "marion/*", "--format=%(refname:short)"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_spawn_with_no_parent_that_asks_for_a_worktree_is_a_contracted_top_level_node() {
    let Some(bed) = bed("top-contracted", true) else {
        return;
    };
    let paths = paths_for(&bed.state, &bed.repo);
    let mut watcher = Client::dial(&paths);
    watcher.read_bound(BOUND);
    watcher.tree();
    let mut c = Client::dial(&paths);
    let id = c.send(Call::AgentSpawn(operator_params(&bed.repo)));
    let (_, outcome) = c.read_to_response(id);
    let Outcome::Result(body) = outcome else {
        panic!("the operator's contracted node was refused: {outcome:?}")
    };
    let MethodResult::AgentSpawn(spawned) = Method::AgentSpawn.decode_result(&body).unwrap() else {
        panic!("wrong result")
    };
    let task = spawned
        .task_id
        .clone()
        .expect("a contracted node answers with its contract's id");
    let agent = spawned.agent_id.clone();
    watcher.wait_for_event(|n| {
        matches!(n, Note::NodeState { agent_id, state, .. }
            if agent_id == &agent && state.is_exited())
    });

    let contract = persisted_contract(&bed.state, &task.0);
    let completion = contract
        .completion
        .as_ref()
        .expect("the node's end is recorded");
    assert_eq!(completion.status, ExitStatus::Ok, "{completion:?}");
    assert_eq!(
        contract.requester.0,
        marion_supervisor::run::OPERATOR_REQUESTER,
        "the operator asked, not a node"
    );
    assert_eq!(completion.evidence.len(), 1, "verification ran");
    assert_eq!(completion.evidence[0].exit_code, Some(0), "and passed");
    assert!(
        !bed.repo.join(DONE_FILE).exists(),
        "the work never landed in the operator's checkout"
    );
    let branches = task_branches(&bed.repo);
    assert!(
        !branches.is_empty(),
        "the work landed on the node's own marion/ branch: {branches:?}"
    );

    // A top-level entry with a contract: not a root, and one level below where a root sits.
    let journal = records(&bed.project.journal());
    let intent = journal
        .iter()
        .find_map(|k| match k {
            RecordKind::SpawnIntent(i) if i.agent_id == agent => Some(i.clone()),
            _ => None,
        })
        .expect("the node's intent is journaled");
    assert_eq!(intent.parent_id, None);
    assert_eq!(intent.depth, 1);
    assert_eq!(intent.task_id, Some(task.clone()));
    assert!(!intent.is_root());
    let tree = c.tree();
    let node = tree
        .iter()
        .find(|n| n.agent_id == agent)
        .expect("in the tree");
    assert!(node.parent_id.is_none() && !node.is_root(), "{node:?}");

    // **A second life is asked for on the child path, as the operator's node again**, not refused
    // as a node with no parent. This one finished and its worktree is gone once its end is filed,
    // so the relaunch is refused for that, by name — a refusal only the child path reaches.
    let worktree = bed.project.agent(&agent).path().join("worktree");
    assert!(
        marion_testsupport::until_within(BOUND, Duration::from_millis(20), || !worktree.exists()),
        "the finished node's worktree is removed"
    );
    let id = c.send(Call::NodeResume(NodeResumeParams {
        agent_id: agent.clone(),
        prompt: format!("{MARKER}: carry on."),
    }));
    let (_, outcome) = c.read_to_response(id);
    assert!(
        matches!(&outcome, Outcome::Error(e) if e.message.contains("no longer exists")),
        "{outcome:?}"
    );

    // A root's own fields are refused on it, before anything is written.
    for (field, edit) in [
        (
            "no_change_record",
            Box::new(|p: &mut AgentSpawnParams| p.no_change_record = Some(true))
                as Box<dyn Fn(&mut AgentSpawnParams)>,
        ),
        ("pane", Box::new(|p| p.pane = Some(true))),
    ] {
        let mut p = operator_params(&bed.repo);
        edit(&mut p);
        let id = c.send(Call::AgentSpawn(p));
        let (_, outcome) = c.read_to_response(id);
        assert!(
            matches!(&outcome, Outcome::Error(e) if e.message.contains(field)),
            "{field}: {outcome:?}"
        );
    }
}

#[test]
fn a_race_with_no_parent_seats_contracted_top_level_nodes() {
    let Some(bed) = bed("top-race", true) else {
        return;
    };
    let paths = paths_for(&bed.state, &bed.repo);
    let mut watcher = Client::dial(&paths);
    watcher.read_bound(BOUND);
    watcher.tree();
    let mut c = Client::dial(&paths);
    let id = c.send(Call::AgentSpawn(AgentSpawnParams {
        agent_type: String::new(),
        prompt: "Create the file the task needs at the repository root, then report.".into(),
        isolation: None,
        candidates: MODELS.iter().map(|m| format!("claude:{m}")).collect(),
        ..operator_params(&bed.repo)
    }));
    let (_, outcome) = c.read_to_response(id);
    let Outcome::Result(body) = outcome else {
        panic!("the operator's race was refused: {outcome:?}")
    };
    let MethodResult::AgentSpawn(spawned) = Method::AgentSpawn.decode_result(&body).unwrap() else {
        panic!("wrong result")
    };
    let started = spawned.race.expect("a race answers with its seats");
    watcher.wait_for_event(|n| {
        matches!(n, Note::NodeAdded { node, .. }
            if node.race.as_ref().is_some_and(|b| b.verdict.is_some()))
    });
    let result = marion_supervisor::race::read_result(&bed.project, &started.race_id)
        .expect("the scoreboard is on disk once the decision is announced");
    assert_eq!(result.winner, Some(2), "{}", result.scoreboard());
    assert_eq!(result.decided_by, DecidedBy::Verification);
    assert_eq!(
        result.seats.iter().map(|r| r.verdict).collect::<Vec<_>>(),
        vec![SeatVerdict::Failed, SeatVerdict::Won]
    );
    assert_eq!(
        result.requester.0,
        marion_supervisor::run::OPERATOR_REQUESTER
    );
    let journal = records(&bed.project.journal());
    assert!(journal.iter().any(
        |k| matches!(k, RecordKind::RaceOpened(o) if o.race_id == started.race_id && o.parent_id.is_none())
    ));
    for seat in started.seats.iter().filter_map(|s| s.agent_id.as_ref()) {
        let intent = journal
            .iter()
            .find_map(|k| match k {
                RecordKind::SpawnIntent(i) if &i.agent_id == seat => Some(i),
                _ => None,
            })
            .expect("every seat's intent is journaled");
        assert!(
            intent.parent_id.is_none() && intent.depth == 1 && intent.task_id.is_some(),
            "a seat is a contracted top-level node: {intent:?}"
        );
    }
    assert!(!bed.repo.join(DONE_FILE).exists());
}

// ---------------------------------------------------------------------------------------------
// The same, end to end through `marion mcp`
// ---------------------------------------------------------------------------------------------

/// `marion mcp` over stdio, as an MCP client's config starts it. Its supervisor is the one it starts
/// on demand, against the canned provider.
struct Mcp {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<Option<String>>,
    next_id: i64,
}

impl Mcp {
    fn start(bed: &Bed) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_marion"))
            .arg("mcp")
            .arg("--repo")
            .arg(&bed.repo)
            .arg("--state-dir")
            .arg(&bed.state)
            .args(["--canned", "--base-url", &bed.server.base_url()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the marion binary starts");
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                match stdout.read_line(&mut line) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(None);
                        return;
                    }
                    Ok(_) if line.trim().is_empty() => {}
                    Ok(_) => {
                        if tx.send(Some(line)).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let mut mcp = Mcp {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        mcp.call(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "top-level-contracted-test", "version": "0"}}),
        );
        mcp
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let w = self.stdin.as_mut().expect("stdin is open");
        writeln!(
            w,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        w.flush().unwrap();
        loop {
            let line = match self.lines.recv_timeout(BOUND) {
                Ok(Some(line)) => line,
                other => panic!("no reply to {method}: {other:?}"),
            };
            let reply: Value = serde_json::from_str(line.trim()).unwrap();
            if reply.get("id") == Some(&json!(id)) {
                return reply;
            }
        }
    }

    /// A `tools/call`'s text and whether it is an error.
    fn tool(&mut self, name: &str, arguments: Value) -> (String, bool) {
        let r = self.call("tools/call", json!({"name": name, "arguments": arguments}));
        let text = r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("a tool answer carries text: {r}"))
            .to_string();
        (text, r["result"]["isError"].as_bool() == Some(true))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// **`marion mcp`'s writer gets a worktree and a contract, and its race is decided**, where both
/// used to be refused unless the checkout was stated.
#[test]
fn marion_mcp_spawns_a_writer_into_a_worktree_and_races_contracted_seats() {
    let Some(bed) = bed("top-mcp", false) else {
        return;
    };
    let mut mcp = Mcp::start(&bed);
    let (text, is_error) = mcp.tool(
        "spawn",
        json!({
            "agent_type": "claude",
            "prompt": format!("{MARKER}: create the file the task needs, then report."),
            "verification": [format!("test -f {DONE_FILE}")],
            "timeout_secs": 120,
        }),
    );
    assert!(!is_error, "the writer's spawn was served: {text}");
    assert!(
        text.contains("marion/"),
        "the answer names the node's own branch: {text}"
    );
    assert!(!bed.repo.join(DONE_FILE).exists(), "not in the checkout");

    let (text, is_error) = mcp.tool(
        "spawn",
        json!({
            "candidates": MODELS.iter().map(|m| format!("claude:{m}")).collect::<Vec<_>>(),
            "prompt": "Create the file the task needs at the repository root, then report.",
            "verification": [format!("test -f {DONE_FILE}")],
            "timeout_secs": 120,
        }),
    );
    assert!(!is_error, "the race was decided with a winner: {text}");
    assert!(
        text.contains(MODELS[1]),
        "the scoreboard names the winner: {text}"
    );
    let seats = records(&bed.project.journal())
        .into_iter()
        .filter(|k| matches!(k, RecordKind::SpawnIntent(i) if i.race.is_some() && i.parent_id.is_none()))
        .count();
    assert_eq!(seats, 2, "both seats are top-level contracted nodes");
    drop(mcp);
    // The supervisor `marion mcp` started outlives it by its idle grace; this bed ends it.
    let left = marion_testsupport::sweep(&bed.state.to_string_lossy());
    assert!(left.is_empty(), "processes outlived the bed: {left:?}");
}

/// **`marion race` from a terminal**: the seats are the operator's contracted nodes, the scoreboard
/// is on stdout, the winner's branch is named, and the command exits 0 because a seat passed.
#[test]
fn marion_race_runs_the_seats_and_prints_the_scoreboard() {
    let Some(bed) = bed("top-race-cli", false) else {
        return;
    };
    let out = Command::new(env!("CARGO_BIN_EXE_marion"))
        .args(["race", "--prompt"])
        .arg("Create the file the task needs at the repository root, then report.")
        .arg("--on")
        .arg(MODELS.map(|m| format!("claude:{m}")).join(","))
        .args(["--verify", &format!("test -f {DONE_FILE}")])
        .args([
            "--timeout",
            "120",
            "--canned",
            "--base-url",
            &bed.server.base_url(),
        ])
        .arg("--repo")
        .arg(&bed.repo)
        .arg("--state-dir")
        .arg(&bed.state)
        .stdin(Stdio::null())
        .output()
        .expect("the marion binary runs");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // The supervisor the command started outlives it by its idle grace; the bed ends it.
    marion_testsupport::sweep(&bed.state.to_string_lossy());
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("decided by: verification") && stdout.contains(MODELS[1]),
        "{stdout}"
    );
    assert!(
        stderr.contains("seat 2 won") && stderr.contains("marion/"),
        "{stderr}"
    );
    assert!(!bed.repo.join(DONE_FILE).exists());
}
