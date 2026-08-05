//! **The true harness matrix: every harness as a ROOT spawning every harness as a CHILD.**
//!
//! Three files already stand at three corners of this square and none of them is the square:
//!
//! - `harness_matrix.rs` drives four children through `run_spawn` **directly**, with no root in the
//!   run at all. It proves the *child* axis.
//! - `launch_only_root.rs` drives the three **`LaunchOnly`** roots — codex, gemini and opencode —
//!   through the real `marion` binary against a **stub** harness on `PATH`, one `#[test]` per
//!   harness per property. It proves the *root* axis for that surface: claude-code's duplex root
//!   path is not in it, and nothing real is on the other end of `spawn`.
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
//!    is careful to say is not the same as "no violation" — and, in the cells whose child can
//!    write at all, `changed_paths` carries the file that child wrote;
//! 10. **the contract reached the ROOT**, as the tool result of its own `spawn` call, read back off
//!     the root's *next request* on its own wire and compared to the persisted copy modulo §6.7's
//!     cap rules. Everything above it is satisfied by a marion that persists a contract and hands
//!     the model a stub, or someone else's contract;
//! 11. **both nodes reached a terminal `NodeState` in the journal** — one root, one child, each
//!     `Exited(Ok)`, and nothing left `unresolved()`.
//!
//! # Eight cells write, and the other eight say why they cannot
//!
//! Criterion 9 is the only one that requires marion to have opened a worktree; every other
//! criterion is satisfiable from the two nodes' streams and marion's own bookkeeping — the request
//! log, the persisted contract, the journal. It was also, until now, the one the
//! matrix did not make: twelve cells had a child that called nothing but `report`, so
//! `changed_paths` was empty **by construction** and a marion that never diffed anything passed
//! them.
//!
//! Two of the four harnesses now drive a real edit, each through its own wire's tool-call shape —
//! codex's `tools.apply_patch` and opencode's declared `write`. The other two **declare no write
//! tool to the model at all**, which is a property of how marion launches them and not a choice
//! this file is free to make: a canned provider may only emit calls to tools the harness declared,
//! and a call to anything else tests the fixture rather than marion. See
//! `Node::child_writes_worktree` for the per-harness measurement, taken off these cells' own
//! request logs. Those cells pin the gap instead of ignoring it.
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
use marion_core::node::NodeState;
use marion_core::paths::ProjectDir;
use marion_core::registry::Replay;
use marion_harness::adapter_for;
use marion_provider::script::{ROOT_CALL_ID, ROOT_TOOL_USE_ID};
use marion_provider::{CannedServer, Config, EditTurn, RootScript, RootTurn, Script};
use marion_supervisor::journal::read_path;
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
///
/// **A file that cannot be read or parsed is a failure naming the file, never a file this function
/// does not mention.** It used to be dropped by a `filter_map`, and the drop was invisible in
/// exactly the direction that matters: assertion 2 counts the contracts and requires exactly one,
/// so one good contract beside one corrupt one counted as one and passed. §6.7 makes a contract an
/// audit record, and an audit record marion wrote and cannot read back is a defect whichever half
/// is wrong — so the count and the parse are the same question and are answered in the same place.
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
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap_or_else(|e| {
                panic!(
                    "marion persisted {} and this test cannot read it back: {e}",
                    p.display()
                )
            });
            let v = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                panic!(
                    "{} is not JSON: {e}\nfirst 512 bytes:\n{}",
                    p.display(),
                    String::from_utf8_lossy(&bytes)
                        .chars()
                        .take(512)
                        .collect::<String>()
                )
            });
            (p, v)
        })
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
    /// `--model` for a root, and `spawn`'s `model` for a child. **Every node asks for one**, which
    /// is what makes [`Node::child_model`] worth asserting: a matrix that asked for nothing and
    /// then asserted nothing had been recorded would pass against an adapter that dropped the
    /// field, one that invented a value, and one that did neither.
    model: &'static str,
    /// `TaskContract.child.model` when this harness is the **child**: the value the adapter
    /// **compiled**, which is not always the value [`Node::model`] asked for.
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
    /// **Does this harness's child have any route to edit its worktree at all?**
    ///
    /// Not a preference and not a script-authoring choice: it is whether the harness *declares a
    /// write tool to the model*, read off the request bodies these very cells produce. A canned
    /// provider may only emit calls to tools the harness declared — a call to anything else is a
    /// turn no real model could have taken, and answering with one would test the fixture rather
    /// than marion.
    ///
    /// Measured here against the request log of a child spawned through `spawn`, per harness:
    ///
    /// - **codex** — `tools.apply_patch` under code mode. Writes. This is M1's child.
    /// - **opencode** — declares `write`, `edit` and `bash` alongside marion's MCP tools. Writes.
    /// - **claude** — declares `mcp__marion__report` and `mcp__marion__spawn` and **nothing else**:
    ///   `compile_headless` passes `--tools ""` to every node, root or child, and
    ///   `marion_core::agent_type::AgentType` has no `tools` field for anything to populate it
    ///   from. §3.1's availability axis is unimplemented, so a Claude Code child is read-only by
    ///   construction.
    /// - **gemini** — declares `list_directory`, `read_file`, `grep_search`, `glob`,
    ///   `google_web_search`, `enter_plan_mode`, `invoke_agent` and marion's two. `write_file`,
    ///   `replace` and `run_shell_command` appear only in the *prose* of its system instruction,
    ///   never in `functionDeclarations`: 0.53.0 withholds the mutating tools under the default
    ///   approval mode, and s12 measured them appearing only under `-y` — which marion does not
    ///   pass, and which an admin can veto anyway (`security.disableYoloMode`).
    ///
    /// The two `false`s are a **coverage gap that is not this file's to close**, and
    /// [`assert_cell`] pins it rather than passing over it: those cells assert that the worktree is
    /// untouched, and say what to do when that stops being true.
    child_writes_worktree: bool,
}

const CLAUDE: Node = Node {
    agent_type: "claude",
    harness: Harness::ClaudeCode,
    // Claude Code's `--model` is legitimately omissible, and these cells pass one anyway: an
    // omitted flag makes `child.model == None` true for two unrelated reasons at once — the
    // adapter carried the absence, or the adapter drops the field — and the contract cannot tell
    // them apart. `compile_headless` carries `--model` through verbatim, so an asked-for `haiku`
    // must come back as `haiku`. The canned provider ignores the value.
    model: "haiku",
    child_model: Some("haiku"),
    wire: "anthropic",
    program: "claude",
    child_writes_worktree: false,
};

const CODEX: Node = Node {
    agent_type: "codex-impl",
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
    child_writes_worktree: true,
};

const GEMINI: Node = Node {
    agent_type: "gemini",
    harness: Harness::Gemini,
    // Explicit: the adapter REFUSES to compile without `-m` (S12's `auto` router hang).
    model: "gemini-2.5-flash",
    child_model: Some("gemini-2.5-flash"),
    wire: "gemini",
    program: "gemini",
    child_writes_worktree: false,
};

const OPENCODE: Node = Node {
    agent_type: "opencode",
    harness: Harness::OpenCode,
    // `provider/model`, the only spelling `-m` accepts; the generated provider block repeats it.
    model: "marion/canned-1",
    child_model: Some("marion/canned-1"),
    wire: "openai",
    program: "opencode",
    child_writes_worktree: true,
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
    // A **string**, on every harness, never a JSON `null`. §3.1 makes an omitted `model` mean "the
    // agent type's own default" (`marion-supervisor::main` reads it with `as_str()`), and a JSON
    // `null` is not the same thing to every harness: measured here, gemini 0.53.0 validates a tool
    // call against the declared schema *before* dispatching it and refuses `"model": null` with
    // `params/model must be string` — an `invalid_tool_params` tool_result, after which the root
    // happily finished its turn having spawned nothing.
    let spawn_args = json!({
        "agent_type": child.agent_type,
        "prompt": CHILD_PROMPT,
        "acceptance_criteria": ["a file exists under src/ containing the marker"],
        "writable_scope": ["src/**"],
        "timeout_secs": CHILD_TIMEOUT_SECS,
        "model": child.model,
    });
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
        Harness::Gemini => {
            s.gemini_report_tool = report;
            s.gemini_report_args = json!({ "narrative": NARRATIVE });
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
    timeout: &str,
) -> Vec<String> {
    let prompt = format!("{ROOT_MARKER}: delegate the marker-file task to a child.");
    let args: Vec<String> = vec![
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
        timeout.into(),
        // Passed for every root, including the two whose `--model` is omissible: see
        // [`Node::model`]. What the adapter does with it is the cell's assertion, not the
        // builder's.
        "--model".into(),
        root.model.into(),
    ];
    args
}

/// **The regression this file's launch path can suffer without any cell being wrong.**
///
/// Sixteen cells go through [`marion_argv`], and all sixteen fail identically — at argument
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
    for node in [&CLAUDE, &CODEX, &GEMINI, &OPENCODE] {
        for base_url in ["http://127.0.0.1:8080/v1", "http://localhost:1/v1"] {
            let args = marion_argv(
                node,
                Path::new("/tmp/repo"),
                Path::new("/tmp/state"),
                base_url,
                ROOT_BLOCKED_SECS,
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
    let args = marion_argv(root, &repo, &state, &server.base_url(), timeout);

    let out = run_bounded(
        Command::new(env!("CARGO_BIN_EXE_marion"))
            .args(&args)
            .current_dir(&dir),
        RUN_BOUND,
    )
    .expect("marion run starts");

    let requests = server.requests().unwrap_or_default();
    // §4.3's location, resolved the one way marion resolves it — never a second literal here.
    let journal = read_path(&ProjectDir::new(&state, &repo).journal()).map_err(|e| e.to_string());
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
/// Four wires, four frames, and none of them is a substring scan: each finds the *structured*
/// result of the root's own call, by the id the provider minted for it (or, on Gemini, by the tool
/// name — the CLI mints its own call id, so ours is not there to match).
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
        // Responses: a `function_call_output` item quoting our `call_id`. Its `output` is **not**
        // the MCP result — codex 0.146.0 frames it as `Wall time: <n> seconds\nOutput:\n<blocks>`,
        // where `<blocks>` is the MCP content list. Unwrapped structurally rather than by hunting
        // for a `{`: the contract itself is full of braces.
        "responses" => {
            let output =
                body["input"].as_array()?.iter().find(|i| {
                    i["type"] == "function_call_output" && i["call_id"] == ROOT_CALL_ID
                })?["output"]
                    .as_str()?;
            let blocks = output.split_once("\nOutput:\n").unwrap_or_else(|| {
                panic!(
                    "codex framed its mcp result as something other than `…\\nOutput:\\n<blocks>`, \
                     so this cell cannot read what the root was handed:\n{output}"
                )
            });
            let parsed: Value = serde_json::from_str(blocks.1).unwrap_or_else(|e| {
                panic!("codex's `Output:` section is not JSON: {e}\n{}", blocks.1)
            });
            mcp_blocks_text(&parsed)
        }
        // Gemini: a `functionResponse` part. The CLI mints its own call id, so the match is on the
        // tool name; and it wraps every MCP result in `<untrusted_context>` before showing it to
        // the model, which is stripped here rather than tolerated by a substring assertion.
        "gemini" => {
            let tool = spawn_tool(root);
            let output =
                body["contents"]
                    .as_array()?
                    .iter()
                    .filter_map(|c| c["parts"].as_array())
                    .flatten()
                    .find(|p| p["functionResponse"]["name"] == tool.as_str())?["functionResponse"]
                    ["response"]["output"]
                    .as_str()?;
            let inner = output
                .trim()
                .strip_prefix("<untrusted_context>")
                .and_then(|s| s.strip_suffix("</untrusted_context>"))
                .unwrap_or(output);
            Some(inner.trim().to_string())
        }
        // Chat Completions: the `role: "tool"` message answering our `tool_call_id`, whose content
        // is the MCP text flattened by opencode itself.
        "openai" => Some(
            body["messages"]
                .as_array()?
                .iter()
                .find(|m| m["role"] == "tool" && m["tool_call_id"] == ROOT_CALL_ID)?["content"]
                .as_str()?
                .to_string(),
        ),
        w => panic!("no reader for wire {w:?}; a fifth wire needs its own frame here"),
    }
}

/// An MCP `content` payload — a block list, or the bare string a harness may flatten it to — as
/// text. Shared by the two wires that hand the block list through unflattened.
fn mcp_blocks_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

/// Drop everything §6.7's cap rules 0–6 may shorten, **and the metadata that records the
/// shortening**, from a contract's JSON.
///
/// The returned copy is legitimately *shorter* than the persisted one — that is what the caps are
/// for — so the comparison cannot be byte equality, and §9 says exactly which fields it normalizes:
/// *"every `Capped.truncated`/`original_bytes` pair and every `*_omitted` counter may differ
/// between the two copies […] The comparison normalizes both the shortened fields and their
/// metadata; it is not an equality over the metadata."* Everything left — the ids, the repo, the
/// workspace, the scope lists, the status, the flags, the exit, the timestamps — must match byte
/// for byte. `m1_hop.rs` normalizes the same set for the same reason.
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

/// The eleven assertions every cell makes, in the order that makes a failure most diagnosable: the
/// run's own outcome first, then what the provider saw, then what marion recorded, then what the
/// root was actually handed, and last the two whole-run properties — the tree's terminal states and
/// the leak check.
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
    if child.child_writes_worktree {
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
             scope_violations: [], scope_enforced: true.\nRequest log:\n{}",
            comp.changed_paths,
            ev.log_summary()
        );
        assert!(
            comp.scope_violations.is_empty(),
            "{cell}: {CHILD_FILE} is inside the `src/**` this cell's spawn asked for, so a \
             violation here means the scope comparison, not the child, is wrong: {:?}",
            comp.scope_violations
        );
    } else {
        // **A pinned gap, not an assertion that marion is broken.** This harness declares no write
        // tool to the model at all (see `Node::child_writes_worktree` for the per-harness
        // measurement), so its child cannot change its worktree and this cell cannot witness
        // `changed_paths`. Pinned rather than passed over so the gap is visible in the file that
        // has it: **if this ever fails, that is good news** — the harness has gained a write route,
        // and this cell should be moved to the branch above by giving the node
        // `child_writes_worktree: true` and scripting its edit in `script()`.
        assert!(
            comp.changed_paths.is_empty(),
            "{cell}: this cell's child has no write tool declared to it, so it could not have \
             changed anything — yet changed_paths is {:?}. If the harness has gained a write \
             route, set `child_writes_worktree: true` on this node and script its edit in \
             `script()`; the assertion above is the one this cell should be making.\n\
             Request log:\n{}",
            comp.changed_paths,
            ev.log_summary()
        );
    }

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
    let returned: TaskContract = serde_json::from_str(&result_text).unwrap_or_else(|e| {
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
