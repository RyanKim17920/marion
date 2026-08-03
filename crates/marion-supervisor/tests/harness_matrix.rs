//! The child-side harness matrix: **one real child per harness, end to end, at zero cost.**
//!
//! `m1_hop` proves one cell of the cross-product — a claude-code root spawning a codex child. This
//! file proves the *child* axis for all four harnesses marion can name, because that is the axis
//! the adapter seam was built for and the one whose failures are silent: until `run_spawn`
//! dispatched on `agent_type.harness`, a gemini agent type wrote a Codex config, ran `codex`, and
//! recorded `"gemini"` in the contract (§6.7's audit record describing a run that never happened).
//!
//! For each harness the run is driven by the in-process [`CannedServer`], which speaks all four
//! wires, and every cell asserts the same five things:
//!
//! 1. a real binary ran and **reported through marion's bridge** — the persisted
//!    `contracts/<task_id>.json` deserializes, its narrative is the child's own
//!    (`narrative_synthesized == false`) and its status is `Ok`;
//! 2. `contract.child.harness` is the harness that **actually ran**, read off the adapter;
//! 3. `contract.child.model` is the **compiled, harness-native** value — `None` for codex, whose
//!    `exec` surface carries no model argument however loudly one was asked for;
//! 4. the provider's request log shows **that harness's wire**. This is what makes (2) more than a
//!    tautology: a contract stamped `gemini` whose only traffic was OpenAI Responses would mean
//!    codex ran and something else succeeded by accident;
//! 5. nothing outlived the run (the S7 class of failure).
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test harness_matrix
//! ```
//!
//! It needs real `claude` (2.1.220), `codex` (0.146.0), `gemini` (0.53.0) and `opencode` (1.17.3)
//! on `PATH`. Like `m1_hop` and `timeout_kill` it is **not** `#[ignore]`d and it does **not** skip
//! when a binary is missing: §9's standing rule is that *a criterion that quietly passes on a
//! machine that cannot run it is worth less than no criterion*. One `#[test]` per harness, never a
//! loop over four, so a failure names its own cell instead of hiding the three behind it.

use std::path::{Path, PathBuf};
use std::process::Command;

use marion_core::contract::{ExitStatus, TaskContract, TaskId};
use marion_core::harness::Harness;
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{Env, SpawnRequest, run_spawn};
use serde_json::{Value, json};

/// Every child's bound. Short on purpose: **opencode never exits on a provider hang** (S13
/// measured a 500 still retrying at 90 s and a connection-refused still hung at 180 s), so this is
/// load-bearing rather than defensive — a wedged cell must fail fast and loudly instead of
/// wedging CI. A healthy run of any of the four is seconds.
const CHILD_TIMEOUT_SECS: u64 = 60;

/// The narrative every wire's script reports. One string, so a cell that read *another* harness's
/// stream would still have to have produced the right wire's traffic to get here.
const NARRATIVE: &str = "Wrote the matrix marker under src/ and reported back.";

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGKILL: i32 = 9;

fn scratch(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("marion-matrix-{name}-{}", std::process::id()));
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

/// Every `contracts/<task_id>.json` marion persisted under `state`.
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

/// One cell of the matrix: which agent type to spawn, and what the run must look like afterwards.
struct Cell {
    /// The built-in agent type `spawn` resolves.
    agent_type: &'static str,
    /// `spawn`'s own `model`, where the harness needs one. `None` leaves the agent type's default
    /// (or, on codex and claude-code, no model at all).
    model: Option<&'static str>,
    /// The scripted behaviour for this harness's wire.
    script: Script,
    /// The harness the contract must name — read off the adapter that actually ran.
    expected_harness: Harness,
    /// `TaskContract.child.model`: the **compiled** value, not the asked-for one.
    expected_model: Option<&'static str>,
    /// The wire the provider must have been spoken to on. The proof that this harness ran.
    expected_wire: &'static str,
}

/// What a driven cell left behind, gathered **after** every process and directory is cleaned up so
/// no assertion below can turn a failing run into the leak this file tests for.
struct Evidence {
    contract: Result<TaskContract, String>,
    /// The persisted `contracts/*.json`, read back as JSON before the scratch dir was removed.
    persisted: Vec<Value>,
    /// The canned provider's verbatim request log — the primary evidence for any failure.
    requests: Vec<Value>,
    /// Survivors of the run, if any.
    leaked: Vec<String>,
}

impl Evidence {
    /// The distinct wires the provider recorded, in a stable order.
    fn wires(&self) -> Vec<&str> {
        let mut w: Vec<&str> = self
            .requests
            .iter()
            .filter_map(|r| r["wire"].as_str())
            .collect();
        w.sort_unstable();
        w.dedup();
        w
    }

    /// A compact rendering of the request log for a failure message. Paths and wires only: the
    /// bodies are tens of kilobytes each and the log itself is the place to read them.
    fn log_summary(&self) -> String {
        if self.requests.is_empty() {
            return "  (the provider received NO requests at all)".into();
        }
        self.requests
            .iter()
            .map(|r| {
                format!(
                    "  seq {} {} {} → wire {:?}",
                    r["seq"], r["method"], r["path"], r["wire"]
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What marion made of the run — the other half of the evidence, and the half that carries the
    /// child's stderr. Included in the *first* assertion's message because a cell that never
    /// reached the provider has its whole explanation here.
    fn marion_summary(&self) -> String {
        match &self.contract {
            Err(e) => format!("  run_spawn errored: {e}"),
            Ok(c) => match &c.completion {
                None => "  the contract has no completion".into(),
                Some(comp) => format!(
                    "  status {:?}, narrative {:?}, exit: {}",
                    comp.status,
                    comp.narrative.as_ref().map(|n| &n.value),
                    comp.exit.description
                ),
            },
        }
    }
}

/// Stand up a canned provider, build a fixture repo, spawn one child through `run_spawn`, then
/// clean up **unconditionally** and hand back what happened.
fn drive(cell: &Cell) -> Evidence {
    let root_dir = scratch(cell.agent_type);
    let repo = fixture_repo(&root_dir);
    let state = root_dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root_dir.join("provider-requests.jsonl"),
        script: cell.script.clone(),
    })
    .expect("the canned provider binds");

    let env = Env {
        repo: repo.clone(),
        project_dir: ProjectDir::new(&state, &repo),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: server.base_url(),
    };
    let req = SpawnRequest {
        agent_type: cell.agent_type.into(),
        prompt: "Add the matrix marker file under src/ and report back through marion.".into(),
        acceptance_criteria: vec!["a file exists under src/ containing the matrix marker".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: cell.model.map(str::to_string),
    };
    let contract = run_spawn(
        &env,
        &req,
        &TaskId(format!("matrix-{}", cell.agent_type)),
        "root",
    )
    .map_err(|e| e.to_string());

    let requests = server.requests().unwrap_or_default();
    let persisted = persisted_contracts(&state)
        .iter()
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect();

    // Cleanup first, and unconditionally: a failing cell must never become the leak it is testing
    // for. `timeout_kill` takes the same line for the same reason.
    drop(server);
    let leaked = survivors(&root_dir.to_string_lossy());
    for (pid, _) in &leaked {
        let _ = unsafe { kill(*pid, SIGKILL) };
    }
    let _ = std::fs::remove_dir_all(&root_dir);

    Evidence {
        contract,
        persisted,
        requests,
        leaked: leaked.into_iter().map(|(_, line)| line).collect(),
    }
}

/// The five assertions every cell makes, in the order that makes a failure most diagnosable: what
/// the provider saw first (it explains everything downstream), then what marion recorded.
fn assert_cell(cell: &Cell, ev: &Evidence) {
    let harness = cell.expected_harness;

    // ---- the wire: proof that THIS harness ran, and that nothing was paid for. ------------------
    assert_eq!(
        ev.wires(),
        vec![cell.expected_wire],
        "{harness}: the canned provider must have been spoken to on exactly this harness's wire. \
         Request log:\n{}\nWhat marion recorded:\n{}",
        ev.log_summary(),
        ev.marion_summary()
    );
    assert!(
        !ev.requests.iter().any(|r| {
            ["x-api-key", "authorization", "x-goog-api-key"]
                .iter()
                .any(|h| r["headers"][h].is_string() && r["headers"][h] != "<redacted>")
        }),
        "{harness}: a run that reached the canned server with a verbatim credential is not the \
         zero-cost, repeatable run this matrix is about"
    );

    // ---- the child ran and reported through marion's bridge. -----------------------------------
    let contract = match &ev.contract {
        Ok(c) => c,
        Err(e) => panic!(
            "{harness}: run_spawn refused or failed: {e}\nRequest log:\n{}",
            ev.log_summary()
        ),
    };
    assert_eq!(
        ev.persisted.len(),
        1,
        "{harness}: exactly one child ran, so exactly one contract is persisted"
    );
    let persisted: TaskContract = serde_json::from_value(ev.persisted[0].clone())
        .unwrap_or_else(|e| panic!("{harness}: the persisted contract does not deserialize: {e}"));
    let comp = persisted
        .completion
        .as_ref()
        .expect("a finished run has a completion");
    assert!(
        comp.narrative.as_ref().is_some_and(|n| !n.value.is_empty()),
        "{harness}: the narrative is the child's own, sourced from its `report` call — an absent \
         one means the report never arrived through the MCP channel, or the adapter could not read \
         this harness's stream. exit: {}\nRequest log:\n{}",
        comp.exit.description,
        ev.log_summary()
    );
    assert!(
        !comp.narrative_synthesized,
        "{harness}: a synthesized narrative is marion's words, not the child's"
    );
    assert_eq!(
        comp.status,
        ExitStatus::Ok,
        "{harness}: exit: {}\nRequest log:\n{}",
        comp.exit.description,
        ev.log_summary()
    );

    // ---- the contract does not lie about which harness or model produced the work. --------------
    assert_eq!(
        persisted.child.harness, harness,
        "{harness}: the contract must name the harness that actually ran (read off the adapter)"
    );
    assert_eq!(
        persisted.child.model.as_deref(),
        cell.expected_model,
        "{harness}: `child.model` records the COMPILED, harness-native value — codex carries none \
         however loudly one was asked for"
    );
    assert_eq!(
        contract.child.harness, persisted.child.harness,
        "{harness}: the returned copy and the persisted one describe one run"
    );
    assert_eq!(contract.child.model, persisted.child.model, "{harness}");

    // ---- nothing outlived the run. --------------------------------------------------------------
    assert!(
        ev.leaked.is_empty(),
        "{harness}: processes from this run are still alive — the S7 class of failure:\n{}",
        ev.leaked.join("\n")
    );
}

/// The claude-code child's script. The Anthropic wire's two-step root script *is* a child script
/// once its tool is re-aimed: turn one calls a tool, and `classify_root` finishes the run as soon
/// as the transcript carries that call's `tool_result`.
fn claude_code_script() -> Script {
    Script {
        root_tool: "mcp__marion__report".into(),
        root_tool_input: json!({ "narrative": NARRATIVE }),
        root_final_text: "Reported back through marion. Done.".into(),
        ..Script::default()
    }
}

/// **This cell is RED, and it is red about a real defect.** It is left failing rather than
/// `#[ignore]`d or weakened, because §9's rule cuts both ways: a criterion that quietly passes on a
/// machine that cannot run it is worth less than no criterion, and so is one deleted because the
/// answer was inconvenient.
///
/// What it proves, measured against 2.1.220 while writing it: **`run_spawn`'s `LaunchOnly` path
/// cannot drive a Claude Code child.** The launch itself is now correct — the adapter compiles the
/// prompt into argv, the bridge starts, answers `tools/list` and touches its marker — but the CLI
/// does **not gate its first turn on that**. The `system/init` frame reads, verbatim:
///
/// ```text
/// "tools":[],"mcp_servers":[{"name":"marion","status":"pending"}]
/// ```
///
/// and the turn goes out with `tools: []` while the server is still connecting. The canned provider
/// answers a no-tools request with the session-title stub, the CLI takes that as `end_turn`, and the
/// run exits **0** having called nothing — §12's silent-failure shape exactly, and word for word the
/// hazard §6.1 step 8 exists to prevent. `MCP_TIMEOUT` does not change it; there is no flag that
/// makes the CLI wait, and no later turn to recover on, because the run has already ended.
///
/// The only remedy is §6.1 step 8's own: **withhold the first frame until the bridge signals
/// ready**, which requires `--input-format stream-json` — a typed control plane. That is precisely
/// what this adapter's `surfaces()` declares (`headless(TypedKind::StreamJson)`) and what the other
/// three declare they do *not* have (`launch_only_with_protocol_events`). So the gap is structural
/// and named: `run_spawn` drives `LaunchOnly` children, and claude-code is not one.
///
/// **This is also a live warning for any node whose prompt rides argv, root or child.** A Claude
/// Code node launched that way loses marion's tools on turn one, silently, whatever the tools were
/// for.
#[test]
fn a_claude_code_child_reports_through_marions_bridge_over_the_anthropic_wire() {
    assert!(
        on_path("claude"),
        "this cell drives a REAL claude child; put `claude` (2.1.220) on PATH"
    );
    let cell = Cell {
        agent_type: "claude",
        // Claude Code's `--model` is legitimately omissible and the canned provider ignores it, so
        // the cell asserts the absence rather than pinning a vendor id marion has no basis for.
        model: None,
        script: claude_code_script(),
        expected_harness: Harness::ClaudeCode,
        expected_model: None,
        expected_wire: "anthropic",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

#[test]
fn a_codex_child_edits_a_worktree_and_reports_through_marions_bridge() {
    assert!(
        on_path("codex"),
        "this cell drives a REAL codex child; put `codex` (0.146.0) on PATH"
    );
    let cell = Cell {
        agent_type: "codex-impl",
        // Asked for on purpose: `codex exec` carries no model argument, so the contract must record
        // `None` regardless — the property commit d0340d1 established.
        model: Some("gpt-5.6-sol"),
        script: Script {
            child_narrative: NARRATIVE.into(),
            ..Script::default()
        },
        expected_harness: Harness::Codex,
        expected_model: None,
        expected_wire: "responses",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

#[test]
fn a_gemini_child_reports_through_marions_bridge_over_the_gemini_wire() {
    assert!(
        on_path("gemini"),
        "this cell drives a REAL gemini child; put `gemini` (0.53.0) on PATH"
    );
    let cell = Cell {
        agent_type: "gemini",
        // Explicit: the adapter REFUSES to compile without `-m` (S12's `auto` router hang).
        model: Some("gemini-2.5-flash"),
        script: Script {
            // gemini's own spelling of marion's report tool: `mcp_<server>_<tool>`.
            gemini_report_tool: "mcp_marion_report".into(),
            gemini_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        expected_harness: Harness::Gemini,
        expected_model: Some("gemini-2.5-flash"),
        expected_wire: "gemini",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

#[test]
fn an_opencode_child_reports_through_marions_bridge_over_the_openai_wire() {
    assert!(
        on_path("opencode"),
        "this cell drives a REAL opencode child; put `opencode` (1.17.3) on PATH"
    );
    let cell = Cell {
        agent_type: "opencode",
        // `provider/model`, the only spelling `-m` accepts; the generated provider block repeats it.
        model: Some("marion/canned-1"),
        script: Script {
            // opencode's own spelling: `<serverName>_<toolName>`.
            openai_report_tool: "marion_report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        expected_harness: Harness::OpenCode,
        expected_model: Some("marion/canned-1"),
        expected_wire: "openai",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}
