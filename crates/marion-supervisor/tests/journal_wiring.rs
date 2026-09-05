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
//! **One process writes it, and that is the point — it used to be the opposite one.**
//!
//! This file was built on §10's split: `marion run` journalled the root, and the child's `spawn` was
//! served by a `marion-supervisor mcp` bridge the *harness* started in a separate process, whose
//! records had to land in the same file. The writer identities were asserted **disjoint**, and that
//! was the strongest available evidence that a run had crossed a process boundary at all.
//!
//! §11 item 28 steps 5 and 6 deleted the boundary rather than moved it. The supervisor owns every
//! node: it drives the root (step 6) and it runs every child a bridge asks for (step 5), so one
//! process writes every record about every node of a project. The assertion is therefore inverted
//! rather than dropped, and the inverted form is the **stronger** of the two — a *second* writer
//! appearing now means a bridge has started journalling on its own, which is precisely the
//! in-process `run_spawn` fallback [`marion_supervisor::courier`] refuses to have. The old form
//! could not have caught that; this one fails on it, per harness pairing, in a real run.
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

use marion_core::contract::Isolation;
use marion_core::contract::{AgentId, ExitStatus, TaskContract, TaskId};
use marion_core::harness::Harness;
use marion_core::journal::{RecordKind, decode};
use marion_core::node::NodeState;
use marion_core::paths::ProjectDir;
use marion_harness::adapter_for;
use marion_provider::{CannedServer, Config, EditTurn, RootScript, RootTurn, Script};
use marion_supervisor::journal::read_path;
use marion_supervisor::root::{RootPath, root_path};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_bounded, run_spawn};
use marion_testsupport::{fixture_repo, judge, on_path, persisted_contracts, scratch};
use serde_json::json;

/// Generous: the bound exists so a hung harness fails loudly instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(300);

/// The root's bound on a **`LaunchOnly`** surface, where `--timeout` is a wall clock over the whole
/// run — and the child's entire run happens inside the root's `spawn` call, so this has to cover
/// both. Matches `cross_product`, for the reason that file gives: opencode never exits on a
/// provider hang, so a short-enough ceiling is what turns a hang into a failure.
const ROOT_WALL_CLOCK_SECS: &str = "150";

/// The root's bound on a **duplex** surface, where `--timeout` is §9's per-episode `Blocked`-only
/// budget and *not* a wall clock. Short: an unanswerable permission request must fail in seconds.
const ROOT_BLOCKED_SECS: &str = "5";

/// The child's own wall clock, through `spawn`'s `timeout_secs`.
const CHILD_TIMEOUT_SECS: u64 = 60;

/// Present in the **root's** prompt and nowhere in the child's — the provider's role discriminator
/// when both nodes of a pairing speak the same wire. Same device, and same reason, as
/// `cross_product`'s marker of the same name.
const ROOT_MARKER: &str = "MARION-JOURNAL-ROOT-TURN-8c31";

/// The child's task, free of [`ROOT_MARKER`] so a child request never reads as the root's.
const CHILD_PROMPT: &str = "Add the journal marker file under src/ and report back.";

/// The narrative every child's script reports.
const NARRATIVE: &str = "Wrote the journal marker under src/ and reported back.";

/// The file a child that *can* write is driven to write. Worktree-relative and inside
/// `writable_scope`, for the reason `cross_product` states: the absolute path does not exist when
/// this script is written, because the agent id is minted inside `spawn`.
const CHILD_FILE: &str = "src/journal-marker.txt";

/// What that file contains.
const CHILD_FILE_CONTENT: &str = "marion journal marker\n";

// --- the node table ------------------------------------------------------------------------------
//
// The same shape `cross_product` uses, and deliberately not a second vocabulary: a pairing is one
// row in each role. Trimmed to what a journal assertion reads — this file asserts about records,
// not about worktree contents, so `child_writes_worktree` has no counterpart here.

/// One harness, in both of the roles it can play.
#[derive(Debug, Clone, Copy)]
struct Node {
    /// The built-in agent type. `codex-impl` is `codex`'s canonical name, so both roles use it.
    agent_type: &'static str,
    harness: Harness,
    /// `--model` for a root, and `spawn`'s `model` for a child. `None` where the harness takes
    /// none; the two that refuse to compile without one state it.
    model: Option<&'static str>,
    /// The binary that must be on `PATH`.
    program: &'static str,
}

const CLAUDE: Node = Node {
    agent_type: "claude",
    harness: Harness::ClaudeCode,
    model: None,
    program: "claude",
};

const CODEX: Node = Node {
    agent_type: "codex-impl",
    harness: Harness::Codex,
    model: None,
    program: "codex",
};

const GEMINI: Node = Node {
    agent_type: "gemini",
    // Explicit: the adapter REFUSES to compile without `-m` (S12's `auto` router hang).
    model: Some("gemini-2.5-flash"),
    harness: Harness::Gemini,
    program: "gemini",
};

const OPENCODE: Node = Node {
    agent_type: "opencode",
    // `provider/model`, the only spelling `-m` accepts.
    model: Some("marion/canned-1"),
    harness: Harness::OpenCode,
    program: "opencode",
};

/// marion's `spawn`, in the spelling **this harness's wire** dispatches on. Codex is the exception
/// and it is a wire fact: its `marion_tool_name` is the code-mode JavaScript identifier, while the
/// wire dispatch form is the bare verb beside `namespace: "mcp__marion"` (§11 item 12).
fn spawn_tool(node: &Node) -> String {
    let adapter = adapter_for(node.harness).expect("every harness in this table has an adapter");
    match node.harness {
        Harness::Codex => "spawn".to_string(),
        _ => adapter.marion_tool_name("spawn"),
    }
}

/// marion's `report`, in the same per-harness spelling, for the **child**'s script.
fn report_tool(node: &Node) -> String {
    let adapter = adapter_for(node.harness).expect("every harness in this table has an adapter");
    match node.harness {
        Harness::Codex => "report".to_string(),
        _ => adapter.marion_tool_name("report"),
    }
}

/// The `Script` that answers **both** nodes of one pairing — the root's half keyed on
/// [`ROOT_MARKER`], the child's half on its own wire's fields.
fn script(root: &Node, child: &Node) -> Script {
    let mut spawn_args = json!({
        "agent_type": child.agent_type,
        "prompt": CHILD_PROMPT,
        "acceptance_criteria": ["a file exists under src/ containing the marker"],
        "writable_scope": ["src/**"],
        "timeout_secs": CHILD_TIMEOUT_SECS,
    });
    // **Absent, never null**: gemini 0.53.0 validates a tool call against the declared schema before
    // dispatching it and refuses `"model": null` outright.
    if let Some(m) = child.model {
        spawn_args["model"] = json!(m);
    }
    let mut s = Script {
        root: Some(RootScript {
            marker: ROOT_MARKER.into(),
            turn: RootTurn {
                tool: spawn_tool(root),
                args: spawn_args,
                final_text: "The child completed the task and reported back.".into(),
            },
        }),
        ..Script::default()
    };
    let report = report_tool(child);
    match child.harness {
        Harness::ClaudeCode => {
            s.root_tool = report;
            s.root_tool_input = json!({ "narrative": NARRATIVE });
            s.root_final_text = "Reported back through marion. Done.".into();
        }
        Harness::Codex => {
            s.child_narrative = NARRATIVE.into();
            s.child_patch = format!(
                "*** Begin Patch\n*** Add File: {CHILD_FILE}\n+{}\n*** End Patch",
                CHILD_FILE_CONTENT.trim_end()
            );
            s.child_final_text = json!({"narrative": NARRATIVE, "result_commits": []}).to_string();
        }
        Harness::Gemini => {
            s.gemini_report_tool = report;
            s.gemini_report_args = json!({ "narrative": NARRATIVE });
        }
        Harness::OpenCode => {
            s.openai_report_tool = report;
            s.openai_report_args = json!({ "narrative": NARRATIVE });
            s.openai_edit = Some(EditTurn {
                tool: "write".into(),
                args: json!({ "filePath": CHILD_FILE, "content": CHILD_FILE_CONTENT }),
            });
        }
        // **Not a cell of this matrix, and that is a refusal rather than an omission.** Every cell
        // here drives a node through marion's *canned* provider, and an ACP node has no canned
        // mode at all: `AcpAdapter::compile` refuses `Auth::Canned` by name, because ACP has no
        // protocol-level way to point an agent at an endpoint. There is nothing to script, so this
        // says so rather than scripting something that would not be an ACP run.
        Harness::Copilot => unreachable!("no cell of this matrix names `copilot` yet"),
        Harness::Acp => unreachable!("no cell of this matrix names `acp`"),
    }
    s
}

fn marion_argv(
    root: &Node,
    repo: &Path,
    state: &Path,
    base_url: &str,
    timeout: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        root.agent_type.into(),
        "--prompt".into(),
        format!("{ROOT_MARKER}: delegate the marker-file task to a child."),
        "--repo".into(),
        repo.to_string_lossy().into_owned(),
        "--state-dir".into(),
        state.to_string_lossy().into_owned(),
        "--base-url".into(),
        base_url.into(),
        // Zero paid calls, and not optional: without it the binary refuses the loopback URL above
        // at argument parsing and the pairing never starts.
        "--canned".into(),
        "--timeout".into(),
        timeout.into(),
    ];
    if let Some(m) = root.model {
        args.push("--model".into());
        args.push(m.into());
    }
    args
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

/// Which writer wrote each record about each node.
///
/// It was the *cross-process* evidence — two distinct writers had to appear, because `marion run`
/// and the bridge were two processes (§10). Since §11 item 28 steps 5 and 6 one supervisor writes
/// every node's records, so the same reading is now the evidence that **no second writer exists**.
fn writers_by_agent(journal: &Path) -> Vec<(String, String)> {
    std::fs::read(journal)
        .expect("the journal is readable")
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(decode)
        .filter_map(|r| {
            r.agent_id()
                .map(|agent| (agent.0.clone(), r.writer.0.clone()))
        })
        .collect()
}

// --- driving one pairing -------------------------------------------------------------------------

/// What one driven pairing left behind, gathered **before** the scratch dir is removed and asserted
/// **after** — so a failing pairing can never become the leak this suite also tests for. The same
/// discipline `cross_product`, `harness_matrix` and `timeout_kill` take, for the same reason.
struct JournalEvidence {
    timed_out: bool,
    code: Option<i32>,
    stderr: String,
    /// `None` when the run wrote no journal at all, which is a distinct failure from an empty one
    /// and is reported as such.
    replay: Option<marion_core::registry::Replay>,
    /// The agent-dirs marion actually created — the ground truth the replayed nodes are compared
    /// against, rather than a count written here.
    agent_dirs: BTreeSet<String>,
    /// `(agent_id, writer_id)` for every record — the evidence that one process wrote them all.
    writers: Vec<(String, String)>,
    /// Each record's kind, in file order.
    kinds: Vec<&'static str>,
    /// `(path, parsed)` for each persisted contract, read back before cleanup.
    contracts: Vec<(PathBuf, TaskContract)>,
    /// Where the journal was, for failure messages naming a path that really existed.
    journal_path: PathBuf,
}

fn record_kinds(journal: &Path) -> Vec<&'static str> {
    std::fs::read(journal)
        .unwrap_or_default()
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
            RecordKind::KillIntent(_) => "KillIntent",
            RecordKind::KillConfirmed(_) => "KillConfirmed",
            RecordKind::ContractPersisted(_) => "ContractPersisted",
            RecordKind::PermissionDenied(_) => "PermissionDenied",
            RecordKind::RootChanged(_) => "RootChanged",
            RecordKind::RootGrantDecided(_) => "RootGrantDecided",
            RecordKind::SessionObserved(_) => "SessionObserved",
            RecordKind::SupervisorExited(_) => "SupervisorExited",
        })
        .collect()
}

/// Run one `root → child` pairing end to end through the real `marion` binary, and collect
/// everything the assertions need before anything is cleaned up.
fn drive(root: &Node, child: &Node) -> JournalEvidence {
    let dir = scratch(&format!(
        "journal-e2e-{}-{}",
        root.agent_type, child.agent_type
    ));
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(root, child),
    })
    .expect("the canned provider binds");

    // §3.4: what `--timeout` bounds follows the surface, so the value does too — derived from the
    // adapter exactly as `marion run` derives the path itself, never from the harness's name.
    let adapter = adapter_for(root.harness).expect("the root's harness has an adapter");
    let timeout = match root_path(&adapter.surfaces()) {
        Some(RootPath::Duplex) => ROOT_BLOCKED_SECS,
        _ => ROOT_WALL_CLOCK_SECS,
    };
    let args = marion_argv(root, &repo, &state, &server.base_url(), timeout);

    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args(&args)
            .current_dir(&*dir),
        RUN_BOUND,
    )
    .expect("marion run starts");

    // §2's key: the git common dir, the same call `root::prepare` makes.
    let project = ProjectDir::new(&state, &marion_supervisor::socket::project_root(&repo));
    let journal_path = project.journal();
    let replay = journal_path
        .is_file()
        .then(|| read_path(&journal_path).expect("the journal replays"));
    let writers = if journal_path.is_file() {
        writers_by_agent(&journal_path)
    } else {
        Vec::new()
    };
    let kinds = record_kinds(&journal_path);
    let walked = persisted_contracts(&state)
        .map_err(|e| format!("{} cannot be walked for contracts: {e}", state.display()));
    let agent_dirs = agent_dirs(&project);

    drop(server);

    // Judged after the provider is down: a contract marion wrote and cannot read back is a defect
    // whichever half is wrong, so it fails naming the file — but it must not fail while a server
    // and a child's processes are still up.
    let walked = walked.unwrap_or_else(|e| panic!("{e}"));
    let contracts: Vec<(PathBuf, TaskContract)> = judge(&walked)
        .into_iter()
        .map(|(p, v)| {
            let parsed = serde_json::from_value(v.clone())
                .unwrap_or_else(|e| panic!("parsing {}: {e}", p.display()));
            (p.to_path_buf(), parsed)
        })
        .collect();

    JournalEvidence {
        timed_out: out.timed_out,
        code: out.code,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        replay,
        agent_dirs,
        writers,
        kinds,
        contracts,
        journal_path,
    }
    // `dir` drops here: the scratch guard removes it whether these assertions pass or panic.
}

/// **Every claim this file makes about one root/child pairing.**
///
/// Identical in substance to the claude→codex test below, which stays written out longhand as the
/// worked example; this is that same argument applied to a pairing named by the table. The one that
/// cannot be made by any single-process test is the writer-identity assertion at the end.
fn assert_pairing(root: &Node, child: &Node) {
    for n in [root, child] {
        assert!(
            on_path(n.program),
            "this pairing drives a REAL {}; put it on PATH — §9's rule is that a criterion which \
             quietly passes on a machine that cannot run it is worth less than no criterion",
            n.program
        );
    }
    let label = format!("{} root → {} child", root.agent_type, child.agent_type);
    let ev = drive(root, child);

    assert!(
        !ev.timed_out,
        "{label}: marion run hung\nstderr:\n{}",
        ev.stderr
    );
    assert_eq!(
        ev.code,
        Some(0),
        "{label}: marion run exited {:?}\nstderr:\n{}",
        ev.code,
        ev.stderr
    );
    let replay = ev.replay.as_ref().unwrap_or_else(|| {
        panic!(
            "{label}: a run that created two nodes wrote no journal at {}\nstderr:\n{}",
            ev.journal_path.display(),
            ev.stderr
        )
    });
    assert_eq!(
        replay.truncation, None,
        "{label}: a clean run leaves an intact file"
    );
    assert!(
        replay.gaps.is_empty(),
        "{label}: a per-writer ordinal gap means a record was written and lost: {:?}",
        replay.gaps
    );

    // ---- same nodes, against what the run left on disk. --------------------------------------
    let replayed: BTreeSet<String> = replay
        .nodes()
        .iter()
        .map(|n| n.agent_id.0.clone())
        .collect();
    assert_eq!(
        replayed, ev.agent_dirs,
        "{label}: every node marion created must appear in the journal, and no node it did not"
    );
    assert_eq!(
        replay.nodes().len(),
        2,
        "{label}: one root, one child: {replayed:?}"
    );

    // ---- same parent edges, and each node's own harness. --------------------------------------
    let roots = replay.roots();
    assert_eq!(roots.len(), 1, "{label}: one root: {roots:?}");
    let root_id = roots[0].agent_id.clone();
    let root_node = replay.get(&root_id).unwrap();
    assert_eq!(
        root_node.parent_id(),
        None,
        "{label}: §9: a root has no parent"
    );
    assert_eq!(
        root_node.depth(),
        Some(0),
        "{label}: §3.1: the root is depth 0"
    );
    assert_eq!(
        root_node.task_id(),
        None,
        "{label}: §9: a root has no TaskContract — an absence, not a placeholder"
    );
    assert_eq!(
        root_node.harness(),
        Some(root.harness),
        "{label}: the journal must record the harness the root actually ran"
    );
    assert!(
        root_node.spawn_confirmed,
        "{label}: the root's process really ran"
    );

    let children = replay.children(&root_id);
    assert_eq!(
        children.len(),
        1,
        "{label}: the root spawned exactly one child: {children:?}"
    );
    let child_node = children[0];
    assert_eq!(child_node.parent_id(), Some(&root_id), "{label}");
    assert_eq!(
        child_node.depth(),
        Some(1),
        "{label}: one level below its caller"
    );
    assert_eq!(
        child_node.harness(),
        Some(child.harness),
        "{label}: the child's own harness, not its caller's"
    );
    assert_eq!(child_node.agent_type(), Some(child.agent_type), "{label}");
    assert!(child_node.spawn_confirmed, "{label}");

    // ---- same terminal states. ----------------------------------------------------------------
    assert_eq!(
        child_node.state,
        NodeState::Exited(ExitStatus::Ok),
        "{label}: the child reported through marion's tool and exited clean"
    );
    assert!(
        child_node
            .exit
            .as_ref()
            .is_some_and(|e| e.description.contains("child exited")),
        "{label}: the terminal record carries §6.7's ProcessExit, so replay needs no contract \
         file: {:?}",
        child_node.exit
    );
    assert!(
        root_node.state.is_exited(),
        "{label}: the root's terminal transition must be recorded too, got {:?}",
        root_node.state
    );
    assert!(
        replay.unresolved().is_empty(),
        "{label}: a node recorded live with no exit is what §7.2's Orphaned marking is about: {:?}",
        replay
            .unresolved()
            .iter()
            .map(|n| &n.agent_id.0)
            .collect::<Vec<_>>()
    );

    // ---- same contracts, named against the files the run actually wrote. ----------------------
    assert!(
        root_node.contracts.is_empty(),
        "{label}: §9: a root has no contract to record"
    );
    assert_eq!(
        child_node.contracts.len(),
        1,
        "{label}: one run, one contract"
    );
    let recorded = &child_node.contracts[0];
    assert_eq!(
        recorded.requester, root_id,
        "{label}: §9: requester for a top-level spawn is the root's own AgentId"
    );
    assert_eq!(recorded.status, Some(ExitStatus::Ok), "{label}");
    assert_eq!(
        ev.contracts.len(),
        1,
        "{label}: exactly one contract on disk: {:?}",
        ev.contracts.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
    let (path, persisted) = &ev.contracts[0];
    assert_eq!(
        persisted.task_id, recorded.task_id,
        "{label}: the journal's contract record must name the task id the file itself carries"
    );
    assert_eq!(
        persisted.requester,
        AgentId(root_id.0.clone()),
        "{label}: and the same requester, in the file at {}",
        path.display()
    );
    assert_eq!(
        persisted.completion.as_ref().map(|c| c.status),
        recorded.status,
        "{label}: how the contract ended, as the journal records it, is how the contract says it \
         ended"
    );

    // ---- one process wrote both nodes' records, and it is not the harness's bridge. -----------
    // **The inverted claim (§11 item 28 steps 5-6), and why it is worth more than the one it
    // replaced.** This used to assert the root's and the child's writers were *disjoint*: the root's
    // records came from `marion run` and the child's from the bridge the root's harness started, so
    // disjointness was the evidence that a run had crossed a process boundary. The boundary is gone
    // — the supervisor drives the root and runs every child — so the same records now carry one
    // writer, and the assertion says so.
    //
    // It is still evidence about the ROOT's adapter, for the same reason: the bridge only reaches
    // the supervisor at all if that adapter's MCP declaration carried `MARION_REPO`,
    // `MARION_AGENT_ID` and §5.4's capability token, and a bridge that could not dial refuses
    // instead of spawning. And it is evidence about something the old form could not see: a second
    // writer here means a bridge journalled a child *itself*, which is the in-process fallback
    // `courier.rs` exists to refuse.
    let root_writers: BTreeSet<&String> = ev
        .writers
        .iter()
        .filter(|(a, _)| *a == root_id.0)
        .map(|(_, w)| w)
        .collect();
    let child_writers: BTreeSet<&String> = ev
        .writers
        .iter()
        .filter(|(a, _)| *a == child_node.agent_id.0)
        .map(|(_, w)| w)
        .collect();
    assert_eq!(
        root_writers.len(),
        1,
        "{label}: one process journals the root: {root_writers:?}"
    );
    assert_eq!(
        child_writers.len(),
        1,
        "{label}: one process journals the child: {child_writers:?}"
    );
    assert_eq!(
        root_writers, child_writers,
        "{label}: the root and its child must be journalled by the same process — the supervisor \
         owns both since §11 item 28 steps 5 and 6. A second writer means something else ran a \
         node: a bridge that spawned in-process rather than dialling, or a client that drove the \
         root itself. root {root_writers:?}, child {child_writers:?}"
    );

    // ---- and the record vocabulary is the one `marion-core` already defines. ------------------
    for expected in ["SpawnIntent", "Spawned", "Exited", "ContractPersisted"] {
        assert!(
            ev.kinds.contains(&expected),
            "{label}: no {expected} record in a run that spawned a child: {:?}",
            ev.kinds
        );
    }
    assert!(
        !ev.kinds.contains(&"SpawnAborted"),
        "{label}: nothing was abandoned in a run both of whose nodes exited: {:?}",
        ev.kinds
    );
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

    let root_dir = scratch("journal-e2e-hop");
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
    // §2's key: the git common dir, the same call `root::prepare` makes.
    let project = ProjectDir::new(&state, &marion_supervisor::socket::project_root(&repo));
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

    let walked = persisted_contracts(&state).expect("the state tree enumerates");
    let files: Vec<&Path> = judge(&walked).into_iter().map(|(p, _)| p).collect();
    assert_eq!(files.len(), 1, "exactly one contract on disk: {files:?}");
    assert_eq!(
        files[0],
        project.agent(&child.agent_id).contract(&recorded.task_id),
        "the journal's contract record must name the path the run actually wrote"
    );
    let persisted: TaskContract =
        serde_json::from_slice(&std::fs::read(files[0]).unwrap()).unwrap();
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

    // ---- one process wrote both nodes' records. ------------------------------------------------
    // The inverted form of §10's old cross-process claim — see the module doc and the per-pairing
    // assertion above. Since §11 item 28 steps 5 and 6 the supervisor owns the root *and* every
    // child, so a second writer identity here means a bridge journalled a node itself.
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
    assert_eq!(child_writers.len(), 1, "one process journals the child");
    assert_eq!(
        root_writers, child_writers,
        "the root and its child must be journalled by the same process — the supervisor owns both \
         since §11 item 28 steps 5 and 6: root {root_writers:?}, child {child_writers:?}"
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
            RecordKind::KillIntent(_) => "KillIntent",
            RecordKind::KillConfirmed(_) => "KillConfirmed",
            RecordKind::ContractPersisted(_) => "ContractPersisted",
            RecordKind::PermissionDenied(_) => "PermissionDenied",
            RecordKind::RootChanged(_) => "RootChanged",
            RecordKind::RootGrantDecided(_) => "RootGrantDecided",
            RecordKind::SessionObserved(_) => "SessionObserved",
            RecordKind::SupervisorExited(_) => "SupervisorExited",
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
    let root_dir = scratch("journal-e2e-child-denial");
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
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state: state.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    let contract = run_spawn(
        &env,
        &SpawnRequest {
            agent_type: "claude".into(),
            prompt: "Try the verb you were not given.".into(),
            repo: repo.clone(),
            acceptance_criteria: vec![],
            writable_scope: vec!["src/**".into()],
            timeout_secs: 60,
            model: None,
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
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

// --- the pairings ---------------------------------------------------------------------------------
//
// One `#[test]` per pairing, never a loop: a loop reports the first failure and hides the rest, and
// the whole point of a matrix is which cells fail.
//
// **Fifteen, not sixteen** — `claude → codex` is the longhand test above, which stays written out
// as the worked example this function is the generalisation of. Running it twice would buy nothing.

#[test]
fn b_claude_root_journals_a_claude_child_into_one_supervisors_journal() {
    assert_pairing(&CLAUDE, &CLAUDE);
}

#[test]
fn c_claude_root_journals_a_gemini_child_into_one_supervisors_journal() {
    assert_pairing(&CLAUDE, &GEMINI);
}

#[test]
fn d_claude_root_journals_an_opencode_child_into_one_supervisors_journal() {
    assert_pairing(&CLAUDE, &OPENCODE);
}

#[test]
fn e_codex_root_journals_a_claude_child_into_one_supervisors_journal() {
    assert_pairing(&CODEX, &CLAUDE);
}

#[test]
fn f_codex_root_journals_a_codex_child_into_one_supervisors_journal() {
    assert_pairing(&CODEX, &CODEX);
}

#[test]
fn g_codex_root_journals_a_gemini_child_into_one_supervisors_journal() {
    assert_pairing(&CODEX, &GEMINI);
}

#[test]
fn h_codex_root_journals_an_opencode_child_into_one_supervisors_journal() {
    assert_pairing(&CODEX, &OPENCODE);
}

#[test]
fn i_gemini_root_journals_a_claude_child_into_one_supervisors_journal() {
    assert_pairing(&GEMINI, &CLAUDE);
}

#[test]
fn j_gemini_root_journals_a_codex_child_into_one_supervisors_journal() {
    assert_pairing(&GEMINI, &CODEX);
}

#[test]
fn k_gemini_root_journals_a_gemini_child_into_one_supervisors_journal() {
    assert_pairing(&GEMINI, &GEMINI);
}

#[test]
fn l_gemini_root_journals_an_opencode_child_into_one_supervisors_journal() {
    assert_pairing(&GEMINI, &OPENCODE);
}

#[test]
fn m_opencode_root_journals_a_claude_child_into_one_supervisors_journal() {
    assert_pairing(&OPENCODE, &CLAUDE);
}

#[test]
fn n_opencode_root_journals_a_codex_child_into_one_supervisors_journal() {
    assert_pairing(&OPENCODE, &CODEX);
}

#[test]
fn o_opencode_root_journals_a_gemini_child_into_one_supervisors_journal() {
    assert_pairing(&OPENCODE, &GEMINI);
}

#[test]
fn p_opencode_root_journals_an_opencode_child_into_one_supervisors_journal() {
    assert_pairing(&OPENCODE, &OPENCODE);
}

/// **A real run journals the harness's own session id for every node it creates**
/// (`plan-restart-resume.md` step 4) — the handle a `node/resume` hands back to the harness, read
/// from each node's stream through its row's `StreamGrammar::session` and written as
/// `SessionObserved` on first sighting. A codex root with an opencode child: two `LaunchOnly`
/// harnesses, so the id has to be read off the live pipe rather than recovered from a capture after
/// exit — a node killed mid-run is exactly the one a resume is for, and it never gets to a capture.
/// No claude cell: the installed binary is ahead of the fixture pin on this machine.
#[test]
fn a_real_run_journals_the_harness_session_id_for_every_node() {
    let ev = drive(&CODEX, &OPENCODE);
    let replay = ev.replay.as_ref().unwrap_or_else(|| {
        panic!(
            "the run wrote no journal at {} (stderr: {})",
            ev.journal_path.display(),
            ev.stderr
        )
    });
    assert_eq!(
        replay.nodes().len(),
        2,
        "one root and one child: {:?} (stderr: {})",
        replay.nodes(),
        ev.stderr
    );
    for node in replay.nodes() {
        let session = node.harness_session.as_deref().unwrap_or_else(|| {
            panic!(
                "{} ({}) replays with no harness session; kinds were {:?}",
                node.agent_id.0,
                node.harness().map(|h| h.to_string()).unwrap_or_default(),
                ev.kinds
            )
        });
        assert!(
            !session.trim().is_empty(),
            "{}: an empty id",
            node.agent_id.0
        );
        // The id is the harness's own, in its own spelling: opencode's `ses_` + 26 chars, codex's
        // UUID. Anything else would be marion inventing a session.
        match node.harness() {
            Some(Harness::OpenCode) => assert!(
                session.starts_with("ses_"),
                "{}: {session:?} is not an opencode session id",
                node.agent_id.0
            ),
            Some(Harness::Codex) => assert_eq!(
                session.len(),
                36,
                "{}: {session:?} is not a codex thread id",
                node.agent_id.0
            ),
            other => panic!("{}: unexpected harness {other:?}", node.agent_id.0),
        }
    }
    assert_eq!(
        ev.kinds.iter().filter(|k| **k == "SessionObserved").count(),
        2,
        "one record per node, on first sighting and never again: {:?}",
        ev.kinds
    );
}
