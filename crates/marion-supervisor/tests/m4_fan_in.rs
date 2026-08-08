//! **M4's acceptance criterion, run end to end against real binaries** (design §9):
//!
//! > *A real `codex` root spawns **two** `claude` children concurrently, both report, and the root
//! > receives both contracts. Proves marion is not Claude-centric (the harnesses are swapped
//! > relative to M1, with no adapter-specific orchestration code) and that fan-in aggregates rather
//! > than serializing.*
//!
//! One run, six machine-checkable clauses. The seventh — *no adapter-specific orchestration code* —
//! is a claim about marion's source and is not something a green test can establish; it is recorded
//! in `MILESTONES.md` against the fan-in path it was read on.
//!
//! # How "concurrently" is proved, and why no clock appears below
//!
//! Wall-clock overlap is not asserted, and no duration is compared against any threshold. Two
//! independent facts stand in for it, and a serial execution satisfies neither:
//!
//! 1. **A rendezvous inside the canned provider.** Neither child is answered its first turn until
//!    *both* children have asked one ([`marion_provider::Rendezvous`]). So when child A is
//!    answered, child B's process already existed and had already spoken. A marion that ran the
//!    children one after the other never gets a second party to the rendezvous: the first child
//!    waits alone, the bound expires, and `expired()` says so **as a reading rather than as a
//!    hang** — which is why the bound exists and why nothing here can fail only by timing out.
//! 2. **Interval overlap in the journal, written by marion and read afterwards.** The journal's
//!    total order is its byte order (`marion_core::journal::WriterId`: *"the journal's total order
//!    is the **file's byte order** — the order the kernel serialized the appends in"*), so the test
//!    reads line indices, never timestamps. Each child's `SpawnIntent` precedes the *other* child's
//!    `Exited`. Allen's overlap: both were live at one instant. Serial execution puts one `Exited`
//!    before the other's `SpawnIntent` and fails it.
//!
//! Fact 1 is the test's own instrument — it *causes* the overlap. Fact 2 is marion's own account of
//! what happened, written by the code under test, and is what the clause is graded on.
//!
//! # Running it
//!
//! ```sh
//! cargo test -p marion-supervisor --test m4_fan_in
//! ```
//!
//! It needs real `codex` and `claude` on `PATH` at the versions
//! [`marion_testsupport::PINNED_HARNESSES`] lists, and it does **not** skip when they are missing:
//! it fails naming the binary, on `m1_hop`'s reasoning.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use marion_core::contract::{ExitStatus, TaskContract};
use marion_provider::{CannedServer, Config, Hold, NodeScript, Rendezvous, Script, ScriptedCall};
use marion_supervisor::run::run_bounded;
use marion_testsupport::{
    fixture_repo, judge, on_path, persisted_contracts, pinned_version, scratch, survivors,
};
use serde_json::{Value, json};

/// Generous. The bound exists so a hung harness fails loudly instead of wedging the suite; nothing
/// is measured against it.
const RUN_BOUND: Duration = Duration::from_secs(300);

/// How long a child sits at the rendezvous before giving up on its sibling.
///
/// **Not a threshold on anything asserted.** The only readings taken from the rendezvous are *met*
/// and *expired*; how long it took is never looked at. It is here so that a marion which serialises
/// the children fails on [`Rendezvous::expired`] with a sentence, in a minute, instead of parking a
/// child until [`RUN_BOUND`].
const RENDEZVOUS_BOUND: Duration = Duration::from_secs(60);

/// The children's own `timeout_secs`. Comfortably past [`RENDEZVOUS_BOUND`], so a child that is
/// waiting for its sibling is never killed by its own bound first — that would turn one failure
/// (no concurrency) into a different and less informative one (a timed-out child).
const CHILD_TIMEOUT_SECS: u64 = 240;

/// Markers, one per node. Every harness replays its node's task text on every turn, which is what
/// makes these a *shape* discriminator rather than an ordering assumption — see
/// [`marion_provider::NodeScript`].
const ROOT_MARKER: &str = "MARION-M4-ROOT-0b7c";
const A_MARKER: &str = "MARION-M4-CHILD-A-0b7c";
const B_MARKER: &str = "MARION-M4-CHILD-B-0b7c";

/// Each child's narrative, and its file. **Distinct per child on purpose**: identical children
/// would leave the contracts interchangeable, and the assertion that each report reached its own
/// contract would pass against a marion that attributed both reports to one node.
const A_NARRATIVE: &str = "child A wrote src/m4_a.txt and is reporting for A";
const B_NARRATIVE: &str = "child B wrote src/m4_b.txt and is reporting for B";
const A_FILE: &str = "src/m4_a.txt";
const B_FILE: &str = "src/m4_b.txt";

const ROOT_PREFIX: &str = "call_m4_root";
const A_PREFIX: &str = "call_m4_a";
const B_PREFIX: &str = "call_m4_b";

// ---- the scripts ------------------------------------------------------------------------------

/// A `claude` child: write one file into its own worktree, then return through marion's `report`.
///
/// The write is not decoration. §6.7 gives `changed_paths` exactly one source — a git diff of the
/// child's worktree — so a child that only reports leaves that set empty, and two such children
/// would be indistinguishable in every field a contract carries about work done.
fn child(marker: &str, prefix: &str, file: &str, narrative: &str) -> NodeScript {
    NodeScript {
        marker: marker.into(),
        call_prefix: prefix.into(),
        turns: vec![
            ScriptedCall::new(
                "Write",
                json!({"file_path": file, "content": format!("{marker}\n")}),
            ),
            ScriptedCall::new("mcp__marion__report", json!({"narrative": narrative})),
        ],
        final_text: format!("{marker}: reported through marion. Done."),
    }
}

/// The `codex` root: two backgrounded spawns, then two `wait`s.
///
/// **The order is the whole criterion.** Both `spawn`s are issued before either `wait`, which is
/// only expressible because `background: true` answers with a handle rather than a contract; a
/// `spawn` that blocked until its child finished would make the second child start after the first
/// had ended. The `wait` arguments are placeholders because the id they need is minted by marion at
/// spawn time — a real model reads it out of the handle it was just handed, and
/// [`marion_provider::HANDLE_KEY`] reads it out of the same place.
fn root() -> NodeScript {
    let spawn = |marker: &str, file: &str| {
        ScriptedCall::new(
            "spawn",
            json!({
                "agent_type": "claude-impl",
                "prompt": format!("{marker}: write {file} and report back through marion."),
                "acceptance_criteria": [format!("{file} exists and contains {marker}")],
                "writable_scope": ["src/**"],
                "timeout_secs": CHILD_TIMEOUT_SECS,
                "model": "haiku",
                "background": true,
            }),
        )
    };
    NodeScript {
        marker: ROOT_MARKER.into(),
        call_prefix: ROOT_PREFIX.into(),
        turns: vec![
            spawn(A_MARKER, A_FILE),
            spawn(B_MARKER, B_FILE),
            ScriptedCall::new("wait", json!({"task_id": {marion_provider::HANDLE_KEY: 0}})),
            ScriptedCall::new("wait", json!({"task_id": {marion_provider::HANDLE_KEY: 1}})),
        ],
        final_text: "Both children reported. Fan-in complete.".into(),
    }
}

/// Every version of `program` this suite admits — the pin and everything re-measured green since.
///
/// Read out of [`marion_testsupport::PINNED_HARNESSES`] rather than restated, so a version admitted
/// there is admitted here and nowhere is there a second table to keep in step.
fn accepted(program: &str) -> &'static [&'static str] {
    marion_testsupport::PINNED_HARNESSES
        .iter()
        .find(|p| p.program == program)
        .unwrap_or_else(|| panic!("{program:?} is not pinned; see PINNED_HARNESSES"))
        .accepted
}

// ---- reading the evidence back ------------------------------------------------------------------

/// The text codex recorded as the result of the call it made under `call_id`.
///
/// **Asserted on the request side**, as §9 requires: this is what the *harness* sent back up to the
/// model on its next turn, not what marion says it replied. codex wraps an MCP result as
/// `"Wall time: … seconds\nOutput:\n<block list>"`; the wrapper is peeled here rather than matched
/// on, so a change in its shape fails naming the string that was found.
fn codex_tool_output(request: &Value, call_id: &str) -> Option<String> {
    const MARKER: &str = "Output:\n";
    let raw = request
        .pointer("/body/input")?
        .as_array()?
        .iter()
        .find(|it| {
            it.get("type").and_then(Value::as_str) == Some("function_call_output")
                && it.get("call_id").and_then(Value::as_str) == Some(call_id)
        })?
        .get("output")?
        .as_str()?;
    let at = raw.find(MARKER).unwrap_or_else(|| {
        panic!("codex's tool-output wrapper no longer carries {MARKER:?}:\n{raw}")
    });
    let blocks: Value = serde_json::from_str(&raw[at + MARKER.len()..])
        .unwrap_or_else(|e| panic!("the block list after {MARKER:?} is not JSON: {e}\n{raw}"));
    Some(
        blocks
            .as_array()?
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
    )
}

/// Every `journal.jsonl` under `state`, as lines, in file order.
///
/// The project hash naming the directory is derived inside marion, so the path is discovered rather
/// than reconstructed: a test that recomputed the hash would assert against its own copy of the
/// derivation. One project means one file, which the caller asserts.
fn journal_lines(state: &Path) -> Vec<Value> {
    fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.file_name().is_some_and(|n| n == "journal.jsonl") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(state, &mut files);
    assert_eq!(
        files.len(),
        1,
        "one project, so one journal — {files:?} is a different run's evidence mixed in"
    );
    std::fs::read_to_string(&files[0])
        .unwrap_or_else(|e| panic!("{} does not read back: {e}", files[0].display()))
        .lines()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("journal line is not JSON: {e}\n{l}"))
        })
        .collect()
}

/// The index, in the journal's byte order, of the one record of kind `kind` about `agent_id`.
///
/// Exactly one, not the first: two `Exited` records about one node would mean the terminal
/// transition was recorded twice, and taking the earlier would hide it.
fn only_index(records: &[Value], kind: &str, agent_id: &str) -> usize {
    let hits: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.pointer(&format!("/kind/{kind}/agent_id"))
                .and_then(Value::as_str)
                == Some(agent_id)
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one {kind} about {agent_id}, found {hits:?}"
    );
    hits[0]
}

/// §6.7's cap rules 0–6 may shorten these, and the metadata recording the shortening may differ
/// between the returned and persisted copies. Normalized on both sides, exactly as `m1_hop` does
/// and for the reason §9 states.
fn normalize_for_cap_rules(mut v: Value) -> Value {
    v["instructions"] = Value::Null;
    v["acceptance_criteria"] = Value::Null;
    if let Some(c) = v.get_mut("completion").and_then(Value::as_object_mut) {
        for key in [
            "narrative",
            "diff",
            "evidence",
            "changed_paths",
            "scope_violations",
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

// ---- the criterion ------------------------------------------------------------------------------

#[test]
fn a_real_codex_root_runs_two_real_claude_children_concurrently_and_receives_both_contracts() {
    assert!(
        on_path("codex"),
        "M4's root is a REAL codex; put `codex` ({}) on PATH",
        pinned_version("codex")
    );
    assert!(
        on_path("claude"),
        "M4's children are REAL claudes; put `claude` ({}) on PATH",
        pinned_version("claude")
    );

    let dir = scratch("m4-fan-in");
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let script = Script {
        // The root first: its transcript carries its children's task text inside the `spawn`
        // arguments it sent, so the entries are ordered rather than disjoint (`NodeScript`).
        nodes: vec![
            root(),
            child(A_MARKER, A_PREFIX, A_FILE, A_NARRATIVE),
            child(B_MARKER, B_PREFIX, B_FILE, B_NARRATIVE),
        ],
        ..Script::default()
    };

    // Both children speak the anthropic wire, so the hold is on identity and not on a count — see
    // this file's header, and `Rendezvous`'s own doc for why counting cannot do this job.
    let rendezvous = Rendezvous::on_wire("anthropic", [A_MARKER, B_MARKER], RENDEZVOUS_BOUND);
    let server = CannedServer::start_held(
        Config {
            addr: ([127, 0, 0, 1], 0).into(),
            reqlog: dir.join("provider-requests.jsonl"),
            script,
        },
        Some(Arc::clone(&rendezvous) as Arc<dyn Hold>),
    )
    .expect("the canned provider binds");

    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([
                "run",
                "codex-impl",
                "--prompt",
                &format!("{ROOT_MARKER}: delegate two marker-file tasks and collect both."),
                "--repo",
                &repo.to_string_lossy(),
                "--state-dir",
                &state.to_string_lossy(),
                "--model",
                "gpt-5.6-sol",
                "--canned",
                "--base-url",
                &server.base_url(),
                "--timeout",
                "150",
            ])
            .current_dir(&dir),
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

    let requests = server.requests().expect("the request log is readable");
    let records = journal_lines(&state);

    // ---- clause 1: the root is a real codex, and it is the node marion launched. ---------------
    let root_intent = records
        .iter()
        .find_map(|r| r.pointer("/kind/SpawnIntent"))
        .filter(|i| i.get("parent_id").is_none_or(Value::is_null))
        .unwrap_or_else(|| panic!("no parentless SpawnIntent: marion never launched a root"));
    let root_id = root_intent["agent_id"].as_str().unwrap().to_string();
    assert_eq!(root_intent["harness"], json!("codex"), "{root_intent}");
    assert_eq!(root_intent["depth"], json!(0));
    assert!(
        root_intent.get("task_id").is_none_or(Value::is_null),
        "§9: a root has no contract, so its intent carries no task id"
    );
    let root_spawned =
        records[only_index(&records, "Spawned", &root_id)]["kind"]["Spawned"].clone();
    assert!(
        root_spawned["pid"].as_i64().is_some_and(|p| p > 0),
        "a root with no recorded pid is not a process this criterion can call real: {root_spawned}"
    );
    // **The root's `Spawned` records `harness_version: "unknown"`, deliberately** (`root.rs`: a
    // root has no contract, so nothing on that path ever runs `<program> --version`, and doing it
    // to fill a journal field would add a process execution to every `marion run`). So the version
    // gate for the root is `on_path("codex")` above, and *that a real codex spoke* is asserted from
    // the wire instead: S6 measured code mode's `{name, namespace}` dispatch form, which is codex's
    // own internal spelling and which no other harness in this workspace emits.
    let codex_spoke = requests.iter().any(|r| {
        r["wire"] == json!("responses")
            && r.pointer("/body/input")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.iter().any(|it| {
                        it.get("namespace").and_then(Value::as_str) == Some("mcp__marion")
                    })
                })
    });
    assert!(
        codex_spoke,
        "no recorded request carries codex's code-mode `namespace` dispatch form, so the root \
         that called marion was not a real codex"
    );

    // ---- clause 2: two real claude children, of that root. -------------------------------------
    let mut children: Vec<(String, String)> = records
        .iter()
        .filter_map(|r| r.pointer("/kind/SpawnIntent"))
        .filter(|i| i["parent_id"].as_str() == Some(root_id.as_str()))
        .map(|i| {
            assert_eq!(i["harness"], json!("claude-code"), "{i}");
            assert_eq!(i["agent_type"], json!("claude-impl"), "{i}");
            assert_eq!(i["depth"], json!(1), "{i}");
            (
                i["agent_id"].as_str().unwrap().to_string(),
                i["task_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        children.len(),
        2,
        "M4 is two children under one root, and the journal records {}",
        children.len()
    );
    // A child's version **is** measured — `run_spawn` runs `<program> --version` for §6.7's
    // `TaskContract.child.version` — so this is marion's own reading of the binary it launched,
    // checked against the same table `on_path` gates on rather than against a second list.
    for (agent_id, _) in &children {
        let spawned = &records[only_index(&records, "Spawned", agent_id)]["kind"]["Spawned"];
        let version = spawned["harness_version"].as_str().unwrap_or_default();
        assert!(
            accepted("claude").iter().any(|v| version.starts_with(v)),
            "child {agent_id} reported {version:?}, which is not one of the claude versions this \
             suite admits ({:?})",
            accepted("claude")
        );
    }

    // ---- clause 3: they overlapped in time. ----------------------------------------------------
    //
    // First the instrument's own reading, which says *why* they overlapped, then marion's account,
    // which is what the clause is graded on.
    assert!(
        !rendezvous.expired(),
        "a child sat at the rendezvous for the whole bound while its sibling never arrived — \
         which is exactly the shape a marion that runs its children one after the other \
         produces. Arrived: {:?}",
        rendezvous.arrived()
    );
    assert!(
        rendezvous.met(),
        "both children must have asked the provider for a turn: {:?}",
        rendezvous.arrived()
    );
    children.sort();
    let (a, b) = (&children[0].0, &children[1].0);
    let (a_start, a_end) = (
        only_index(&records, "SpawnIntent", a),
        only_index(&records, "Exited", a),
    );
    let (b_start, b_end) = (
        only_index(&records, "SpawnIntent", b),
        only_index(&records, "Exited", b),
    );
    assert!(
        a_start < b_end && b_start < a_end,
        "the two children's lives do not overlap in the journal's own order, so they ran one \
         after the other: {a} spans lines {a_start}..={a_end}, {b} spans {b_start}..={b_end}"
    );

    // ---- clause 4: both reported, and each report reached its own contract. --------------------
    let walked = persisted_contracts(&state).expect("the state tree enumerates");
    let contracts = judge(&walked);
    assert_eq!(
        contracts.len(),
        2,
        "two children ran, so two contracts are persisted: {contracts:?}"
    );
    let persisted: Vec<TaskContract> = contracts
        .iter()
        .map(|(_, v)| serde_json::from_value((*v).clone()).expect("a persisted contract parses"))
        .collect();
    let mut narratives: Vec<&str> = persisted
        .iter()
        .map(|c| {
            let comp = c
                .completion
                .as_ref()
                .expect("a finished run has a completion");
            assert_eq!(
                comp.status,
                ExitStatus::Ok,
                "an Unreported status means no report arrived through the MCP channel"
            );
            assert!(
                !comp.narrative_synthesized,
                "the narrative must be the child's own, sourced from its `report`"
            );
            assert_eq!(c.requester.0, root_id, "both contracts name the codex root");
            comp.narrative
                .as_ref()
                .map(|n| n.value.as_str())
                .unwrap_or("")
        })
        .collect();
    narratives.sort();
    assert_eq!(
        narratives,
        vec![A_NARRATIVE, B_NARRATIVE],
        "each child's own report must land in its own contract — two copies of one narrative is \
         a report attributed to the wrong node, and no other assertion here would see it"
    );
    // The same discrimination on the work each child did, which the narrative alone cannot give:
    // `changed_paths` comes from a git diff of that child's worktree and nothing else.
    let mut changed: Vec<String> = persisted
        .iter()
        .flat_map(|c| c.completion.as_ref().unwrap().changed_paths.iter())
        .map(|p| p.display().to_string())
        .collect();
    changed.sort();
    assert_eq!(changed, vec![A_FILE, B_FILE]);
    // **And the two must agree inside one contract.** The two assertions above are each over a
    // *set*, and a marion that handed each child's report to the other satisfies both of them:
    // both narratives are present once, both paths are present once, and only the pairing is
    // wrong. That was run as a mutation — each child's `report` written into its sibling's
    // contract — and the test passed. `changed_paths` is the one field no report can influence
    // (§6.7 sources it from a git diff of that child's own worktree), so it is what says which
    // node a contract is about, and the narrative is checked against it rather than beside it.
    let mut paired: Vec<(String, String)> = persisted
        .iter()
        .map(|c| {
            let comp = c.completion.as_ref().unwrap();
            let paths: Vec<String> = comp
                .changed_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            assert_eq!(
                paths.len(),
                1,
                "each child writes exactly one file, so its contract names one changed path"
            );
            (
                paths[0].clone(),
                comp.narrative
                    .as_ref()
                    .map(|n| n.value.to_string())
                    .unwrap_or_default(),
            )
        })
        .collect();
    paired.sort();
    assert_eq!(
        paired,
        vec![
            (A_FILE.to_string(), A_NARRATIVE.to_string()),
            (B_FILE.to_string(), B_NARRATIVE.to_string()),
        ],
        "a contract's narrative must be the report of the child whose worktree that diff came \
         from — crossed pairs are two reports that reached the wrong nodes"
    );

    // ---- clause 5: the root received both contracts, asserted on the request side. -------------
    let last = requests
        .iter()
        .rfind(|r| {
            r["wire"] == json!("responses")
                && codex_tool_output(r, &format!("{ROOT_PREFIX}_03")).is_some()
        })
        .unwrap_or_else(|| {
            panic!("no recorded root request carries the result of the second `wait`")
        });
    for i in [2usize, 3] {
        let text = codex_tool_output(last, &format!("{ROOT_PREFIX}_{i:02}"))
            .unwrap_or_else(|| panic!("turn {i}'s tool output is missing from the root's request"));
        assert!(
            !text.contains("<persisted-output>"),
            "the tool result was replaced by a stub, so the contract never reached the model:\n{text}"
        );
        let returned: TaskContract = serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!("turn {i}'s tool result does not deserialize to a TaskContract: {e}\n{text}")
        });
        let persisted_json = contracts
            .iter()
            .map(|(_, v)| (*v).clone())
            .find(|v| v["task_id"] == json!(returned.task_id.0))
            .unwrap_or_else(|| {
                panic!(
                    "the root was handed a contract for {:?}, which is not on disk",
                    returned.task_id
                )
            });
        assert_eq!(
            normalize_for_cap_rules(serde_json::to_value(&returned).unwrap()),
            normalize_for_cap_rules(persisted_json),
            "the tool result must be the persisted contract, modulo what the cap rules shorten"
        );
    }
    // Two *different* contracts, not one delivered twice.
    let delivered: Vec<String> = [2usize, 3]
        .iter()
        .map(|i| {
            let text = codex_tool_output(last, &format!("{ROOT_PREFIX}_{i:02}")).unwrap();
            serde_json::from_str::<TaskContract>(&text)
                .unwrap()
                .task_id
                .0
        })
        .collect();
    assert_ne!(
        delivered[0], delivered[1],
        "both `wait`s resolved to the same child, so one child's contract never reached the root"
    );
    let mut delivered_sorted = delivered.clone();
    delivered_sorted.sort();
    let mut spawned_tasks: Vec<String> = children.iter().map(|(_, t)| t.clone()).collect();
    spawned_tasks.sort();
    assert_eq!(
        delivered_sorted, spawned_tasks,
        "the contracts the root received must be the ones its two children ran under"
    );

    // ---- clause 7: fan-in aggregates rather than serializing. ----------------------------------
    //
    // Distinct from clause 3. Both `spawn`s answered with a **handle** and not a result, which is
    // what let the second child start before the first was collected; a `spawn` that returned a
    // contract would have been the serialization this clause forbids, and would still have
    // produced the same four turns.
    for i in [0usize, 1] {
        let text = codex_tool_output(last, &format!("{ROOT_PREFIX}_{i:02}"))
            .unwrap_or_else(|| panic!("spawn turn {i}'s tool output is missing"));
        assert!(
            text.contains("this is a handle, not a result"),
            "spawn turn {i} answered with something other than a handle, so the root's next turn \
             was gated on that child finishing:\n{text}"
        );
        assert!(
            serde_json::from_str::<TaskContract>(&text).is_err(),
            "spawn turn {i} answered with a whole contract — that is the synchronous path, and \
             two of them in a row is serialization:\n{text}"
        );
    }
    let order: Vec<String> = last["body"]["input"]
        .as_array()
        .expect("the root's transcript is an array")
        .iter()
        .filter_map(|it| it.get("call_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter()
        .fold(Vec::new(), |mut acc, id| {
            if acc.last() != Some(&id) {
                acc.push(id);
            }
            acc
        });
    assert_eq!(
        order,
        (0..4)
            .map(|i| format!("{ROOT_PREFIX}_{i:02}"))
            .collect::<Vec<_>>(),
        "both spawns must precede both waits in the root's own transcript — that is what \
         fanning out and then collecting looks like from the harness's side"
    );

    // ---- nothing outlived the run. -------------------------------------------------------------
    drop(server);
    let leaked = survivors(&dir.to_string_lossy());
    assert!(
        leaked.is_empty(),
        "processes from this run are still alive — the S7 class of failure:\n{}",
        leaked
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}
