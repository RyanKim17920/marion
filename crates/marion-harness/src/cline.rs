//! The Cline CLI adapter — the headless positional-prompt surface (fixture `tests/fixtures/s27/`).
//!
//! Measured against **cline 3.0.61** on 2026-09-05, every probe against a canned local OpenAI
//! Chat Completions endpoint at $0.00. The shape is opencode's: marion writes the provider
//! document and the declaration, starts the process, and reads its JSONL. There is no channel to
//! steer a turn, so control is `LaunchOnly` and the prompt is the one positional argument.
//!
//! What is *not* like the other six, and is the content of this module:
//!
//! - **The provider is a document, and a misplaced one is a silent vendor fallback.**
//!   `<data>/settings/providers.json` — the exact file `cline auth openai-compatible -k … -m … -b …`
//!   writes — selects the `openai-compatible` provider with its key, model and base URL. A
//!   `providers.json` anywhere else (including at `CLINE_PROVIDER_SETTINGS_PATH`) is **not read**:
//!   cline writes a fresh default selecting its own `cline` provider and POSTs to
//!   `api.cline.bot` with no credential, exit 1, nothing at marion's endpoint. So the document is
//!   written under the data dir and nowhere else, and the model is mandatory because the document
//!   needs one.
//! - **Isolation is three variables *and* two flags, together.** `CLINE_DIR`, `CLINE_DATA_DIR` and
//!   `HOME` relocate config, state and the rules/skills roots; `--config`/`--data-dir` are what set
//!   `sandbox` and keep the CLI from forking a **hub daemon that outlives the run** (`bin/.cline
//!   --cline-hub-daemon`, alive ten minutes later, a lock file with its pid and auth token). Flags
//!   alone still write `$HOME/.cline/data/db/sessions.db`. Only the combination left `home/`
//!   empty, `pgrep` empty and `~/.cline` untouched.
//! - **The built-in tool set cannot be narrowed.** 26 `type: "function"` tools — `editor`,
//!   `run_commands`, `read_files`, `spawn_agent` and nineteen `team_*` — are offered whatever
//!   `global-settings.json` says and whatever flag is passed. So `writes_without_a_declaration`
//!   is `true`, the availability axis answers (`read → read_files`, `write → editor`) and compiles
//!   nothing, and the contract records `harness-default:unconstrained`, as opencode's does.
//! - **The stream is two frame families, and neither is the exit code.** `agent_event` frames
//!   carry the turn: `content_start {contentType: "tool", toolName, toolCallId, input}` is the
//!   call, `content_end` with the same `toolCallId` its verdict — `output.isError` on an MCP
//!   answer, `output.error` on a refusal — and `run_result {finishReason}` closes the run. An
//!   `isError: true` answer is forwarded to the model, `finishReason` stays `completed`, exit 0.
//!   A provider 500 is three tries over ~6 s, then `agent_event error {recoverable: false}`,
//!   `run_result finishReason: "error"`, exit 1.
//! - **The session id is in no frame.** `hook_event` frames carry `agentId`/`taskId`, which are
//!   not it; the id (`<epoch-ms>_<slug>`) is a directory name under `data/sessions/` and a field of
//!   `cline history --json`. `--id <session-id>` forces interactive mode before anything else and
//!   exits 1 headless, so the row's `resume` is `None`.

use std::path::{Path, PathBuf};

use marion_core::agent_type;
use marion_core::harness::Harness;
use serde_json::{Value, json};

use crate::grammar::{
    Cond, Failure, Name, OnRefusedReport, Pairing, StreamGrammar, Verdict, Where,
};
pub use crate::mcp_bridge::BridgeEnv;
use crate::spec::{
    Arg, Constraint, Env, Field, HarnessSpec, LiveDeclaration, McpRoute, McpRoutes, Spelling,
    Surfaces, ToolSpelling, UpdatePolicy, Val, When,
};

/// [`mcp_settings_json`]'s file name under the node's own directory — one spelling for [`SPEC`]'s
/// env row and [`mcp_settings_path`]. Named by [`MCP_SETTINGS_PATH_ENV`] in both auth modes.
pub const MCP_SETTINGS_FILE: &str = "cline_mcp_settings.json";

/// [`mcp_settings_json`] as the bytes `CLINE_MCP_SETTINGS_PATH` reads.
pub fn mcp_settings_document(b: &BridgeEnv) -> String {
    serde_json::to_string_pretty(&mcp_settings_json(b)).expect("a Value always serialises")
}

/// The three directories under the node's config dir: `$CLINE_DIR`, `$CLINE_DATA_DIR`, `$HOME`.
const CONFIG_DIR: &str = "cfg";
const DATA_DIR: &str = "data";
const HOME_DIR: &str = "home";

/// cline's row. Measured against 3.0.61 on 2026-09-05 (`tests/fixtures/s27/`), every probe against
/// a canned local OpenAI Chat Completions endpoint at $0.00.
///
/// **Live mode is a removal.** The three relocation variables, the two isolation flags and the
/// provider document go together: they exist to point a node at marion's endpoint without touching
/// `~/.cline`, and a live node's premise is the opposite. The survivor is the MCP document's path,
/// which is resolved by its own variable independently of the data dir (s27 item 14) and so is the
/// injection route in both modes.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Cline,
    surfaces: Surfaces::LaunchOnly,
    program: Some("cline"),
    argv: &[
        Arg::Lit("--json"),
        Arg::Flag("-c", Field::Cwd),
        // Canned only: see `Arg::Isolation`. Without them the CLI forks a hub daemon that outlives
        // the run; on a live node the operator's own hub is the operator's own business.
        Arg::Isolation("--config", CONFIG_DIR),
        Arg::Isolation("--data-dir", DATA_DIR),
        // Act mode, auto-approve on by default (3.0.61 has no `-y`): stated, because `false` is
        // measured to decline every tool call inside cline at exit 0.
        Arg::Lit("--auto-approve"),
        Arg::Lit("true"),
        // Measured to win over `providers.json`'s model (s27 item 15) — the same model under
        // canned, and a live node's only channel for one. Omitted where the launch names none.
        Arg::Flag("-m", Field::Model),
        // Last, positional, and never empty: an empty prompt is interactive mode, which exits 1
        // headless.
        Arg::Pos(Field::Prompt),
    ],
    pane: None,
    env: &[
        Env {
            key: HOME_ENV,
            val: Val::Under(HOME_DIR),
            when: When::Canned,
        },
        Env {
            key: DIR_ENV,
            val: Val::Under(CONFIG_DIR),
            when: When::Canned,
        },
        Env {
            key: DATA_DIR_ENV,
            val: Val::Under(DATA_DIR),
            when: When::Canned,
        },
        // Both modes: the one file cline reads from a path its own variable names.
        Env {
            key: MCP_SETTINGS_PATH_ENV,
            val: Val::Under(MCP_SETTINGS_FILE),
            when: When::Always,
        },
    ],
    stream: Some(&STREAM),
    // Answered and never compiled: the 26 built-ins are offered whatever the launch says (s27 item
    // 12), so a declaration here changes the contract's record of intent and nothing on argv.
    tool_names: &[
        (agent_type::TOOL_READ, "read_files"),
        (agent_type::TOOL_WRITE, "editor"),
    ],
    // `<serverName>__<toolName>` — measured in `tools[]` and in `content_start.toolName`
    // (`cline-report-ok.provider-request-1.json`, `cline-report-ok.stdout.jsonl`).
    spelling: Spelling::Fixed(ToolSpelling::ServerDoubleUnderscoreTool),
    // A document in both modes, named by `CLINE_MCP_SETTINGS_PATH`, which is resolved without the
    // data dir — so live mode drops the relocation and the injection route survives (s27 item 14).
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Document,
    },
    live_declaration: Some(LiveDeclaration::EnvDocument {
        key: MCP_SETTINGS_PATH_ENV,
        file: MCP_SETTINGS_FILE,
        body: mcp_settings_document,
    }),
    // marion compiles no constraint at all, and says so (opencode's record, for the same reason).
    constraint: Constraint::Fixed {
        prefix: "harness-default:",
        value: "unconstrained",
    },
    // `--id <session-id>` sets the startup target to interactive before the prompt is read, and
    // headless it exits 1 under both `--json` and plain output (s27 item 11).
    resume: None,
    // cline 3.0.61 updates itself **silently, on every invocation** — headless prompt and `--acp`
    // alike: `autoUpdateOnStartup` runs in `runCli` before argv is parsed, fetches npm's latest
    // version, and `applyDeferredUpdate` spawns the package-manager update detached at exit
    // (unless the hub reports other live sessions). Nothing is printed, so there is no banner to
    // notice; the binary just changes under the next node. The bundle's check, verbatim:
    // `if(process.env.CLINE_NO_AUTO_UPDATE==="1")return;` — the strict string `1`, and the only
    // update-related `CLINE_*` variable. The other gate, `autoUpdateEnabled: false` in the data
    // dir's `globalState.json`, is a file a live node does not let marion write.
    updates: UpdatePolicy::Env {
        key: "CLINE_NO_AUTO_UPDATE",
        value: "1",
        note: "3.0.61 `bin/.cline` bundle: `autoUpdateOnStartup` returns on \
               `CLINE_NO_AUTO_UPDATE === \"1\"` before fetching npm's latest; no update-notifier, \
               no banner headless",
    },
    note: "S27 on cline 3.0.61: the positional headless surface, providers.json under the data dir \
           as the provider, CLINE_MCP_SETTINGS_PATH as the declaration route in both modes, the \
           three variables plus two flags that leave no daemon and nothing under ~/.cline; \
           harness_matrix's cline cell runs this row end to end",
};

/// How a `cline --json` stream is read (`tests/fixtures/s27/`).
///
/// A call is an `agent_event` whose `event.type` is `content_start` with `contentType: "tool"`;
/// its result is the `content_end` with the same `toolCallId`. The verdict is `output.isError` —
/// the MCP answer's own flag, forwarded — and a refusal inside cline (`--auto-approve false`)
/// arrives as `output.error` with no `isError` at all, which [`Verdict::ErrorFlag`] reads as
/// answered; that shape is caught by the `agent_event error` failure rule below instead, because
/// cline logs every failed tool call as one. The exit code follows `run_result.finishReason`, so
/// the last rule reads that too.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[
            Cond::Eq("/type", "agent_event"),
            Cond::Eq("/event/type", "content_start"),
            Cond::Eq("/event/contentType", "tool"),
        ],
        each: None,
        unit: &[],
    },
    name: Name::Prefixed("/event/toolName"),
    args: "/event/input",
    pairing: Pairing::Separate {
        call_id: "/event/toolCallId",
        result: Where {
            frame: &[
                Cond::Eq("/type", "agent_event"),
                Cond::Eq("/event/type", "content_end"),
                Cond::Eq("/event/contentType", "tool"),
            ],
            each: None,
            unit: &[],
        },
        result_id: "/event/toolCallId",
        verdict: Verdict::ErrorFlag {
            path: "/event/output/isError",
            words: &["/event/output/content", "/event/output/error"],
            fallback: "the tool call failed without a message",
        },
    },
    refused_report: OnRefusedReport::Fail,
    failures: &[
        // Both the provider fault (`recoverable: false`, then `run_result error`) and a tool call
        // cline declined itself (`recoverable: true`, run completes at exit 0).
        Failure::Frame {
            at: Where {
                frame: &[
                    Cond::Eq("/type", "agent_event"),
                    Cond::Eq("/event/type", "error"),
                ],
                each: None,
                unit: &[],
            },
            words: &["/event/error/message"],
            fallback: "the child's stream carried an error event",
        },
        Failure::NotOk {
            at: Where {
                frame: &[Cond::Eq("/type", "run_result")],
                each: None,
                unit: &[],
            },
            path: "/finishReason",
            ok: "completed",
            words: &["/text"],
            label: "cline's run_result finishReason: ",
        },
    ],
    file_changes: None,
    // No frame carries the session id (s27 item 7): `hook_event.taskId` is a conversation id, and
    // the session id is a directory name.
    session: None,
};

/// Relocates the rules/skills roots (`~/.agents`, `~/Documents/Cline`, `~/Cline`) and whatever
/// else the CLI keys off the home. **Dropped under [`Auth::Inherited`]**.
pub const HOME_ENV: &str = "HOME";
/// Roots configuration (default `~/.cline`). **Dropped under [`Auth::Inherited`]**.
pub const DIR_ENV: &str = "CLINE_DIR";
/// Roots state (default `$CLINE_DIR/data`): `settings/providers.json`, `db/`, `sessions/`, `logs/`.
/// **Dropped under [`Auth::Inherited`]**.
pub const DATA_DIR_ENV: &str = "CLINE_DATA_DIR";
/// Names the MCP document. Resolved independently of the data dir, measured honoured with the file
/// outside it (`cline-mcp-settings-path-env.mcp.jsonl`). Kept under **both** modes.
pub const MCP_SETTINGS_PATH_ENV: &str = "CLINE_MCP_SETTINGS_PATH";

/// The provider `providers.json` selects: cline's OpenAI Chat Completions client, which takes
/// marion's `…/v1` base URL verbatim and appends `/chat/completions`.
pub const PROVIDER: &str = "openai-compatible";

/// The MCP server alias. The model-facing tool name is `<alias>__<tool>`.
pub const MCP_ALIAS: &str = crate::spec::MCP_ALIAS;

/// `$CLINE_DATA_DIR` for a node.
pub fn data_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(DATA_DIR)
}

/// Where `providers.json` is written: `<data>/settings/providers.json`, **the only path cline reads
/// it from** (s27 item 14).
pub fn providers_path(config_dir: &Path) -> PathBuf {
    data_dir(config_dir).join("settings").join("providers.json")
}

/// Where [`mcp_settings_json`] is written, named by [`MCP_SETTINGS_PATH_ENV`].
pub fn mcp_settings_path(config_dir: &Path) -> PathBuf {
    config_dir.join(MCP_SETTINGS_FILE)
}

/// What a `providers.json` needs to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
}

/// The `providers.json` document, byte-for-byte the shape `cline auth openai-compatible` writes
/// (`tests/fixtures/s27/providers.json`): `version: 1`, the provider named as `lastUsedProvider`,
/// and one entry whose `settings` carry provider, key, model and base URL. `updatedAt` is a fixed
/// epoch rather than the wall clock, so two compiles of one launch are byte-identical.
pub fn providers_json(p: &ProviderSpec) -> Value {
    json!({
        "version": 1,
        "lastUsedProvider": PROVIDER,
        "modes": {},
        "providers": {
            PROVIDER: {
                "settings": {
                    "provider": PROVIDER,
                    "apiKey": p.api_key,
                    "model": p.model,
                    "baseUrl": p.base_url,
                },
                "updatedAt": "1970-01-01T00:00:00.000Z",
                "tokenSource": "manual",
            }
        }
    })
}

/// The MCP document: the shape `cline mcp install marion --yes --transport stdio -- <cmd> <args>`
/// writes (`tests/fixtures/s27/cline_mcp_settings.json`) — `mcpServers.<name>.transport.{type,
/// command, args}` — with the bridge's `env` block beside it.
///
/// `ready_file` is `None` on this surface: the prompt rides argv, and s27 measured `tools/list`
/// answered before the first request on every capture.
pub fn mcp_settings_json(b: &BridgeEnv) -> Value {
    json!({
        "mcpServers": {
            MCP_ALIAS: {
                "transport": {
                    "type": "stdio",
                    "command": b.bridge.to_string_lossy(),
                    "args": b.args,
                    "env": b.env_json(),
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT_OK: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s27/cline-report-ok.stdout.jsonl"
    ));
    const REPORT_ISERROR: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s27/cline-report-iserror.stdout.jsonl"
    ));
    const PROVIDER_500: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s27/cline-provider-500.stdout.jsonl"
    ));
    const DENIED: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s27/cline-report-denied-no-auto-approve.stdout.jsonl"
    ));
    const PROVIDERS_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s27/providers.json"
    ));
    const MCP_FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s27/cline_mcp_settings.json"
    ));

    use marion_core::contract::AgentId;

    use crate::auth::Auth;
    use crate::invocation::Invocation;
    use crate::spec::{Axes, Fields, Shape, render};
    use crate::stream::{CallOutcome, MarionCall, StreamOutcome};

    fn spec() -> Fields {
        Fields {
            cwd: "/tmp/wt".into(),
            config_dir: "/tmp/cfg".into(),
            auth: Auth::Canned,
            prompt: "do the task".into(),
            model: Some("canned-1".into()),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("sk-fake".into()),
            axes: Axes::default(),
            ..Fields::default()
        }
    }

    fn live_spec() -> Fields {
        Fields {
            base_url: None,
            api_key: None,
            auth: Auth::Inherited,
            ..spec()
        }
    }

    fn compile(f: &Fields) -> Invocation {
        render(&SPEC, Shape::Headless, f).unwrap()
    }

    fn env_of(inv: &Invocation, k: &str) -> Option<String> {
        inv.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn bridge() -> BridgeEnv {
        BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-child".into()),
            agent_type: "cline".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
        }
    }

    fn prefix() -> String {
        ToolSpelling::ServerDoubleUnderscoreTool.spell("")
    }

    fn parse_stream(s: &str) -> StreamOutcome {
        crate::grammar::parse_stream(&STREAM, s, &prefix())
    }

    fn marion_calls(s: &str) -> Vec<MarionCall> {
        crate::grammar::marion_calls(&STREAM, s, &prefix())
    }

    #[test]
    fn the_argv_is_the_measured_headless_surface_with_the_prompt_last() {
        let inv = compile(&spec());
        assert_eq!(inv.program, "cline");
        assert_eq!(
            inv.args,
            vec![
                "--json",
                "-c",
                "/tmp/wt",
                "--config",
                "/tmp/cfg/cfg",
                "--data-dir",
                "/tmp/cfg/data",
                "--auto-approve",
                "true",
                "-m",
                "canned-1",
                "do the task",
            ]
        );
        assert_eq!(inv.cwd, std::path::PathBuf::from("/tmp/wt"));
    }

    #[test]
    fn the_canned_isolation_is_three_variables_and_two_flags_together() {
        let inv = compile(&spec());
        assert_eq!(env_of(&inv, HOME_ENV).as_deref(), Some("/tmp/cfg/home"));
        assert_eq!(env_of(&inv, DIR_ENV).as_deref(), Some("/tmp/cfg/cfg"));
        assert_eq!(env_of(&inv, DATA_DIR_ENV).as_deref(), Some("/tmp/cfg/data"));
        assert_eq!(
            env_of(&inv, MCP_SETTINGS_PATH_ENV).as_deref(),
            Some("/tmp/cfg/cline_mcp_settings.json")
        );
        assert_eq!(
            providers_path(&spec().config_dir),
            std::path::PathBuf::from("/tmp/cfg/data/settings/providers.json"),
            "the one path cline reads providers.json from"
        );
    }

    /// The flags and the variables leave together: pointing either at marion's directory on a
    /// live node hides the operator's own configuration. The MCP path stays.
    #[test]
    fn live_mode_removes_the_relocation_and_keeps_the_declaration_route() {
        let inv = compile(&live_spec());
        for k in [HOME_ENV, DIR_ENV, DATA_DIR_ENV] {
            assert_eq!(env_of(&inv, k), None, "{k} survived live mode");
        }
        for flag in ["--config", "--data-dir"] {
            assert!(
                !inv.args.contains(&flag.to_string()),
                "{flag} survived live mode"
            );
        }
        assert_eq!(
            env_of(&inv, MCP_SETTINGS_PATH_ENV).as_deref(),
            Some("/tmp/cfg/cline_mcp_settings.json")
        );
        assert_eq!(inv.args.last().map(String::as_str), Some("do the task"));
        // A live launch naming a model carries it on `-m`; one naming none carries nothing.
        assert!(inv.args.windows(2).any(|w| w == ["-m", "canned-1"]));
        let inv = compile(&Fields {
            model: None,
            ..live_spec()
        });
        assert!(!inv.args.contains(&"-m".to_string()));
    }

    /// Byte-for-byte the document `cline auth openai-compatible` wrote, modulo the two values the
    /// fixture redacts and the timestamp.
    #[test]
    fn the_providers_document_is_the_one_cline_auth_writes() {
        let ours = providers_json(&ProviderSpec {
            model: "canned-1".into(),
            base_url: "http://127.0.0.1:8127/v1".into(),
            api_key: "<API-KEY>".into(),
        });
        let mut theirs: Value = serde_json::from_str(PROVIDERS_FIXTURE).unwrap();
        theirs["providers"][PROVIDER]["updatedAt"] = json!("1970-01-01T00:00:00.000Z");
        assert_eq!(ours, theirs);
    }

    /// The shape `cline mcp install` writes, with the bridge's env beside the transport.
    #[test]
    fn the_mcp_document_is_the_shape_cline_mcp_install_writes() {
        let ours = mcp_settings_json(&bridge());
        let theirs: Value = serde_json::from_str(MCP_FIXTURE).unwrap();
        let server = &ours["mcpServers"][MCP_ALIAS];
        let fixture = &theirs["mcpServers"]["marion"];
        assert_eq!(server["transport"]["type"], fixture["transport"]["type"]);
        assert!(fixture["transport"]["command"].is_string());
        assert_eq!(
            server["transport"]["command"],
            json!("/bin/marion-supervisor")
        );
        assert_eq!(server["transport"]["args"], json!(["mcp"]));
        assert_eq!(
            server["transport"]["env"]["MARION_AGENT_ID"],
            json!("019f-child")
        );
        assert!(
            mcp_settings_document(&bridge()).contains("\"marion\""),
            "the document names marion's server"
        );
    }

    #[test]
    fn the_report_is_a_content_start_and_its_verdict_a_content_end() {
        let out = parse_stream(REPORT_OK);
        assert_eq!(
            out.narrative.as_deref(),
            Some("hello from cline under a canned provider")
        );
        assert_eq!(out.failure, None);
        assert_eq!(
            marion_calls(REPORT_OK),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered,
            }]
        );
    }

    /// `finishReason` stays `completed` and the run exits 0; `output.isError` is the verdict.
    #[test]
    fn an_is_error_result_is_a_refusal_whatever_the_exit_code_says() {
        let calls = marion_calls(REPORT_ISERROR);
        assert_eq!(calls.len(), 1);
        match &calls[0].outcome {
            CallOutcome::Refused(words) => {
                assert!(words.contains("refused: not authorized"), "{words}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(parse_stream(REPORT_ISERROR).failure.is_some());
    }

    #[test]
    fn a_provider_fault_is_an_error_event_and_a_failed_run_result() {
        let out = parse_stream(PROVIDER_500);
        assert_eq!(out.narrative, None);
        assert!(marion_calls(PROVIDER_500).is_empty());
        assert_eq!(out.failure.as_deref(), Some("canned failure"));
    }

    /// A call cline declined itself carries `output.error` and no `isError`; the error event that
    /// follows is what names it.
    #[test]
    fn a_call_declined_inside_cline_fails_the_run_through_its_error_event() {
        let out = parse_stream(DENIED);
        assert!(
            out.failure
                .as_deref()
                .is_some_and(|f| f.contains("requires approval in a TTY session")),
            "{:?}",
            out.failure
        );
    }

    #[test]
    fn no_frame_carries_a_session_id() {
        for frame in crate::stream::json_frames(REPORT_OK) {
            assert_eq!(crate::grammar::session_id(&STREAM, &frame), None);
        }
    }
}
