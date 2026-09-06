//! The child-side harness matrix: **one real child per harness, end to end, at zero cost.**
//!
//! `m1_hop` proves one cell of the cross-product — a claude-code root spawning a codex child. This
//! file proves the *child* axis for all four harnesses marion can name, because that is the axis
//! the adapter seam was built for and the one whose failures are silent: until `run_spawn`
//! dispatched on `agent_type.harness`, a gemini agent type wrote a Codex config, ran `codex`, and
//! recorded `"gemini"` in the contract (§6.7's audit record describing a run that never happened).
//!
//! For each harness the run is driven by the in-process [`CannedServer`], which speaks all four
//! wires, and every cell asserts the same six things:
//!
//! 1. a real binary ran and **reported through marion's bridge** — the persisted
//!    `contracts/<task_id>.json` deserializes, its narrative is the child's own
//!    (`narrative_synthesized == false`) and its status is `Ok`;
//! 2. `contract.child.harness` is the harness that **actually ran**, read off the adapter;
//! 3. `contract.child.model` is the **compiled, harness-native** value — `None` for codex, whose
//!    `exec` surface carries no model argument however loudly one was asked for;
//! 4. `contract.allowed_tools` is the **constraint that harness actually ran under**, in that
//!    harness's own vocabulary (§3.1) — a per-tool allowlist on Claude Code, a sandbox mode on
//!    codex, an approval mode on gemini, and on opencode an explicit record that marion compiled
//!    no constraint at all. The third field of the same kind as (2) and (3), and the last of the
//!    three to stop being a constant: it was `["apply_patch", "shell"]` on every harness, which
//!    named tools three of them have never had;
//! 5. the provider's request log shows **that harness's wire**. This is what makes (2) more than a
//!    tautology: a contract stamped `gemini` whose only traffic was OpenAI Responses would mean
//!    codex ran and something else succeeded by accident;
//! 6. nothing outlived the run (the S7 class of failure).
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test harness_matrix
//! ```
//!
//! It needs real `claude`, `codex`, `gemini`, `opencode` and `copilot` on `PATH`, **at the versions
//! [`marion_testsupport::PINNED_HARNESSES`] lists** — that table is the source of truth, and the
//! gate refuses a binary of the right name at an unrecognised version rather than reporting a
//! matrix result attributed to a build that never ran.
//! Like `m1_hop` and `timeout_kill` it is **not** `#[ignore]`d and it does **not** skip
//! when a binary is missing: §9's standing rule is that *a criterion that quietly passes on a
//! machine that cannot run it is worth less than no criterion*. One `#[test]` per harness, never a
//! loop over five, so a failure names its own cell instead of hiding the four behind it.

use std::path::PathBuf;

use marion_core::contract::Isolation;
use marion_core::contract::{ExitStatus, TaskContract, TaskId};
use marion_core::harness::Harness;
use marion_core::paths::ProjectDir;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::{Caller, Env, SpawnRequest, run_spawn};
use marion_testsupport::{
    fixture_repo, judge, kill_hard, on_path, persisted_contracts, pinned_version, scratch,
    survivors,
};
use serde_json::{Value, json};

/// Every child's bound. Short on purpose: **opencode never exits on a provider hang** (S13
/// measured a 500 still retrying at 90 s and a connection-refused still hung at 180 s), so this is
/// load-bearing rather than defensive — a wedged cell must fail fast and loudly instead of
/// wedging CI. A healthy run of any of the four is seconds.
const CHILD_TIMEOUT_SECS: u64 = 60;

/// The narrative every wire's script reports. One string, so a cell that read *another* harness's
/// stream would still have to have produced the right wire's traffic to get here.
const NARRATIVE: &str = "Wrote the matrix marker under src/ and reported back.";

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
    /// `TaskContract.allowed_tools`: §6.7's audit record of **the constraint this harness actually
    /// ran under**, in that harness's own vocabulary — §3.1's *"the compiled, harness-native
    /// constraint, or the harness's coarsest equivalent where it has no per-tool allowlist at
    /// all"*.
    ///
    /// **Four harnesses, four different shapes of answer, and that is the content of the field.**
    /// Claude Code has a real per-tool allowlist and records its literal contents; codex has one
    /// sandbox mode; gemini has one approval mode; opencode has nothing marion compiles at all and
    /// records that it has nothing. A *uniform* value across the four is what this field carried
    /// until now — `["apply_patch", "shell"]`, hardcoded in `build_contract` — and it was wrong on
    /// every one of them: on three it named tools those harnesses have never had, and on codex,
    /// where it looks plausible, it is exactly the per-tool echo §3.1 forbids.
    expected_allowed_tools: &'static [&'static str],
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
    let root_dir = scratch(&format!("matrix-{}", cell.agent_type));
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
        project_dir: ProjectDir::new(&state, &repo),
        project_root: repo.clone(),
        state: state.clone(),
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some(server.base_url()),
        auth: marion_harness::Auth::Canned,
    };
    let req = SpawnRequest {
        agent_type: cell.agent_type.into(),
        prompt: "Add the matrix marker file under src/ and report back through marion.".into(),
        repo: repo.clone(),
        acceptance_criteria: vec!["a file exists under src/ containing the matrix marker".into()],
        writable_scope: vec!["src/**".into()],
        timeout_secs: CHILD_TIMEOUT_SECS,
        model: cell.model.map(str::to_string),
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
    };
    // A root caller: depth 0, so §6.1 step 2's gates see a top-level `spawn` — the same thing
    // `marion run` hands the bridge. `claude` is marion's root type and its `max_depth` is the
    // bound every cell here runs under.
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let contract = run_spawn(
        &env,
        &req,
        &TaskId(format!("matrix-{}", cell.agent_type)),
        &caller,
    )
    .map_err(|e| e.to_string());

    let requests = server.requests().unwrap_or_default();
    // Read here, because cleanup below removes the directory these files live in — but *judged*
    // after it, since a panic on this line would leave the child's processes and the scratch dir
    // behind, which is the very failure the next block exists to prevent.
    let walked = persisted_contracts(&state)
        .map_err(|e| format!("{} cannot be walked for contracts: {e}", state.display()));

    // Cleanup first, and unconditionally: a failing cell must never become the leak it is testing
    // for. `timeout_kill` takes the same line for the same reason. The scratch dir is not swept
    // here — `root_dir` is a `Scratch`, so it goes on the way out of this function whether the
    // assertions below pass, fail, or panic.
    drop(server);
    let leaked = survivors(&root_dir.to_string_lossy());
    for (pid, _) in &leaked {
        kill_hard(*pid);
    }

    // Nothing is left running, so a contract marion wrote and this test cannot read back is now
    // safe to fail on — and it must. `filter_map(…ok())` dropped such a file quietly, which let
    // "exactly one contract is persisted" pass on a run that persisted one good file and one
    // corrupt one: the corruption is a defect in the §6.7 audit record this cell is about, not
    // noise to filter out.
    let walked = walked.unwrap_or_else(|e| panic!("{e}"));
    let persisted = judge(&walked).into_iter().map(|(_, v)| v.clone()).collect();

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
        persisted.allowed_tools, cell.expected_allowed_tools,
        "{harness}: `allowed_tools` records the constraint this harness actually ran under, in its \
         own vocabulary — not marion's words for what was asked for, and not one constant shared \
         by four harnesses that constrain their children in four different ways"
    );
    assert!(
        !persisted
            .allowed_tools
            .iter()
            .any(|t| t == "apply_patch" || t == "shell" || t == "write"),
        "{harness}: `apply_patch`/`shell` are the hardcoded constant this field used to carry on \
         every harness, and `write` is marion's word for the request rather than any harness's \
         word for the constraint. Got: {:?}",
        persisted.allowed_tools
    );
    assert_eq!(
        contract.child.harness, persisted.child.harness,
        "{harness}: the returned copy and the persisted one describe one run"
    );
    assert_eq!(contract.child.model, persisted.child.model, "{harness}");
    assert_eq!(
        contract.allowed_tools, persisted.allowed_tools,
        "{harness}: the returned copy and the persisted one describe one run"
    );

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

/// **This cell was RED, and what turned it green is §6.1 step 8's gate — nothing else.**
///
/// Measured against 2.1.220 when it was written: `run_spawn`'s `LaunchOnly` path *cannot* drive a
/// Claude Code child. The launch was correct in every other respect — the adapter compiled the
/// prompt into argv, the bridge started, answered `tools/list` and touched its marker — but the CLI
/// does **not gate its first turn on that**. Its own `system/init` frame read, verbatim:
///
/// ```text
/// "tools":[],"mcp_servers":[{"name":"marion","status":"pending"}]
/// ```
///
/// so the turn went out with `tools: []` while the server was still connecting, the canned provider
/// answered a no-tools request with the session-title stub, the CLI took that as `end_turn`, and the
/// run exited **0** having called nothing — §12's silent-failure shape exactly. `MCP_TIMEOUT` does
/// not change it; there is no flag that makes the CLI wait, and no later turn to recover on.
///
/// The remedy is §6.1 step 8's own and there is no other: **withhold the first frame until marion
/// observes its own bridge flush `tools/list`, then complete an `initialize` round trip, then write
/// the prompt** — which requires `--input-format stream-json`, a typed control plane. That is
/// precisely what this adapter's `surfaces()` declares (`headless(TypedKind::StreamJson)`) and what
/// the other three declare they do *not* have (`launch_only_with_protocol_events`). So `run_spawn`
/// now routes a child on `adapter.surfaces().control` exactly as `marion run` routes a root, and
/// drives a `Typed(_)` child through `marion_supervisor::duplex` — the same gate, the same code.
///
/// **This cell is the gate's only end-to-end witness.** §12 records that the bug is invisible
/// against a real endpoint (a model reply takes seconds; the MCP connect ~70 ms) and appears only
/// against a fast one, which is what the `CannedServer` is. Stub the wait out — `if false &&
/// !wait_for_ready(…)` — and this cell regresses to the toolless-turn shape above, verbatim:
///
/// ```text
/// claude-code: the narrative is the child's own, sourced from its `report` call — an absent
/// one means the report never arrived through the MCP channel […] exit: child exited with code 0
/// ```
///
/// Two `anthropic` requests, no marion call in either, exit 0. That is the whole bug.
///
/// **It is also a live warning for any node whose prompt rides argv, root or child.** A Claude Code
/// node launched that way loses marion's tools on turn one, silently, whatever the tools were for.
#[test]
fn a_claude_code_child_reports_through_marions_bridge_over_the_anthropic_wire() {
    assert!(
        on_path("claude"),
        "this cell drives a REAL claude child; put `claude` ({}) on PATH",
        pinned_version("claude")
    );
    let cell = Cell {
        agent_type: "claude",
        // Claude Code's `--model` is legitimately omissible and the canned provider ignores it, so
        // the cell asserts the absence rather than pinning a vendor id marion has no basis for.
        model: None,
        script: claude_code_script(),
        expected_harness: Harness::ClaudeCode,
        expected_model: None,
        // A real per-tool allowlist: the literal contents of `--allowedTools`. `claude` declares no
        // tools, so marion's own verb is the whole of it.
        expected_allowed_tools: &["mcp__marion__report"],
        expected_wire: "anthropic",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

#[test]
fn a_codex_child_edits_a_worktree_and_reports_through_marions_bridge() {
    assert!(
        on_path("codex"),
        "this cell drives a REAL codex child; put `codex` ({}) on PATH",
        pinned_version("codex")
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
        // §3.1's own worked example: `codex exec` has no allowlist to check a call against, so the
        // sandbox mode is the whole constraint. Named `apply_patch`/`shell` until now, which is
        // the per-tool echo that section forbids by name.
        expected_allowed_tools: &["sandbox:workspace-write"],
        expected_wire: "responses",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

#[test]
fn a_gemini_child_reports_through_marions_bridge_over_the_gemini_wire() {
    assert!(
        on_path("gemini"),
        "this cell drives a REAL gemini child; put `gemini` ({}) on PATH",
        pinned_version("gemini")
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
        // The mode IS the constraint on this harness, and the withholding one is recorded quite as
        // explicitly as the relaxing one: under `default`, 0.53.0 keeps the mutating tools out of
        // `functionDeclarations` entirely.
        expected_allowed_tools: &["approval-mode:default"],
        expected_wire: "gemini",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

#[test]
fn an_opencode_child_reports_through_marions_bridge_over_the_openai_wire() {
    assert!(
        on_path("opencode"),
        "this cell drives a REAL opencode child; put `opencode` ({}) on PATH",
        pinned_version("opencode")
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
        // marion compiles no tool or permission constraint for opencode at all — its generated
        // config carries `model`, `provider` and `mcp` and nothing else — so the record says so
        // rather than claiming one. An empty list here would read as "no tool was allowed", which
        // is the opposite of the truth for a child that can run `bash`.
        expected_allowed_tools: &["harness-default:unconstrained"],
        expected_wire: "openai",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

/// **The fifth binary, on the wire the opencode cell already proved.** copilot 1.0.83's BYOK path
/// speaks OpenAI Chat Completions by default (`COPILOT_PROVIDER_TYPE=openai`,
/// `COPILOT_PROVIDER_WIRE_API=completions`), so the same `Script` fields the opencode cell fills
/// drive it — with the tool under copilot's own `marion-report` spelling, a hyphen, which is the
/// fifth spelling of one tool and the thing (5) below is really asserting.
///
/// What this cell witnesses that `tests/fixtures/s24/` alone cannot: marion's **own** bridge behind
/// `--additional-mcp-config`, started by copilot from the document `CopilotAdapter::config_files`
/// wrote, answering `tools/list` before copilot's first turn and `tools/call` during it — and the
/// persisted contract carrying that call's narrative as the child's own words.
#[test]
fn a_copilot_child_reports_through_marions_bridge_over_the_openai_wire() {
    assert!(
        on_path("copilot"),
        "this cell drives a REAL copilot child; put `copilot` ({}) on PATH",
        pinned_version("copilot")
    );
    let cell = Cell {
        agent_type: "copilot",
        // Explicit: BYOK refuses to start without one (`BYOK providers require an explicit model`,
        // exit 1), and the adapter refuses first. The canned provider ignores the name.
        model: Some("canned-1"),
        script: Script {
            // copilot's own spelling: `<serverName>-<toolName>`.
            openai_report_tool: "marion-report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        expected_harness: Harness::Copilot,
        expected_model: Some("canned-1"),
        // A real allowlist, in copilot's pattern grammar and prefixed with the axis: the literal
        // `--allow-tool=marion(report)`, which is what `-p` mode checks the call against. `copilot`
        // declares no tools, so no `allow-tool:write` joins it.
        expected_allowed_tools: &["allow-tool:marion(report)"],
        expected_wire: "openai",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

/// **The sixth binary, again on the opencode cell's wire.** goose 1.49.0's `openai` provider speaks
/// OpenAI Chat Completions (`OPENAI_HOST` + the default `OPENAI_BASE_PATH`), so the same `Script`
/// fields drive it — with the tool under goose's own `marion__report` spelling, a double underscore
/// between server and tool, which is the sixth spelling of one tool and what (5) asserts here.
///
/// What this cell witnesses that `tests/fixtures/s26/` alone cannot: marion's **own** bridge behind
/// `--with-extension`, started by goose from the one argv token `GooseAdapter` renders, answering
/// `tools/list` before goose's first turn and `tools/call` during it — and the persisted contract
/// carrying that call's narrative as the child's own words. The bridge's environment reaches it by
/// inheritance, not through the extension string: goose persists that string's `ENV=v` pairs in
/// its session store verbatim, so no node token may travel on it (s26 item 11).
#[test]
fn a_goose_child_reports_through_marions_bridge_over_the_openai_wire() {
    assert!(
        on_path("goose"),
        "this cell drives a REAL goose child; put `goose` ({}) on PATH",
        pinned_version("goose")
    );
    let cell = Cell {
        agent_type: "goose",
        // `GOOSE_MODEL` is mandatory: without one the `openai` provider has no model to name and
        // the CLI refuses before any request. The canned provider ignores the name.
        model: Some("canned-1"),
        script: Script {
            // goose's own spelling: `<extension>__<tool>`.
            openai_report_tool: "marion__report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        expected_harness: Harness::Goose,
        expected_model: Some("canned-1"),
        // On goose the built-in extension **is** the constraint: `--no-profile` loads nothing but
        // marion, and a `write` declaration adds `--with-builtin developer`, which is `edit`,
        // `shell`, `write`, `tree` and `read_image` as one unit. `goose` declares no tools, so the
        // record says the default — no builtin at all.
        expected_allowed_tools: &["with-builtin:none"],
        expected_wire: "openai",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}

/// **The seventh binary, on the same wire.** cline 3.0.61's `openai-compatible` provider speaks
/// OpenAI Chat Completions from a `providers.json` the adapter writes into the node's own data
/// dir, so the same `Script` fields drive it — with the tool under cline's `marion__report`
/// spelling, goose's double underscore again.
///
/// What this cell witnesses that `tests/fixtures/s27/` alone cannot: marion's **own** bridge behind
/// `CLINE_MCP_SETTINGS_PATH`, started by cline from the document `ClineAdapter::config_files`
/// writes, answering `tools/list` and `tools/call` — with `--config`/`--data-dir` **and** the three
/// relocation variables together, which is the one combination s27 measured leaving no hub daemon
/// behind and nothing under `~/.cline`; (6)'s survivor sweep is what holds that here.
#[test]
fn a_cline_child_reports_through_marions_bridge_over_the_openai_wire() {
    assert!(
        on_path("cline"),
        "this cell drives a REAL cline child; put `cline` ({}) on PATH",
        pinned_version("cline")
    );
    let cell = Cell {
        agent_type: "cline",
        // The model is a field of `providers.json`, and the adapter refuses a canned launch
        // without one rather than writing a document that names none.
        model: Some("canned-1"),
        script: Script {
            // cline's own spelling: `<serverName>__<toolName>`.
            openai_report_tool: "marion__report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        expected_harness: Harness::Cline,
        expected_model: Some("canned-1"),
        // marion compiles no tool or permission constraint for cline at all — s27 measured
        // `disabledTools` and `tools.*.enabled` in `global-settings.json` changing nothing on the
        // wire and no flag narrowing the 26 built-ins — so the record says so, as opencode's does.
        expected_allowed_tools: &["harness-default:unconstrained"],
        expected_wire: "openai",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}
/// **The eighth binary, on the same wire.** qwen 0.23.0's OpenAI provider is env-only
/// (`OPENAI_BASE_URL`/`OPENAI_API_KEY`/`OPENAI_MODEL`), so the same `Script` fields drive it — with
/// the tool under `mcp__marion__report`, Claude Code's spelling, because qwen's headless surface is
/// Claude Code's shape and not its Gemini CLI ancestor's (s25 item 2).
///
/// What this cell witnesses that `tests/fixtures/s25/` alone cannot: marion's **own** bridge behind
/// the `settings.json` `QwenAdapter::config_files` writes under the relocated `QWEN_HOME`, connected
/// **before** turn one under `QWEN_CODE_LEGACY_MCP_BLOCKING=1` — without which the tool is deferred
/// behind `tool_search` and a canned model that never calls that never reaches it.
#[test]
fn a_qwen_child_reports_through_marions_bridge_over_the_openai_wire() {
    assert!(
        on_path("qwen"),
        "this cell drives a REAL qwen child; put `qwen` ({}) on PATH",
        pinned_version("qwen")
    );
    let cell = Cell {
        agent_type: "qwen",
        // `OPENAI_MODEL` is how the provider is told what to name; the adapter refuses a canned
        // launch without one. The canned provider ignores the name.
        model: Some("canned-1"),
        script: Script {
            openai_report_tool: "mcp__marion__report".into(),
            openai_report_args: json!({ "narrative": NARRATIVE }),
            ..Script::default()
        },
        expected_harness: Harness::Qwen,
        expected_model: Some("canned-1"),
        // A real allowlist: `--core-tools` is what the model is offered (s25 item 5), and the
        // record is its literal contents prefixed with the axis. `qwen` declares no tools, so
        // marion's own verb is the whole list.
        expected_allowed_tools: &["core-tools:mcp__marion__report"],
        expected_wire: "openai",
    };
    let ev = drive(&cell);
    assert_cell(&cell, &ev);
}
