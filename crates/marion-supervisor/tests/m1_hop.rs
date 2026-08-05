//! M1's acceptance criteria, run end to end against real binaries (design §9).
//!
//! One run, four criteria:
//!
//! 1. a real `claude` root (headless) calls `mcp__marion__spawn` for a `codex` agent type;
//! 2. a real `codex` child starts in a worktree, edits a file, and returns through marion's
//!    `report` tool;
//! 3. the parent receives the structured task contract as a tool result — **asserted on the
//!    request side**, against what the provider recorded, never on the reply marion sent;
//! 4. the scope is enforced **detectively**: a deliberate out-of-scope write appears in
//!    `changed_paths`, is listed in `scope_violations`, and `scope_enforced` is `true`;
//! 5. the whole run is driven by the CannedProvider — no paid tokens, repeatable.
//!
//! **What is deliberately not asserted.** §9: *"Asserting that the root's next turn 'references
//! the child's output' would be vacuous — that text is scripted SSE, fixed before the run, and
//! would pass against a marion that dropped the contract entirely."*
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test m1_hop
//! ```
//!
//! It needs real `claude` (2.1.220) and `codex` (0.146.0) on `PATH`. It is **not** `#[ignore]`d
//! and it does **not** skip when they are missing: it fails, naming the binary. A criterion that
//! quietly passes on a machine that cannot run it is worth less than no criterion, and this
//! crate's existing process tests take the same line about `perl`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use marion_core::contract::TaskContract;
use marion_provider::script::ROOT_TOOL_USE_ID;
use marion_provider::{CannedServer, Config, Script};
use marion_supervisor::run::run_bounded;
use marion_testsupport::{fixture_repo, judge, on_path, persisted_contracts, scratch, survivors};
use serde_json::Value;

/// Generous: the bound exists so a hung harness fails loudly instead of wedging the suite, not to
/// measure anything. The measured run is a couple of seconds.
const RUN_BOUND: Duration = Duration::from_secs(300);

/// The child's deliberate out-of-scope write. Outside `src/**`, which is the `writable_scope` the
/// root asks for, and inside `**`, which is the agent type's ceiling — so it is refused by the
/// request and not by the ceiling, which is the case §9's criterion is about.
const OUT_OF_SCOPE: &str = "out_of_scope/marion_m1.txt";
const IN_SCOPE: &str = "src/marion_m1.txt";

/// The text of the `tool_result` the root sent back for `tool_use_id`, from a recorded request.
fn tool_result_text(request: &Value, tool_use_id: &str) -> Option<String> {
    for message in request.pointer("/body/messages")?.as_array()? {
        for block in message.get("content")?.as_array()? {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            if block.get("tool_use_id").and_then(Value::as_str) != Some(tool_use_id) {
                continue;
            }
            return match block.get("content") {
                // Claude Code 2.1.220 sends an MCP tool result as a block list.
                Some(Value::Array(blocks)) => Some(
                    blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            };
        }
    }
    None
}

fn calls_spawn(request: &Value) -> bool {
    request
        .pointer("/body/messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("content").and_then(Value::as_array))
        .flatten()
        .any(|b| {
            b.get("type").and_then(Value::as_str) == Some("tool_use")
                && b.get("name").and_then(Value::as_str) == Some("mcp__marion__spawn")
        })
}

fn offers_spawn(request: &Value) -> bool {
    request
        .pointer("/body/tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|t| t.get("name").and_then(Value::as_str) == Some("mcp__marion__spawn"))
}

/// Drop everything §6.7's cap rules 0–6 may shorten, **and the metadata that records the
/// shortening**, from a contract's JSON.
///
/// §9 is explicit that the comparison normalizes both: *"every `Capped.truncated`/`original_bytes`
/// pair and every `*_omitted` counter may differ between the two copies […] The comparison
/// normalizes both the shortened fields and their metadata; it is not an equality over the
/// metadata."* Everything left — the ids, the repo, the workspace, the scope lists, the status,
/// the flags, the exit, the timestamps — must match byte for byte.
fn normalize_for_cap_rules(mut v: Value) -> Value {
    // Rule 5(e).
    v["instructions"] = Value::Null;
    v["acceptance_criteria"] = Value::Null;
    if let Some(c) = v.get_mut("completion").and_then(Value::as_object_mut) {
        for key in [
            // Rules 0, 2/3, 1, 5(a)-(d).
            "narrative",
            "diff",
            "evidence",
            "changed_paths",
            "scope_violations",
            // The metadata recording the shortening.
            "evidence_omitted",
            "changed_paths_omitted",
            "scope_violations_omitted",
            "acceptance_criteria_omitted",
        ] {
            c.insert(key.to_string(), Value::Null);
        }
    }
    v
}

/// Nothing in the *persisted* copy was ever capped, so every flag reads false and every counter 0.
fn assert_persisted_copy_records_no_shortening(c: &TaskContract) {
    assert!(!c.instructions.truncated, "instructions");
    for cr in &c.acceptance_criteria {
        assert!(!cr.truncated, "acceptance_criteria");
    }
    let comp = c
        .completion
        .as_ref()
        .expect("a finished run has a completion");
    if let Some(n) = &comp.narrative {
        assert!(!n.truncated, "narrative");
    }
    if let Some(d) = &comp.diff {
        assert!(!d.truncated, "diff");
    }
    for e in &comp.evidence {
        assert!(!e.stdout.truncated && !e.stderr.truncated, "evidence");
    }
    assert_eq!(
        (
            comp.evidence_omitted,
            comp.changed_paths_omitted,
            comp.scope_violations_omitted,
            comp.acceptance_criteria_omitted
        ),
        (0, 0, 0, 0),
        "the persisted contract is the complete one; nothing there was ever dropped"
    );
}

#[test]
fn a_real_claude_root_spawns_a_real_codex_child_and_receives_its_contract_as_a_tool_result() {
    assert!(
        on_path("claude"),
        "M1's first acceptance criterion is about a REAL claude root; put `claude` (2.1.220) on PATH"
    );
    assert!(
        on_path("codex"),
        "M1's second acceptance criterion is about a REAL codex child; put `codex` (0.146.0) on PATH"
    );

    let root_dir = scratch("m1-hop");
    let repo = fixture_repo(&root_dir);
    let state = root_dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    // The `Script` is public precisely so a test can aim the child's patch somewhere the run's
    // `writable_scope` does not cover. Two files in one patch, so `scope_violations` is a strict
    // subset of `changed_paths` rather than equal to it — an implementation that simply copied one
    // list into the other would pass a single-file version of this test.
    let script = Script {
        child_patch: format!(
            "*** Begin Patch\n\
             *** Add File: {IN_SCOPE}\n\
             +marion M1: written inside the requested scope\n\
             *** Add File: {OUT_OF_SCOPE}\n\
             +marion M1: written OUTSIDE the requested scope, on purpose\n\
             *** End Patch"
        ),
        ..Script::default()
    };

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: root_dir.join("provider-requests.jsonl"),
        script,
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
                // The canned provider is the whole model side of this hop, and saying so is what
                // lets the binary accept a loopback endpoint: under real vendor auth it refuses
                // one rather than aim the operator's credential at a fake server.
                "--canned",
                "--base-url",
                &server.base_url(),
                // The root's per-episode `Blocked` bound. Short, so a permission request that
                // cannot be answered fails this test in seconds instead of stalling it for 900.
                "--timeout",
                "5",
            ])
            .current_dir(&root_dir),
        RUN_BOUND,
    )
    .expect("marion run starts");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.timed_out,
        "marion run did not finish inside {RUN_BOUND:?}\nstderr:\n{stderr}"
    );
    assert_eq!(
        out.code,
        Some(0),
        "marion run exited {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.code
    );

    // ---- criterion 5: every model call went to the canned server, and both wires arrived. -----
    let requests = server.requests().expect("the request log is readable");
    let wires: Vec<&str> = requests
        .iter()
        .filter_map(|r| r["wire"].as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(
        wires,
        vec!["anthropic", "responses"],
        "both harnesses must have been served by the canned provider: {wires:?}"
    );
    assert!(
        !requests
            .iter()
            .any(|r| r["headers"]["x-api-key"].is_string()
                && r["headers"]["x-api-key"] != "<redacted>"),
        "a run that reached the canned server with a verbatim api key is not the repeatable, \
         zero-cost run this criterion is about"
    );

    // ---- criterion 1: the root really called mcp__marion__spawn. -------------------------------
    let spawn_turn = requests.iter().find(|r| calls_spawn(r)).unwrap_or_else(|| {
        panic!("no recorded request carries the root's mcp__marion__spawn call")
    });
    assert!(
        requests.iter().any(offers_spawn),
        "no request even offered mcp__marion__spawn: the root took its turn before the harness \
         had connected marion's MCP server, so the call could not have been made"
    );

    // ---- criterion 3: the contract came back as the tool result, asserted on the request. -----
    let result_text = tool_result_text(spawn_turn, ROOT_TOOL_USE_ID)
        .expect("the request that follows the spawn call carries its tool_result");
    assert!(
        !result_text.contains("<persisted-output>"),
        "the tool result was replaced by a stub, so the contract never reached the model. \
         Keeping it under that threshold is what §6.7's cap rules exist to guarantee.\n{result_text}"
    );
    let returned: TaskContract = serde_json::from_str(&result_text).unwrap_or_else(|e| {
        panic!("the tool result does not deserialize to a TaskContract: {e}\n{result_text}")
    });

    let walked = persisted_contracts(&state).expect("the state tree enumerates");
    let contracts = judge(&walked);
    assert_eq!(
        contracts.len(),
        1,
        "exactly one child ran, so exactly one contract is persisted: {contracts:?}"
    );
    let persisted_json: Value = contracts[0].1.clone();
    let persisted: TaskContract = serde_json::from_value(persisted_json.clone()).unwrap();
    assert_persisted_copy_records_no_shortening(&persisted);
    assert_eq!(
        normalize_for_cap_rules(serde_json::to_value(&returned).unwrap()),
        normalize_for_cap_rules(persisted_json),
        "the tool result must be the persisted contract, modulo what the cap rules may shorten"
    );
    // Nothing in this run is anywhere near a cap, so the shortenable fields survived intact too.
    let returned_comp = returned.completion.as_ref().unwrap();
    let persisted_comp = persisted.completion.as_ref().unwrap();
    assert_eq!(
        returned_comp.narrative.as_ref().map(|n| &n.value),
        persisted_comp.narrative.as_ref().map(|n| &n.value)
    );

    // ---- criterion 1, continued: the requester is the root's own AgentId. ----------------------
    let root_agent_dir = state
        .join(
            contracts[0]
                .0
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .and_then(|p| p.strip_prefix(&state).ok())
                .expect("agents dir sits under the project hash"),
        )
        .join(&persisted.requester.0);
    assert!(
        root_agent_dir.is_dir(),
        "TaskContract.requester must name the root node's own agent-dir (§9), got {:?} — {} \
         does not exist",
        persisted.requester,
        root_agent_dir.display()
    );
    assert_ne!(
        persisted.requester.0, "unattributed-root",
        "a hand-started bridge's placeholder means marion did not launch the root"
    );

    // ---- criterion 4: detective scope enforcement. ---------------------------------------------
    assert!(
        persisted_comp.scope_enforced,
        "false would mean the check never ran, which is not the same as passing"
    );
    assert!(
        persisted_comp
            .changed_paths
            .iter()
            .any(|p| p == Path::new(OUT_OF_SCOPE)),
        "the out-of-scope write must appear in changed_paths: {:?}",
        persisted_comp.changed_paths
    );
    assert!(
        persisted_comp
            .changed_paths
            .iter()
            .any(|p| p == Path::new(IN_SCOPE)),
        "the in-scope write must appear too: {:?}",
        persisted_comp.changed_paths
    );
    assert_eq!(
        persisted_comp.scope_violations,
        vec![PathBuf::from(OUT_OF_SCOPE)],
        "exactly the out-of-scope path is a violation — detective, not preventive: the write \
         happened and was reported"
    );
    assert_eq!(
        persisted.scope_requested,
        vec![marion_core::contract::Glob("src/**".into())]
    );

    // ---- criterion 2, re-asserted: the child returned through marion's `report` tool. ----------
    assert_eq!(
        persisted_comp.status,
        marion_core::contract::ExitStatus::Ok,
        "an Unreported status would mean no report arrived through the MCP channel"
    );
    assert!(
        persisted_comp
            .narrative
            .as_ref()
            .is_some_and(|n| !n.value.is_empty()),
        "the narrative is the child's, sourced from its report"
    );
    assert!(!persisted_comp.narrative_synthesized);

    // ---- nothing outlived the run. -------------------------------------------------------------
    drop(server);
    let leaked = survivors(&root_dir.to_string_lossy());
    assert!(
        leaked.is_empty(),
        "processes from this run are still alive — the S7 class of failure:\n{}",
        leaked
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    // No `remove_dir_all` here: `root_dir`'s own `Drop` owns that, and owns it on the failing runs
    // too. The leak check above still has to come first, because it names the dir it searches for.
}
