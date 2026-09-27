//! The agy adapter — Google's Antigravity CLI, headless `-p` with `--output-format stream-json`
//! (fixture `tests/fixtures/s32/`).
//!
//! Measured against **agy 1.2.8** on the operator's own keychain login. Not a Gemini CLI fork, and
//! no ACP. The surface is `LaunchOnly`: the prompt rides `-p`, marion reads the NDJSON.
//!
//! Five facts shape the row, each with its fixture:
//!
//! - **The declaration is a directory, not a file.** agy has no MCP-config flag; `--add-dir
//!   <root>` loads `<root>/.agents/mcp_config.json` as that workspace's own MCP servers. marion's
//!   root is `<config_dir>/agy-root` and holds only that document.
//! - **A second workspace moves "the current directory".** With two `--add-dir` roots the model
//!   treats the lexicographically first as its working directory, and with none it does not know
//!   it at all. So the launch names the cwd with `--add-dir` too, and the adapter states the
//!   working directory at the head of the prompt ([`working_directory_preamble`]) — measured
//!   landing the write in the cwd even with marion's root sorting first.
//! - **Headless approval is the operator's.** Headless mode auto-denies every tool it would prompt
//!   for, at exit 0 with `status: SUCCESS`. No flag, variable or marion-owned document approves
//!   marion's tools short of `--dangerously-skip-permissions`, which approves everything; the
//!   operator's own settings do ([`spec::Approval::OperatorAllowlist`]), and `marion doctor`
//!   names the line when it is missing. `--mode accept-edits` approves file edits, so a `write`
//!   declaration compiles it.
//! - **A denied call can look finished.** A call ends `ERROR` with a message, or `DONE` with no
//!   output; `DONE` alone is not an answer ([`crate::grammar::Verdict::TerminalWithOutput`]).
//! - **An unknown `--conversation` starts a new conversation**, exit 0, with only a stderr
//!   warning. The stream names the conversation in its `init` frame, which is what a resume is
//!   checked against.

use marion_core::agent_type;
use marion_core::harness::Harness;
use serde_json::json;

use crate::grammar::{
    ActivityRule, Cond, Failure, Name, OnRefusedReport, Pairing, SessionId, StreamGrammar,
    TextUnit, ToolUnit, UsageFold, UsageRule, Verdict, Where,
};
pub use crate::mcp_bridge::BridgeEnv;
use crate::spec::{
    Approval, Arg, BootDialogs, Constraint, Deliveries, Field, HarnessSpec, LiveDeclaration,
    MCP_ALIAS, McpRoute, McpRoutes, Push, Resume, Spelling, Surfaces, ToolSpelling, TurnDelivery,
    UpdatePolicy,
};

/// marion's workspace root under the node's config dir: the directory `--add-dir` names.
pub const ROOT_DIR: &str = "agy-root";
/// The declaration document's place under [`ROOT_DIR`], where agy looks for a workspace's MCP
/// servers.
pub const MCP_CONFIG_FILE: &str = ".agents/mcp_config.json";
/// The `--mode` value that approves file edits headless (1.2.8 `--help`: `accept-edits, plan`;
/// measured approving `write_to_file`, s32).
pub const ACCEPT_EDITS_MODE: &str = "accept-edits";
/// The mode agy runs in when marion passes no `--mode` — recorded in the audit record too, because
/// a node that ran under it ran under a real constraint: its edits were auto-denied.
pub const DEFAULT_MODE: &str = "default";
/// agy's own name for the tool that edits a file, and the one [`ACCEPT_EDITS_MODE`] is for.
pub const WRITE_TOOL: &str = "write_to_file";

/// agy's row. Measured against 1.2.8 (`tests/fixtures/s32/`).
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Antigravity,
    surfaces: Surfaces::LaunchOnly,
    program: Some("agy"),
    argv: &[
        // `--conversation <id>`, first: a flag like every other, and an unknown id is read back
        // off the stream's `init` frame rather than trusted.
        Arg::Resume,
        Arg::Flag("-p", Field::Prompt),
        Arg::Lit("--output-format"),
        Arg::Lit("stream-json"),
        // Omitted where the launch names none: the operator's own account default.
        Arg::Flag("--model", Field::Model),
        // Only when an edit tool is declared: `accept-edits` approves file edits and nothing else.
        Arg::Flag("--mode", Field::Mode),
        // The cwd as a workspace, or the model has no working directory at all (it runs `pwd`).
        Arg::Flag("--add-dir", Field::Cwd),
        // marion's root, carrying the declaration. See the module docs for why both are named.
        Arg::Flag("--add-dir", Field::McpConfig),
    ],
    pane: None,
    // Nothing to relocate: agy runs on the operator's own login, and there is no canned route.
    env: &[],
    // No canned route and so no overlay to point at an endpoint: agy runs only on the operator's
    // own login, and an endpoint launch is refused like a canned one.
    boot_dialogs: BootDialogs {
        dialogs: &[],
        note: "agy's first screen was not measured for dialogs, and its native lane ships disabled",
    },
    wires: &[],
    // No carrier: agy keeps its login in the keychain, outside any directory it reads.
    profile: None,
    stream: Some(&STREAM),
    // `read` → `view_file`, `write` → `write_to_file`: the names 1.2.8's `init.tools` lists and
    // the model was watched calling (s32).
    tool_names: &[
        (agent_type::TOOL_READ, "view_file"),
        (agent_type::TOOL_WRITE, WRITE_TOOL),
    ],
    spelling: Spelling::Fixed(ToolSpelling::ServerSlashTool),
    // One route: the root's document. There is no canned route to state a second one for; the
    // adapter refuses a canned launch by name.
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Document,
    },
    live_declaration: Some(LiveDeclaration::ArgvRoot {
        flag: "--add-dir",
        root: ROOT_DIR,
        file: MCP_CONFIG_FILE,
        body: mcp_config_document,
    }),
    constraint: Constraint::Mode {
        prefix: "mode:",
        default: DEFAULT_MODE,
    },
    resume: Some(Resume::Flag("--conversation")),
    updates: UpdatePolicy::Env {
        key: "AGY_CLI_DISABLE_AUTO_UPDATE",
        value: "true",
        note: "1.2.8 binary strings: `AGY_CLI_DISABLE_AUTO_UPDATE` is the only update switch; no \
               settings key or flag disables it",
    },
    push: Push::None,
    approval: Approval::OperatorAllowlist {
        file: ".gemini/antigravity-cli/settings.json",
        pointer: "/permissions/allow",
        rule: "mcp(marion/*)",
        note: "s32 on 1.2.8: headless auto-denies `mcp(marion/report)`; --mode accept-edits, \
               --sandbox, ANTIGRAVITY_PERM_GRANTS and a PreToolUse hook answering allow do not \
               approve it; the operator's permissions.allow rule does",
    },
    client_name: Some("antigravity-client"),
    delivery: Deliveries {
        headless: TurnDelivery::Continuation {
            note: "s32 on 1.2.8: `--conversation <id> -p …` continues the conversation under the \
                   same id and remembers the first turn",
        },
        interactive: TurnDelivery::bracketed_paste(
            "s32 tui/ on 1.2.8: DECSET 2004, bracketed paste + CR submits; the native lane ships \
             disabled",
        ),
    },
    note: "s32 on agy 1.2.8: the -p stream-json surface, the --add-dir root declaration, the \
           workspace sort order, headless auto-denial and the operator allow rule, \
           --conversation resume; the gated live test runs this row end to end",
};

/// How an `agy -p --output-format stream-json` stream is read (`tests/fixtures/s32/`).
///
/// A marion call is a `step_update` for the generic `call_mcp_tool` whose parameters name
/// marion's server; the verb is its `ToolName` and the arguments its `Arguments`. Each step is
/// revised in place by `step_index` — `ACTIVE`, then `DONE` or `ERROR` — and `DONE` answers only
/// with an `output`, because a headless denial was measured ending `DONE` with none.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[
            Cond::Eq("/event", "step_update"),
            Cond::Eq("/step_update/step_type", "tool"),
            Cond::Eq("/step_update/tool_name", "call_mcp_tool"),
            Cond::Eq("/step_update/tool_info/parameters/ServerName", MCP_ALIAS),
        ],
        each: None,
        unit: &[],
    },
    name: Name::Verb("/step_update/tool_info/parameters/ToolName"),
    args: "/step_update/tool_info/parameters/Arguments",
    pairing: Pairing::SameUnit {
        id: Some("/step_update/step_index"),
        verdict: Verdict::TerminalWithOutput {
            path: "/step_update/state",
            ok: "DONE",
            err: "ERROR",
            output: "/step_update/tool_info/output",
            words: &["/step_update/tool_info/error/message"],
            fallback: "agy ended the call in ERROR with no message",
            silent: "agy ended the call DONE with no output, which is how it records a call \
                     headless mode auto-denied",
        },
    },
    // A denied report at exit 0 with `status: SUCCESS` is the silent success §6.1 step 8 refuses.
    refused_report: OnRefusedReport::Fail,
    failures: &[Failure::NotOk {
        at: Where {
            frame: &[Cond::Eq("/event", "result")],
            each: None,
            unit: &[],
        },
        path: "/result/status",
        ok: "SUCCESS",
        words: &[],
        label: "agy result status: ",
    }],
    // agy has no canned route, so no provider fault was ever put in front of it.
    errors: &[],
    file_changes: None,
    session: Some(SessionId {
        at: Where {
            frame: &[Cond::Eq("/event", "init")],
            each: None,
            unit: &[],
        },
        path: "/conversation_id",
        resumes_in_place: true,
    }),
    // The terminal `result` totals the run; `input_tokens` excludes cache reads (sC: 12733 input
    // beside 32519 cache reads, `total_tokens` = input + output).
    usage: Some(UsageRule {
        at: Where {
            frame: &[Cond::Eq("/event", "result")],
            each: None,
            unit: &[],
        },
        input: "/result/usage/input_tokens",
        output: "/result/usage/output_tokens",
        cache_read: Some("/result/usage/cache_read_tokens"),
        cache_write: None,
        input_includes_cache: false,
        fold: UsageFold::Last,
    }),
    // Every step is a `step_update` revised in place by `step_index`, so a tool seen `ACTIVE` and
    // then `DONE` is one call; the model's words stream as `agent_response` steps' `text_delta`s
    // (`tests/fixtures/s32/`).
    activity: Some(ActivityRule {
        calls: &[ToolUnit {
            at: Where {
                frame: &[
                    Cond::Eq("/event", "step_update"),
                    Cond::Eq("/step_update/step_type", "tool"),
                ],
                each: None,
                unit: &[],
            },
            name: "/step_update/tool_name",
            args: "/step_update/tool_info/parameters",
            id: Some("/step_update/step_index"),
        }],
        text: &[TextUnit {
            at: Where {
                frame: &[
                    Cond::Eq("/event", "step_update"),
                    Cond::Eq("/step_update/step_type", "agent_response"),
                ],
                each: None,
                unit: &[],
            },
            path: "/step_update/text_delta",
            joins: true,
        }],
    }),
    // Not measured: no usage-window reading was recorded for this harness.
    rate_limit: None,
};

/// The root's declaration document: marion's server under `mcpServers`, the bridge's contract
/// verbatim.
pub fn mcp_config_document(b: &BridgeEnv) -> String {
    json!({
        "mcpServers": {
            MCP_ALIAS: {
                "command": b.bridge.to_string_lossy(),
                "args": b.args,
                "env": b.env_json(),
            }
        }
    })
    .to_string()
}

/// The head of every prompt a launch with marion's root carries: where the node works, and that
/// the root is not it. Without it the model takes whichever workspace sorts first as "the current
/// directory" (s32), which for a marion node is as often marion's root as its worktree.
pub fn working_directory_preamble(cwd: &std::path::Path, root: &std::path::Path) -> String {
    format!(
        "Your working directory is {}; every relative path below is relative to it. {} is \
         marion's configuration directory, not a place for your work.\n\n",
        cwd.display(),
        root.display()
    )
}

/// Is this agy-native tool one [`ACCEPT_EDITS_MODE`] is required for?
pub fn is_edit_tool(native: &str) -> bool {
    native == WRITE_TOOL
}

/// The root `--add-dir` names under a node's config dir.
pub fn root_dir(config_dir: &std::path::Path) -> std::path::PathBuf {
    config_dir.join(ROOT_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::{marion_calls, parse_stream, session_id, usage};
    use crate::stream::CallOutcome;

    macro_rules! fixture {
        ($p:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/fixtures/s32/",
                $p
            ))
        };
    }
    const ANSWERED: &str = fixture!("report-answered.stream.jsonl");
    const DENIED: &str = fixture!("report-denied-error.stream.jsonl");
    const SILENT: &str = fixture!("report-done-without-output.stream.jsonl");
    const WRITE: &str = fixture!("write-then-report.stream.jsonl");
    const RESUMED: &str = fixture!("resume-known.stream.jsonl");

    fn prefix() -> String {
        ToolSpelling::ServerSlashTool.spell("")
    }

    fn outcomes(stream: &str) -> Vec<(String, CallOutcome)> {
        marion_calls(&STREAM, stream, &prefix())
            .into_iter()
            .map(|c| (c.verb, c.outcome))
            .collect()
    }

    #[test]
    fn an_answered_report_is_one_call_revised_from_active_to_done_with_output() {
        assert_eq!(
            outcomes(ANSWERED),
            vec![("report".to_string(), CallOutcome::Answered)]
        );
        assert_eq!(parse_stream(&STREAM, ANSWERED, &prefix()).failure, None);
        assert_eq!(
            outcomes(WRITE),
            vec![("report".to_string(), CallOutcome::Answered)],
            "the write_to_file step before it is not marion's"
        );
    }

    #[test]
    fn a_denied_report_is_a_refusal_in_agys_words_and_fails_the_run() {
        let o = outcomes(DENIED);
        assert!(
            matches!(&o[..], [(v, CallOutcome::Refused(why))]
                if v == "report" && why.contains("permission check failed")),
            "{o:?}"
        );
        let failure = parse_stream(&STREAM, DENIED, &prefix()).failure.unwrap();
        assert!(failure.contains("marion/report"), "{failure}");
    }

    /// The measured trap: `DONE` with no output is what a headless denial leaves behind too.
    #[test]
    fn done_without_output_is_not_an_answer() {
        let o = outcomes(SILENT);
        assert!(
            matches!(&o[..], [(v, CallOutcome::Refused(why))]
                if v == "report" && why.contains("no output")),
            "{o:?}"
        );
        assert!(parse_stream(&STREAM, SILENT, &prefix()).failure.is_some());
    }

    #[test]
    fn a_result_status_other_than_success_is_a_failure() {
        let stream = r#"{"event":"result","result":{"status":"ERROR","response":""}}"#;
        assert_eq!(
            parse_stream(&STREAM, stream, &prefix()).failure.as_deref(),
            Some("agy result status: ERROR")
        );
    }

    #[test]
    fn the_init_frame_names_the_conversation_and_the_result_totals_the_run() {
        let first: serde_json::Value =
            serde_json::from_str(RESUMED.lines().next().unwrap()).unwrap();
        assert_eq!(
            session_id(&STREAM, &first).as_deref(),
            Some("66e0c65e-5fda-41f7-8ddf-c25b84eb67a4")
        );
        let frames = crate::stream::json_frames(ANSWERED);
        let u = usage(STREAM.usage.as_ref().unwrap(), &frames).unwrap();
        assert!(u.input > 0 && u.output > 0, "{u:?}");
    }

    /// An unknown `--conversation` starts a new conversation at exit 0 (s32): a resumed run whose
    /// stream names another conversation is refused, and one that names the resumed one is not.
    #[test]
    fn a_resume_answered_with_another_conversation_is_refused() {
        let unknown = fixture!("resume-unknown.stream.jsonl");
        let why = crate::grammar::resume_refusal(
            &STREAM,
            unknown,
            "0badc0de-0000-4000-8000-000000000000",
        )
        .expect("a fresh conversation under a resume is refused");
        assert!(
            why.contains("0badc0de-0000-4000-8000-000000000000")
                && why.contains("0c2e9a72-d6c6-499f-b947-593d60fc1398"),
            "{why}"
        );
        assert_eq!(
            crate::grammar::resume_refusal(
                &STREAM,
                RESUMED,
                "66e0c65e-5fda-41f7-8ddf-c25b84eb67a4"
            ),
            None
        );
    }

    #[test]
    fn the_declaration_names_marions_server_with_the_bridges_identity() {
        let b = BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: None,
            auth: crate::auth::Auth::Inherited,
            agent_id: marion_core::contract::AgentId("019f-child".into()),
            agent_type: "agy".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
        };
        let v: serde_json::Value = serde_json::from_str(&mcp_config_document(&b)).unwrap();
        let m = &v["mcpServers"]["marion"];
        assert_eq!(m["command"], "/bin/marion-supervisor");
        assert_eq!(m["args"], json!(["mcp"]));
        assert_eq!(m["env"]["MARION_AGENT_ID"], "019f-child");
    }

    #[test]
    fn the_spelling_is_agys_own_server_slash_tool() {
        assert_eq!(
            ToolSpelling::ServerSlashTool.spell("report"),
            "marion/report"
        );
    }
}
