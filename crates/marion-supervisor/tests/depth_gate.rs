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
//! it live, so the witness has to cover all four rather than the one this repository happened to
//! have evidence for.
//!
//! # One `#[test]` per harness, and never a loop
//!
//! The depth plumbing is **per-harness**: the caller's depth and agent type ride the MCP `env` block
//! that each adapter writes in its own file format, and each harness spells marion's `spawn` its own
//! way. That is exactly the shape that breaks for one harness while three keep passing, so each is
//! its own `#[test]`, named for its harness — a loop would report the first failure and hide the
//! other three. `cross_product.rs` takes the same line for the same reason, and the [`Node`] table
//! below is its table.
//!
//! # The two refusals, and why only one of them is the depth gate
//!
//! Three of the four harnesses reach the gate and are refused by it, in marion's own words, on the
//! wire. **claude-code does not, and that is a measured fact about the harness rather than a hole in
//! the gate.** `run_spawn` compiles every child with `allowed_tools = [report]`; Claude Code is the
//! one harness that reads that list, so its `spawn` never leaves the CLI as an MCP call at all — it
//! surfaces as a `can_use_tool` control request, which `duplex` denies (there is no permission
//! answerer, §11 item 22) before marion's supervisor is ever asked for a child.
//!
//! Both refusals are asserted, each in its own terms, and **both tests assert the same outcome**:
//! no second agent-dir, no second contract, no second worktree branch. That outcome is the property
//! §3.1 is about. Which of marion's two defences produced it is recorded per harness so that a
//! harness silently changing sides — a claude-code child that started ignoring `allowed_tools`, say
//! — fails here instead of passing quietly on the other assertion.
//!
//! # The shape of the run
//!
//! `run_spawn` is called with a caller at depth `max_depth - 1`, so the child it launches sits at
//! **exactly `max_depth`** — the deepest legal node. That child is scripted to call `spawn`. marion
//! must refuse it, and must do so before any of the four things a spawn creates exists.
//!
//! The grandchild's half of the script is a complete, working child script for that same harness. If
//! the gate ever fails to fire, the grandchild **really runs** — patch, report, finish — so this
//! file's failure mode is the hazard itself rather than a stall.
//!
//! Every node is driven by the in-process [`CannedServer`], so every test here is free and
//! repeatable, and each asserts that no verbatim credential reached it.
//!
//! ```sh
//! cargo test -p marion-supervisor --test depth_gate
//! ```
//!
//! It needs real `claude` (2.1.220), `codex` (0.146.0), `gemini` (0.53.0) and `opencode` (1.17.3)
//! on `PATH`, and like every other end-to-end file here it is **not** `#[ignore]`d and does **not**
//! skip when a binary is missing: §9's standing rule is that a criterion that quietly passes on a
//! machine that cannot run it is worth less than no criterion.

use std::path::{Path, PathBuf};
use std::process::Command;

use marion_core::agent_type::{DEFAULT_MAX_DEPTH, builtin};
use marion_core::contract::TaskId;
use marion_core::harness::Harness;
use marion_core::paths::ProjectDir;
use marion_harness::adapter_for;
use marion_provider::{CannedServer, Config, RootScript, RootTurn, Script};
use marion_supervisor::journal::read_path;
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, git, kill_hard, on_path};
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

/// The narrative the grandchild's script reports, on whichever wire it would have run on. Present
/// only so the grandchild is a *complete* child script; a run that reaches it has already failed.
const GRANDCHILD_NARRATIVE: &str = "Wrote the depth-gate marker under src/ and reported back.";

/// A scratch dir that removes itself.
///
/// `Drop`, and not a `remove_dir_all` at the end of each test: a failing assertion unwinds
/// straight past any trailing cleanup, so an explicit call leaks on exactly the runs that fail
/// — the ones a developer re-runs most. `Drop` catches those, plus every `?` and early return.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // Ignored: the dir may already be gone (a `git worktree remove` that took it, say), and a
        // cleanup failure must not mask the test's own verdict.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// `Deref` alone is not enough: it coerces `&Scratch` to `&Path` at a call site expecting one, but
/// a generic `P: AsRef<Path>` — `std::fs::remove_dir_all`, `Command::current_dir` — never triggers
/// that coercion and fails to compile instead.
impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Bind the returned guard for the whole test — `scratch("x").join("y")` drops the dir at the end
/// of that statement, deleting it out from under the run. Bind it as `dir`, never as a bare `_`,
/// which drops on the spot.
fn scratch(name: &str) -> Scratch {
    let p = std::env::temp_dir().join(format!("marion-depth-{name}-{}", std::process::id()));
    // Removed on the way *in* as well: a run killed hard enough to skip `Drop` leaves a dir behind,
    // and pids recycle, so a later run can inherit that exact name.
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    Scratch(p.canonicalize().expect("scratch dir canonicalises"))
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

/// Processes still alive with `needle` on their command line, as `(pid, line)`.
///
/// `ps` is the *only* witness this file has for a leak, so every way it can fail to answer is a
/// failure of the test rather than an empty answer. Reporting "no survivors" because `ps` was
/// missing, errored, or printed nothing would make the leak assertion pass for free on exactly the
/// machines where it cannot be checked — the silent pass this file exists to rule out.
fn survivors(needle: &str) -> Vec<(i32, String)> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
        .expect("`ps` must run: without it nothing here can tell a clean run from a leak");
    assert!(
        out.status.success(),
        "`ps -axo pid=,command=` exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let listing = String::from_utf8_lossy(&out.stdout);
    // `ps -ax` lists at minimum this very test process, so an empty listing means a witness that
    // did not work, not a machine with nothing running on it.
    assert!(
        !listing.trim().is_empty(),
        "`ps` printed nothing; the leak check would report no survivors whatever had leaked"
    );
    listing
        .lines()
        .filter(|l| l.contains(needle))
        // A matching line whose pid will not parse is a survivor this test cannot name. Dropping it
        // would be the same silent pass one line down, so say so instead.
        .map(|l| {
            let pid = l
                .split_whitespace()
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| {
                    panic!("`ps` line matches {needle:?} but carries no pid: {l:?}")
                });
            (pid, l.to_string())
        })
        .collect()
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

// --- the four harnesses, as data ---------------------------------------------------------------

/// Which of marion's two defences stops this harness's `spawn`. Not a knob: a **measured** property
/// of the harness, asserted so that a harness changing sides fails here rather than quietly passing
/// on whichever assertion still holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusedBy {
    /// The call reached marion's supervisor and §6.1 step 2's depth gate answered it. The refusal
    /// comes back as the `spawn` call's own tool result, so marion's sentence is on the wire.
    DepthGate,
    /// The call never left the CLI as an MCP call. Claude Code is the one harness that reads
    /// `allowed_tools`, which `run_spawn` sets to `[report]`, so `spawn` surfaces as a
    /// `can_use_tool` control request instead — and `duplex` denies it, there being no permission
    /// answerer in M1 (§11 item 22). marion's supervisor is never asked for a child at all.
    PermissionDenial,
}

/// One harness, as the node at `max_depth` whose `spawn` must be refused.
#[derive(Debug, Clone, Copy)]
struct Node {
    /// The built-in agent type. `codex-impl` is `codex`'s canonical name.
    agent_type: &'static str,
    harness: Harness,
    /// The model both the delegator and the grandchild ask for. `None` where the harness takes
    /// none; the two that refuse to compile without one state it.
    model: Option<&'static str>,
    /// The wire this harness speaks to the canned provider on.
    wire: &'static str,
    /// The binary that must be on `PATH`.
    program: &'static str,
    /// See [`RefusedBy`].
    refused_by: RefusedBy,
}

const CLAUDE: Node = Node {
    agent_type: "claude",
    // Claude Code's `--model` is legitimately omissible and the canned provider ignores it, so no
    // vendor id is pinned here that marion has no basis for.
    model: None,
    harness: Harness::ClaudeCode,
    wire: "anthropic",
    program: "claude",
    refused_by: RefusedBy::PermissionDenial,
};

const CODEX: Node = Node {
    agent_type: "codex-impl",
    harness: Harness::Codex,
    model: None,
    wire: "responses",
    program: "codex",
    refused_by: RefusedBy::DepthGate,
};

const GEMINI: Node = Node {
    agent_type: "gemini",
    harness: Harness::Gemini,
    // Explicit: the adapter REFUSES to compile without `-m` (S12's `auto` router hang).
    model: Some("gemini-2.5-flash"),
    wire: "gemini",
    program: "gemini",
    refused_by: RefusedBy::DepthGate,
};

const OPENCODE: Node = Node {
    agent_type: "opencode",
    harness: Harness::OpenCode,
    // `provider/model`, the only spelling `-m` accepts; the generated provider block repeats it.
    model: Some("marion/canned-1"),
    wire: "openai",
    program: "opencode",
    refused_by: RefusedBy::DepthGate,
};

/// marion's `spawn` and `report`, in the spelling **this harness's wire** dispatches on.
///
/// Three of the four are the adapter's own `marion_tool_name`, which is the whole point of §3.1
/// making that mapping part of the adapter contract — the four harnesses disagree
/// (`mcp__marion__spawn`, `mcp_marion_spawn`, `marion_spawn`) and nothing translates between them.
///
/// **Codex is the exception, and it is a wire fact rather than an inconsistency.** Its
/// `marion_tool_name` is the flat `mcp__marion__spawn` because that is the *code-mode JavaScript
/// identifier* a model writes inside `tools.…`; a `function_call` item carrying that flat name is
/// rejected by 0.146.0 as `unsupported call` (§11 item 12), and the wire dispatch form is the bare
/// verb beside `namespace: "mcp__marion"`, which `marion_provider::responses::mcp_call` supplies.
fn marion_tool(node: &Node, verb: &str) -> String {
    let adapter = adapter_for(node.harness).expect("every harness in this file has an adapter");
    match node.harness {
        Harness::Codex => verb.to_string(),
        _ => adapter.marion_tool_name(verb),
    }
}

/// The `Script` that answers **both** nodes: the delegator at `max_depth`, and the grandchild it is
/// refused — which shares its harness, and therefore its wire.
///
/// The delegator's half is a [`RootScript`] keyed on [`DELEGATOR_MARKER`]; the grandchild's half is
/// the per-wire child script `harness_matrix` already uses, retargeted at this harness's own
/// spelling of `report`. Both halves live on one wire and the marker is what separates them —
/// exactly the same-wire arrangement `cross_product`'s four same-harness cells prove is
/// unambiguous, because the provider dispatches on request *shape* and never on arrival order.
fn script(node: &Node) -> Script {
    // What the child asks marion for: another node of its own type. **No `model` key at all** where
    // the harness takes none — §3.1 makes an omitted `model` mean "the agent type's own default",
    // and a JSON `null` is not the same thing to every harness (gemini 0.53.0 validates a tool call
    // against the declared schema before dispatching it and refuses `"model": null` outright).
    let mut grandchild = json!({
        "agent_type": node.agent_type,
        "prompt": GRANDCHILD_PROMPT,
        "acceptance_criteria": ["a file exists under src/ containing the depth-gate marker"],
        "writable_scope": ["src/**"],
        "timeout_secs": CHILD_TIMEOUT_SECS,
    });
    if let Some(m) = node.model {
        grandchild["model"] = json!(m);
    }
    let mut s = Script {
        root: Some(RootScript {
            marker: DELEGATOR_MARKER.into(),
            turn: RootTurn {
                tool: marion_tool(node, "spawn"),
                args: grandchild,
                final_text: "marion answered the spawn call; nothing left to do.".into(),
            },
        }),
        ..Script::default()
    };
    // The grandchild's half. If the gate fails to fire, this is what runs it to completion.
    let report = marion_tool(node, "report");
    match node.harness {
        // The Anthropic wire's two-step script *is* a child script once its tool is re-aimed:
        // `classify_root` finishes the run as soon as the transcript carries that call's result.
        Harness::ClaudeCode => {
            s.root_tool = report;
            s.root_tool_input = json!({ "narrative": GRANDCHILD_NARRATIVE });
            s.root_final_text = "Reported back through marion. Done.".into();
        }
        // The Responses child keeps its three steps: patch, report, final message.
        Harness::Codex => s.child_narrative = GRANDCHILD_NARRATIVE.into(),
        Harness::Gemini => {
            s.gemini_report_tool = report;
            s.gemini_report_args = json!({ "narrative": GRANDCHILD_NARRATIVE });
        }
        Harness::OpenCode => {
            s.openai_report_tool = report;
            s.openai_report_args = json!({ "narrative": GRANDCHILD_NARRATIVE });
        }
    }
    s
}

// --- driving one harness -----------------------------------------------------------------------

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
    /// The tools the CLI asked marion's permission for and was denied, read back off the journal —
    /// the only place a Claude Code node's refused `spawn` is visible at all, since that call never
    /// becomes an MCP request and so never reaches the provider's log.
    denied_permissions: Vec<String>,
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

/// Drive one real child of `node`'s harness at `max_depth`, whose script calls `spawn`.
fn drive(node: &Node) -> Evidence {
    let dir = scratch(node.agent_type);
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(node),
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
        agent_type: node.agent_type.into(),
        prompt: format!("{DELEGATOR_MARKER}: delegate this task to a child of your own."),
        acceptance_criteria: vec!["the task is delegated".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: node.model.map(str::to_string),
    };
    // **The caller sits one level above `max_depth`,** so the child this launches is the deepest
    // legal node — and the grandchild it asks for is the first illegal one. Deriving it from the
    // constant rather than writing `2` keeps the test honest if the default ever moves.
    //
    // The caller's type is the node's **own** type, not a fixed one: §6.1 step 2 reads the caller's
    // `max_depth`, so a harness whose built-in ever states a different bound is gated by that bound
    // here rather than by another type's.
    let caller = Caller {
        agent_id: "delegator-parent".into(),
        agent_type: builtin(node.agent_type).expect("the child's own type resolves"),
        depth: DEFAULT_MAX_DEPTH - 1,
    };

    let task_id = TaskId(format!("depth-gate-{}", node.agent_type));
    let spawn_result = run_spawn(&env, &req, &task_id, &caller)
        .map(|_| ())
        .map_err(|e| e.to_string());

    // Read off the **journal**, which is where §7.1 puts a denial and the only place it is
    // recorded: the contract carries the run's outcome, not marion's answers to the CLI's
    // permission asks. `journal_wiring` reads it the same way.
    let denied_permissions: Vec<String> = read_path(&env.project_dir.journal())
        .map(|replay| {
            replay
                .nodes()
                .iter()
                .flat_map(|n| n.denied_permissions.iter().map(|d| d.tool.clone()))
                .collect()
        })
        .unwrap_or_default();

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
        kill_hard(*pid);
    }
    // The dir goes with the guard, here rather than at the end of the function, so the order the
    // rest of this block establishes still holds: processes are killed before their cwd is removed.
    drop(dir);

    Evidence {
        spawn_result,
        contracts,
        agent_dirs: dirs,
        branches,
        requests,
        leaked: leaked.into_iter().map(|(_, line)| line).collect(),
        denied_permissions,
    }
}

/// Everything one harness must show, in the order that makes a failure most diagnosable: that the
/// node under test ran at all, then which defence refused it, then that nothing of a grandchild
/// exists, then that nothing was paid for and nothing outlived the run.
fn assert_refused(node: &Node, ev: &Evidence) {
    let h = node.harness;

    // ---- the child itself ran. Without this the rest is vacuous: a child that never started
    // ---- never called `spawn`, and "no grandchild" would be true for the wrong reason.
    ev.spawn_result.as_ref().unwrap_or_else(|e| {
        panic!(
            "{h}: the child at max_depth must itself run — it is the caller under test: {e}\n\
             Request log:\n{}",
            ev.log_summary()
        )
    });
    assert!(
        ev.requests
            .iter()
            .any(|r| carries(&r["body"], DELEGATOR_MARKER)),
        "{h}: no request in the log carries the delegator's own prompt, so the child never took a \
         turn and cannot have called spawn. Request log:\n{}",
        ev.log_summary()
    );
    // Every request belongs to this one harness's wire, so a run that somehow drove a *different*
    // harness cannot satisfy the assertions below by accident.
    for r in &ev.requests {
        if let Some(wire) = r["wire"].as_str() {
            assert_eq!(
                wire,
                node.wire,
                "{h}: a request arrived on the wrong wire — this node speaks {:?}. \
                 Request log:\n{}",
                node.wire,
                ev.log_summary()
            );
        }
    }

    // ---- marion refused the call, and it is the documented defence that did it. -----------------
    match node.refused_by {
        // The refusal is delivered as the `spawn` call's own tool result, and every harness replays
        // its whole transcript on the next turn — so marion's sentence comes back to the provider
        // inside the child's own request body. That is the strongest available evidence that the
        // child really called `spawn` AND that what came back was this gate: it is marion's words,
        // quoted by the harness, on the wire.
        RefusedBy::DepthGate => {
            let refusals: Vec<&Value> = ev
                .requests
                .iter()
                .filter(|r| carries(&r["body"], "max_depth 3"))
                .collect();
            assert!(
                !refusals.is_empty(),
                "{h}: the child's transcript never carried marion's depth refusal. Either it did \
                 not call `spawn` at all, or the call was SERVED — which is the \
                 unbounded-recursion hazard this test exists to close. Request log:\n{}",
                ev.log_summary()
            );
            assert!(
                refusals
                    .iter()
                    .any(|r| carries(&r["body"], "past max_depth 3")),
                "{h}: the refusal must name the bound and the value that broke it, never merely \
                 fail. Request log:\n{}",
                ev.log_summary()
            );
        }
        // Claude Code never lets the call out as an MCP request, so there is nothing in the
        // provider's log to read; `duplex`'s own denial record is the whole witness. The tool name
        // is asserted because "some permission was denied" would also be true of a run that asked
        // about something else entirely and never tried to spawn.
        RefusedBy::PermissionDenial => {
            let spawn = marion_tool(node, "spawn");
            assert!(
                ev.denied_permissions.contains(&spawn),
                "{h}: this harness reads `allowed_tools`, which run_spawn sets to [report], so its \
                 `spawn` must surface as a denied `can_use_tool` for {spawn:?} — marion's outer \
                 defence, refusing the call before the supervisor is asked for a child. Denied: \
                 {:?}. If this list is empty because the call went out as an MCP request instead, \
                 the harness has stopped honouring `allowed_tools` and this node belongs on the \
                 DepthGate branch. Request log:\n{}",
                ev.denied_permissions,
                ev.log_summary()
            );
            assert!(
                !ev.requests
                    .iter()
                    .any(|r| carries(&r["body"], "past max_depth 3")),
                "{h}: marion's depth refusal reached the wire, so the call DID get past \
                 `allowed_tools` and this node belongs on the DepthGate branch — where the refusal \
                 is asserted rather than merely observed. Request log:\n{}",
                ev.log_summary()
            );
        }
    }

    // ---- and nothing of a grandchild exists. ----------------------------------------------------
    //
    // The property §3.1 is actually about, and identical under either defence. §6.1 step 2 puts the
    // gate before the worktree, before `compile()` and before any process, so "refused" has to mean
    // each of these is exactly one — the child's — and never two.
    assert_eq!(
        ev.agent_dirs.len(),
        1,
        "{h}: exactly one node ran, so exactly one agent-dir exists; a second is a grandchild \
         marion created: {:?}",
        ev.agent_dirs
    );
    assert_eq!(
        ev.contracts, 1,
        "{h}: a refused spawn writes no contract, so only the child's is persisted"
    );
    assert_eq!(
        ev.branches.len(),
        1,
        "{h}: one node, one `marion/<task>` worktree branch; a second means the gate ran after \
         `make_worktree` rather than before it: {:?}",
        ev.branches
    );

    // ---- nothing was paid for. ------------------------------------------------------------------
    assert!(
        !ev.requests.iter().any(|r| {
            ["x-api-key", "authorization", "x-goog-api-key"]
                .iter()
                .any(|k| r["headers"][k].is_string() && r["headers"][k] != "<redacted>")
        }),
        "{h}: a run that reached the canned server with a verbatim credential is not the \
         zero-cost, repeatable run this file is about"
    );

    // ---- and nothing outlived the run (the S7 class of failure). --------------------------------
    assert!(
        ev.leaked.is_empty(),
        "{h}: processes from this run are still alive:\n{}",
        ev.leaked.join("\n")
    );
}

/// One harness, start to finish. The binary is asserted first and by name: a missing one is a
/// failure that says which, never a skip.
fn refuses_a_grandchild(node: &Node) {
    assert!(
        on_path(node.program),
        "this test drives a REAL {} child; put it on PATH",
        node.program
    );
    let ev = drive(node);
    assert_refused(node, &ev);
}

// --- the four harnesses -------------------------------------------------------------------------
//
// One `#[test]` each, named for its own harness. Never a loop: a loop reports the first failure and
// hides the other three, and the per-harness plumbing this file exercises is exactly what fails one
// at a time.

#[test]
fn a_claude_code_child_at_max_depth_is_refused_a_grandchild_rather_than_running_one() {
    refuses_a_grandchild(&CLAUDE);
}

#[test]
fn a_codex_child_at_max_depth_is_refused_a_grandchild_rather_than_running_one() {
    refuses_a_grandchild(&CODEX);
}

#[test]
fn a_gemini_child_at_max_depth_is_refused_a_grandchild_rather_than_running_one() {
    refuses_a_grandchild(&GEMINI);
}

#[test]
fn an_opencode_child_at_max_depth_is_refused_a_grandchild_rather_than_running_one() {
    refuses_a_grandchild(&OPENCODE);
}
