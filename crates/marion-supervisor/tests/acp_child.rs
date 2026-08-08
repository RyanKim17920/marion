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
