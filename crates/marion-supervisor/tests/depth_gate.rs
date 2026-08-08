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
//! # The shape of the run, and why it grew a supervisor
//!
//! The node under test sits at **exactly `max_depth`** — the deepest legal node — and is scripted
//! to call `spawn`. marion must refuse it, and must do so before any of the four things a spawn
//! creates exists.
//!
//! Getting a node to that depth used to be a struct literal: this file called `run_spawn` directly
//! with a `Caller { depth: max_depth - 1 }` it had made up. **§11 item 28 step 5 made that bed
//! meaningless, and the reason is the point of the change.** The gate no longer believes anything a
//! caller says about itself: the supervisor reads the caller's depth out of the `SpawnIntent` it
//! wrote, because a caller that can state its own depth can state `0` — and once `spawn` travels
//! over a socket any process of this user can reach, "can state" means "can forge". A fabricated
//! `Caller` is exactly that forgery, so a test built on one would have been asserting about a path
//! production no longer has.
//!
//! So the depth is real now. A detached supervisor is started, and the chain to `max_depth` is
//! built one honest `agent/spawn` at a time, the test presenting each node's own capability token
//! the way that node's bridge would (see [`spawn_over_socket`]). The ancestors are shims that block
//! on a gate — no model calls, no cost — and the node under test is the real binary, routed to by
//! the shim on [`DELEGATOR_MARKER`]. What the file measures is unchanged; what it measures it
//! *through* is now the production path.
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
use std::time::Duration;

use marion_core::agent_type::DEFAULT_MAX_DEPTH;
use marion_core::harness::Harness;
use marion_core::paths::ProjectDir;
use marion_harness::adapter_for;
use marion_proto::params::AgentSpawnParams;
use marion_proto::{Call, Method, MethodResult, SpawnCaller};
use marion_provider::{CannedServer, Config, RootScript, RootTurn, Script};
use marion_supervisor::journal::read_path;
use marion_supervisor::socket::project_root;
use marion_testsupport::{
    fixture_repo, git, kill_hard, on_path, persisted_contracts, scratch, survivors,
};
use serde_json::{Value, json};

mod common;

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

/// **The caller chain, and why the test has to build one.**
///
/// §6.1 step 2's gates read the caller's depth, and since §11 item 28 step 5 the supervisor reads
/// it out of the `SpawnIntent` **it wrote itself** — never out of the frame, because a caller that
/// can state its own depth can state `0`. That is the whole point of the change, and it is what
/// this file's old bed can no longer do: it called `run_spawn` directly with a `Caller` struct it
/// had made up, which is exactly the forgery the gate now refuses to accept from anybody.
///
/// So the depth has to be *real*. The chain is three nodes the supervisor genuinely started — a
/// root at 0 and two children — each one asked for by the test presenting the previous node's own
/// capability token, which is precisely what that node's bridge would present. The node under test
/// is then spawned by the node at `max_depth - 1`, lands at `max_depth`, and asks for one more.
///
/// Every chain node is a **shim**: a `codex` on the supervisor's `PATH` that blocks on a gate file,
/// so the chain stays live and costs no model call. The one invocation that must be real — the node
/// under test — is told apart by [`DELEGATOR_MARKER`] in its argv, and the shim `exec`s the real
/// binary for it. That keeps the production invocation byte-for-byte the adapter's own while
/// letting its ancestors be free.
const CHAIN_TYPE: &str = "codex-impl";

/// The chain's harness binary, which the shim stands in for.
const CHAIN_PROGRAM: &str = "codex";

/// The wall clock every chain node runs under. Never reached: the gate ends them.
const CHAIN_TIMEOUT_SECS: u64 = 600;

/// §5.7's idle grace for the fixture's supervisor. Never waited out — a live chain keeps it
/// resident and the fixture ends it explicitly — and generous so that a test which released its
/// chain early cannot race a supervisor that had decided to leave.
const IDLE_GRACE: Duration = Duration::from_secs(600);

/// A bound that exists only to fail. Nothing here waits on work the test has not already caused.
const BOUND: Duration = Duration::from_secs(180);

/// The shim: a `codex` that blocks until the gate exists, and `exec`s the real binary for the one
/// invocation that must be real.
fn chain_shim(dir: &Path, gate: &Path, real: &Path) -> PathBuf {
    let bin = dir.join(CHAIN_PROGRAM);
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  --version) echo "codex-cli 0.146.0-marion-depth-shim"; exit 0 ;;
esac
case "$*" in
  # The node under test. Its argv is the adapter's own, so this hands the real harness exactly what
  # marion compiled — the shim is a router, never a translator.
  *{marker}*) exec {real} "$@" ;;
esac
# A chain node: hold this depth open until the fixture releases it. Mortal by construction, because
# a shim that could only ever wait for a gate would outlive a panicking test.
waited=0
while [ ! -e {gate} ]; do
  sleep 0.05
  waited=$((waited + 1))
  if [ "$waited" -gt 4000 ]; then exit 0; fi
done
exit 0
"#,
        marker = DELEGATOR_MARKER,
        real = shell_quote(real),
        gate = shell_quote(gate),
    );
    std::fs::write(&bin, script).expect("the shim is written");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("the shim is executable");
    bin
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// Where `program` really lives, so the shim can hand its one real invocation on.
fn which(program: &str) -> PathBuf {
    let out = std::process::Command::new("which")
        .arg(program)
        .output()
        .expect("`which` runs");
    assert!(
        out.status.success(),
        "this test drives a REAL {program}; put it on PATH"
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// A node the supervisor owns, and the capability its own bridge would present.
struct Owned {
    agent_id: marion_core::contract::AgentId,
    token: String,
}

/// One `agent/spawn`, answered — as a root when `caller` is `None`, as that node's child otherwise.
///
/// The test is standing exactly where a bridge stands: it holds the caller's `agent_id` and the
/// token marion wrote into that node's declaration, and it states nothing else. It cannot state a
/// depth, an agent type or a child count, because [`marion_proto::SpawnCaller`] has nowhere to put
/// them — which is the property this whole file now rests on.
fn spawn_over_socket(
    sup: &common::Supervisor,
    state: &Path,
    caller: Option<&Owned>,
    p: AgentSpawnParams,
) -> Result<Owned, String> {
    let p = AgentSpawnParams {
        caller: caller.map(|c| SpawnCaller {
            agent_id: c.agent_id.clone(),
            node_token: c.token.clone(),
        }),
        ..p
    };
    let answered = sup.call(Call::AgentSpawn(p))?;
    let MethodResult::AgentSpawn(r) = Method::AgentSpawn
        .decode_result(&answered)
        .map_err(|e| e.to_string())?
    else {
        panic!("agent/spawn answers with an agent/spawn result");
    };
    let token = common::declaration_of(state, &r.agent_id)
        .remove("MARION_NODE_TOKEN")
        .expect("declaration_of asserts the token is there");
    Ok(Owned {
        agent_id: r.agent_id,
        token,
    })
}

/// The spawn a chain node is asked for: a shim of the chain's own type, holding its depth open.
fn chain_params(repo: Option<&Path>, depth: u32) -> AgentSpawnParams {
    AgentSpawnParams {
        agent_type: CHAIN_TYPE.into(),
        prompt: format!("depth-gate chain node at depth {depth}: hold until released"),
        caller: None,
        repo: repo.map(Path::to_path_buf),
        acceptance_criteria: vec![],
        writable_scope: vec![],
        timeout_secs: Some(CHAIN_TIMEOUT_SECS),
        model: None,
        // Root-only, and declined: nothing here asserts on §9's change record and taking it would
        // walk the fixture repo on every node.
        no_change_record: repo.map(|_| true),
        pane: None,
        // A root: `isolation` and `allow_concurrent_writes` are child-only and refused
        // beside `caller: None` (§6.6, §9).
        isolation: None,
        allow_concurrent_writes: None,
    }
}

/// Has this node reached a terminal state, according to the journal every process here writes?
fn is_terminal(journal: &Path, id: &marion_core::contract::AgentId) -> bool {
    read_path(journal)
        .map(|replay| {
            replay
                .nodes()
                .iter()
                .any(|n| &n.agent_id == id && n.state.is_exited())
        })
        .unwrap_or(false)
}

/// Drive one real child of `node`'s harness at `max_depth`, whose script calls `spawn`.
fn drive(node: &Node) -> Evidence {
    let dir = scratch(&format!("depth-{}", node.agent_type));
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    let shim_dir = dir.join("bin");
    let gate = dir.join("chain-gate");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&shim_dir).unwrap();
    chain_shim(&shim_dir, &gate, &which(CHAIN_PROGRAM));

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(node),
    })
    .expect("the canned provider binds");

    // **The supervisor's `PATH`, because the supervisor is what `exec`s a harness now.** The shim is
    // ahead of the real binaries; `execvp` skips a match that fails to exec and keeps searching, so
    // the shim is a working program that routes rather than a broken one that gets passed over.
    let path_env = format!(
        "{}:{}",
        shim_dir.to_string_lossy(),
        std::env::var("PATH").unwrap_or_default()
    );
    let key = project_root(&repo);
    let project = ProjectDir::new(&state, &key);
    let mut sup =
        common::Supervisor::start(&state, &key, &path_env, &server.base_url(), IDLE_GRACE);

    // ---- the chain: a real root and real children, each asking as the last one ------------------
    let mut caller = spawn_over_socket(&sup, &state, None, chain_params(Some(&repo), 0))
        .expect("the chain's root is created over the socket");
    for depth in 1..DEFAULT_MAX_DEPTH {
        caller = spawn_over_socket(&sup, &state, Some(&caller), chain_params(None, depth))
            .unwrap_or_else(|e| panic!("the chain node at depth {depth} must be served: {e}"));
    }

    // ---- the node under test: the deepest legal node, asked for by the node above it ------------
    let spawn_result = spawn_over_socket(
        &sup,
        &state,
        Some(&caller),
        AgentSpawnParams {
            agent_type: node.agent_type.into(),
            prompt: format!("{DELEGATOR_MARKER}: delegate this task to a child of your own."),
            acceptance_criteria: vec!["the task is delegated".into()],
            writable_scope: vec!["src/**".into()],
            timeout_secs: Some(CHILD_TIMEOUT_SECS),
            model: node.model.map(str::to_string),
            ..chain_params(None, DEFAULT_MAX_DEPTH)
        },
    );
    // Answered at `Spawned`, so the run is still going: wait for the node's own terminal record.
    // The journal is a seam every process here shares and the condition is a *fact about it* —
    // never an elapsed time, which is what §6.1 step 8 forbids substituting for an observation.
    //
    // **This used to wait for the contract as well, and no longer does.** `run_spawn` journalled
    // `Exited` *before* it wrote the contract, so a walk taken on the terminal record alone raced
    // one `write(2)` — measured here, on gemini, as a run with zero contracts where one was about
    // to exist. `run::persist_contract_then_record_exit` now writes the file first, so the terminal
    // record means the contract is on disk, and waiting for both would be waiting around a rule
    // that holds. Asserted instead of polled, deliberately: a test that polls past a race it could
    // assert is a test that will not notice the race coming back.
    if let Ok(under_test) = &spawn_result {
        let deadline = std::time::Instant::now() + BOUND;
        while !is_terminal(&project.journal(), &under_test.agent_id) {
            assert!(
                std::time::Instant::now() < deadline,
                "{}: the node at max_depth never reached a terminal record",
                node.harness
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            persisted_contracts(&state)
                .map(|c| !c.is_empty())
                .unwrap_or(false),
            "{}: the journal says the node at max_depth is over, so its contract is already on \
             disk — the terminal record is written after the file precisely so a reader can act \
             on it",
            node.harness
        );
    }
    let spawn_result = spawn_result.map(|_| ()).map_err(|e| e.to_string());

    // Read off the **journal**, which is where §7.1 puts a denial and the only place it is
    // recorded: the contract carries the run's outcome, not marion's answers to the CLI's
    // permission asks. `journal_wiring` reads it the same way.
    let denied_permissions: Vec<String> = read_path(&project.journal())
        .map(|replay| {
            replay
                .nodes()
                .iter()
                .flat_map(|n| n.denied_permissions.iter().map(|d| d.tool.clone()))
                .collect()
        })
        .unwrap_or_default();

    let requests = server.requests().unwrap_or_default();
    // Walked now, judged after the cleanup below: the walk is fallible, and a state tree that will
    // not enumerate must not be reported as a tree with no contracts in it — that is precisely how
    // "no grandchild contract was written" would pass for the wrong reason.
    //
    // Gathered **before** the chain is released, so the only contract that can be here is the one
    // the node under test wrote: a chain node writes its own the moment the gate opens.
    let walked = persisted_contracts(&state)
        .map_err(|e| format!("{} cannot be walked for contracts: {e}", state.display()));
    let dirs = agent_dirs(&state);
    let branches: Vec<String> = git(&repo, &["branch", "--list", "marion/*"])
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // ---- teardown, in the one order that leaves nothing behind ----------------------------------
    //
    // Release the chain first: the supervisor does not signal a node's process group when it dies,
    // so killing it first would strand every blocked shim on its own life cap.
    std::fs::write(&gate, b"go").expect("the chain is released");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !survivors(&dir.to_string_lossy()).is_empty() {
        std::thread::sleep(Duration::from_millis(20));
    }
    sup.stop();
    drop(server);
    let leaked = survivors(&dir.to_string_lossy());
    for (pid, _) in &leaked {
        kill_hard(*pid);
    }
    // The dir goes with the guard, here rather than at the end of the function, so the order the
    // rest of this block establishes still holds: processes are killed before their cwd is removed.
    drop(dir);

    let contracts = walked.unwrap_or_else(|e| panic!("{e}")).len();

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
            "{h}: the node at max_depth must itself be served — it is the caller under test: {e}\n\
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
    // gate before the worktree, before `compile()` and before any process, so "refused" means each
    // count below is **exactly the chain plus the node under test**, and never one more.
    //
    // The expected numbers are derived from `DEFAULT_MAX_DEPTH` rather than written down, because
    // the chain's length is: a root at 0, children at 1..max_depth-1, and the node under test at
    // max_depth. Only the children get a worktree — a root runs in the operator's own checkout
    // (§9) — so there is one fewer branch than agent directory.
    let chain = DEFAULT_MAX_DEPTH as usize;
    assert_eq!(
        ev.agent_dirs.len(),
        chain + 1,
        "{h}: the chain is {chain} nodes and the node under test is one more; anything beyond that \
         is a grandchild marion created: {:?}",
        ev.agent_dirs
    );
    assert_eq!(
        ev.contracts, 1,
        "{h}: a refused spawn writes no contract, and the chain is still running, so the only \
         contract on disk is the node under test's"
    );
    assert_eq!(
        ev.branches.len(),
        chain,
        "{h}: one `marion/<task>` worktree branch per node marion made a worktree for — every node \
         but the root — and a further one would mean the gate ran after `make_worktree` rather \
         than before it: {:?}",
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
