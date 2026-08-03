//! **A real child, at its type's `max_depth`, asks marion for a grandchild — and is refused.**
//!
//! This is the end-to-end witness for §3.1's depth bound. Everything else about that bound is a
//! pure function: `marion_core::agent_type::check_spawn_gates` has always been correct, and its own
//! unit tests have always passed. What was missing was a **caller** — nothing in the supervisor ever
//! invoked it, no node struct carried a depth, and `max_depth: 3` was a number no code read. So a
//! child could `spawn` a grandchild, and that grandchild another, without bound: real OS process
//! trees and real git worktrees, all the way down.
//!
//! # Why the hazard was live rather than theoretical
//!
//! The only thing that ever stopped a grandchild was **accidental**, and it covered exactly one of
//! the four harnesses. `run_spawn` compiles a child with `allowed_tools = [report]`, and only the
//! Claude Code adapter reads `allowed_tools` at all. The other three ignore it, and codex's
//! generated `config.toml` additionally sets `default_tools_approval_mode = "approve"` — the key
//! whose *absence* the codex adapter's own test calls out as silently cancelling every marion call.
//! A codex child that called `spawn` was therefore **served**, and its grandchild ran to completion.
//!
//! Before this session that path was unreachable by construction: only claude-code was ever a root
//! and only codex ever a child. Generality — any harness as a root, three of four as a child — made
//! it live.
//!
//! # Why codex is the harness under test
//!
//! Of the three that ignore `allowed_tools`, codex is the one whose ungated `spawn` is *measured*
//! to be served rather than merely un-refused: `default_tools_approval_mode = "approve"` is written
//! into every codex node's config by marion itself, and `marion_provider::responses::mcp_call`
//! already supplies the bare-`spawn` wire form 0.146.0 dispatches on (§11 item 12). gemini and
//! opencode are allow-by-default too and would do as well; codex is the one this repository has
//! end-to-end evidence for on both halves.
//!
//! # The shape of the run
//!
//! `run_spawn` is called with a caller at depth `max_depth - 1`, so the child it launches sits at
//! **exactly `max_depth`** — the deepest legal node. That child is scripted to call `spawn`. marion
//! must refuse it by depth, and must do so before any of the four things a spawn creates exists.
//!
//! The child is driven by the in-process [`CannedServer`], so the run is free and repeatable.
//!
//! ```sh
//! cargo test -p marion-supervisor --test depth_gate
//! ```
//!
//! It needs a real `codex` (0.146.0) on `PATH`, and like every other end-to-end file here it is
//! **not** `#[ignore]`d and does **not** skip when the binary is missing: §9's standing rule is
//! that a criterion that quietly passes on a machine that cannot run it is worth less than no
//! criterion.

use std::path::{Path, PathBuf};
use std::process::Command;

use marion_core::agent_type::{DEFAULT_MAX_DEPTH, builtin};
use marion_core::contract::TaskId;
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, RootScript, RootTurn, Script};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use serde_json::{Value, json};

/// The child's own bound. Short: a wedged cell must fail fast rather than wedge CI.
const CHILD_TIMEOUT_SECS: u64 = 60;

/// Present in the **child's** prompt and nowhere in the grandchild's — the provider's role
/// discriminator, exactly as `cross_product` uses it, and shape dispatch rather than arrival order.
///
/// It is what lets the node under test be scripted to *delegate*: `RootScript` is not "the script
/// for a root", it is "the script for the node whose prompt carries this marker", and here that
/// node is a child at `max_depth`.
const DELEGATOR_MARKER: &str = "MARION-DEPTH-GATE-DELEGATOR-9c31";

/// The grandchild's task. Free of [`DELEGATOR_MARKER`], so that if the gate ever fails to fire the
/// grandchild is scripted as an ordinary child — patch, report, finish — and **really runs**, which
/// is what makes this test's failure mode the hazard itself rather than a stall.
const GRANDCHILD_PROMPT: &str = "Add the depth-gate marker file under src/ and report back.";

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGKILL: i32 = 9;

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("marion-depth-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p.canonicalize().expect("scratch dir canonicalises")
}

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).into_owned()
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

/// Every `contracts/<task_id>.json` marion persisted under `state`.
///
/// Walked rather than probed at a guessed path: asserting "the grandchild's contract is not at
/// `<x>`" would pass if it had simply been written somewhere else, and the whole claim here is
/// that it was never written at all.
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

/// Every agent-dir marion minted under `state`. One node, one dir (§4.3).
fn agent_dirs(state: &Path) -> Vec<String> {
    std::fs::read_dir(state)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|project| std::fs::read_dir(project.path().join("agents")))
        .flatten()
        .flatten()
        .filter_map(|a| a.file_name().into_string().ok())
        .collect()
}

fn survivors(needle: &str) -> Vec<(i32, String)> {
    Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.contains(needle))
                .filter_map(|l| {
                    let pid = l.split_whitespace().next()?.parse().ok()?;
                    Some((pid, l.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Does any string anywhere in `v` contain `needle`? The same whole-body scan the provider uses to
/// route a request, restated here so an assertion reads the log the way the server read it.
fn carries(v: &Value, needle: &str) -> bool {
    match v {
        Value::String(s) => s.contains(needle),
        Value::Array(a) => a.iter().any(|x| carries(x, needle)),
        Value::Object(o) => o.values().any(|x| carries(x, needle)),
        _ => false,
    }
}

/// What the run left behind, gathered **after** every process and directory is cleaned up, so no
/// assertion can turn a failing run into a leak. `harness_matrix` and `timeout_kill` take the same
/// line for the same reason.
struct Evidence {
    spawn_result: Result<(), String>,
    contracts: usize,
    agent_dirs: Vec<String>,
    /// `marion/*` branches left in the fixture repo — one per node that got as far as a worktree.
    branches: Vec<String>,
    requests: Vec<Value>,
    leaked: Vec<String>,
}

impl Evidence {
    fn log_summary(&self) -> String {
        if self.requests.is_empty() {
            return "  (the provider received NO requests at all)".into();
        }
        self.requests
            .iter()
            .map(|r| {
                format!(
                    "  seq {} {} {} → wire {:?}, delegator {}",
                    r["seq"],
                    r["method"],
                    r["path"],
                    r["wire"],
                    carries(&r["body"], DELEGATOR_MARKER)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Drive one real codex child at `max_depth` whose script calls `spawn`.
fn drive() -> Evidence {
    let dir = scratch("grandchild");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // What the child asks marion for. `codex-impl` again, and **no `model` key at all** — §3.1
    // makes an omitted `model` mean "the agent type's own default", and codex takes none.
    let grandchild = json!({
        "agent_type": "codex-impl",
        "prompt": GRANDCHILD_PROMPT,
        "acceptance_criteria": ["a file exists under src/ containing the depth-gate marker"],
        "writable_scope": ["src/**"],
        "timeout_secs": CHILD_TIMEOUT_SECS,
    });
    let script = Script {
        root: Some(RootScript {
            marker: DELEGATOR_MARKER.into(),
            turn: RootTurn {
                // The bare verb, which is what 0.146.0 dispatches an MCP call on: the flat
                // `mcp__marion__spawn` is the code-mode *identifier*, and a `function_call` item
                // carrying it is rejected as `unsupported call` (§11 item 12).
                tool: "spawn".into(),
                args: grandchild,
                final_text: "marion answered the spawn call; nothing left to do.".into(),
            },
        }),
        // The grandchild's half, left at the default three-step codex child script. If the gate
        // fails to fire, this is what runs it to completion.
        ..Script::default()
    };

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script,
    })
    .expect("the canned provider binds");

    let env = Env {
        repo: repo.clone(),
        project_dir: ProjectDir::new(&state, &repo),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    let req = SpawnRequest {
        agent_type: "codex-impl".into(),
        prompt: format!("{DELEGATOR_MARKER}: delegate this task to a child of your own."),
        acceptance_criteria: vec!["the task is delegated".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: None,
    };
    // **The caller sits one level above `max_depth`,** so the child this launches is the deepest
    // legal node — and the grandchild it asks for is the first illegal one. Deriving it from the
    // constant rather than writing `2` keeps the test honest if the default ever moves.
    let caller = Caller {
        agent_id: "delegator-parent".into(),
        agent_type: builtin("codex-impl").expect("the child's own type resolves"),
        depth: DEFAULT_MAX_DEPTH - 1,
    };

    let spawn_result = run_spawn(&env, &req, &TaskId("depth-gate".into()), &caller)
        .map(|_| ())
        .map_err(|e| e.to_string());

    let requests = server.requests().unwrap_or_default();
    let contracts = persisted_contracts(&state).len();
    let dirs = agent_dirs(&state);
    let branches: Vec<String> = git(&repo, &["branch", "--list", "marion/*"])
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    drop(server);
    let leaked = survivors(&dir.to_string_lossy());
    for (pid, _) in &leaked {
        let _ = unsafe { kill(*pid, SIGKILL) };
    }
    let _ = std::fs::remove_dir_all(&dir);

    Evidence {
        spawn_result,
        contracts,
        agent_dirs: dirs,
        branches,
        requests,
        leaked: leaked.into_iter().map(|(_, line)| line).collect(),
    }
}

#[test]
fn a_real_child_at_max_depth_is_refused_a_grandchild_rather_than_running_one() {
    assert!(
        on_path("codex"),
        "this test drives a REAL codex child; put `codex` (0.146.0) on PATH"
    );
    let ev = drive();

    // ---- the child itself ran. Without this the rest is vacuous: a child that never started
    // ---- never called `spawn`, and "no grandchild" would be true for the wrong reason.
    ev.spawn_result.as_ref().unwrap_or_else(|e| {
        panic!(
            "the child at max_depth must itself run — it is the caller under test: {e}\n\
             Request log:\n{}",
            ev.log_summary()
        )
    });
    assert!(
        ev.requests
            .iter()
            .any(|r| carries(&r["body"], DELEGATOR_MARKER)),
        "no request in the log carries the delegator's own prompt, so the child never took a \
         turn and cannot have called spawn. Request log:\n{}",
        ev.log_summary()
    );

    // ---- marion refused the call, **by depth, and said so.** ------------------------------------
    //
    // The refusal is delivered as the `spawn` call's own tool result, and codex replays its whole
    // transcript on the next turn — so marion's sentence comes back to the provider inside the
    // child's own request body. That is the strongest available evidence that the child really
    // called `spawn` AND that what came back was this gate: it is marion's words, quoted by the
    // harness, on the wire.
    let refusals: Vec<&Value> = ev
        .requests
        .iter()
        .filter(|r| carries(&r["body"], "max_depth 3"))
        .collect();
    assert!(
        !refusals.is_empty(),
        "the child's transcript never carried marion's depth refusal. Either it did not call \
         `spawn` at all, or the call was SERVED — which is the unbounded-recursion hazard this \
         test exists to close. Request log:\n{}",
        ev.log_summary()
    );
    assert!(
        refusals
            .iter()
            .any(|r| carries(&r["body"], "past max_depth 3")),
        "the refusal must name the bound and the value that broke it, never merely fail. \
         Request log:\n{}",
        ev.log_summary()
    );

    // ---- and nothing of a grandchild exists. ----------------------------------------------------
    //
    // §6.1 step 2 puts the gate before the worktree, before `compile()` and before any process, so
    // "refused" has to mean each of these is exactly one — the child's — and never two.
    assert_eq!(
        ev.agent_dirs.len(),
        1,
        "exactly one node ran, so exactly one agent-dir exists; a second is a grandchild marion \
         created: {:?}",
        ev.agent_dirs
    );
    assert_eq!(
        ev.contracts, 1,
        "a refused spawn writes no contract, so only the child's is persisted"
    );
    assert_eq!(
        ev.branches.len(),
        1,
        "one node, one `marion/<task>` worktree branch; a second means the gate ran after \
         `make_worktree` rather than before it: {:?}",
        ev.branches
    );

    // ---- and nothing outlived the run (the S7 class of failure). --------------------------------
    assert!(
        ev.leaked.is_empty(),
        "processes from this run are still alive:\n{}",
        ev.leaked.join("\n")
    );
}
