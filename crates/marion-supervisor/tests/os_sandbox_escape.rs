//! **A contained node cannot write outside its workspace**, on every row marion's own OS sandbox
//! contains — the claim behind each of those rows' `Contained` label, made by the real harness.
//!
//! Each cell spawns one real child on the canned provider, whose one tool call is the harness's own
//! shell running `echo in > inside.txt; echo out > <the operator's $HOME>/marion-escape-test-<row>`.
//! The file inside the worktree shows the command ran; the file in the operator's home must not
//! exist afterwards. The home path is absolute and read from this process, because a canned node's
//! own `$HOME` is a directory in its agent dir, which it may write. Any such file is removed
//! whatever the cell's outcome.

use std::path::PathBuf;

use marion_core::contract::{Isolation, TaskId};
use marion_provider::{CannedServer, Config, NodeScript, Script, ScriptedCall};
use marion_supervisor::run::{Caller, SpawnRequest, run_spawn};
use marion_testsupport::{fixture_repo, on_path, pinned_version, scratch};
use serde_json::json;

mod common;

use common::canned::canned_env;

/// The marker in each child's prompt that picks its script.
const MARKER: &str = "MARION-SANDBOX-ESCAPE-7c41";

/// A harness's shell tool and how its model spells a command for it.
type Shell = (&'static str, fn(&str) -> serde_json::Value);

/// One row: its built-in type, its program, and how its model calls the harness's shell.
struct Row {
    agent_type: &'static str,
    program: &'static str,
    /// The shell tool and its arguments, on this harness's wire; `None` for codex, whose shell is
    /// a code-mode `exec`.
    shell: Option<Shell>,
}

fn command_only(cmd: &str) -> serde_json::Value {
    json!({ "command": cmd })
}

fn command_described(cmd: &str) -> serde_json::Value {
    json!({ "command": cmd, "description": "Write two files" })
}

/// The escape target in the operator's own home.
fn outside(tag: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("a HOME"))
        .join(format!("marion-escape-test-{tag}"))
}

/// The provider's script: one shell call, then a report.
fn script(row: &Row, cmd: &str) -> Script {
    match row.shell {
        Some((tool, args)) => Script {
            nodes: vec![NodeScript {
                marker: MARKER.into(),
                call_prefix: "escape".into(),
                turns: vec![ScriptedCall::new(tool, args(cmd))],
                final_text: "Tried both writes.".into(),
            }],
            ..Script::default()
        },
        None => Script {
            child_exec_js: Some(format!(
                "// @exec: {{\"yield_time_ms\": 30000, \"max_output_tokens\": 2000}}\n\
                 const r = await tools.exec_command({{ cmd: {}, shell: \"/bin/sh\", login: false, \
                 yield_time_ms: 30000, max_output_tokens: 2000 }});\n\
                 text(JSON.stringify({{r}}));\n",
                serde_json::to_string(cmd).unwrap()
            )),
            ..Script::default()
        },
    }
}

fn cell(row: &Row) {
    assert!(
        on_path(row.program),
        "this cell drives a REAL {} ({}); put it on PATH",
        row.program,
        pinned_version(row.program)
    );
    assert!(
        marion_harness::os_sandbox::support().available(),
        "{}",
        marion_harness::os_sandbox::support().describe()
    );
    let target = outside(row.agent_type);
    let _ = std::fs::remove_file(&target);
    let dir = scratch(&format!("escape-{}", row.agent_type));
    let repo = fixture_repo(&dir);
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let cmd = format!("echo in > inside.txt; echo out > '{}'", target.display());
    let server = CannedServer::start(Config {
        addr: ([127, 0, 0, 1], 0).into(),
        reqlog: dir.join("provider-requests.jsonl"),
        script: script(row, &cmd),
    })
    .expect("the canned provider binds");
    let env = canned_env(&state, &repo, Some(server.base_url()));
    assert!(env.os_sandbox, "the sandbox is on for this tree");
    let req = SpawnRequest {
        budget: None,
        review: None,
        agent_type: row.agent_type.into(),
        prompt: format!("{MARKER}: write inside.txt, then try to write outside the workspace."),
        repo: repo.clone(),
        acceptance_criteria: vec!["inside.txt exists".into()],
        verification: vec![],
        writable_scope: vec!["**".into()],
        timeout_secs: 120,
        model: None,
        isolation: Isolation::Worktree,
        allow_concurrent_writes: false,
        resume: None,
        profile: None,
        race: None,
        read_only: false,
        workflow: None,
    };
    let caller = Caller::root(
        "root",
        marion_core::agent_type::builtin("claude").expect("the root type resolves"),
    );
    let contract = run_spawn(
        &env,
        &req,
        &TaskId(format!("escape-{}", row.agent_type)),
        &caller,
    );
    let escaped = target.exists();
    let _ = std::fs::remove_file(&target);
    let contract = contract.unwrap_or_else(|e| panic!("{}: the spawn ran: {e}", row.agent_type));
    let inside = contract.workspace.path().join("inside.txt");
    assert!(
        inside.exists() || changed(&contract, "inside.txt"),
        "{}: the command never ran, so the escape was never tried; the tools the harness \
         offered its model: {:?}",
        row.agent_type,
        tools_offered(&dir.join("provider-requests.jsonl"))
    );
    assert!(
        !escaped,
        "{}: the node wrote {} outside its workspace",
        row.agent_type,
        target.display()
    );
}

/// Every tool name the harness offered its model, from the provider's request log.
fn tools_offered(reqlog: &std::path::Path) -> Vec<String> {
    let text = std::fs::read_to_string(reqlog).unwrap_or_default();
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .flat_map(|r| {
            let body = r["body"].to_string();
            body.split("\"name\":\"")
                .skip(1)
                .filter_map(|rest| rest.split('"').next().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether the contract's change record names `file` — the worktree may already be reaped.
fn changed(contract: &marion_core::contract::TaskContract, file: &str) -> bool {
    contract
        .completion
        .as_ref()
        .is_some_and(|c| c.changed_paths.iter().any(|p| p.ends_with(file)))
}

#[test]
fn a_claude_node_cannot_write_outside_its_workspace() {
    cell(&Row {
        agent_type: "claude",
        program: "claude",
        shell: Some(("Bash", command_only)),
    });
}

#[test]
fn a_codex_node_cannot_write_outside_its_workspace() {
    cell(&Row {
        agent_type: "codex",
        program: "codex",
        shell: None,
    });
}

/// **Not reachable through a shell yet**: this row's harness denies its shell tool in a headless
/// canned run ("Permission denied and could not request permission"), so no command runs and
/// the cell would prove nothing. The row's launch is still sandboxed (`os_sandbox` unit tests);
/// a write-tool cell for it is phase 2's.
#[ignore = "the harness denies its shell in a headless canned run; see the doc"]
#[test]
fn a_gemini_node_cannot_write_outside_its_workspace() {
    cell(&Row {
        agent_type: "gemini",
        program: "gemini",
        shell: Some(("run_shell_command", command_only)),
    });
}

#[test]
fn an_opencode_node_cannot_write_outside_its_workspace() {
    cell(&Row {
        agent_type: "opencode",
        program: "opencode",
        shell: Some(("bash", command_described)),
    });
}

/// **Not reachable through a shell yet**: this row's harness denies its shell tool in a headless
/// canned run ("Permission denied and could not request permission"), so no command runs and
/// the cell would prove nothing. The row's launch is still sandboxed (`os_sandbox` unit tests);
/// a write-tool cell for it is phase 2's.
#[ignore = "the harness denies its shell in a headless canned run; see the doc"]
#[test]
fn a_copilot_node_cannot_write_outside_its_workspace() {
    cell(&Row {
        agent_type: "copilot",
        program: "copilot",
        shell: Some(("bash", command_described)),
    });
}

#[test]
fn a_goose_node_cannot_write_outside_its_workspace() {
    cell(&Row {
        agent_type: "goose",
        program: "goose",
        shell: Some(("developer__shell", command_only)),
    });
}

#[test]
fn a_qwen_node_cannot_write_outside_its_workspace() {
    cell(&Row {
        agent_type: "qwen",
        program: "qwen",
        shell: Some(("run_shell_command", command_only)),
    });
}

#[test]
fn a_pi_node_cannot_write_outside_its_workspace() {
    cell(&Row {
        agent_type: "pi",
        program: "pi",
        shell: Some(("bash", command_only)),
    });
}
