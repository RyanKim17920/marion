//! The Qwen Code adapter — the headless `-p` surface (fixture `tests/fixtures/s25/`).
//!
//! Measured against **qwen 0.23.0** on 2026-09-05, every probe against a canned local OpenAI
//! Chat Completions endpoint at $0.00. qwen is a Gemini CLI fork, and the row is nothing like
//! gemini's: its headless surface has since become **Claude Code's shape** — the same
//! `system`/`init`, `assistant` `tool_use`, `user` `tool_result` and `result` frames, the same
//! `mcp__<server>__<tool>` spelling, the same `--resume <session_id>` — over an OpenAI provider
//! selected by env alone. So the row reads its stream through [`crate::claude_code::STREAM`],
//! stated once as a measured fact rather than transcribed twice.
//!
//! What is *not* like the other six, and is the content of this module:
//!
//! - **MCP tools are deferred behind `tool_search` by default.** Discovery runs in the background
//!   and anything that connects after the first declaration list is built is reachable only by
//!   the model first calling `tool_search` — for the whole session. `QWEN_CODE_LEGACY_MCP_BLOCKING=1`
//!   makes discovery block startup, and the tool is in `tools[]` on request one; without it a
//!   canned model that never calls `tool_search` never reaches marion (item 3).
//! - **Three axes of tool restriction, and one combination reaches the declared names.**
//!   `--core-tools <names…>` is a legacy allowlist with **twelve exempt survivors** (`agent`,
//!   `tool_search`, `skill`, the goal and team tools, …); `--exclude-tools` naming those twelve on
//!   top leaves exactly the declared list. `--bare` ignores `--core-tools`. So the row carries
//!   both flags, and the audit record is the `--core-tools` list prefixed with its axis.
//! - **Without `--yolo`, headless mode asks a model.** `permission_mode: "auto"` sends the
//!   provider a classifier request per tool call and, unanswered in schema, declines the call
//!   with `is_error: true` at exit 0. `--yolo` runs everything; its one stderr warning is silenced
//!   by `QWEN_CODE_SUPPRESS_YOLO_WARNING=1`.
//! - **A dead provider is retried 28 times over 86 s and then reported as success.** Seven outer
//!   attempts of four inner tries, then an `assistant` text `[API Error: 500 …]` and
//!   `result.is_error: false`, exit 0. The only bound is marion's own timeout; the grammar's
//!   conditions match whole values, so the fault reads as the report that never came.
//! - **Hidden side turns get tools.** An env-only run makes a second request after the answer —
//!   a "managed memory extraction subagent" prompt offered `write_file`, `edit` and
//!   `run_shell_command`. `memory.enableManagedAutoMemory: false` in `settings.json` removes it,
//!   and a canned node's settings document carries that switch beside its declaration.
//! - **Two routes, as codex has.** A canned node owns its `$QWEN_HOME`, so the declaration sits in
//!   `settings.json` beside the memory switch; a live node's `~/.qwen/settings.json` is the
//!   operator's, so the declaration rides `--mcp-config` inline on argv, measured without `--bare`
//!   (item 15). `QWEN_HOME` relocates settings, sessions, usage and memories; `HOME` still
//!   contributes the operator's `~/.agents/skills` to the prompt (item 11) and is left alone.

use std::path::{Path, PathBuf};

use marion_core::agent_type;
use marion_core::harness::Harness;
use serde_json::{Value, json};

pub use crate::mcp_bridge::BridgeEnv;
use crate::spec::{
    Arg, Constraint, Env, Field, HarnessSpec, LiveDeclaration, McpRoute, McpRoutes, Push, Resume,
    Spelling, Surfaces, ToolSpelling, UpdatePolicy, Val, When,
};

/// `$QWEN_HOME`'s name under the node's config dir — one spelling for [`SPEC`]'s env row and
/// [`home`].
const HOME_DIR: &str = "home";

/// [`settings_json`]'s file name under [`home`]: `$QWEN_HOME/settings.json`, the user-settings
/// layer qwen reads and rewrites (it appends `"$version": 4`).
pub const SETTINGS_FILE: &str = "settings.json";

/// Qwen Code's row. Measured against 0.23.0 on 2026-09-05 (`tests/fixtures/s25/`), every probe
/// against a canned local OpenAI Chat Completions endpoint at $0.00.
///
/// **Live mode is a removal.** The home relocation and the provider block go together. The
/// survivors — the blocking-discovery switch, the warning silencer, and a `OPENAI_MODEL` where the
/// launch names one — are launch requirements, not isolation.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Qwen,
    surfaces: Surfaces::LaunchOnly,
    program: Some("qwen"),
    argv: &[
        // First, as measured (`qwen-resume-turn-2.argv.json`).
        Arg::Resume,
        // `permission_mode: "yolo"`; without it every call costs a classifier request and is
        // declined at exit 0 (item 6).
        Arg::Lit("--yolo"),
        // What the model is offered: marion's verbs plus the declared built-ins, space-separated
        // (`qwen-write-then-report.argv.json`). Never empty — the adapter refuses first.
        Arg::Lit("--core-tools"),
        Arg::Items(Field::Tools),
        // The twelve names `--core-tools` cannot withhold (`qwen-core-tools-alone.provider-request-1.json`).
        Arg::Lit("--exclude-tools"),
        Arg::Lit("agent"),
        Arg::Lit("enter_worktree"),
        Arg::Lit("exit_worktree"),
        Arg::Lit("get_goal"),
        Arg::Lit("list_agents"),
        Arg::Lit("record_artifact"),
        Arg::Lit("report_findings"),
        Arg::Lit("send_message"),
        Arg::Lit("skill"),
        Arg::Lit("task_stop"),
        Arg::Lit("tool_search"),
        Arg::Lit("update_goal"),
        Arg::Flag("--mcp-config", Field::McpConfig),
        // `-p` is deprecated for a positional prompt and is what every capture used (item 16); a
        // bare positional is a different, unmeasured launch.
        Arg::Flag("-p", Field::Prompt),
        Arg::Lit("--output-format"),
        Arg::Lit("stream-json"),
    ],
    pane: None,
    env: &[
        Env {
            key: HOME_ENV,
            val: Val::Under(HOME_DIR),
            when: When::Canned,
        },
        Env {
            key: BASE_URL_ENV,
            val: Val::Field(Field::BaseUrl),
            when: When::Canned,
        },
        Env {
            key: API_KEY_ENV,
            val: Val::Field(Field::ApiKey),
            when: When::Canned,
        },
        // Present or absent: a live launch that names no model leaves the operator's own.
        Env {
            key: MODEL_ENV,
            val: Val::Field(Field::Model),
            when: When::Always,
        },
        Env {
            key: MCP_BLOCKING_ENV,
            val: Val::Lit("1"),
            when: When::Always,
        },
        Env {
            key: SUPPRESS_YOLO_WARNING_ENV,
            val: Val::Lit("1"),
            when: When::Always,
        },
    ],
    // Claude Code's grammar, measured frame for frame on 0.23.0 (item 2): `system`/`init` with
    // `session_id`, `assistant` `tool_use` `{id, name, input}`, `user` `tool_result`
    // `{tool_use_id, is_error, content}`, `result` `{subtype, is_error}`.
    stream: Some(&crate::claude_code::STREAM),
    // Both measured in `tools[]` under `--core-tools`: `write_file` lands a file
    // (`qwen-write-then-report.stdout.jsonl`), `read_file` is one of the 28 defaults.
    tool_names: &[
        (agent_type::TOOL_READ, "read_file"),
        (agent_type::TOOL_WRITE, "write_file"),
    ],
    // `mcp__<server>__<tool>` — Claude Code's spelling, measured in `tools[]`, in `tool_use.name`
    // and in `permission_denials[].tool_name`.
    spelling: Spelling::Fixed(ToolSpelling::McpDoubleUnderscore),
    // codex's two routes, for codex's reason: a canned node owns its `$QWEN_HOME`, so the
    // declaration sits in the settings document beside the memory switch
    // (`qwen-write-then-report.settings.json`); a live node's `~/.qwen/settings.json` is the
    // operator's, so the declaration rides `--mcp-config` inline (`qwen-mcp-config-argv.argv.json`).
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Argv(MCP_CONFIG_KEY),
    },
    live_declaration: Some(LiveDeclaration::ArgvInline {
        flag: "--mcp-config",
        key: MCP_CONFIG_KEY,
        body: mcp_config_document,
    }),
    // The `--core-tools` list itself, prefixed with its axis: the literal contents of what the
    // model was offered.
    constraint: Constraint::Allowed {
        prefix: "core-tools:",
    },
    // `--resume <session_id>`, the id off `init.session_id`, keyed by `QWEN_HOME` **and** cwd
    // (item 13): turn two replays turn one's history and repeats the id.
    resume: Some(Resume::Flag("--resume")),
    // qwen 0.23.0 reads `QWEN_CODE_SKIP_UPDATE_CHECK_ONCE` as the exact string `"true"` on
    // **every** launch despite the `_ONCE` in its name, and skips the check, the banner and the
    // install. The settings spelling (`general.enableAutoUpdate=false`) does the same; the
    // variable is preferred because it needs no document on the live route.
    updates: UpdatePolicy::Env {
        key: "QWEN_CODE_SKIP_UPDATE_CHECK_ONCE",
        value: "true",
        note: "0.23.0 bundle: the update check returns on `QWEN_CODE_SKIP_UPDATE_CHECK_ONCE === \
               \"true\"` at every launch",
    },
    // Unmeasured: MCP's own logging notification, which this harness may show or drop.
    push: Push::McpLog,
    client_name: None,
    note: "S25 on qwen 0.23.0: the -p surface as Claude Code's shape over an env-only OpenAI \
           provider, blocking MCP discovery, --core-tools plus --exclude-tools as the one \
           combination that offers the declared names, --mcp-config inline as the declaration \
           route in both modes, --resume <session_id>; harness_matrix's qwen cell runs this row \
           end to end",
};

/// Relocates `settings.json`, `projects/<cwd-slug>/chats/<session>.jsonl`, `usage/`,
/// `installation_id` and `memories/`; `~/.qwen` kept its mtime across every run. **Dropped under
/// [`Auth::Inherited`]**: relocating it is what hides the operator's own login.
pub const HOME_ENV: &str = "QWEN_HOME";
/// Taken **verbatim** in marion's `…/v1` form; the CLI appends `/chat/completions`. **Dropped
/// under [`Auth::Inherited`]**.
pub const BASE_URL_ENV: &str = "OPENAI_BASE_URL";
/// Sent as `Authorization: Bearer …`. **Dropped under [`Auth::Inherited`]**.
pub const API_KEY_ENV: &str = "OPENAI_API_KEY";
/// The model every request names. Mandatory under canned: without one and without a settings
/// `model.name` the CLI has nothing to send, and marion will not guess.
pub const MODEL_ENV: &str = "OPENAI_MODEL";
/// `1` makes MCP discovery block startup, so the server's tools are in `tools[]` on request one
/// instead of deferred behind `tool_search` (item 3). Kept under **both** modes: a live node whose
/// marion tools the model has to search for first is a node that reaches marion by luck.
pub const MCP_BLOCKING_ENV: &str = "QWEN_CODE_LEGACY_MCP_BLOCKING";
/// `1` silences `--yolo`'s one stderr line. Kept under both modes: hygiene, not isolation.
pub const SUPPRESS_YOLO_WARNING_ENV: &str = "QWEN_CODE_SUPPRESS_YOLO_WARNING";

/// The top-level key of the `--mcp-config` document — the needle the argv route is checked for.
pub const MCP_CONFIG_KEY: &str = "mcpServers";

/// The MCP server alias. The model-facing tool name is `mcp__<alias>__<tool>`.
pub const MCP_ALIAS: &str = crate::spec::MCP_ALIAS;

/// `$QWEN_HOME` for a node: a directory under marion's own config dir, never `~/.qwen`.
pub fn home(config_dir: &Path) -> PathBuf {
    config_dir.join(HOME_DIR)
}

/// Where [`settings_json`] is written: `$QWEN_HOME/settings.json`.
pub fn settings_path(config_dir: &Path) -> PathBuf {
    home(config_dir).join(SETTINGS_FILE)
}

/// The canned node's `settings.json` (`qwen-write-then-report.settings.json`): the memory side
/// turn switched off (item 9), and the declaration where one was asked for. The provider rides
/// env, so nothing else is here.
pub fn settings_json(mcp: Option<&BridgeEnv>) -> Value {
    let mut settings = json!({
        "memory": { "enableManagedAutoMemory": false }
    });
    if let Some(b) = mcp {
        settings[MCP_CONFIG_KEY] = mcp_servers_json(b);
    }
    settings
}

/// The `mcpServers` block, one shape for both routes: `command`, `args` and the bridge's `env`.
///
/// `ready_file` is `None` on this surface: under [`MCP_BLOCKING_ENV`] the CLI itself holds turn
/// one for discovery, and `init.mcp_servers[]` says `connected` before the first request.
pub fn mcp_servers_json(b: &BridgeEnv) -> Value {
    json!({
        MCP_ALIAS: {
            "command": b.bridge.to_string_lossy(),
            "args": b.args,
            "env": b.env_json(),
        }
    })
}

/// The `--mcp-config` document, inline: `{"mcpServers": {…}}` as one argv token
/// (`qwen-mcp-config-argv.argv.json`).
pub fn mcp_config_document(b: &BridgeEnv) -> String {
    json!({ MCP_CONFIG_KEY: mcp_servers_json(b) }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITE_THEN_REPORT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s25/qwen-write-then-report.stdout.jsonl"
    ));
    const REPORT_ISERROR: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s25/qwen-report-iserror.stdout.jsonl"
    ));
    const DENIED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s25/qwen-denied-without-yolo.stdout.jsonl"
    ));
    const PROVIDER_500: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s25/qwen-provider-500.stdout.jsonl"
    ));
    const NO_AUTH: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s25/qwen-no-auth.stdout.jsonl"
    ));
    const RESUME_TURN_2: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s25/qwen-resume-turn-2.stdout.jsonl"
    ));

    use marion_core::contract::AgentId;

    use crate::auth::Auth;
    use crate::invocation::Invocation;
    use crate::spec::{Axes, Fields, Shape, render};
    use crate::stream::{CallOutcome, MarionCall, StreamOutcome};

    fn bridge() -> BridgeEnv {
        BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-child".into()),
            agent_type: "qwen".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
        }
    }

    /// A canned node as the adapter's hooks shape it: marion's verb on the availability axis, the
    /// declaration inline. The hooks' refusals are pinned in `adapter::tests`; these pin **the row**.
    fn spec() -> Fields {
        Fields {
            cwd: "/tmp/wt".into(),
            config_dir: "/tmp/cfg".into(),
            auth: Auth::Canned,
            prompt: "do the task".into(),
            model: Some("canned-1".into()),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("sk-fake".into()),
            axes: Axes {
                tools: vec!["mcp__marion__report".into()],
                allowed: vec!["mcp__marion__report".into()],
                mode: None,
            },
            ..Fields::default()
        }
    }

    /// Live: the declaration rides argv, because the settings document is the operator's.
    fn live_spec() -> Fields {
        Fields {
            base_url: None,
            api_key: None,
            auth: Auth::Inherited,
            mcp_config: Some(mcp_config_document(&bridge())),
            ..spec()
        }
    }

    fn compile(f: &Fields) -> Invocation {
        render(&SPEC, Shape::Headless, f).unwrap()
    }

    fn env_of(inv: &Invocation, k: &str) -> Option<String> {
        inv.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn prefix() -> String {
        ToolSpelling::McpDoubleUnderscore.spell("")
    }

    fn parse_stream(s: &str) -> StreamOutcome {
        crate::grammar::parse_stream(&crate::claude_code::STREAM, s, &prefix())
    }

    fn marion_calls(s: &str) -> Vec<MarionCall> {
        crate::grammar::marion_calls(&crate::claude_code::STREAM, s, &prefix())
    }

    #[test]
    fn the_argv_is_the_measured_yolo_core_tools_exclude_tools_surface() {
        let inv = compile(&spec());
        assert_eq!(inv.program, "qwen");
        assert_eq!(inv.args[0], "--yolo");
        let core = inv.args.iter().position(|a| a == "--core-tools").unwrap();
        let excl = inv
            .args
            .iter()
            .position(|a| a == "--exclude-tools")
            .unwrap();
        assert_eq!(&inv.args[core + 1..excl], ["mcp__marion__report"]);
        let p = inv.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(excl + 13, p, "twelve exempt survivors are excluded");
        assert!(
            !inv.args.contains(&"--mcp-config".to_string()),
            "a canned node's declaration is in its settings document"
        );
        assert_eq!(inv.args[p + 1], "do the task");
        assert_eq!(&inv.args[p + 2..], ["--output-format", "stream-json"]);
        assert_eq!(inv.cwd, PathBuf::from("/tmp/wt"));
    }

    #[test]
    fn a_write_declaration_joins_the_core_tools_list() {
        let mut f = spec();
        f.axes.tools.push("write_file".into());
        let inv = compile(&f);
        let core = inv.args.iter().position(|a| a == "--core-tools").unwrap();
        assert_eq!(
            &inv.args[core + 1..core + 3],
            ["mcp__marion__report", "write_file"]
        );
    }

    #[test]
    fn the_canned_provider_block_is_env_and_the_home_is_relocated() {
        let inv = compile(&spec());
        assert_eq!(env_of(&inv, HOME_ENV).as_deref(), Some("/tmp/cfg/home"));
        assert_eq!(
            env_of(&inv, BASE_URL_ENV).as_deref(),
            Some("http://127.0.0.1:8099/v1")
        );
        assert_eq!(env_of(&inv, API_KEY_ENV).as_deref(), Some("sk-fake"));
        assert_eq!(env_of(&inv, MODEL_ENV).as_deref(), Some("canned-1"));
        assert_eq!(env_of(&inv, MCP_BLOCKING_ENV).as_deref(), Some("1"));
        assert_eq!(
            env_of(&inv, SUPPRESS_YOLO_WARNING_ENV).as_deref(),
            Some("1")
        );
        assert_eq!(
            settings_path(&spec().config_dir),
            PathBuf::from("/tmp/cfg/home/settings.json")
        );
    }

    #[test]
    fn live_mode_removes_the_home_and_the_provider_and_keeps_the_switches() {
        let inv = compile(&live_spec());
        for k in [HOME_ENV, BASE_URL_ENV, API_KEY_ENV] {
            assert_eq!(env_of(&inv, k), None, "{k} survived live mode");
        }
        assert_eq!(env_of(&inv, MCP_BLOCKING_ENV).as_deref(), Some("1"));
        assert_eq!(env_of(&inv, MODEL_ENV).as_deref(), Some("canned-1"));
        let mcp = inv.args.iter().position(|a| a == "--mcp-config").unwrap();
        assert!(inv.args[mcp + 1].starts_with("{\"mcpServers\":{\"marion\":"));
        assert!(inv.args[mcp + 1].contains("/bin/marion-supervisor"));
    }

    #[test]
    fn a_resume_is_the_first_two_tokens() {
        let mut f = spec();
        f.resume = Some("<UUID-1>".into());
        let inv = compile(&f);
        assert_eq!(&inv.args[..3], ["--resume", "<UUID-1>", "--yolo"]);
    }

    /// The shape `qwen-write-then-report.settings.json` recorded, before qwen's own `$version`.
    #[test]
    fn the_settings_document_switches_the_memory_side_turn_off_and_declares_marion() {
        assert_eq!(
            settings_json(None),
            json!({"memory": {"enableManagedAutoMemory": false}})
        );
        let with = settings_json(Some(&bridge()));
        assert_eq!(with["memory"]["enableManagedAutoMemory"], json!(false));
        assert_eq!(
            with["mcpServers"]["marion"]["command"],
            json!("/bin/marion-supervisor")
        );
        assert_eq!(with["mcpServers"]["marion"]["args"], json!(["mcp"]));
        assert_eq!(
            with["mcpServers"]["marion"]["env"]["MARION_AGENT_ID"],
            json!("019f-child")
        );
    }

    #[test]
    fn the_report_is_read_through_claude_codes_grammar() {
        let out = parse_stream(WRITE_THEN_REPORT);
        assert_eq!(
            out.narrative.as_deref(),
            Some("hello from qwen under a canned provider")
        );
        assert_eq!(out.failure, None);
        assert_eq!(
            marion_calls(WRITE_THEN_REPORT),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered,
            }]
        );
        assert_eq!(
            crate::grammar::session_id(
                &crate::claude_code::STREAM,
                &crate::stream::json_frames(WRITE_THEN_REPORT)[0]
            )
            .as_deref(),
            Some("<UUID-1>")
        );
    }

    /// `result.subtype` stays `success` and the run exits 0; `is_error` on the result is the
    /// verdict, and the words embed the MCP answer.
    #[test]
    fn an_is_error_result_is_a_refusal_at_exit_zero() {
        let calls = marion_calls(REPORT_ISERROR);
        assert_eq!(calls.len(), 1);
        match &calls[0].outcome {
            CallOutcome::Refused(words) => {
                assert!(words.contains("refused: not authorized"), "{words}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// Without `--yolo` the call is declined inside qwen — the same `is_error: true` shape, with
    /// the CLI's own words. marion never compiles that state.
    #[test]
    fn a_call_declined_without_yolo_reads_as_a_refusal() {
        match &marion_calls(DENIED)[0].outcome {
            CallOutcome::Refused(words) => {
                assert!(words.contains("cannot prompt for confirmation"), "{words}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// 28 requests, then success: no failure frame, no call, no narrative.
    #[test]
    fn a_provider_fault_is_the_report_that_never_came() {
        let out = parse_stream(PROVIDER_500);
        assert_eq!(out.narrative, None);
        assert!(marion_calls(PROVIDER_500).is_empty());
        assert_eq!(out.failure, None, "the result frame says success");
    }

    /// `result.subtype: "error_during_execution"`, `is_error: true`, exit 1 — the one qwen fault
    /// that is also an exit code. The grammar reads the subtype; the message sits under
    /// `error.message`, which Claude Code's frame does not carry and the shared grammar does not
    /// read.
    #[test]
    fn a_missing_provider_is_a_failed_result_frame() {
        let out = parse_stream(NO_AUTH);
        assert_eq!(out.failure.as_deref(), Some("error_during_execution"));
    }

    /// Turn two repeats turn one's `session_id` in its `init` frame.
    #[test]
    fn a_resumed_turn_repeats_the_session_id() {
        let frames = crate::stream::json_frames(RESUME_TURN_2);
        assert_eq!(
            crate::grammar::session_id(&crate::claude_code::STREAM, &frames[0]).as_deref(),
            Some("<UUID-1>")
        );
    }
}
