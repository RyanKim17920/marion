//! **M5's clause 1: an ACP agent runs as a marion child.**
//!
//! §9's M5 asks for ACP agents that *run as children*, not merely for an adapter that can compile
//! one. Everything under `tests/fixtures/s20..s23` measured the protocol from a probe — S21 drove a
//! turn on a real login, S22 measured three agents' tool spellings, S23 proved a turn runs against
//! a canned provider at $0.00. None of them went through `run_spawn`, and a path reachable only
//! from a probe is the shape this repo has now found seven times: fully tested in isolation and
//! wired to nothing an operator can reach.
//!
//! This is the production path. `run_spawn` derives `LaunchPath::Acp` from the agent type's
//! surfaces, `run_acp_child` drives the session, and marion's bridge is declared over ACP's
//! `session/new` rather than in a configuration document — the one harness where those arrive by
//! different channels.
//!
//! Canned, per S23: `opencode acp` honours the same `$XDG_CONFIG_HOME` document the `run` surface
//! honours, so the child talks to `127.0.0.1` and the suite pays nothing. The other two measured
//! agents are `npx` shims — long-lived stdio children behind a network download that once made a
//! probe return zero frames — so they stay out of the default suite deliberately.

use std::path::{Path, PathBuf};

use marion_core::contract::Isolation;
use marion_core::contract::{TaskContract, TaskId};
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, EditTurn, Script};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, on_path, scratch};

const CHILD_TIMEOUT_SECS: u64 = 90;
const NARRATIVE: &str = "Edited the worktree over ACP and reported back through marion.";

/// The child creates one file, so `changed_paths` has something git can see that no report can
/// fabricate — §6.7 sources it from a diff of the child's own worktree.
///
/// Written through opencode's **own** `write` tool rather than a `*** Begin Patch` document: the
/// patch form is the Responses wire's, consumed only by `respond_responses`, and opencode speaks
/// chat completions. Scripting the wrong one is not a harmless mismatch — the first run of this
/// test returned `status: Ok` with a narrative claiming the file had been added and
/// `changed_paths: []` beside it, which is the escaped-write signature §11 item 24 exists to catch.
/// Marion was telling the truth there and the *model* was not; the assertion below is on
/// `changed_paths` for exactly that reason.
const CHILD_FILE: &str = "src/marion_acp.txt";
const CHILD_FILE_CONTENT: &str = "marion M5: written by the canned opencode ACP child\n";

struct Fixture {
    repo: PathBuf,
    env: Env,
    /// Held, not dropped: dropping the server closes the port the child talks to.
    _server: CannedServer,
}

fn fixture(root: &Path) -> Fixture {
    let repo = fixture_repo(root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root.join("provider-requests.jsonl"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            // S22 measured opencode's model-facing spelling as `marion_report` — `<server>_<tool>`
            // — where the claude shim uses `mcp__marion__report` and the codex shim
            // `mcp.marion.report`. Three agents behind one adapter, three answers, so this is named
            // rather than derived.
            openai_report_tool: "marion_report".into(),
            openai_report_args: serde_json::json!({ "narrative": NARRATIVE }),
            openai_edit: Some(EditTurn {
                tool: "write".into(),
                args: serde_json::json!({
                    "filePath": CHILD_FILE,
                    "content": CHILD_FILE_CONTENT,
                }),
            }),
            ..Script::default()
        },
    })
    .expect("the canned provider binds");

    let env = Env {
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state,
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    Fixture {
        repo,
        env,
        _server: server,
    }
}

fn spawn_acp_child(fx: &Fixture, task_id: &str) -> Result<TaskContract, String> {
    let req = SpawnRequest {
        agent_type: "acp-opencode".into(),
        prompt: "Create a file under src/ and report back through marion.".into(),
        repo: fx.repo.clone(),
        acceptance_criteria: vec!["a file under src/ was created".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
    };
    // A root caller at depth 0, exactly what `marion run` hands the bridge.
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    run_spawn(&fx.env, &req, &TaskId(task_id.into()), &caller).map_err(|e| e.to_string())
}

/// **The clause itself.** A real `opencode acp` agent, spawned as a child through `run_spawn`,
/// reporting through marion's bridge — the bridge it was told about over `session/new` rather than
/// in a document on disk.
///
/// The assertion is on `changed_paths` rather than on the narrative, deliberately: the narrative is
/// whatever the canned script says and a child could return it having done nothing, whereas
/// `changed_paths` is derived from a git diff of the worktree marion made. It is the one field of a
/// contract no report can influence, which is why M4's clause-4 defect — comparing two *sets* and
/// so being unable to see a swap between them — was caught by pinning against it.
#[test]
fn an_acp_agent_runs_as_a_child_and_reports_through_marions_bridge() {
    if !on_path("opencode") {
        eprintln!("skipped: `opencode` is not installed");
        return;
    }
    let root = scratch("acp-child");
    let fx = fixture(&root);

    let contract = spawn_acp_child(&fx, "acp-child-1").expect("the ACP child runs and reports");
    let comp = contract
        .completion
        .as_ref()
        .expect("a child that reported has a completion");

    assert_eq!(
        comp.changed_paths,
        vec![PathBuf::from(CHILD_FILE)],
        "the child's write must reach the contract by git's account, not the report's: {comp:?}"
    );
    assert!(
        comp.scope_enforced,
        "a worktree child affords the diff, so §6.7 records that the check ran"
    );
}

/// The negative control the clause needs: **the path is chosen by the agent type's surfaces**, not
/// by anything this test arranges.
///
/// Without it, the test above could pass for a node that had quietly gone down `LaunchOnly` — the
/// failure that put `the_builtin_agent_types_land_on_the_paths_their_harnesses_afford` in
/// `duplex.rs`, where all four built-ins were driven as `LaunchOnly` and claude-code's turn went
/// out toolless. Asserted here at the type rather than the process, so it fails in milliseconds and
/// names the reason rather than surfacing as an empty contract.
#[test]
fn the_acp_agent_type_resolves_to_the_acp_harness_and_names_its_agent() {
    let t = marion_core::agent_type::builtin("acp-opencode").expect("the built-in resolves");
    assert_eq!(t.harness, marion_core::harness::Harness::Acp);
    assert_eq!(
        t.acp_agent.as_deref(),
        Some("opencode"),
        "`acp` is a protocol, not a program: the type must name which agent it runs"
    );
}

/// **A second real ACP agent as a marion child: `copilot --acp`, through its refinement row.**
///
/// Ignored by default because it runs against the operator's own Copilot login and spends real
/// tokens; run it with `--ignored` when copilot is installed and logged in. S28 measured why the row
/// exists: copilot 1.0.83 ignores `session/new`'s `mcpServers`, so the bridge is declared on
/// `--additional-mcp-config` by `AcpAdapter::fields`, and the model's `marion-report` call is read
/// in the row's measured spelling. The assertions are the same two the canned test makes — the
/// narrative marion read out of the transcript, and git's account of the write — because a live
/// agent that opened a session and reported nothing would pass anything weaker.
#[test]
#[ignore = "spends the operator's Copilot tokens; run with --ignored once `copilot` is logged in"]
fn a_real_copilot_acp_child_reports_through_the_argv_declared_bridge() {
    if !on_path("copilot") {
        eprintln!("skipped: `copilot` is not installed");
        return;
    }
    let root = scratch("acp-copilot");
    let (repo, env) = inherited_fixture(&root);
    let req = SpawnRequest {
        // The row's id, not `copilot --acp`: the command form binds the generic path, which on this
        // version reaches no bridge (S28).
        agent_type: "acp:copilot".into(),
        prompt: format!(
            "Create the file `{CHILD_FILE}` containing exactly `{}` (you may write it with your own \
             file tool), then call the `report` tool of the `marion` MCP server exactly once with \
             the narrative \"{NARRATIVE}\". Do nothing else.",
            CHILD_FILE_CONTENT.trim_end()
        ),
        repo: repo.clone(),
        acceptance_criteria: vec!["a file under src/ was created".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: 180,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
    };
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let contract = run_spawn(&env, &req, &TaskId("acp-copilot-1".into()), &caller)
        .unwrap_or_else(|e| panic!("copilot runs as an ACP child: {e}"));
    let comp = contract
        .completion
        .as_ref()
        .expect("a child that reported has a completion");
    assert_eq!(
        comp.narrative.as_ref().map(|n| n.value.as_str()),
        Some(NARRATIVE),
        "the report must be read out of a transcript spelled `marion-report`: {comp:?}"
    );
    assert_eq!(
        comp.changed_paths,
        vec![PathBuf::from(CHILD_FILE)],
        "the child's write must reach the contract by git's account: {comp:?}"
    );
}

/// The fake agent's own constants, restated: the test asserts on what marion *recorded*, and the
/// record must match what the agent actually did rather than what the test wished it had.
const UNKNOWN_AGENT_FILE: &str = "src/marion_acp.txt";
const UNKNOWN_AGENT_NARRATIVE: &str =
    "Edited the worktree over ACP from an agent marion had never heard of.";

/// A fixture with no provider at all: the fake agent runs no model, so there is nothing canned to
/// point it at, and the launch is `Auth::Inherited` — the shape any operator's own ACP agent runs
/// under.
fn inherited_fixture(root: &Path) -> (PathBuf, Env) {
    let repo = fixture_repo(root);
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let env = Env {
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state,
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: None,
        auth: marion_harness::Auth::Inherited,
    };
    (repo, env)
}

/// **The baseline: an ACP agent marion has never named reaches marion's bridge with zero per-agent
/// code.** The agent is `tests/fixtures/acp/fake_acp_agent.py` — a stdio ACP server that is in no
/// row of `acp::AGENTS`, spells the mirrored tool call `marion/report` (a spelling no measured row
/// carries), and calls the bridge's `report` for real off the `session/new` declaration.
///
/// Selected by command rather than by name: `acp:<command>` is the agent-type shape for an agent
/// marion has no row for, and the whole argv after the prefix is the agent's own.
///
/// Two assertions, and both are on fields the agent cannot fabricate through the report: the
/// narrative is read out of the ACP transcript by marion's reader (so the generic reading, not the
/// table, is what found it), and `changed_paths` is git's account of the worktree.
#[test]
fn a_previously_unknown_acp_agent_reaches_marions_bridge_through_the_generic_path() {
    if !on_path("python3") {
        eprintln!("skipped: `python3` is not installed");
        return;
    }
    let fake = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/acp/fake_acp_agent.py"
    );
    let root = scratch("acp-unknown");
    let (repo, env) = inherited_fixture(&root);
    let req = SpawnRequest {
        agent_type: format!("acp:python3 {fake}"),
        prompt: "Create a file under src/ and report back through marion.".into(),
        repo: repo.clone(),
        acceptance_criteria: vec!["a file under src/ was created".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
    };
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let contract = run_spawn(&env, &req, &TaskId("acp-unknown-1".into()), &caller)
        .unwrap_or_else(|e| panic!("the unknown ACP agent runs through the generic path: {e}"));
    let comp = contract
        .completion
        .as_ref()
        .expect("a child that reported has a completion");
    assert_eq!(
        comp.narrative.as_ref().map(|n| n.value.as_str()),
        Some(UNKNOWN_AGENT_NARRATIVE),
        "the report must be read out of a transcript spelled `marion/report`, which no measured \
         row uses: {comp:?}"
    );
    assert_eq!(
        comp.changed_paths,
        vec![PathBuf::from(UNKNOWN_AGENT_FILE)],
        "the child's write must reach the contract by git's account: {comp:?}"
    );
}
