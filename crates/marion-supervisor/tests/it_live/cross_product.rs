//! **The true harness matrix: every harness as a ROOT spawning every harness as a CHILD.**
//!
//! Three files already stand at three corners of this square and none of them is the square:
//!
//! - `harness_matrix.rs` drives four children through `run_spawn` **directly**, with no root in the
//!   run at all. It proves the *child* axis.
//! - `launch_only_root.rs` drives the **`LaunchOnly`** roots — codex and opencode then —
//!   through the real `marion` binary against a **stub** harness on `PATH`, one `#[test]` per
//!   harness per property. It proves the *root* axis for that surface: claude-code's duplex root
//!   path is not in it, and nothing real is on the other end of `spawn`.
//! - `m1_hop.rs` proved exactly one cell of the cross-product — a claude root spawning a codex
//!   child — end to end against real binaries. It is this file's claude→codex cell now, which
//!   carries its out-of-scope write and its offered-`spawn` check.
//!
//! That left fifteen combinations that had never run. A copilot root spawning an opencode child
//! is not implied by "copilot works as a root" and "opencode works as a child": the two nodes share
//! one canned provider, one bridge binary, one state tree and — in four of the sixteen cells — one
//! **wire**, and each of those is a place the two halves can meet and fail. This file closes it.
//!
//! # What runs by default, and the full square
//!
//! Every cell runs two real harnesses, and the square grew to 63 cells over nine harnesses — the
//! heaviest suite in the workspace. By default the file runs the cells that between them cover
//! every harness **as a root** and every harness **as a child**, and one pair per **wire class**
//! (the same-wire cells below, and a ring over the OpenAI-chat harnesses), plus three pairs that
//! cross wires: claude→claude, claude→codex, claude→opencode (the verifying cell), codex→codex,
//! opencode→copilot, copilot→goose, goose→cline, cline→opencode, pi→pi and
//! qwen→claude. Each runs a harness about as often as a stub on the other side would. The rest of
//! the square is [`full_matrix_cell`]: skipped by name unless `MARION_FULL_MATRIX=1`, which the
//! nightly canary and `scripts/admit-harness.sh` set, so every pair still runs every night and on
//! every admission.
//!
//! # One `#[test]` per cell, and never a loop
//!
//! A loop reports the first failure and hides the other fifteen. Each cell is named for its own
//! pair, so a failing run names the cell in its output.
//!
//! Every cell asserts the same eleven things:
//!
//! 1. the run did not time out and `marion run` exited 0;
//! 2. exactly one `contracts/<task_id>.json` is persisted, and it deserializes to a `TaskContract`;
//! 3. `contract.child.harness` is the harness that **actually ran**, read off the adapter, and
//!    `contract.child.model` is the **compiled** harness-native value, which every cell asks for a
//!    model in order to test: codex is asked for `gpt-5.6-sol` and must record `None`, because a
//!    canned launch compiles no `-m` at all;
//! 4. the narrative is the child's own — non-empty, `narrative_synthesized == false` — and the
//!    status is `Ok`, which together mean the report arrived through marion's MCP channel;
//! 5. **both nodes are visible in the provider's request log, by role and not merely by wire.** See
//!    below: in a same-harness cell the wire is one wire, so a wire-name assertion would pass with
//!    only one of the two nodes having run;
//! 6. `TaskContract.requester` names the root's own agent-dir and is **not** `"unattributed-root"`;
//! 7. no verbatim credential reached the canned server;
//! 8. nothing outlived the run (the S7 class of failure);
//! 9. **the child's worktree was audited**: `scope_enforced` is true — the check *ran*, which §6.7
//!    is careful to say is not the same as "no violation" — and `changed_paths` carries the file
//!    that child wrote, in every cell;
//! 10. **the contract reached the ROOT**, as the tool result of its own `spawn` call, read back off
//!     the root's *next request* on its own wire and compared to the persisted copy modulo §6.7's
//!     cap rules. Everything above it is satisfied by a marion that persists a contract and hands
//!     the model a stub, or someone else's contract;
//! 11. **both nodes reached a terminal `NodeState` in the journal** — one root, one child, each
//!     `Exited(Ok)`, and nothing left `unresolved()`.
//!
//! # All sixteen cells write
//!
//! Criterion 9 is the only one that requires marion to have opened a worktree; every other
//! criterion is satisfiable from the two nodes' streams and marion's own bookkeeping — the request
//! log, the persisted contract, the journal. It was also, for two rounds, the one the matrix did
//! not make. First twelve cells had a child that called nothing but `report`, so `changed_paths`
//! was empty **by construction**. Closing that found a real defect: an opencode child resolved its
//! project directory from `$PWD` and wrote into the operator's own repository while its contract
//! read `changed_paths: [], scope_violations: [], scope_enforced: true` — a clean bill of health
//! for a run that wrote outside every scope list.
//!
//! Then eight cells wrote and eight could not, because a canned provider may only emit calls to
//! tools the harness declared and marion declared claude and gemini children none: `--tools ""`
//! plus an `allowed_tools` of exactly `[report]` (§11 item 24). Those cells asserted their worktree
//! was **untouched** — which is precisely what an escaped write also produces, so the half of the
//! matrix that could not write also could not tell the two apart.
//!
//! §3.1's availability axis closed it. `claude-impl` declares `tools: [read, write]`, which the
//! adapter compiles to `Write` (as `gemini-impl` did to `write_file`, until the gemini CLI was
//! retired), so every child takes a real edit through its own wire's tool-call shape — codex's
//! `tools.apply_patch`, opencode's `write`, claude's `Write`. Every cell exercises placement, and
//! criterion 9 is asserted unconditionally.
//!
//! **One thing these sixteen cells do NOT cover, measured rather than assumed.** The Anthropic
//! wire's child script is ordered by `marion_provider::script::anthropic_called`, a name-based
//! predicate, because `classify_root` ends a run on *any* `tool_result` and would send a child that
//! writes before it reports straight past `report` — its own write's result being the trigger.
//! Swapping the predicate back fails a unit test and **leaves every cell here green**: these
//! scripts make exactly one built-in call before reporting, so the two predicates agree on every
//! transcript that occurs. **The vocabulary has since grown to `[read, write]` and that is still
//! true** — the condition is not how many words the vocabulary has but how many calls a cell's
//! script makes, and none of them makes two. The discriminating case becomes reachable the day a
//! script here reads *and* writes before reporting, and must be re-established then; until it does,
//! the guard lives in `marion-provider`'s unit tests and not in this file.
//!
//! **The grant is the child's and not these roots'.** It rides on [`Node::child_agent_type`], not
//! on the harness: the root types in this matrix (`claude`, `copilot-orchestrator`, …) declare nothing, so their
//! availability axis is empty. That is now a property of what these cells *ask for* rather than an
//! invariant of `root::prepare` — §9's grant gate gives a root its own type's list over a
//! repository marion is recording (`root::availability_axis`).
//!
//! # The same-wire cells, and why they are not ambiguous
//!
//! Same-harness cells — claude→claude, codex→codex, opencode→opencode among them — put the root's
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
//! cargo test -p marion-supervisor --test it_live cross_product::
//! ```
//!
//! It needs real `claude` (2.1.220), `codex` (0.146.0) and `opencode` (1.17.3), and the other rows'
//! binaries for their cells, on `PATH`. Like every other end-to-end file here it is **not** `#[ignore]`d and it does **not**
//! skip when a binary is missing: §9's standing rule is that *a criterion that quietly passes on a
//! machine that cannot run it is worth less than no criterion.*

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use marion_core::contract::{ExitStatus, TaskContract};
use marion_core::harness::Harness;
use marion_core::node::NodeState;
use marion_core::paths::ProjectDir;
use marion_core::registry::Replay;
use marion_harness::adapter_for;
use marion_provider::script::{ROOT_CALL_ID, ROOT_TOOL_USE_ID};
use marion_provider::{CannedServer, Config, EditTurn, RootScript, RootTurn, Script};
use marion_supervisor::journal::read_path;
use marion_supervisor::run::run_bounded;
use marion_testsupport::{
    carries, fixture_repo, judge, kill_hard, on_path, persisted_contracts, scratch, survivors,
};
use serde_json::{Value, json};

use crate::common;
use common::cap_rules::normalize_for_cap_rules;
use common::mcp_result::{codex_call_output_text, contract_json, mcp_blocks_text};

/// The root's `--timeout`: a wall clock over the whole run, on every surface (§9) — and the child's
/// entire run happens inside the root's `spawn` call, so this has to cover both: the two nodes' whole runs, from their rows ([`common::boot::run`]). Still a bound that
/// fails a hang: measured in S13, opencode never exits on a provider hang (a 500 still retrying at
/// 90 s, a connection-refused still hung at 180 s).
///
/// It was a flat 150 s, and at a load average of 150 an opencode root spent ~57 s booting and its
/// child most of the rest, so the root's wall clock expired with the child still starting.
fn root_wall_clock(root: &Node, child: &Node) -> Duration {
    common::boot::run(root.agent_type) + common::boot::run(child.child_agent_type)
}

/// The outermost safety net. Every cell has its own `--timeout`; this only exists so a wedged
/// `marion` fails loudly instead of wedging the suite. Past the root's wall clock, by the minute
/// the run takes to wind down after it.
fn run_bound(root: &Node, child: &Node) -> Duration {
    root_wall_clock(root, child) + Duration::from_secs(60)
}

/// The child's own wall clock, passed through `spawn`'s `timeout_secs`: its whole run, from its row.
/// A flat 60 s used to stand here, and an opencode child under load spent it all booting.
fn child_timeout_secs(child: &Node) -> u64 {
    common::boot::run_secs(child.child_agent_type)
}

/// Present in the **root's** prompt and nowhere in the child's. The provider's role discriminator;
/// see the module docs. Deliberately not a word either prompt would use on its own.
const ROOT_MARKER: &str = "MARION-XPROD-ROOT-TURN-4f2a";

/// The child's task. Shared by all sixteen cells, and free of [`ROOT_MARKER`] — a child prompt that
/// carried it would make every child request look like the root's.
const CHILD_PROMPT: &str = "Add the cross-product marker file under src/ and report back \
                            through marion.";

/// The narrative every child's script reports.
const NARRATIVE: &str = "Wrote the cross-product marker under src/ and reported back.";

/// The file every child that *can* write is driven to write, **worktree-relative** and inside
/// `writable_scope`. One path for every wire, so a cell's failure never turns on which name it used.
///
/// Relative and not absolute because the absolute one does not exist when this script is written: a
/// child's worktree is `<state>/<project-hash>/agents/<agent-id>/worktree` and the agent id is
/// minted inside `spawn`, three processes later. Each harness resolves it against the cwd marion
/// placed the node in — which is exactly the placement [`ROOT_MARKER`]'s counterpart on the
/// containment axis, and which an opencode child did not honour until `opencode::compile_run`
/// exported `PWD`.
const CHILD_FILE: &str = "src/xprod-marker.txt";

/// What that file contains. Distinctive, so a stray copy anywhere on the machine is attributable.
const CHILD_FILE_CONTENT: &str = "marion cross-product marker\n";

/// The claude→codex cell's deliberate out-of-scope write: outside the `src/**` the spawn asks for
/// and inside the agent type's `**` ceiling, so it is the request that refuses it, detectively.
const OUT_OF_SCOPE: &str = "out_of_scope/xprod-violation.txt";

// --- the five harnesses, as data ---------------------------------------------------------------

/// One harness, in both of the roles it can play.
#[derive(Debug, Clone, Copy)]
struct Node {
    /// The built-in agent type this harness is driven as when it is the **root**.
    agent_type: &'static str,
    /// The built-in agent type `spawn` is asked for when this harness is the **child**.
    ///
    /// Two fields and not one, for the same reason [`Node::model`] and [`Node::child_model`] are
    /// two: the roles genuinely differ. §3.1's availability axis is declared per *type*, and the
    /// grant belongs only to the child's — `claude-impl` and `copilot` declare `[read, write]`
    /// while `claude` and `copilot-orchestrator` declare nothing. `codex-impl` and `opencode` are the same string
    /// in both roles, which is what makes the asymmetry visible rather than uniform: it is a
    /// property of two harnesses, not of the matrix.
    ///
    /// **This field decides only what is *asked for*.** The roots in this matrix are orchestrator
    /// types, so they declare nothing and are compiled with an empty availability axis — not
    /// because a root cannot have one (§9's grant gate now gives a root its type's list over a
    /// repository marion is recording, `root::availability_axis`), but because these cells ask for
    /// nothing. See [`assert_cell`]'s worktree criterion for what is asserted about the result.
    child_agent_type: &'static str,
    harness: Harness,
    /// `--model` for a root, and `spawn`'s `model` for a child. **Every node asks for one**, which
    /// is what makes [`Node::child_model`] worth asserting: a matrix that asked for nothing and
    /// then asserted nothing had been recorded would pass against an adapter that dropped the
    /// field, one that invented a value, and one that did neither.
    model: &'static str,
    /// `TaskContract.child.model` when this harness is the **child**: the model its stream names
    /// where the row reads one — which the recorded model must contain, since claude resolves an
    /// asked-for `haiku` to the full id it runs — else the value the adapter **compiled**, which is
    /// not always the value [`Node::model`] asked for.
    ///
    /// The two are deliberately separate fields rather than one, because §6.7 makes the contract an
    /// audit record of what *ran* — so the only interesting case is the one where they differ, and
    /// codex is it: `AdapterCodex::compile` maps `Auth::Canned` to `model: None` whatever was
    /// asked, because under canned auth the endpoint is marion's own server and naming a model
    /// there would be a contract naming something that never reached argv. Asking codex for
    /// `gpt-5.6-sol` and asserting `None` is that property end to end; `harness_matrix.rs` asserts
    /// the same property one node at a time.
    child_model: Option<&'static str>,
    /// The wire this harness speaks to the canned provider on.
    wire: &'static str,
    /// The binary that must be on `PATH`.
    program: &'static str,
}

const CLAUDE: Node = Node {
    agent_type: "claude-orchestrator",
    child_agent_type: "claude-impl",
    harness: Harness::ClaudeCode,
    // Claude Code's `--model` is legitimately omissible, and these cells pass one anyway: an
    // omitted flag makes `child.model == None` true for two unrelated reasons at once — the
    // adapter carried the absence, or the adapter drops the field — and the contract cannot tell
    // them apart. `compile_headless` carries `--model` through verbatim, and the child's `init`
    // frame names the id `haiku` resolved to, which is what the contract records. The canned
    // provider ignores the value.
    model: "haiku",
    child_model: Some("haiku"),
    wire: "anthropic",
    program: "claude",
};

const CODEX: Node = Node {
    agent_type: "codex-impl",
    child_agent_type: "codex-impl",
    harness: Harness::Codex,
    // **Asked for on purpose, and the contract must refuse to record it.** `codex exec` does take
    // `-m` (0.146.0's `--help` lists it), so this is not a harness that cannot carry a model — it
    // is `AdapterCodex::compile` declining to name one under `Auth::Canned`, where the endpoint is
    // marion's own canned server. A contract naming `gpt-5.6-sol` here would be an audit record of
    // a flag that never reached argv.
    model: "gpt-5.6-sol",
    child_model: None,
    wire: "responses",
    program: "codex",
};

const OPENCODE: Node = Node {
    agent_type: "opencode",
    child_agent_type: "opencode",
    harness: Harness::OpenCode,
    // `provider/model`, the only spelling `-m` accepts; the generated provider block repeats it.
    model: "marion/canned-1",
    child_model: Some("marion/canned-1"),
    wire: "openai",
    program: "opencode",
};

const COPILOT: Node = Node {
    agent_type: "copilot-orchestrator",
    child_agent_type: "copilot-impl",
    harness: Harness::Copilot,
    // Explicit: BYOK exits 1 without one (`BYOK providers require an explicit model`), and the
    // adapter refuses first. `--model` is carried through verbatim; the canned provider ignores it.
    model: "canned-1",
    child_model: Some("canned-1"),
    wire: "openai",
    program: "copilot",
};

const GOOSE: Node = Node {
    agent_type: "goose-orchestrator",
    child_agent_type: "goose-impl",
    harness: Harness::Goose,
    // Explicit: `GOOSE_MODEL` is how the `openai` provider is told what to name, and the adapter
    // refuses a canned launch without one. The canned provider ignores the name.
    model: "canned-1",
    child_model: Some("canned-1"),
    wire: "openai",
    program: "goose",
};

const CLINE: Node = Node {
    // One type in both roles, as opencode: cline grants its 26 built-ins unconditionally, so there
    // is no `-impl` flavour to declare a grant marion does not compile.
    agent_type: "cline",
    child_agent_type: "cline",
    harness: Harness::Cline,
    // Explicit: `providers.json` names a model and the adapter refuses to write one that names
    // none; `-m` carries the same name and is measured to win (s27 item 15).
    model: "canned-1",
    child_model: Some("canned-1"),
    wire: "openai",
    program: "cline",
};

const QWEN: Node = Node {
    agent_type: "qwen-orchestrator",
    child_agent_type: "qwen-impl",
    harness: Harness::Qwen,
    // Explicit: `OPENAI_MODEL` is how the provider is told what to name, and the adapter refuses a
    // canned launch without one. The canned provider ignores the name.
    model: "canned-1",
    child_model: Some("canned-1"),
    wire: "openai",
    program: "qwen",
};

const PI: Node = Node {
    // The plain name is pi's implementer and `-orchestrator` its read-only flavour, so the roles
    // swap spellings relative to the older rows: the root asks for nothing, the child for
    // `[read, write]`, which `--tools` turns into `read,write,edit`.
    agent_type: "pi-orchestrator",
    child_agent_type: "pi",
    harness: Harness::Pi,
    // Explicit: `models.json` names a model and the adapter refuses to write one that names none.
    model: "canned-1",
    child_model: Some("canned-1"),
    wire: "openai",
    program: "pi",
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
fn script(root: &Node, child: &Node, verification: &[&str]) -> Script {
    // A **string**, on every harness, never a JSON `null`. §3.1 makes an omitted `model` mean "the
    // agent type's own default" (`marion-supervisor::main` reads it with `as_str()`), and a JSON
    // `null` is not the same thing to every harness: measured here, gemini 0.53.0 validates a tool
    // call against the declared schema *before* dispatching it and refuses `"model": null` with
    // `params/model must be string` — an `invalid_tool_params` tool_result, after which the root
    // happily finished its turn having spawned nothing.
    let mut spawn_args = json!({
        "agent_type": child.child_agent_type,
        "prompt": CHILD_PROMPT,
        "acceptance_criteria": ["a file exists under src/ containing the marker"],
        "writable_scope": ["src/**"],
        "timeout_secs": child_timeout_secs(child),
        "model": child.model,
    });
    if !verification.is_empty() {
        spawn_args["verification"] = json!(verification);
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
        marion_core::Harness::Named(_) => unreachable!("these tests run built-in harnesses"),
        // The Anthropic wire's two-step script *is* a child script once its tool is re-aimed:
        // `classify_root` finishes the run as soon as the transcript carries that call's result.
        Harness::ClaudeCode => {
            s.root_tool = report;
            s.root_tool_input = json!({ "narrative": NARRATIVE });
            s.root_final_text = "Reported back through marion. Done.".into();
            // The claude child writes before it reports, through the built-in `Write` that
            // `claude-impl`'s `tools: [read, write]` makes the adapter declare. `{file_path, content}` is
            // read off the live `input_schema`, and the path is **relative** deliberately: claude's
            // own tool description demands an absolute one, and an absolute path would prove
            // nothing about where marion placed the node.
            s.anthropic_edit = Some(EditTurn {
                tool: "Write".into(),
                args: json!({ "file_path": CHILD_FILE, "content": CHILD_FILE_CONTENT }),
            });
        }
        // The Responses child keeps its three steps: patch, report, final message. The patch is
        // re-aimed at this file's own [`CHILD_FILE`] rather than left at the M1 default, so all
        // eight writing cells attest to one path.
        Harness::Codex => {
            s.child_narrative = NARRATIVE.into();
            s.child_patch = format!(
                "*** Begin Patch\n*** Add File: {CHILD_FILE}\n+{}\n*** End Patch",
                CHILD_FILE_CONTENT.trim_end()
            );
            s.child_final_text = json!({"narrative": NARRATIVE, "result_commits": []}).to_string();
        }
        // The opencode child writes before it reports, through the harness's own `write` — the tool
        // it declares to the model as `tools[].function.name == "write"`, taking `{filePath,
        // content}`. That makes it the second of the four wires whose child leaves something behind
        // for §6.7's git-derived `changed_paths` to find.
        Harness::OpenCode => {
            s.openai_report_tool = report;
            s.openai_report_args = json!({ "narrative": NARRATIVE });
            s.openai_edit = Some(EditTurn {
                tool: "write".into(),
                args: json!({ "filePath": CHILD_FILE, "content": CHILD_FILE_CONTENT }),
            });
        }
        // **Not a cell of this matrix, and that is a refusal rather than an omission.** Every cell
        // here drives a node through marion's *canned* provider, and an ACP node has no canned
        // mode at all: `AcpAdapter::compile` refuses `Auth::Canned` by name, because ACP has no
        // protocol-level way to point an agent at an endpoint. There is nothing to script, so this
        // says so rather than scripting something that would not be an ACP run.
        // The copilot child writes before it reports, through the `create` that `copilot-impl`'s
        // `tools: [read, write]` makes the adapter put in `--available-tools` and grant through
        // `--allow-tool=write` — withheld and denied respectively without the grant (s24). Same
        // wire and same `Script` fields as opencode, different tool name and different argument
        // names (`{path, file_text}`, read off the live `tools[]` schema). The path is **relative**
        // deliberately: copilot's own description demands an absolute one, and s24 measured a
        // relative `create` resolving against `-C`, which is what proves where marion placed it.
        Harness::Copilot => {
            s.openai_report_tool = report;
            s.openai_report_args = json!({ "narrative": NARRATIVE });
            s.openai_edit = Some(EditTurn {
                tool: "create".into(),
                args: json!({ "path": CHILD_FILE, "file_text": CHILD_FILE_CONTENT }),
            });
        }
        // The goose child writes before it reports, through the developer extension's `write`
        // that `goose-impl`'s `tools: [write]` makes the adapter load with `--with-builtin
        // developer` — absent entirely without the grant (s26: `--no-profile` offers marion and
        // nothing else). Same wire and same `Script` fields as opencode and copilot, different
        // tool name and argument names (`{path, content}`, read off the live `tools[]` schema in
        // `goose-with-builtin-developer.provider-request-1.json`).
        Harness::Goose => {
            s.openai_report_tool = report;
            s.openai_report_args = json!({ "narrative": NARRATIVE });
            s.openai_edit = Some(EditTurn {
                tool: "write".into(),
                args: json!({ "path": CHILD_FILE, "content": CHILD_FILE_CONTENT }),
            });
        }
        // The cline child writes through `editor` — `{path, new_text}` with no `old_text` is the
        // create-a-missing-file shape, read off the live `tools[]` schema — one of the 26 built-ins
        // every cline launch carries (s27 item 12), so no grant is compiled and none is recorded.
        // The path is **relative** deliberately: cline's own description demands an absolute one,
        // and this cell is what measures a relative `editor` path resolving against `-c`.
        Harness::Cline => {
            s.openai_report_tool = report;
            s.openai_report_args = json!({ "narrative": NARRATIVE });
            s.openai_edit = Some(EditTurn {
                tool: "editor".into(),
                args: json!({ "path": CHILD_FILE, "new_text": CHILD_FILE_CONTENT }),
            });
        }
        // **Not a child of this matrix, and that is a measured refusal.** qwen 0.23.0's
        // `write_file` refuses a relative `file_path` (`File path must be absolute`, s25 item 17),
        // and the worktree's absolute path does not exist until `spawn` creates it, so no canned
        // script can name it. qwen is driven as a **root** here and as a child in `harness_matrix`.
        Harness::Antigravity => unreachable!(
            "no cell names an agy child: agy has no canned route, and runs only on the \
             operator's own login (the gated live test in harness_matrix drives it)"
        ),
        Harness::Qwen => unreachable!(
            "no cell names a qwen child: its write_file refuses the relative path every child \
             cell writes (s25 item 17)"
        ),
        // The pi child writes before it reports, through the built-in `write` that `pi`'s
        // `tools: [read, write]` puts on `--tools` — absent without the grant (s34-pi item 3).
        // `{path, content}` is read off the live `tools[]` schema, and pi takes a relative path
        // against its cwd, which is what proves where marion placed the node.
        Harness::Pi => {
            s.openai_report_tool = report;
            s.openai_report_args = json!({ "narrative": NARRATIVE });
            s.openai_edit = Some(EditTurn {
                tool: "write".into(),
                args: json!({ "path": CHILD_FILE, "content": CHILD_FILE_CONTENT }),
            });
        }
        Harness::Acp => unreachable!("no cell of this matrix names `acp`"),
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
    /// §4.3's journal for this run's project, replayed off disk before the scratch dir went away.
    ///
    /// A `Result` and not an `expect` inside [`drive`], because everything in [`drive`] runs before
    /// the unconditional cleanup below it: a panic there would leave the very processes and
    /// directories this file asserts about. The error is carried out and raised as an assertion.
    journal: Result<Replay, String>,
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
/// - the **auxiliary** request the provider answers with a fixed stub on the one wire that makes
///   it: Claude Code's concurrent session-title generation. It is recognised by carrying no tools,
///   which is exactly how `classify_anthropic` recognises it. The rule is deliberately **not**
///   applied to the other wires: codex declares its code-mode catalogue inside `input` rather
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
        "anthropic" => has_tools,
        _ => true,
    }
}

/// Is `url`'s host the loopback interface? The same question `bin/marion.rs` asks of `--base-url`,
/// restated here so the harness can hold itself to the binary's own gate without importing it.
fn is_loopback(url: &str) -> bool {
    let host = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    host == "127.0.0.1" || host == "localhost" || host == "[::1]" || host == "::1"
}

/// The argv [`drive`] hands the real `marion` binary for one cell, built apart from the run so the
/// invariant it has to hold is testable without a provider, a child process, or five minutes.
///
/// **A loopback `--base-url` appears only alongside `--canned`.** The binary refuses that
/// combination without the flag, and rightly: real vendor auth is what a bare `marion run`
/// presents, and pointing that credential at a fake server is the accident the gate exists to
/// prevent. Every cell in this file is driven at $0.00 against the in-process canned provider, so
/// every cell **says** it wants the fixture. See
/// [`argv_that_names_a_loopback_endpoint_always_says_canned`].
fn marion_argv(
    root: &Node,
    repo: &Path,
    state: &Path,
    base_url: &str,
    wall_clock: Duration,
    wider_children: bool,
) -> Vec<String> {
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
        base_url.into(),
        // Not optional and not incidental: without it the binary refuses the loopback URL above at
        // argument parsing and the cell never starts.
        "--canned".into(),
        "--timeout".into(),
        wall_clock.as_secs().to_string(),
        // Passed for every root, including the two whose `--model` is omissible: see
        // [`Node::model`]. What the adapter does with it is the cell's assertion, not the
        // builder's.
        "--model".into(),
        root.model.into(),
    ];
    // The operator's opt-in, for the cells whose sandboxed root asks for a less-contained child.
    if wider_children {
        args.push("--allow-wider-children".into());
    }
    args
}

/// **The regression this file's launch path can suffer without any cell being wrong.**
///
/// Every cell goes through [`marion_argv`], and all of them fail identically — at argument
/// parsing, before a provider or a harness is involved — if the builder ever emits a loopback
/// `--base-url` without `--canned`. That is a property of the *harness*, not of any cell, and it is
/// checkable in microseconds against the same gate `bin/marion.rs` enforces in seconds.
///
/// It runs over every node, and checks the second thing this builder owes every cell alongside it:
/// **`--model` is passed, and with this node's own asked-for value.** Criterion 3 asserts what the
/// adapter *compiled*, and its whole meaning rests on something having been asked for — a builder
/// that quietly stopped passing the flag would turn codex's `None` back into the tautology this
/// file used to assert, with every cell still green.
#[test]
fn argv_that_names_a_loopback_endpoint_always_says_canned() {
    for node in [&CLAUDE, &CODEX, &OPENCODE] {
        for base_url in ["http://127.0.0.1:8080/v1", "http://localhost:1/v1"] {
            let args = marion_argv(
                node,
                Path::new("/tmp/repo"),
                Path::new("/tmp/state"),
                base_url,
                Duration::from_secs(150),
                false,
            );
            let url = args
                .iter()
                .position(|a| a == "--base-url")
                .and_then(|i| args.get(i + 1))
                .unwrap_or_else(|| panic!("{} argv passes --base-url", node.agent_type));
            assert!(
                is_loopback(url),
                "{}: the fixture endpoint is loopback — if this ever stops being true the check \
                 below stops meaning anything",
                node.agent_type
            );
            assert!(
                args.iter().any(|a| a == "--canned"),
                "{}: argv names the loopback endpoint {url} without --canned. marion refuses that \
                 combination on purpose (it aims a real credential at a fake server), so every \
                 cell built this way would fail at argument parsing. A run that wants the fixture \
                 has to say so.\nargv: {args:?}",
                node.agent_type
            );
            let asked = args
                .iter()
                .position(|a| a == "--model")
                .and_then(|i| args.get(i + 1));
            assert_eq!(
                asked.map(String::as_str),
                Some(node.model),
                "{}: every cell asks its root for a model, so that criterion 3's assertion about \
                 the COMPILED value has something to be about.\nargv: {args:?}",
                node.agent_type
            );
        }
    }
}

/// Stand up a canned provider, build a fixture repo, run the real `marion` binary on `root`, then
/// clean up **unconditionally** and hand back what happened.
fn drive(root: &Node, child: &Node) -> Evidence {
    drive_with(root, child, &[])
}

/// [`drive`], with the root's `spawn` also carrying `verification` commands.
fn drive_with(root: &Node, child: &Node, verification: &[&str]) -> Evidence {
    drive_script(root, child, script(root, child, verification))
}

/// [`drive`], on a script the cell has adjusted from the default one.
fn drive_script(root: &Node, child: &Node, script: Script) -> Evidence {
    let opt_in = common::needs_wider_opt_in(root.agent_type, child.child_agent_type);
    drive_run(root, child, script, opt_in)
}

/// [`drive_with`], stating the operator's containment opt-in rather than deriving it.
fn drive_as(root: &Node, child: &Node, verification: &[&str], opt_in: bool) -> Evidence {
    drive_run(root, child, script(root, child, verification), opt_in)
}

/// One cell: `script` served, the opt-in as given.
fn drive_run(root: &Node, child: &Node, script: Script, opt_in: bool) -> Evidence {
    let name = format!(
        "{}-{}{}",
        root.agent_type,
        child.child_agent_type,
        if opt_in { "" } else { "-contained" }
    );
    let dir = scratch(&format!("xp-{name}"));
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();

    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script,
    })
    .expect("the canned provider binds");

    let args = marion_argv(
        root,
        &repo,
        &state,
        &server.base_url(),
        root_wall_clock(root, child),
        opt_in,
    );

    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args(&args)
            .current_dir(&dir),
        run_bound(root, child),
    )
    .expect("marion run starts");

    let requests = server.requests().unwrap_or_default();
    // §4.3's location, resolved the one way marion resolves it — never a second literal here.
    // §2's key is the git common dir, not the cwd, which is what `root::prepare` now hashes.
    let key = marion_supervisor::socket::project_root(&repo);
    let journal = read_path(&ProjectDir::new(&state, &key).journal()).map_err(|e| e.to_string());
    // Walked here, **judged after the cleanup below.** The walk is fallible and the judgement is
    // separate for a reason this call site is the reason for: the version that stood here panicked
    // on an unreadable contract *before* the provider was dropped and the survivors swept, so one
    // corrupt audit record became the leaked processes and stranded worktree the next block exists
    // to prevent.
    let walked = persisted_contracts(&state)
        .map_err(|e| format!("{} cannot be walked for contracts: {e}", state.display()));
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
    // The dir itself is removed by `dir`'s own `Drop` as this function returns — which is the same
    // point in the same order, and unlike the `remove_dir_all` that used to stand here it also
    // covers the `.expect`s above, which unwind straight past any trailing statement.
    drop(server);
    let leaked = survivors(&dir.to_string_lossy());
    for (pid, _) in &leaked {
        kill_hard(*pid);
    }

    // Nothing is left running, so a contract marion wrote and this test cannot read back is now
    // safe to fail on — and it must: §6.7 makes a contract an audit record, and one marion cannot
    // read back is a defect whichever half is wrong.
    let walked = walked.unwrap_or_else(|e| panic!("{e}"));
    let contracts: Vec<Value> = judge(&walked).into_iter().map(|(_, v)| v.clone()).collect();

    Evidence {
        timed_out: out.timed_out,
        code: out.code,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        persisted: contracts,
        agent_dirs_present,
        requests,
        journal,
        leaked: leaked.into_iter().map(|(_, line)| line).collect(),
    }
}

// --- what the ROOT was handed back ---------------------------------------------------------------

/// The text of the tool result the **root** received for its own `spawn` call, read back off the
/// root's *next* request on its own wire.
///
/// **This is the only thing in the file that looks at the reply marion sent rather than at what
/// marion wrote down.** Every other criterion is satisfied by a marion that persists a perfectly
/// good contract and hands the model a stub, an error, or some other run's contract — the model
/// never sees the file, it sees this string. §9 asserts it *on the request side* for the same
/// reason it asserts everything else there: what the harness replayed back to the provider is what
/// the harness actually received, whereas marion's own outgoing frame is marion's testimony about
/// itself.
///
/// One frame per wire, and none of them is a substring scan: each finds the *structured*
/// result of the root's own call, by the id the provider minted for it.
fn root_tool_result(root: &Node, ev: &Evidence, cell: &str) -> String {
    // Latest first: on every wire the whole transcript is replayed each turn, so the last root
    // request is the one that carries the most, and a cell that somehow took extra turns still
    // finds the result rather than the frame before it.
    ev.root_requests()
        .iter()
        .rev()
        .find_map(|r| tool_result_on(root, &r["body"]))
        .unwrap_or_else(|| {
            panic!(
                "{cell}: no request the root sent carries the result of its own `spawn` call. The \
                 child ran and marion persisted a contract, so the missing piece is the tool \
                 result going back to the model — which is the whole of §9's third criterion.\n\
                 Request log:\n{}\n{}",
                ev.log_summary(),
                ev.marion_summary()
            )
        })
}

/// [`root_tool_result`] for one request body, on one wire. `None` means "this request does not
/// carry the result yet", which is the ordinary state of the root's *first* turn.
fn tool_result_on(root: &Node, body: &Value) -> Option<String> {
    match root.wire {
        // Anthropic Messages: a `tool_result` block quoting our own `tool_use.id`, whose content
        // Claude Code 2.1.220 sends as an MCP block list.
        "anthropic" => {
            let block = body["messages"]
                .as_array()?
                .iter()
                .filter_map(|m| m["content"].as_array())
                .flatten()
                .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == ROOT_TOOL_USE_ID)?;
            mcp_blocks_text(&block["content"])
        }
        // Responses: a `function_call_output` item quoting our `call_id`, whose `output` is codex's
        // wrapper around the MCP result, a string before 0.155.1 and a block list from it. Both
        // are unwrapped structurally in `common::mcp_result`, not by hunting for a `{`.
        "responses" => codex_call_output_text(body, ROOT_CALL_ID),
        // Chat Completions: the `role: "tool"` message answering our `tool_call_id`. Three
        // harnesses, three framings of one MCP result, each measured off these cells' own request
        // logs: opencode, copilot and goose flatten it to the text; qwen 0.23.0 hands the block
        // list through unflattened; cline 3.0.61 serialises the **whole MCP envelope**
        // (`{"content": [...], "isError": false}`) as one string. Unwrapped structurally in each
        // case rather than by hunting for a `{`: the contract itself is full of braces.
        "openai" => {
            let content = &body["messages"]
                .as_array()?
                .iter()
                .find(|m| m["role"] == "tool" && m["tool_call_id"] == ROOT_CALL_ID)?["content"];
            match content {
                Value::String(s) => match serde_json::from_str::<Value>(s) {
                    Ok(envelope) if envelope["content"].is_array() => {
                        mcp_blocks_text(&envelope["content"])
                    }
                    _ => Some(s.clone()),
                },
                blocks @ Value::Array(_) => mcp_blocks_text(blocks),
                _ => None,
            }
        }
        w => panic!("no reader for wire {w:?}; a fifth wire needs its own frame here"),
    }
}

/// The eleven assertions every cell makes, in the order that makes a failure most diagnosable: the
/// run's own outcome first, then what the provider saw, then what marion recorded, then what the
/// root was actually handed, and last the two whole-run properties — the tree's terminal states and
/// the leak check.
fn assert_cell(root: &Node, child: &Node, ev: &Evidence) {
    assert_cell_with(root, child, ev, &[]);
}

/// [`assert_cell`], for a cell whose child also wrote `violations` — paths outside the `src/**` the
/// spawn asked for — which must appear in `changed_paths` and be exactly the scope violations.
fn assert_cell_with(root: &Node, child: &Node, ev: &Evidence, violations: &[&str]) {
    let cell = format!("{} root → {} child", root.harness, child.harness);

    // ---- 1: the run finished, and cleanly. ------------------------------------------------------
    assert!(
        !ev.timed_out,
        "{cell}: marion run did not finish inside {:?}\n{}\nRequest log:\n{}",
        run_bound(root, child),
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
    assert!(
        match (contract.child.model.as_deref(), child.child_model) {
            (Some(got), Some(want)) => got.contains(want),
            (got, want) => got == want,
        },
        "{cell}: `child.model` records the model the child's stream names where its row reads one, \
         else the COMPILED, harness-native value — codex carries none however loudly one was asked \
         for. got {:?}, want one naming {:?}",
        contract.child.model,
        child.child_model
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

    // ---- 9: the worktree was audited, and the child's edit is in the audit. ----------------------
    //
    // Everything above this point is derivable from the two nodes' streams and from marion's own
    // bookkeeping; **nothing above it requires marion to have opened a worktree at all**. These are
    // the §6.7 fields that do — `changed_paths`, and the `scope_enforced` flag that says the check
    // behind them ran.
    assert!(
        comp.scope_enforced,
        "{cell}: scope_enforced is false, which records that the containment check NEVER RAN — it \
         is not the same as, and must never be read as, `no violation`. Every cell here gives its \
         child a `Workspace::Worktree`, which is exactly the case that affords a git-derived \
         changed_paths, so false means the derivation failed rather than that there was nothing to \
         derive. changed_paths: {:?}\nRequest log:\n{}",
        comp.changed_paths,
        ev.log_summary()
    );
    // **Every cell asserts this, unconditionally.** It was a two-branch check while some children
    // had no write route: those cells asserted the worktree was *untouched*, which
    // is the one thing an escaped write also produces, so half the matrix could not tell a child
    // that never wrote from one that wrote somewhere marion never looked. §3.1's availability axis
    // closed that — `claude-impl` declares `write` (and, since s14, `read`) — and
    // the branch went with it.
    // A guard that is always true reads as coverage while asserting nothing, and invites someone to
    // "restore" the false case later.
    //
    // What the first harnesses write with, measured off these cells' own request logs (`claude`
    // 2.1.222, `codex` 0.146.0, `opencode` 1.17.3): codex's `tools.apply_patch` under code mode,
    // opencode's declared `write` and claude's `Write`. The last arrives only because the child's
    // *agent type* grants it
    // — see [`Node::child_agent_type`] — and a canned provider may only call a tool the harness
    // declared, so a cell that stopped being granted one fails here rather than passing vacuously.
    assert!(
        comp.changed_paths
            .iter()
            .any(|p| p == Path::new(CHILD_FILE)),
        "{cell}: the child wrote {CHILD_FILE} through its own harness's write tool, so §6.7's \
         git derivation must attest to it. changed_paths: {:?}\n\
         An EMPTY set here with an `Ok` status is the failure this assertion exists for: it is \
         a clean audit record for a run whose write went somewhere marion never looked. That \
         is what an opencode child did before `opencode::compile_run` exported `PWD` — it \
         worked in the directory marion was launched from, not in its worktree, and wrote into \
         the operator's own repository while the contract recorded changed_paths: [], \
         scope_violations: [], scope_enforced: true.\n\
         Two other shapes land here. If this cell's child was granted no write tool, the call \
         the provider scripted was one no real model could have made — check \
         `Node::child_agent_type` resolves to a type whose `tools` carry `write`. If the write \
         happened but resolved somewhere else, that is the placement defect above: all four \
         harnesses are measured to resolve {CHILD_FILE} against the process cwd.\n\
         Request log:\n{}",
        comp.changed_paths,
        ev.log_summary()
    );
    // Detective, not preventive: an out-of-scope write happened, is in the audit, and is exactly
    // the violation — a strict subset of `changed_paths`, never a copy of it.
    for v in violations {
        assert!(
            comp.changed_paths.iter().any(|p| p == Path::new(v)),
            "{cell}: the out-of-scope write {v} must appear in changed_paths: {:?}",
            comp.changed_paths
        );
    }
    let expected: Vec<std::path::PathBuf> =
        violations.iter().map(std::path::PathBuf::from).collect();
    assert_eq!(
        comp.scope_violations, expected,
        "{cell}: {CHILD_FILE} is inside the `src/**` this cell's spawn asked for, so it is never a \
         violation; exactly the cell's deliberate out-of-scope writes are"
    );
    assert_eq!(
        contract.scope_requested,
        vec![marion_core::contract::Glob("src/**".into())],
        "{cell}: the scope the spawn asked for is the one the audit compared against"
    );

    // ---- 10: the ROOT received the contract, and it is the one on disk. -------------------------
    //
    // Everything above is an assertion about what marion *wrote down*. A marion that persisted a
    // flawless contract and handed its root a stub — or a truncation notice, or another run's
    // contract — passes every one of them, and the model, which is the only consumer §6.7 has, sees
    // none of what they checked. Read off the root's own next request, on its own wire.
    let result_text = root_tool_result(root, ev, &cell);
    assert!(
        !result_text.contains("<persisted-output>"),
        "{cell}: the tool result was replaced by a stub, so the contract never reached the model. \
         Keeping it under that threshold is what §6.7's cap rules exist to guarantee.\n\
         {result_text}"
    );
    let returned: TaskContract = serde_json::from_str(contract_json(&result_text)).unwrap_or_else(|e| {
        panic!("{cell}: the root's tool result does not deserialize to a TaskContract: {e}\n{result_text}")
    });
    assert_eq!(
        returned.task_id, contract.task_id,
        "{cell}: the root was handed a contract for a different task than the one persisted — the \
         failure a per-cell equality below would otherwise report as a diff a reader has to \
         squint at"
    );
    // Nothing in these cells is anywhere near a cap, so the shortenable fields survived intact and
    // the normalization below is not hiding a difference in them. Asserted, not assumed: if a cap
    // ever does fire here, this says so instead of letting the comparison quietly compare nulls.
    assert!(
        !contract.instructions.truncated
            && contract.acceptance_criteria.iter().all(|c| !c.truncated)
            && comp.narrative.as_ref().is_none_or(|n| !n.truncated)
            && comp.diff.as_ref().is_none_or(|d| !d.truncated)
            && (comp.evidence_omitted, comp.changed_paths_omitted) == (0, 0)
            && (
                comp.scope_violations_omitted,
                comp.acceptance_criteria_omitted
            ) == (0, 0),
        "{cell}: the persisted contract is the complete one and nothing here is near a cap, so \
         nothing in it should record having been shortened: {:?}",
        ev.persisted[0]
    );
    let returned_comp = returned
        .completion
        .as_ref()
        .unwrap_or_else(|| panic!("{cell}: the returned copy carries the same completion"));
    assert_eq!(
        returned_comp.narrative.as_ref().map(|n| &n.value),
        comp.narrative.as_ref().map(|n| &n.value),
        "{cell}: the child's own words are what the root is told; nothing here is near rule 0's cap"
    );
    assert_eq!(
        returned_comp.changed_paths, comp.changed_paths,
        "{cell}: §6.7's audit of the worktree is part of what the root is answering to, and this \
         run's set is far under rule 5(b)'s cap"
    );
    assert_eq!(
        normalize_for_cap_rules(
            serde_json::to_value(&returned).expect("a TaskContract serializes")
        ),
        normalize_for_cap_rules(ev.persisted[0].clone()),
        "{cell}: the tool result must BE the persisted contract, modulo only what §6.7's cap rules \
         may shorten. Every field outside those rules — the ids, the repo, the workspace, the \
         scope lists, the status, the flags, the exit, the timestamps — is the same run described \
         twice.\nRequest log:\n{}",
        ev.log_summary()
    );

    // ---- 11: both nodes reached a terminal state, in §4.3's journal. ----------------------------
    //
    // The contract says how the *child's task* ended; the journal says how the two *nodes* ended,
    // and they are not the same claim. §7.2's `Orphaned` marking is about exactly the gap: a node
    // marion recorded live and never recorded resolving. A run can hand back a clean `Ok` contract
    // and still leave its tree saying a process is running.
    let replay = ev
        .journal
        .as_ref()
        .unwrap_or_else(|e| panic!("{cell}: the run's journal does not replay: {e}"));
    assert_eq!(
        replay.nodes().len(),
        2,
        "{cell}: one root, one child: {:?}",
        replay
            .nodes()
            .iter()
            .map(|n| &n.agent_id.0)
            .collect::<Vec<_>>()
    );
    let roots = replay.roots();
    assert_eq!(roots.len(), 1, "{cell}: one root: {roots:?}");
    let root_node = roots[0];
    let children = replay.children(&root_node.agent_id);
    assert_eq!(
        children.len(),
        1,
        "{cell}: the root spawned exactly one child: {children:?}"
    );
    let child_node = children[0];
    // Which node is which, before saying anything about how they ended — in a same-harness cell
    // these two agree, which is why the parent edge above and not the harness is what tells them
    // apart.
    assert_eq!(
        (root_node.harness(), child_node.harness()),
        (Some(root.harness), Some(child.harness)),
        "{cell}: the journal must name the harness each node actually ran"
    );
    for (label, node) in [("root", root_node), ("child", child_node)] {
        assert_eq!(
            node.state,
            NodeState::Exited(ExitStatus::Ok),
            "{cell}: the {label} node's terminal state. `Exited(_)` with a non-`Ok` status is a \
             node marion watched fail; anything else is a node marion never recorded resolving at \
             all, which is what §7.2's Orphaned marking is for. exit: {:?}\nRequest log:\n{}",
            node.exit,
            ev.log_summary()
        );
    }
    assert!(
        replay.unresolved().is_empty(),
        "{cell}: a node recorded live with no exit outlives the run in marion's own tree, whatever \
         `ps` says: {:?}",
        replay
            .unresolved()
            .iter()
            .map(|n| &n.agent_id.0)
            .collect::<Vec<_>>()
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
/// The environment switch that runs the whole square (the nightly canary sets it).
const FULL_MATRIX_ENV: &str = "MARION_FULL_MATRIX";

/// A cell of the full square beyond the default set: run under `MARION_FULL_MATRIX=1`, and skipped
/// **loudly, by name** otherwise — the default set already runs each harness as a root, each as a
/// child and one pair per wire class (see the module docs).
fn full_matrix_cell(root: &Node, child: &Node) {
    if std::env::var(FULL_MATRIX_ENV).as_deref() != Ok("1") {
        eprintln!(
            "cross_product: {} root × {} child is in the full square; skipped without \
             {FULL_MATRIX_ENV}=1 (the nightly canary runs it)",
            root.program, child.program
        );
        return;
    }
    cell(root, child);
}

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

// --- the cells ----------------------------------------------------------------------------------
//
// One `#[test]` each, named for its own pair. Never a loop: a loop reports the first failure and
// hides the rest. Sixteen cells over the first four harnesses, then each later harness's column
// and row; every count of "sixteen" in the prose above predates the fifth harness and describes
// the original square. A cell that calls `cell` runs by default; `full_matrix_cell` is the rest of
// the square (see "What runs by default").

#[test]
fn a_claude_root_spawns_a_claude_child_and_receives_its_contract() {
    cell(&CLAUDE, &CLAUDE);
}

/// **The claude→codex hop, end to end** — M1's acceptance run, and the cell that took over
/// `m1_hop.rs`'s assertions: besides every criterion [`assert_cell`] makes, the codex child writes
/// a second file outside the `src/**` its spawn asked for, which must be caught detectively — in
/// `changed_paths` and exactly the scope violation — and the root's harness offered
/// `mcp__marion__spawn` before it called it.
#[test]
fn b_claude_root_spawns_a_codex_child_and_receives_its_contract() {
    for n in [&CLAUDE, &CODEX] {
        assert!(on_path(n.program), "this cell drives a REAL {}", n.program);
    }
    let mut s = script(&CLAUDE, &CODEX, &[]);
    s.child_patch = format!(
        "*** Begin Patch\n*** Add File: {CHILD_FILE}\n+{}\n*** Add File: {OUT_OF_SCOPE}\n+written \
         outside the requested scope, on purpose\n*** End Patch",
        CHILD_FILE_CONTENT.trim_end()
    );
    let ev = drive_script(&CLAUDE, &CODEX, s);
    assert_cell_with(&CLAUDE, &CODEX, &ev, &[OUT_OF_SCOPE]);
    let offers_spawn = |r: &Value| {
        r.pointer("/body/tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|t| t.get("name").and_then(Value::as_str) == Some("mcp__marion__spawn"))
    };
    assert!(
        ev.requests.iter().any(offers_spawn),
        "no request offered mcp__marion__spawn: the root took its turn before the harness had \
         connected marion's MCP server, so its call could not have been made"
    );
}

/// **An opencode child of a claude root, with every feature a child has** (s36's parity cell): the
/// eleven assertions [`cell`] makes, and on top of them the three a plain cell does not — the
/// spawn's `verification` ran in the child's worktree and passed, the child's write landed on its
/// `marion/<task_id>` branch at a commit marion read back, and the child's own opencode session
/// id (`ses_…`) is journaled for a resume to hand back.
#[test]
fn d_claude_root_spawns_an_opencode_child_that_verifies_lands_its_branch_and_journals_its_session()
{
    verifies_lands_and_journals_its_session(&OPENCODE, |s| s.starts_with("ses_"));
}

/// **The same parity cell for a pi child** (opencode's s36 cell, row for row): the spawn's
/// verification passes in the child's worktree, its write lands on its branch, and the session
/// pi's `get_state` handshake named (a UUID, the id `--session` takes) is journaled.
#[test]
fn zzt_claude_root_spawns_a_pi_child_that_verifies_lands_its_branch_and_journals_its_session() {
    verifies_lands_and_journals_its_session(&PI, |s| {
        s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    });
}

/// A claude root's `child` with every feature a child has: the eleven assertions [`cell`] makes,
/// the spawn's verification recorded and passing, the write on the child's own branch at a commit
/// marion read back, and a harness session id `session_shape` accepts journaled for a resume.
fn verifies_lands_and_journals_its_session(child_node: &Node, session_shape: fn(&str) -> bool) {
    for n in [&CLAUDE, child_node] {
        assert!(on_path(n.program), "this cell drives a REAL {}", n.program);
    }
    let verify = format!("grep -q 'cross-product marker' {CHILD_FILE}");
    let ev = drive_with(&CLAUDE, child_node, &[&verify]);
    assert_cell(&CLAUDE, child_node, &ev);

    let contract: TaskContract = serde_json::from_value(ev.persisted[0].clone()).unwrap();
    let comp = contract.completion.as_ref().unwrap();
    assert_eq!(
        contract.verification.len(),
        1,
        "the spawn's verification was recorded"
    );
    assert_eq!(comp.evidence.len(), 1, "and ran: {:?}", comp.evidence);
    assert_eq!(
        comp.evidence[0].exit_code,
        Some(0),
        "the child's file is there for the check to find: {:?}",
        comp.evidence[0]
    );
    assert_eq!(
        comp.branch.as_ref(),
        match &contract.workspace {
            marion_core::contract::Workspace::Worktree { branch, .. } => Some(branch),
            other => panic!("a worktree child: {other:?}"),
        },
        "the child's write landed on its own branch"
    );
    assert!(comp.commit.is_some(), "at a commit marion read back");
    let journal = ev.journal.as_ref().expect("the journal replays");
    let child = journal
        .nodes()
        .iter()
        .find(|n| n.depth() == Some(1))
        .expect("the child is journaled");
    assert!(
        child.harness_session.as_deref().is_some_and(session_shape),
        "the child's {} session is journaled for a resume: {:?}",
        child_node.program,
        child.harness_session
    );
}

#[test]
fn e_codex_root_spawns_a_claude_child_and_receives_its_contract() {
    full_matrix_cell(&CODEX, &CLAUDE);
}

/// **Without the operator's opt-in, a codex root cannot start a cline child**: codex runs
/// sandboxed, cline has no sandbox marion can apply (its row is not yet measured under marion's
/// own), so the spawn is refused by name — the root is told why and how to allow it, and no child
/// node exists. A claude child is contained by marion's sandbox here and needs no opt-in.
#[test]
fn e_codex_root_is_refused_a_cline_child_without_the_opt_in() {
    for n in [&CODEX, &CLINE] {
        assert!(on_path(n.program), "this cell drives a REAL {}", n.program);
    }
    let ev = drive_as(&CODEX, &CLINE, &[], false);
    let told = ev
        .root_requests()
        .iter()
        .any(|r| carries(&r["body"], "codex runs sandboxed here"));
    assert!(
        told,
        "the root is handed the refusal in its own turn:\n{:#?}",
        ev.requests
    );
    let journal = ev.journal.as_ref().expect("the journal replays");
    assert_eq!(
        journal.nodes().len(),
        1,
        "only the root: the refused child never existed"
    );
    assert!(ev.leaked.is_empty(), "{:?}", ev.leaked);
}

#[test]
fn f_codex_root_spawns_a_codex_child_and_receives_its_contract() {
    cell(&CODEX, &CODEX);
}

#[test]
fn h_codex_root_spawns_an_opencode_child_and_receives_its_contract() {
    full_matrix_cell(&CODEX, &OPENCODE);
}

#[test]
fn m_opencode_root_spawns_a_claude_child_and_receives_its_contract() {
    full_matrix_cell(&OPENCODE, &CLAUDE);
}

#[test]
fn n_opencode_root_spawns_a_codex_child_and_receives_its_contract() {
    full_matrix_cell(&OPENCODE, &CODEX);
}

#[test]
fn p_opencode_root_spawns_an_opencode_child_and_receives_its_contract() {
    full_matrix_cell(&OPENCODE, &OPENCODE);
}

// The copilot column and row: the fifth harness as a child of each of the four, and as a root
// over each of the five. Same wire as opencode, so what these nine add is the fifth spelling of
// marion's verbs on both of copilot's axes, driven by a real binary in both roles.

#[test]
fn q_claude_root_spawns_a_copilot_child_and_receives_its_contract() {
    full_matrix_cell(&CLAUDE, &COPILOT);
}

#[test]
fn r_codex_root_spawns_a_copilot_child_and_receives_its_contract() {
    full_matrix_cell(&CODEX, &COPILOT);
}

#[test]
fn t_opencode_root_spawns_a_copilot_child_and_receives_its_contract() {
    cell(&OPENCODE, &COPILOT);
}

#[test]
fn u_copilot_root_spawns_a_claude_child_and_receives_its_contract() {
    full_matrix_cell(&COPILOT, &CLAUDE);
}

#[test]
fn v_copilot_root_spawns_a_codex_child_and_receives_its_contract() {
    full_matrix_cell(&COPILOT, &CODEX);
}

#[test]
fn x_copilot_root_spawns_an_opencode_child_and_receives_its_contract() {
    full_matrix_cell(&COPILOT, &OPENCODE);
}

#[test]
fn y_copilot_root_spawns_a_copilot_child_and_receives_its_contract() {
    full_matrix_cell(&COPILOT, &COPILOT);
}

// The goose column and row: the sixth harness as a child of each of the five, and as a root over
// each of the six. Same wire as opencode and copilot; what these eleven add is the sixth spelling
// of marion's verbs (`marion__spawn`) offered to a root that `--no-profile` leaves with nothing
// else, and a child whose only route to a file is the extension its grant loads.

#[test]
fn za_claude_root_spawns_a_goose_child_and_receives_its_contract() {
    full_matrix_cell(&CLAUDE, &GOOSE);
}

#[test]
fn zb_codex_root_spawns_a_goose_child_and_receives_its_contract() {
    full_matrix_cell(&CODEX, &GOOSE);
}

#[test]
fn zd_opencode_root_spawns_a_goose_child_and_receives_its_contract() {
    full_matrix_cell(&OPENCODE, &GOOSE);
}

#[test]
fn ze_copilot_root_spawns_a_goose_child_and_receives_its_contract() {
    cell(&COPILOT, &GOOSE);
}

#[test]
fn zf_goose_root_spawns_a_claude_child_and_receives_its_contract() {
    full_matrix_cell(&GOOSE, &CLAUDE);
}

#[test]
fn zg_goose_root_spawns_a_codex_child_and_receives_its_contract() {
    full_matrix_cell(&GOOSE, &CODEX);
}

#[test]
fn zi_goose_root_spawns_an_opencode_child_and_receives_its_contract() {
    full_matrix_cell(&GOOSE, &OPENCODE);
}

#[test]
fn zj_goose_root_spawns_a_copilot_child_and_receives_its_contract() {
    full_matrix_cell(&GOOSE, &COPILOT);
}

#[test]
fn zk_goose_root_spawns_a_goose_child_and_receives_its_contract() {
    full_matrix_cell(&GOOSE, &GOOSE);
}

// The cline column and row: the seventh harness as a child of each of the six, and as a root over
// each of the seven. Same wire again; what these thirteen add is a root and a child whose tool set
// marion does not narrow at all, reading marion's verbs under the `marion__` spelling.

#[test]
fn zl_claude_root_spawns_a_cline_child_and_receives_its_contract() {
    full_matrix_cell(&CLAUDE, &CLINE);
}

#[test]
fn zm_codex_root_spawns_a_cline_child_and_receives_its_contract() {
    full_matrix_cell(&CODEX, &CLINE);
}

#[test]
fn zo_opencode_root_spawns_a_cline_child_and_receives_its_contract() {
    full_matrix_cell(&OPENCODE, &CLINE);
}

#[test]
fn zp_copilot_root_spawns_a_cline_child_and_receives_its_contract() {
    full_matrix_cell(&COPILOT, &CLINE);
}

#[test]
fn zq_goose_root_spawns_a_cline_child_and_receives_its_contract() {
    cell(&GOOSE, &CLINE);
}

#[test]
fn zr_cline_root_spawns_a_claude_child_and_receives_its_contract() {
    full_matrix_cell(&CLINE, &CLAUDE);
}

#[test]
fn zs_cline_root_spawns_a_codex_child_and_receives_its_contract() {
    full_matrix_cell(&CLINE, &CODEX);
}

#[test]
fn zu_cline_root_spawns_an_opencode_child_and_receives_its_contract() {
    cell(&CLINE, &OPENCODE);
}

#[test]
fn zv_cline_root_spawns_a_copilot_child_and_receives_its_contract() {
    full_matrix_cell(&CLINE, &COPILOT);
}

#[test]
fn zw_cline_root_spawns_a_goose_child_and_receives_its_contract() {
    full_matrix_cell(&CLINE, &GOOSE);
}

#[test]
fn zx_cline_root_spawns_a_cline_child_and_receives_its_contract() {
    full_matrix_cell(&CLINE, &CLINE);
}

// The qwen **row**: the eighth harness as a root over each of the other seven. What these add is Claude
// Code's spelling and stream read off a different binary over a different wire, with
// `--core-tools` as the root's whole tool set. **There is no qwen column, and that is a measured
// refusal rather than an omission**: qwen 0.23.0's `write_file` refuses a relative path outright
// (`File path must be absolute`, s25 item 17), the worktree a child writes into does not exist
// until `spawn` creates it, and a canned script that cannot name the path cannot script the write
// every child cell asserts. Its `harness_matrix` cell is the child-side witness instead.

#[test]
fn zzf_qwen_root_spawns_a_claude_child_and_receives_its_contract() {
    cell(&QWEN, &CLAUDE);
}

#[test]
fn zzg_qwen_root_spawns_a_codex_child_and_receives_its_contract() {
    full_matrix_cell(&QWEN, &CODEX);
}

#[test]
fn zzi_qwen_root_spawns_an_opencode_child_and_receives_its_contract() {
    full_matrix_cell(&QWEN, &OPENCODE);
}

#[test]
fn zzj_qwen_root_spawns_a_copilot_child_and_receives_its_contract() {
    full_matrix_cell(&QWEN, &COPILOT);
}

#[test]
fn zzk_qwen_root_spawns_a_goose_child_and_receives_its_contract() {
    full_matrix_cell(&QWEN, &GOOSE);
}

#[test]
fn zzl_qwen_root_spawns_a_cline_child_and_receives_its_contract() {
    full_matrix_cell(&QWEN, &CLINE);
}

// The pi **row and column**: the ninth harness, the first with no MCP client of its own, reached
// through marion's own `-e` extension. As a child it writes through `write` and reports; as a root
// it spawns through the same extension, whose `tools/call` blocks until marion answers.

#[test]
fn zzm_claude_root_spawns_a_pi_child_and_receives_its_contract() {
    full_matrix_cell(&CLAUDE, &PI);
}

#[test]
fn zzn_codex_root_spawns_a_pi_child_and_receives_its_contract() {
    full_matrix_cell(&CODEX, &PI);
}

#[test]
fn zzo_opencode_root_spawns_a_pi_child_and_receives_its_contract() {
    full_matrix_cell(&OPENCODE, &PI);
}

#[test]
fn zzp_pi_root_spawns_a_claude_child_and_receives_its_contract() {
    full_matrix_cell(&PI, &CLAUDE);
}

#[test]
fn zzq_pi_root_spawns_a_codex_child_and_receives_its_contract() {
    full_matrix_cell(&PI, &CODEX);
}

#[test]
fn zzr_pi_root_spawns_an_opencode_child_and_receives_its_contract() {
    full_matrix_cell(&PI, &OPENCODE);
}

#[test]
fn zzs_pi_root_spawns_a_pi_child_and_receives_its_contract() {
    cell(&PI, &PI);
}
