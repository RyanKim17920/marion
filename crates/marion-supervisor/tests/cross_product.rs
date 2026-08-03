//! **The true harness matrix: every harness as a ROOT spawning every harness as a CHILD.**
//!
//! Three files already stand at three corners of this square and none of them is the square:
//!
//! - `harness_matrix.rs` drives four children through `run_spawn` **directly**, with no root in the
//!   run at all. It proves the *child* axis.
//! - `launch_only_root.rs` drives three roots through the real `marion` binary against a **stub**
//!   harness on `PATH`. It proves the *root* axis, with nothing real on the other end of `spawn`.
//! - `m1_hop.rs` proves exactly one cell of the cross-product — a claude root spawning a codex
//!   child — end to end against real binaries.
//!
//! That leaves fifteen combinations that had never run. A gemini root spawning an opencode child
//! is not implied by "gemini works as a root" and "opencode works as a child": the two nodes share
//! one canned provider, one bridge binary, one state tree and — in four of the sixteen cells — one
//! **wire**, and each of those is a place the two halves can meet and fail. This file closes it.
//!
//! # One `#[test]` per cell, sixteen of them, and never a loop
//!
//! A loop reports the first failure and hides the other fifteen. Each cell is named for its own
//! pair, so a failing run names the cell in its output.
//!
//! Every cell asserts the same eight things:
//!
//! 1. the run did not time out and `marion run` exited 0;
//! 2. exactly one `contracts/<task_id>.json` is persisted, and it deserializes to a `TaskContract`;
//! 3. `contract.child.harness` is the harness that **actually ran**, read off the adapter, and
//!    `contract.child.model` is the **compiled** harness-native value (`None` for codex, whose
//!    `exec` surface carries no model argument however loudly one was asked for);
//! 4. the narrative is the child's own — non-empty, `narrative_synthesized == false` — and the
//!    status is `Ok`, which together mean the report arrived through marion's MCP channel;
//! 5. **both nodes are visible in the provider's request log, by role and not merely by wire.** See
//!    below: in a same-harness cell the wire is one wire, so a wire-name assertion would pass with
//!    only one of the two nodes having run;
//! 6. `TaskContract.requester` names the root's own agent-dir and is **not** `"unattributed-root"`;
//! 7. no verbatim credential reached the canned server;
//! 8. nothing outlived the run (the S7 class of failure).
//!
//! # The same-wire cells, and why they are not ambiguous
//!
//! Four cells — claude→claude, codex→codex, gemini→gemini, opencode→opencode — put the root's
//! delegate turn and the child's report turn on **one wire**, at a provider that dispatches on
//! request *shape* and is forbidden (`marion-provider`'s module docs) from dispatching on arrival
//! order. The child's entire run happens *inside* the root's `spawn` call, so the two conversations
//! are interleaved by construction and counting requests would be wrong even in principle.
//!
//! They are told apart by the only thing that genuinely distinguishes them: **whose task the
//! request carries**. Every harness replays its node's whole conversation on every turn, so the
//! root's prompt is present in every root request and in no child request — `RootScript::marker`.
//! That is a function of the body, exactly like `classify_root` and `classify_child`, and the
//! provider's own unit tests replay a run backwards to prove it. **No same-wire pair proved
//! ambiguous**, and the four same-harness cells below are the end-to-end witnesses.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test cross_product
//! ```
//!
//! It needs real `claude` (2.1.220), `codex` (0.146.0), `gemini` (0.53.0) and `opencode` (1.17.3)
//! on `PATH`. Like every other end-to-end file here it is **not** `#[ignore]`d and it does **not**
//! skip when a binary is missing: §9's standing rule is that *a criterion that quietly passes on a
//! machine that cannot run it is worth less than no criterion.*

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use marion_core::contract::{ExitStatus, TaskContract};
use marion_core::harness::Harness;
use marion_harness::adapter_for;
use marion_provider::{CannedServer, Config, RootScript, RootTurn, Script};
use marion_supervisor::root::{RootPath, root_path};
use marion_supervisor::run::run_bounded;
use serde_json::{Value, json};

/// The outermost safety net. Every cell has its own `--timeout` below; this only exists so a wedged
/// `marion` fails loudly instead of wedging the suite.
const RUN_BOUND: Duration = Duration::from_secs(300);

/// The root's bound on a **`LaunchOnly`** surface, where `--timeout` is a wall clock over the whole
/// run — and the child's entire run happens inside the root's `spawn` call, so this has to cover
/// both. Short enough that a hang fails fast: measured in S13, opencode never exits on a provider
/// hang (a 500 still retrying at 90 s, a connection-refused still hung at 180 s).
const ROOT_WALL_CLOCK_SECS: &str = "150";

/// The root's bound on a **duplex** surface, where `--timeout` is §9's per-episode `Blocked`-only
/// budget and *not* a wall clock. Short, exactly as `m1_hop` keeps it: a permission request that
/// cannot be answered must fail this test in seconds rather than stall it.
const ROOT_BLOCKED_SECS: &str = "5";

/// The child's own wall clock, passed through `spawn`'s `timeout_secs`.
const CHILD_TIMEOUT_SECS: u64 = 60;

/// Present in the **root's** prompt and nowhere in the child's. The provider's role discriminator;
/// see the module docs. Deliberately not a word either prompt would use on its own.
const ROOT_MARKER: &str = "MARION-XPROD-ROOT-TURN-4f2a";

/// The child's task. Shared by all sixteen cells, and free of [`ROOT_MARKER`] — a child prompt that
/// carried it would make every child request look like the root's.
const CHILD_PROMPT: &str = "Add the cross-product marker file under src/ and report back \
                            through marion.";

/// The narrative every child's script reports.
const NARRATIVE: &str = "Wrote the cross-product marker under src/ and reported back.";

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGKILL: i32 = 9;

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("marion-xp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch dir");
    p.canonicalize().expect("scratch dir canonicalises")
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

/// A repository for the child to worktree. Its own, not marion's: the run writes to it.
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

/// Every `contracts/<task_id>.json` marion persisted under `state`, as `(path, parsed)`.
fn persisted_contracts(state: &Path) -> Vec<(PathBuf, Value)> {
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
    let mut paths = Vec::new();
    walk(state, &mut paths);
    paths
        .into_iter()
        .filter_map(|p| {
            let v = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
            Some((p, v))
        })
        .collect()
}

/// Processes still alive with `needle` on their command line, as `(pid, line)`.
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
/// decide a request's role, restated here so the assertions read the log the way the server read it.
fn carries(v: &Value, needle: &str) -> bool {
    match v {
        Value::String(s) => s.contains(needle),
        Value::Array(a) => a.iter().any(|x| carries(x, needle)),
        Value::Object(o) => o.values().any(|x| carries(x, needle)),
        _ => false,
    }
}

// --- the four harnesses, as data ---------------------------------------------------------------

/// One harness, in both of the roles it can play.
#[derive(Debug, Clone, Copy)]
struct Node {
    /// The built-in agent type. `codex-impl` is `codex`'s canonical name, so both roles use it.
    agent_type: &'static str,
    harness: Harness,
    /// `--model` for a root, and `spawn`'s `model` for a child. `None` where the harness takes
    /// none; the two that refuse to compile without one state it.
    model: Option<&'static str>,
    /// `TaskContract.child.model` when this harness is the **child**: the compiled value, which is
    /// `None` on codex whatever was asked for.
    child_model: Option<&'static str>,
    /// The wire this harness speaks to the canned provider on.
    wire: &'static str,
    /// The binary that must be on `PATH`.
    program: &'static str,
}

const CLAUDE: Node = Node {
    agent_type: "claude",
    harness: Harness::ClaudeCode,
    // Claude Code's `--model` is legitimately omissible and the canned provider ignores it, so the
    // cells assert the absence rather than pinning a vendor id marion has no basis for.
    model: None,
    child_model: None,
    wire: "anthropic",
    program: "claude",
};

const CODEX: Node = Node {
    agent_type: "codex-impl",
    harness: Harness::Codex,
    model: None,
    child_model: None,
    wire: "responses",
    program: "codex",
};

const GEMINI: Node = Node {
    agent_type: "gemini",
    harness: Harness::Gemini,
    // Explicit: the adapter REFUSES to compile without `-m` (S12's `auto` router hang).
    model: Some("gemini-2.5-flash"),
    child_model: Some("gemini-2.5-flash"),
    wire: "gemini",
    program: "gemini",
};

const OPENCODE: Node = Node {
    agent_type: "opencode",
    harness: Harness::OpenCode,
    // `provider/model`, the only spelling `-m` accepts; the generated provider block repeats it.
    model: Some("marion/canned-1"),
    child_model: Some("marion/canned-1"),
    wire: "openai",
    program: "opencode",
};

/// marion's `spawn`, in the spelling **this harness's wire** dispatches on.
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
fn spawn_tool(node: &Node) -> String {
    let adapter = adapter_for(node.harness).expect("every harness in this matrix has an adapter");
    match node.harness {
        Harness::Codex => "spawn".to_string(),
        _ => adapter.marion_tool_name("spawn"),
    }
}

/// marion's `report`, in the same per-harness spelling, for the **child**'s script.
fn report_tool(node: &Node) -> String {
    let adapter = adapter_for(node.harness).expect("every harness in this matrix has an adapter");
    match node.harness {
        Harness::Codex => "report".to_string(),
        _ => adapter.marion_tool_name("report"),
    }
}

/// The `Script` that answers **both** nodes of one cell.
///
/// The root's half is a [`RootScript`] keyed on [`ROOT_MARKER`]; the child's half is the per-wire
/// fields the `harness_matrix` cells already use, retargeted at this child's own spelling of
/// `report`. In a same-harness cell both halves live on one wire and the marker is what separates
/// them — see the module docs.
fn script(root: &Node, child: &Node) -> Script {
    let mut spawn_args = json!({
        "agent_type": child.agent_type,
        "prompt": CHILD_PROMPT,
        "acceptance_criteria": ["a file exists under src/ containing the marker"],
        "writable_scope": ["src/**"],
        "timeout_secs": CHILD_TIMEOUT_SECS,
    });
    // **Absent, never null**, on the two harnesses that take no model. §3.1 makes an omitted
    // `model` mean "the agent type's own default" (`marion-supervisor::main` reads it with
    // `as_str()`), and a JSON `null` is not the same thing to every harness: measured here, gemini
    // 0.53.0 validates a tool call against the declared schema *before* dispatching it and refuses
    // `"model": null` with `params/model must be string` — an `invalid_tool_params` tool_result,
    // after which the root happily finished its turn having spawned nothing.
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
        // The Anthropic wire's two-step script *is* a child script once its tool is re-aimed:
        // `classify_root` finishes the run as soon as the transcript carries that call's result.
        Harness::ClaudeCode => {
            s.root_tool = report;
            s.root_tool_input = json!({ "narrative": NARRATIVE });
            s.root_final_text = "Reported back through marion. Done.".into();
        }
        // The Responses child keeps its three steps: patch, report, final message.
        Harness::Codex => s.child_narrative = NARRATIVE.into(),
        Harness::Gemini => {
            s.gemini_report_tool = report;
            s.gemini_report_args = json!({ "narrative": NARRATIVE });
        }
        Harness::OpenCode => {
            s.openai_report_tool = report;
            s.openai_report_args = json!({ "narrative": NARRATIVE });
        }
    }
    s
}

// --- driving one cell --------------------------------------------------------------------------

/// What a driven cell left behind, gathered **after** every process and directory is cleaned up so
/// no assertion below can turn a failing run into the leak this file also tests for. `timeout_kill`
/// and `harness_matrix` take the same line for the same reason.
struct Evidence {
    timed_out: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
    /// The persisted `contracts/*.json`, read back as JSON before the scratch dir was removed.
    persisted: Vec<Value>,
    /// The state-relative directory each contract sat under, so `requester` can be checked against
    /// an agent-dir that really existed rather than against a path rebuilt from guesses.
    agent_dirs_present: Vec<String>,
    /// The canned provider's verbatim request log — the primary evidence for any failure.
    requests: Vec<Value>,
    leaked: Vec<String>,
}

impl Evidence {
    /// Requests belonging to the **root**, by the same whole-body rule the provider dispatched on.
    fn root_requests(&self) -> Vec<&Value> {
        self.requests
            .iter()
            .filter(|r| is_a_turn(r) && carries(&r["body"], ROOT_MARKER))
            .collect()
    }

    /// Requests belonging to the **child**: every turn that is not the root's.
    fn child_requests(&self) -> Vec<&Value> {
        self.requests
            .iter()
            .filter(|r| is_a_turn(r) && !carries(&r["body"], ROOT_MARKER))
            .collect()
    }

    fn log_summary(&self) -> String {
        if self.requests.is_empty() {
            return "  (the provider received NO requests at all)".into();
        }
        self.requests
            .iter()
            .map(|r| {
                format!(
                    "  seq {} {} {} → wire {:?}, role {}",
                    r["seq"],
                    r["method"],
                    r["path"],
                    r["wire"],
                    if !is_a_turn(r) {
                        "not a node's turn"
                    } else if carries(&r["body"], ROOT_MARKER) {
                        "root"
                    } else {
                        "child"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn marion_summary(&self) -> String {
        format!(
            "  marion run exited {:?} (timed_out {})\n  stderr:\n{}\n  stdout (first 2 KiB):\n{}",
            self.code,
            self.timed_out,
            self.stderr.trim(),
            self.stdout.chars().take(2048).collect::<String>()
        )
    }
}

/// Is this logged request one of the two nodes' turns at all?
///
/// Two kinds are not, and both are measured rather than assumed:
///
/// - a request the provider could not route to a wire — Claude Code opens `HEAD /api/hello` as a
///   connectivity probe, which carries no body and belongs to nobody;
/// - the two **auxiliary** requests the provider answers with fixed stubs, each on the one wire
///   that makes it: Claude Code's concurrent session-title generation and gemini's model-routing
///   classifier probe. Both are recognised by carrying no tools, which is exactly how
///   `classify_anthropic` and `classify_gemini` recognise them. The rule is deliberately **not**
///   applied to the other two wires: codex declares its code-mode catalogue inside `input` rather
///   than in a top-level `tools` array, so a no-tools rule there would classify every one of a
///   codex node's real turns as auxiliary.
fn is_a_turn(request: &Value) -> bool {
    let Some(wire) = request["wire"].as_str() else {
        return false;
    };
    let has_tools = request["body"]["tools"]
        .as_array()
        .is_some_and(|t| !t.is_empty());
    match wire {
        "anthropic" | "gemini" => has_tools,
        _ => true,
    }
}

/// Stand up a canned provider, build a fixture repo, run the real `marion` binary on `root`, then
/// clean up **unconditionally** and hand back what happened.
fn drive(root: &Node, child: &Node) -> Evidence {
    let name = format!("{}-{}", root.agent_type, child.agent_type);
    let dir = scratch(&name);
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(root, child),
    })
    .expect("the canned provider binds");

    let adapter = adapter_for(root.harness).expect("the root's harness has an adapter");
    // §3.4: what `--timeout` bounds follows the surface, so the value does too. Derived from the
    // adapter rather than from the harness's name, exactly as `marion run` derives the path itself.
    let timeout = match root_path(&adapter.surfaces()) {
        Some(RootPath::Duplex) => ROOT_BLOCKED_SECS,
        _ => ROOT_WALL_CLOCK_SECS,
    };
    let prompt = format!("{ROOT_MARKER}: delegate the marker-file task to a child.");
    let mut args: Vec<String> = vec![
        "run".into(),
        root.agent_type.into(),
        "--prompt".into(),
        prompt,
        "--repo".into(),
        repo.to_string_lossy().into_owned(),
        "--state-dir".into(),
        state.to_string_lossy().into_owned(),
        "--base-url".into(),
        server.base_url(),
        "--timeout".into(),
        timeout.into(),
    ];
    if let Some(m) = root.model {
        args.push("--model".into());
        args.push(m.into());
    }

    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args(&args)
            .current_dir(&dir),
        RUN_BOUND,
    )
    .expect("marion run starts");

    let requests = server.requests().unwrap_or_default();
    let contracts = persisted_contracts(&state);
    let agent_dirs_present = std::fs::read_dir(&state)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|project| std::fs::read_dir(project.path().join("agents")))
        .flatten()
        .flatten()
        .filter_map(|a| a.file_name().into_string().ok())
        .collect();

    // Cleanup first, and unconditionally: a failing cell must never become the leak it tests for.
    drop(server);
    let leaked = survivors(&dir.to_string_lossy());
    for (pid, _) in &leaked {
        let _ = unsafe { kill(*pid, SIGKILL) };
    }
    let _ = std::fs::remove_dir_all(&dir);

    Evidence {
        timed_out: out.timed_out,
        code: out.code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        persisted: contracts.into_iter().map(|(_, v)| v).collect(),
        agent_dirs_present,
        requests,
        leaked: leaked.into_iter().map(|(_, line)| line).collect(),
    }
}

/// The eight assertions every cell makes, in the order that makes a failure most diagnosable: the
/// run's own outcome first, then what the provider saw, then what marion recorded.
fn assert_cell(root: &Node, child: &Node, ev: &Evidence) {
    let cell = format!("{} root → {} child", root.harness, child.harness);

    // ---- 1: the run finished, and cleanly. ------------------------------------------------------
    assert!(
        !ev.timed_out,
        "{cell}: marion run did not finish inside {RUN_BOUND:?}\n{}\nRequest log:\n{}",
        ev.marion_summary(),
        ev.log_summary()
    );
    assert_eq!(
        ev.code,
        Some(0),
        "{cell}: marion run exited non-zero\n{}\nRequest log:\n{}",
        ev.marion_summary(),
        ev.log_summary()
    );

    // ---- 7: nothing was paid for. ---------------------------------------------------------------
    assert!(
        !ev.requests.iter().any(|r| {
            ["x-api-key", "authorization", "x-goog-api-key"]
                .iter()
                .any(|h| r["headers"][h].is_string() && r["headers"][h] != "<redacted>")
        }),
        "{cell}: a run that reached the canned server with a verbatim credential is not the \
         zero-cost, repeatable run this matrix is about"
    );

    // ---- 5: both nodes are in the log, by ROLE. -------------------------------------------------
    //
    // Not by wire name: in a same-harness cell there is only one wire, so `wires() == [w]` would
    // pass with only one of the two nodes having ever run. The role is read back with the same
    // whole-body rule the provider dispatched on.
    let root_reqs = ev.root_requests();
    let child_reqs = ev.child_requests();
    assert!(
        root_reqs.len() >= 2,
        "{cell}: the root must have taken at least two turns — one to call spawn and one after \
         its result came back. Request log:\n{}\n{}",
        ev.log_summary(),
        ev.marion_summary()
    );
    assert!(
        !child_reqs.is_empty(),
        "{cell}: no request in the log belongs to the child, so the child never took a turn. \
         Request log:\n{}\n{}",
        ev.log_summary(),
        ev.marion_summary()
    );
    for (label, reqs, node) in [("root", &root_reqs, root), ("child", &child_reqs, child)] {
        for r in reqs.iter() {
            assert_eq!(
                r["wire"].as_str(),
                Some(node.wire),
                "{cell}: a {label} request arrived on the wrong wire — the {} node speaks {:?}. \
                 Request log:\n{}",
                label,
                node.wire,
                ev.log_summary()
            );
        }
    }

    // ---- 2 and 4: one contract, and the child's own report is in it. ----------------------------
    assert_eq!(
        ev.persisted.len(),
        1,
        "{cell}: exactly one child ran, so exactly one contract is persisted. Request log:\n{}\n{}",
        ev.log_summary(),
        ev.marion_summary()
    );
    let contract: TaskContract = serde_json::from_value(ev.persisted[0].clone())
        .unwrap_or_else(|e| panic!("{cell}: the persisted contract does not deserialize: {e}"));
    let comp = contract
        .completion
        .as_ref()
        .unwrap_or_else(|| panic!("{cell}: a finished run has a completion"));
    assert!(
        comp.narrative.as_ref().is_some_and(|n| !n.value.is_empty()),
        "{cell}: the narrative is the child's own, sourced from its `report` call — an absent one \
         means the report never arrived through the MCP channel, or the adapter could not read \
         this harness's stream. exit: {}\nRequest log:\n{}",
        comp.exit.description,
        ev.log_summary()
    );
    assert!(
        !comp.narrative_synthesized,
        "{cell}: a synthesized narrative is marion's words, not the child's"
    );
    assert_eq!(
        comp.status,
        ExitStatus::Ok,
        "{cell}: exit: {}\nRequest log:\n{}",
        comp.exit.description,
        ev.log_summary()
    );

    // ---- 3: the contract does not lie about which harness or model produced the work. -----------
    assert_eq!(
        contract.child.harness, child.harness,
        "{cell}: the contract must name the harness that actually ran (read off the adapter)"
    );
    assert_eq!(
        contract.child.model.as_deref(),
        child.child_model,
        "{cell}: `child.model` records the COMPILED, harness-native value — codex carries none \
         however loudly one was asked for"
    );

    // ---- 6: the requester is the root's own AgentId. --------------------------------------------
    assert_ne!(
        contract.requester.0, "unattributed-root",
        "{cell}: that placeholder is what a bridge with no MARION_AGENT_ID reports, so a contract \
         carrying it is §6.7's audit record naming an agent-dir that does not exist. It is what a \
         codex root produced until `codex::config_toml` learned to emit an `env` block"
    );
    assert!(
        ev.agent_dirs_present.contains(&contract.requester.0),
        "{cell}: TaskContract.requester must name a real agent-dir marion created (§9), got {:?}; \
         the run's agent-dirs were {:?}",
        contract.requester,
        ev.agent_dirs_present
    );

    // ---- 8: nothing outlived the run. -----------------------------------------------------------
    assert!(
        ev.leaked.is_empty(),
        "{cell}: processes from this run are still alive — the S7 class of failure:\n{}",
        ev.leaked.join("\n")
    );
}

/// One cell, start to finish. The binaries are asserted first and by name: a missing one is a
/// failure that says which, never a skip.
fn cell(root: &Node, child: &Node) {
    for n in [root, child] {
        assert!(
            on_path(n.program),
            "this cell drives a REAL {}; put it on PATH",
            n.program
        );
    }
    let ev = drive(root, child);
    assert_cell(root, child, &ev);
}

// --- the sixteen cells --------------------------------------------------------------------------
//
// One `#[test]` each, named for its own pair. Never a loop: a loop reports the first failure and
// hides the other fifteen.

#[test]
fn a_claude_root_spawns_a_claude_child_and_receives_its_contract() {
    cell(&CLAUDE, &CLAUDE);
}

#[test]
fn b_claude_root_spawns_a_codex_child_and_receives_its_contract() {
    cell(&CLAUDE, &CODEX);
}

#[test]
fn c_claude_root_spawns_a_gemini_child_and_receives_its_contract() {
    cell(&CLAUDE, &GEMINI);
}

#[test]
fn d_claude_root_spawns_an_opencode_child_and_receives_its_contract() {
    cell(&CLAUDE, &OPENCODE);
}

#[test]
fn e_codex_root_spawns_a_claude_child_and_receives_its_contract() {
    cell(&CODEX, &CLAUDE);
}

#[test]
fn f_codex_root_spawns_a_codex_child_and_receives_its_contract() {
    cell(&CODEX, &CODEX);
}

#[test]
fn g_codex_root_spawns_a_gemini_child_and_receives_its_contract() {
    cell(&CODEX, &GEMINI);
}

#[test]
fn h_codex_root_spawns_an_opencode_child_and_receives_its_contract() {
    cell(&CODEX, &OPENCODE);
}

#[test]
fn i_gemini_root_spawns_a_claude_child_and_receives_its_contract() {
    cell(&GEMINI, &CLAUDE);
}

#[test]
fn j_gemini_root_spawns_a_codex_child_and_receives_its_contract() {
    cell(&GEMINI, &CODEX);
}

#[test]
fn k_gemini_root_spawns_a_gemini_child_and_receives_its_contract() {
    cell(&GEMINI, &GEMINI);
}

#[test]
fn l_gemini_root_spawns_an_opencode_child_and_receives_its_contract() {
    cell(&GEMINI, &OPENCODE);
}

#[test]
fn m_opencode_root_spawns_a_claude_child_and_receives_its_contract() {
    cell(&OPENCODE, &CLAUDE);
}

#[test]
fn n_opencode_root_spawns_a_codex_child_and_receives_its_contract() {
    cell(&OPENCODE, &CODEX);
}

#[test]
fn o_opencode_root_spawns_a_gemini_child_and_receives_its_contract() {
    cell(&OPENCODE, &GEMINI);
}

#[test]
fn p_opencode_root_spawns_an_opencode_child_and_receives_its_contract() {
    cell(&OPENCODE, &OPENCODE);
}
