//! **The journal, wired to real node lifecycle events** (design §4.3, §9's M2 criterion).
//!
//! `tests/journal.rs` proves the journal survives being a journal — torn tails, unparsable lines,
//! concurrent writers. It proves nothing about whether marion *writes* to it. This file is that
//! other half, and it is the only test in the suite whose failure means "a node marion created is
//! not in the tree": §9 measures M2 against a replay that is **structurally identical** to what the
//! run produced — *"same nodes, same parent edges, same terminal states, same contracts"* — so the
//! assertion here is over a real run's real journal, replayed, compared to the nodes and contracts
//! that run actually left on disk.
//!
//! **Two processes write it, and that is the point.** `marion run` journals the root; the child's
//! `spawn` is served by a `marion-supervisor mcp` bridge the *harness* started, in a separate
//! process (§10), and its records have to land in the same file. That is the case no single-process
//! test can cover, and it is asserted directly below — by writer identity, off the raw records.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test journal_wiring
//! ```
//!
//! It needs real `claude` and `codex` on `PATH` and it does **not** skip when they are missing, for
//! the reason `m1_hop.rs` gives. Every model call is served by the CannedProvider: **no paid
//! tokens.**

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use marion_core::contract::{AgentId, ExitStatus, TaskContract, TaskId};
use marion_core::harness::Harness;
use marion_core::journal::{RecordKind, decode};
use marion_core::node::NodeState;
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::journal::read_path;
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_bounded, run_spawn};

/// Generous: the bound exists so a hung harness fails loudly instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(300);

/// A scratch dir that removes itself.
///
/// `Drop`, and not a `remove_dir_all` at the end of each test: a failing assertion unwinds straight
/// past any trailing cleanup, so an explicit call leaks on exactly the runs that fail — the ones a
/// developer re-runs most. `Drop` catches those, plus every `?` and early return.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // Ignored: the dir may already be gone, and a cleanup failure must not mask the test's own
        // verdict.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// Bind the returned guard for the whole test — `scratch("x").join("y")` drops the dir at the end
/// of that statement, deleting it out from under the test.
fn scratch(name: &str) -> Scratch {
    let p = std::env::temp_dir().join(format!("marion-journal-e2e-{name}-{}", std::process::id()));
    // Removed on the way *in* as well: a run killed hard enough to skip `Drop` leaves a dir behind,
    // and pids recycle, so a later run can inherit that exact name.
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    Scratch(p.canonicalize().expect("scratch dir canonicalises"))
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fixture_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/keep.txt"), "keep\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=marion@example.invalid",
            "-c",
            "user.name=marion",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    repo
}

fn on_path(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Every `contracts/<task_id>.json` marion actually persisted — the ground truth the journal's
/// `ContractPersisted` records are compared against.
fn persisted_contracts(state: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "json")
                && p.parent().is_some_and(|d| d.ends_with("contracts"))
            {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(state, &mut out);
    out
}

/// The agent-dirs marion actually created — the ground truth the replayed *nodes* are compared
/// against. A node in one set and not the other is the M2 failure this file exists to catch.
fn agent_dirs(project: &ProjectDir) -> BTreeSet<String> {
    std::fs::read_dir(project.agents_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Which writer wrote each record about each node — the cross-process evidence. Two distinct
/// writers must appear, because `marion run` and the bridge are two processes (§10).
fn writers_by_agent(journal: &Path) -> Vec<(String, String)> {
    std::fs::read(journal)
        .expect("the journal is readable")
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(decode)
        .map(|r| (r.agent_id().0.clone(), r.writer.0.clone()))
        .collect()
}

#[test]
fn a_real_run_journals_every_node_it_creates_and_replay_reconstructs_the_tree() {
    assert!(
        on_path("claude"),
        "this is about a REAL root; put `claude` on PATH"
    );
    assert!(
        on_path("codex"),
        "this is about a REAL child; put `codex` on PATH"
    );

    let root_dir = scratch("hop");
    let repo = fixture_repo(&root_dir);
    let state = root_dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root_dir.join("provider-requests.jsonl"),
        script: Script::default(),
    })
    .expect("the canned provider binds");

    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                "claude",
                "--prompt",
                "Delegate the marker-file task to a codex child.",
                "--repo",
                &repo.to_string_lossy(),
                "--state-dir",
                &state.to_string_lossy(),
                // Zero paid calls: every model request in this test is the canned provider's.
                "--canned",
                "--base-url",
                &server.base_url(),
                "--timeout",
                "5",
            ])
            .current_dir(&*root_dir),
        RUN_BOUND,
    )
    .expect("marion run starts");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.timed_out, "marion run hung\nstderr:\n{stderr}");
    assert_eq!(
        out.code,
        Some(0),
        "marion run exited {:?}\nstderr:\n{stderr}",
        out.code
    );

    // §4.3's location, resolved the one way marion resolves it — never a second literal.
    let project = ProjectDir::new(&state, &repo);
    let journal = project.journal();
    assert!(
        journal.is_file(),
        "a run that created two nodes wrote no journal at {}",
        journal.display()
    );

    let replay = read_path(&journal).expect("the journal replays");
    assert_eq!(replay.truncation, None, "a clean run leaves an intact file");
    assert!(
        replay.gaps.is_empty(),
        "a per-writer ordinal gap means a record was written and lost: {:?}",
        replay.gaps
    );

    // ---- same nodes. -------------------------------------------------------------------------
    // Compared against what the *run* left on disk, not against a number written here: an
    // agent-dir with no node in the replayed tree is exactly the untracked live process M2 forbids.
    let replayed: BTreeSet<String> = replay
        .nodes()
        .iter()
        .map(|n| n.agent_id.0.clone())
        .collect();
    assert_eq!(
        replayed,
        agent_dirs(&project),
        "every node marion created must appear in the journal, and no node it did not"
    );
    assert_eq!(replay.nodes().len(), 2, "one root, one child: {replayed:?}");

    // ---- same parent edges. ------------------------------------------------------------------
    let roots = replay.roots();
    assert_eq!(roots.len(), 1, "one root: {roots:?}");
    let root_id = roots[0].agent_id.clone();
    let root = replay.get(&root_id).unwrap();
    assert_eq!(root.parent_id(), None, "§9: a root has no parent");
    assert_eq!(root.depth(), Some(0), "§3.1: the root is depth 0");
    assert_eq!(
        root.task_id(),
        None,
        "§9: a root has no TaskContract — an absence, not a placeholder"
    );
    assert_eq!(root.harness(), Some(Harness::ClaudeCode));
    assert!(root.spawn_confirmed, "the root's process really ran");

    let children = replay.children(&root_id);
    assert_eq!(
        children.len(),
        1,
        "the root spawned exactly one child: {children:?}"
    );
    let child = children[0];
    assert_eq!(child.parent_id(), Some(&root_id));
    assert_eq!(child.depth(), Some(1), "one level below its caller");
    assert_eq!(child.harness(), Some(Harness::Codex));
    assert_eq!(child.agent_type(), Some("codex-impl"));
    assert!(child.spawn_confirmed);

    // ---- same terminal states. ---------------------------------------------------------------
    assert_eq!(
        child.state,
        NodeState::Exited(ExitStatus::Ok),
        "the child reported through marion's tool and exited clean"
    );
    assert!(
        child
            .exit
            .as_ref()
            .is_some_and(|e| e.description.contains("child exited")),
        "the terminal record carries §6.7's ProcessExit, so replay needs no contract file: {:?}",
        child.exit
    );
    assert!(
        root.state.is_exited(),
        "the root's terminal transition must be recorded too, got {:?}",
        root.state
    );
    assert!(
        replay.unresolved().is_empty(),
        "a node recorded live with no exit is what §7.2's Orphaned marking is about; this run \
         resolved both: {:?}",
        replay
            .unresolved()
            .iter()
            .map(|n| &n.agent_id.0)
            .collect::<Vec<_>>()
    );

    // ---- same contracts, named against the files the run actually wrote. ----------------------
    assert!(
        root.contracts.is_empty(),
        "§9: a root has no contract to record"
    );
    assert_eq!(child.contracts.len(), 1, "one run, one contract");
    let recorded = &child.contracts[0];
    assert_eq!(
        recorded.requester, root_id,
        "§9: requester for a top-level spawn is the root's own AgentId"
    );
    assert_eq!(recorded.status, Some(ExitStatus::Ok));

    let files = persisted_contracts(&state);
    assert_eq!(files.len(), 1, "exactly one contract on disk: {files:?}");
    assert_eq!(
        files[0],
        project.agent(&child.agent_id).contract(&recorded.task_id),
        "the journal's contract record must name the path the run actually wrote"
    );
    let persisted: TaskContract =
        serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(
        persisted.task_id, recorded.task_id,
        "and the same task id the file itself carries"
    );
    assert_eq!(persisted.requester, AgentId(root_id.0.clone()));
    assert_eq!(
        persisted.completion.as_ref().map(|c| c.status),
        recorded.status,
        "how the contract ended, as the journal records it, is how the contract says it ended"
    );

    // ---- the bridge is a separate process, and its records are in the same file. --------------
    // The case a single-process test cannot cover (§10): the root's records are written by
    // `marion run`, the child's by the `marion-supervisor mcp` bridge the *harness* started. Two
    // writer identities, one journal.
    let by_agent = writers_by_agent(&journal);
    let root_writers: BTreeSet<&String> = by_agent
        .iter()
        .filter(|(a, _)| *a == root_id.0)
        .map(|(_, w)| w)
        .collect();
    let child_writers: BTreeSet<&String> = by_agent
        .iter()
        .filter(|(a, _)| *a == child.agent_id.0)
        .map(|(_, w)| w)
        .collect();
    assert_eq!(root_writers.len(), 1, "one process journals the root");
    assert_eq!(child_writers.len(), 1, "one bridge journals the child");
    assert!(
        root_writers.is_disjoint(&child_writers),
        "the child's records must come from the bridge's own process, not the root's — otherwise \
         this run never crossed the process boundary and the cross-process claim is untested: \
         root {root_writers:?}, child {child_writers:?}"
    );

    // ---- and the record vocabulary is the one `marion-core` already defines. ------------------
    let kinds: Vec<&'static str> = std::fs::read(&journal)
        .unwrap()
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(decode)
        .map(|r| match r.kind {
            RecordKind::SpawnIntent(_) => "SpawnIntent",
            RecordKind::Spawned(_) => "Spawned",
            RecordKind::SpawnAborted(_) => "SpawnAborted",
            RecordKind::StateChanged(_) => "StateChanged",
            RecordKind::Exited(_) => "Exited",
            RecordKind::ReapIntent(_) => "ReapIntent",
            RecordKind::ReapConfirmed(_) => "ReapConfirmed",
            RecordKind::ContractPersisted(_) => "ContractPersisted",
            RecordKind::PermissionDenied(_) => "PermissionDenied",
        })
        .collect();
    for expected in ["SpawnIntent", "Spawned", "Exited", "ContractPersisted"] {
        assert!(
            kinds.contains(&expected),
            "no {expected} record in a run that spawned a child: {kinds:?}"
        );
    }
    assert!(
        !kinds.contains(&"SpawnAborted"),
        "nothing was abandoned in a run both of whose nodes exited: {kinds:?}"
    );
}

/// **A child's denied permission reaches the journal** (§9, §7.1, design §11 item 14).
///
/// The ask is real, and the comment that used to say otherwise was wrong: a Claude Code child is
/// compiled through the same `compile_headless` a root is, which emits
/// `--permission-prompt-tool stdio` **unconditionally**, so a call to any verb outside the child's
/// one-entry allowlist (`report`) produces an inbound `can_use_tool`. marion denies it — with a
/// zero `Blocked` bound, because a child's only bound is the wall clock its contract records — and
/// until this test existed that denial went **nowhere**: `duplex_child` dropped
/// `DuplexOutcome.denied_permissions` and the only `PermissionDenied` emitter was the root's.
///
/// It is provoked exactly as S9 provokes a root's (`tests/fixtures/s9/README.md`): aim the canned
/// turn at a real marion verb the node is not allowed to use. No argv surgery — marion's production
/// invocation, bridge and allowlist, unmodified — and **no model call and no paid tokens**.
#[test]
fn a_childs_denied_permission_is_journaled_and_replays_back_against_the_child() {
    assert!(
        on_path("claude"),
        "this drives a REAL claude child, the one harness of four that asks at runtime; put \
         `claude` on PATH"
    );
    let root_dir = scratch("child-denial");
    let repo = fixture_repo(&root_dir);
    let state = root_dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root_dir.join("provider-requests.jsonl"),
        script: Script {
            // Offered by marion's own bridge (the availability axis) and **absent from the child's
            // `--allowedTools`** (the permission axis), which is what makes the CLI ask instead of
            // either running it or ignoring it. The input is never read: the call is refused.
            root_tool: DENIED_VERB.into(),
            root_tool_input: serde_json::json!({
                "agent_type": "codex-impl",
                "prompt": "a verb this child may not use",
                "acceptance_criteria": [],
                "writable_scope": ["src/**"],
            }),
            root_final_text: "The call was refused; nothing further to do.".into(),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");

    let env = Env {
        repo: repo.clone(),
        project_dir: ProjectDir::new(&state, &repo),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    let contract = run_spawn(
        &env,
        &SpawnRequest {
            agent_type: "claude".into(),
            prompt: "Try the verb you were not given.".into(),
            acceptance_criteria: vec![],
            writable_scope: vec!["src/**".into()],
            timeout_secs: 60,
            model: None,
        },
        &TaskId("denial-1".into()),
        &Caller::root(
            "root",
            marion_core::agent_type::builtin("claude").expect("the root type resolves"),
        ),
    );
    drop(server);
    // The run itself is not the assertion — a refused call leaves the child with nothing to report
    // — but a *failed launch* would make the journal claim below vacuous, so it is checked.
    let contract = contract.expect("the child launched and ran");

    let project = ProjectDir::new(&state, &repo);
    let replay = read_path(&project.journal()).expect("the journal replays");
    assert_eq!(replay.truncation, None);
    let node = replay
        .nodes()
        .iter()
        .find(|n| n.task_id() == Some(&contract.task_id))
        .expect("the child is in the replayed tree")
        .clone();
    let denied: Vec<&str> = node
        .denied_permissions
        .iter()
        .map(|d| d.tool.as_str())
        .collect();
    assert_eq!(
        denied,
        vec![DENIED_VERB],
        "the child asked for a verb it was not allowed and marion denied it; that denial must be \
         in the journal, against the child's own agent_id, or it is recorded nowhere at all"
    );
    assert_eq!(
        node.denied_permissions[0].agent_id, node.agent_id,
        "§4.3: every record is about exactly one node, and this one is about the child"
    );
    assert!(
        node.denied_permissions[0].reason.contains("bound is zero"),
        "the record must say why marion denied rather than waited: {}",
        node.denied_permissions[0].reason
    );
    // The same replay still reconstructs the rest of the node, so the new record is additive
    // rather than a shape that displaced anything.
    assert_eq!(node.harness(), Some(Harness::ClaudeCode));
    assert!(node.spawn_confirmed, "the child's process really ran");
}

/// A verb marion's bridge really serves and a **child** is never allowed to call: `run_spawn`
/// compiles a child's `--allowedTools` as exactly `[report]`.
const DENIED_VERB: &str = "mcp__marion__spawn";
