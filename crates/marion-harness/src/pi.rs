//! The pi adapter: the headless `-p --mode json` surface (fixture `tests/fixtures/s34-pi/`).
//!
//! Measured against **pi 0.80.2** on 2026-09-27. Every probe ran against a canned local OpenAI Chat
//! Completions endpoint and cost $0.00. pi is the first row with **no MCP client at all**: its README
//! says "No MCP", and an extension is how it gains a tool. So marion's declaration is an extension
//! of its own. [`EXTENSION`] is a stdio MCP client for exactly one server, rendered per node around
//! that node's bridge and named on argv with `-e`, which pi loads for that one run. That is the
//! shape Claude Code's `--mcp-config <path>` already has ([`LiveDeclaration::ArgvDocument`]), so
//! pi needs no new vocabulary: the harness-specific part is the document's body, exactly as it is
//! on every other row.
//!
//! What the row rests on (item numbers are the fixture README's):
//!
//! - **`--tools` is one list on both axes** (item 3). It is an allowlist over built-in and extension
//!   tools alike, so marion's verbs and the declared built-ins are what the model is offered. An
//!   empty list offers nothing, which makes `--tools ""` an honest orchestrator and not a hole.
//! - **No approval surface** (item 4). pi never asks to run a tool, so the argv has no grant flag.
//! - **Failures exit 0** (item 6). A provider fault is an assistant `stopReason: "error"` inside
//!   the final `agent_end`, the one whose `willRetry` is false. Retried attempts carry the same
//!   shape under `willRetry: true` and are not claims.
//! - **stdin is read to EOF** when it is not a terminal (item 9). The supervisor's `Stdio::null()`
//!   is what lets a launch start at all.
//! - **`PI_CODING_AGENT_DIR` holds the operator's credentials** (item 11). A canned node relocates
//!   it to hold marion's `models.json`, while a live node keeps the operator's and adds only `-e`.

use std::path::{Path, PathBuf};

use marion_core::agent_type;
use marion_core::harness::Harness;
use serde_json::{Value, json};

use crate::grammar::{
    ActivityRule, CallShape, Cond, ErrorRule, Failure, Name, OnRefusedReport, Pairing, SessionId,
    StreamGrammar, TextUnit, ToolUnit, UsageFold, UsageRule, Verdict, Where,
};
use crate::jsonl_channel::{Command, JsonlChannel};
pub use crate::mcp_bridge::BridgeEnv;
use crate::spec::{
    Approval, Arg, BootDialogs, BootSignal, Constraint, Deliveries, Env, Field, HarnessSpec,
    LiveDeclaration, McpRoute, McpRoutes, MidTurn, Push, Resume, Spelling, Surfaces, ToolSpelling,
    TurnDelivery, UpdatePolicy, Val, When,
};

/// `$PI_CODING_AGENT_DIR`'s name under the node's config dir. One spelling for [`SPEC`]'s env row
/// and [`models_path`].
const AGENT_DIR: &str = "agent";

/// The file [`extension_source`] is written to, under the node's config dir, in both modes.
pub const EXTENSION_FILE: &str = "marion-pi.js";

/// The provider id the canned `models.json` declares and `--provider` names.
pub const PROVIDER: &str = "marion";

/// marion's pi extension, before a node's server and spelling are filled in.
pub const EXTENSION: &str = include_str!("pi_extension.js");

/// pi's row. Measured against 0.80.2 on 2026-09-27 (`tests/fixtures/s34-pi/`), every probe against
/// a canned local OpenAI Chat Completions endpoint at $0.00.
///
/// **Live mode is a removal.** The agent-dir relocation, `--no-extensions` and `--provider marion`
/// go together. What survives is the extension, `--tools`, and a `--model` where the launch names one.
///
/// **Headless is `--mode rpc`, a JSONL command channel ([`RPC`], item 12).** The prompt is the
/// channel's first command rather than argv, a message for a running node is folded as pi's steer,
/// and a node past its wall clock is aborted before it is killed. The one-shot `-p --mode json`
/// launch is kept as [`LAUNCH_ONLY`], the same row with the channel removed.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Pi,
    surfaces: Surfaces::JsonlRpc(&RPC),
    program: Some("pi"),
    argv: &[
        Arg::Lit("--mode"),
        Arg::Lit("rpc"),
        // `--session <id>`, the id the `get_state` reply names (item 8).
        Arg::Resume,
        // The operator's own extensions stay out of a canned node; explicit `-e` still loads
        // (item 1). A live node keeps them, since an extension can be where a provider lives.
        Arg::CannedLit("--no-extensions"),
        // The provider `models.json` declares under the relocated agent dir.
        Arg::CannedLit("--provider"),
        Arg::CannedLit(PROVIDER),
        Arg::Flag("--model", Field::Model),
        // Emitted even when empty: `--tools ""` offers nothing (`pi-empty-tools.*`).
        Arg::Joined("--tools", Field::Tools),
        Arg::Flag("-e", Field::McpConfig),
    ],
    pane: None,
    env: &[Env {
        key: AGENT_DIR_ENV,
        val: Val::Under(AGENT_DIR),
        when: When::Overlay,
    }],
    stream: Some(&RPC_STREAM),
    // `read`, and both built-ins that change a file: `write` creates, `edit` replaces text in an
    // existing file, and each is offered only when named (`pi-report.provider-request-1.json`).
    tool_names: &[
        (agent_type::TOOL_READ, "read"),
        (agent_type::TOOL_WRITE, "write"),
        (agent_type::TOOL_WRITE, "edit"),
    ],
    // The names marion's own extension registers: Claude Code's spelling, passed to the provider
    // verbatim and repeated in `tool_execution_start.toolName` (item 2).
    spelling: Spelling::Fixed(ToolSpelling::McpDoubleUnderscore),
    // One route in both modes: the extension is marion's own file under marion's own directory,
    // so writing it never touches the operator's configuration.
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Document,
    },
    live_declaration: Some(LiveDeclaration::ArgvDocument {
        flag: "-e",
        file: EXTENSION_FILE,
        prefix: "",
        body: extension_source,
    }),
    // The `--tools` list itself, prefixed with its axis.
    constraint: Constraint::Allowed { prefix: "tools:" },
    resume: Some(Resume::Flag("--session")),
    // pi never replaces itself (`pi update` is manual); the one startup update traffic is the
    // version check, which `dist/utils/version-check.js` skips on `PI_SKIP_VERSION_CHECK`.
    // `PI_OFFLINE` would skip it too, but it also cuts every other startup network operation,
    // which on a live node includes the operator's own package checks (item 10).
    updates: UpdatePolicy::Env {
        key: "PI_SKIP_VERSION_CHECK",
        value: "1",
        note: "0.80.2 dist/utils/version-check.js returns early on PI_SKIP_VERSION_CHECK; updates \
               themselves are only ever `pi update`",
    },
    // Nothing reaches the model: marion's extension reads only responses and drops every
    // notification the bridge sends.
    push: Push::None,
    // pi has no approval surface: `--tools` is both what the model is offered and all it may run.
    approval: Approval::None {
        note: "S34 on 0.80.2: pi asks nothing headless; a tool named in --tools runs, and marion's \
               verb reaches the bridge with no prompt",
    },
    // Not measured: S34 ran pi only against marion's canned models.json provider, and an endpoint
    // recipe (a models.json naming the operator's provider and key) was never tried.
    boot_dialogs: BootDialogs {
        dialogs: &[],
        note: "S37 0.80.2, fresh directory, --no-extensions: the first screen is the composer. \
               An operator's own extension may draw a `Press any key to continue` splash; that \
               is the operator's configuration, not the harness",
    },
    wires: &[],
    // pi 0.80.2 `dist/config.js`: `getAuthPath()` is `<agent dir>/auth.json` and the agent dir is
    // `PI_CODING_AGENT_DIR` — the variable the canned row already relocates (item 3). A profile is
    // an agent dir of its own; existence is the whole probe, and the file is never opened. The
    // login is pi's own `/login`, inside its TUI.
    profile: Some(crate::profile::ProfileCarrier {
        env: AGENT_DIR_ENV,
        clear: &[],
        status: crate::profile::Status::FileExists("auth.json"),
        login_hint: "",
        home_default: ".pi/agent",
        shared: &["settings.json"],
        note: "pi 0.80.2 dist/config.js: getAuthPath() = <PI_CODING_AGENT_DIR>/auth.json",
    }),
    client_name: None,
    delivery: Deliveries {
        headless: TurnDelivery::TypedTurn {
            mid_turn: MidTurn::Fold,
            note: "S34 pi-rpc-steer-mid-tool (0.80.2): a steer written during a tool call is a \
                   user message after the tool result in the same turn's next request; marion \
                   writes it as `prompt` with streamingBehavior `steer`, which pi queues the same \
                   way while streaming and runs as a new turn when idle",
        },
        interactive: TurnDelivery::bracketed_paste(
            BootSignal::FirstDraw,
            "S34 spikes/s34/pi_tui.py (0.80.2): DECSET 2004 on; a bracketed multi-line paste then \
             CR submits and reaches the provider byte-exact; idle output 0 B over 10 s, busy \
             repaints at <= 88 ms gaps; Enter while busy folds as a steer after the tool result",
        ),
    },
    note: "S34 on pi 0.80.2: --mode rpc over a canned models.json provider, marion's own MCP \
           client extension loaded with -e as the declaration in both modes, --tools as the one \
           list on both axes, provider faults read from the final agent_end, --session <id>; \
           harness_matrix's pi cell and tests/pi_rpc.rs run this row end to end",
};

/// **pi's `--mode rpc` vocabulary** (item 12, `pi-rpc-steer-mid-tool`, `pi-rpc-abort-mid-tool`).
///
/// - Every command carries its kind in `type`; a reply is `type: "response"` echoing `id`.
/// - `get_state` is the handshake: its reply proves the command loop runs, which it does only once
///   the extensions — marion's among them — have loaded, and it names the session.
/// - A turn is one agent run: `agent_start` opens it and the `agent_end` pi will not retry closes
///   it (a retried provider fault ends a run with `willRetry: true` and starts another).
/// - `prompt` starts a turn on an idle node. A bare `prompt` mid-turn is refused ("Agent is
///   already processing"), so the fold is `prompt` with `streamingBehavior: "steer"`: pi's
///   `AgentSession.prompt` queues it exactly as the `steer` command does while streaming, and
///   runs it as a new turn when idle, so a write racing the turn's end is never stranded in the
///   queue (0.80.2 `dist/core/agent-session.js`).
/// - `abort` ends the running turn: the in-flight call completes and the next model request ends
///   `stopReason: "aborted"`. The process stays up until stdin closes, then exits 0.
/// - `follow_up` (measured: its own turn after the last answer, inside the same agent run) is not
///   used: marion queues on its own side, so each message it delivers has a turn of its own.
pub const RPC: JsonlChannel = JsonlChannel {
    id: "id",
    reply: &[Cond::Eq("/type", "response")],
    handshake: Command {
        fields: &[("type", "get_state")],
        text: None,
    },
    prompt: Command {
        fields: &[("type", "prompt")],
        text: Some("message"),
    },
    steer: Command {
        fields: &[("type", "prompt"), ("streamingBehavior", "steer")],
        text: Some("message"),
    },
    abort: Command {
        fields: &[("type", "abort")],
        text: None,
    },
    turn_started: &[Cond::Eq("/type", "agent_start")],
    turn_ended: &[
        Cond::Eq("/type", "agent_end"),
        Cond::Eq("/willRetry", "false"),
    ],
    note: "S34 pi-rpc-steer-mid-tool and pi-rpc-abort-mid-tool on 0.80.2 (spikes/s34/pi_rpc.py)",
};

/// The one-shot `-p --mode json` launch pi's row measured first (items 1–11): the prompt rides
/// argv and a later turn is a `--session` relaunch. **The row with its channel removed**, kept so
/// that falling back is one edit of row data, and so the json-mode fixtures keep a row to be read
/// against.
pub const LAUNCH_ONLY: HarnessSpec = HarnessSpec {
    surfaces: Surfaces::LaunchOnly,
    argv: &[
        Arg::Lit("-p"),
        Arg::Lit("--mode"),
        Arg::Lit("json"),
        Arg::Resume,
        Arg::CannedLit("--no-extensions"),
        Arg::CannedLit("--provider"),
        Arg::CannedLit(PROVIDER),
        Arg::Flag("--model", Field::Model),
        Arg::Joined("--tools", Field::Tools),
        Arg::Flag("-e", Field::McpConfig),
        // Positional and last, as every capture ran it.
        Arg::Pos(Field::Prompt),
    ],
    stream: Some(&STREAM),
    delivery: Deliveries {
        headless: TurnDelivery::Continuation {
            note: "S34 pi-resume-turn-2 (0.80.2): `--session <id> -p …` continues the session",
        },
        ..SPEC.delivery
    },
    ..SPEC
};

/// [`STREAM`] as `--mode rpc` prints it: the same event frames, except that no `session` header is
/// written, so the session id is read off the handshake's `get_state` reply (`data.sessionId`, the
/// id `--session` takes; measured 2026-09-27 on 0.80.2).
pub const RPC_STREAM: StreamGrammar = StreamGrammar {
    session: Some(SessionId {
        at: Where {
            frame: &[
                Cond::Eq("/type", "response"),
                Cond::Eq("/command", "get_state"),
            ],
            each: None,
            unit: &[],
        },
        path: "/data/sessionId",
        // As on [`STREAM`]: a resumed rpc launch naming its own session was not measured.
        resumes_in_place: false,
    }),
    ..STREAM
};

/// pi's stream (item 5). A tool call is a `tool_execution_start` frame and its verdict is the
/// `tool_execution_end` with the same `toolCallId`. Only the final `agent_end` makes a failure
/// claim.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[Cond::Eq("/type", "tool_execution_start")],
        each: None,
        unit: &[],
    },
    name: Name::Prefixed("/toolName"),
    args: "/args",
    pairing: Pairing::Separate {
        call_id: "/toolCallId",
        result: Where {
            frame: &[Cond::Eq("/type", "tool_execution_end")],
            each: None,
            unit: &[],
        },
        result_id: "/toolCallId",
        // The extension throws on an MCP `isError`, and pi marks the call `isError: true` with
        // the thrown words as its text (`pi-report-iserror.stdout.jsonl`).
        verdict: Verdict::ErrorFlag {
            path: "/isError",
            words: &["/result/content"],
            fallback: "pi marked the call isError with no words",
        },
    },
    // A refused `report` is recorded on the call. The run itself continues and exits 0, and its
    // own failure claims decide (`pi-report-iserror`).
    refused_report: OnRefusedReport::Record,
    failures: &[Failure::Frame {
        at: Where {
            frame: &[
                Cond::Eq("/type", "agent_end"),
                Cond::Eq("/willRetry", "false"),
            ],
            each: Some("/messages"),
            unit: &[
                Cond::Eq("/role", "assistant"),
                Cond::Eq("/stopReason", "error"),
            ],
        },
        words: &["/errorMessage"],
        fallback: "pi's last turn ended in error",
    }],
    // s34 (`pi-provider-500.stdout.jsonl`): each retry is an `auto_retry_start` naming the
    // error, and the failed turn's `message_end` carries `stopReason: error` and `errorMessage`
    // (`500 canned failure`, `401 canned failure`).
    errors: &[
        ErrorRule {
            at: Where {
                frame: &[Cond::Eq("/type", "auto_retry_start")],
                each: None,
                unit: &[],
            },
            status: None,
            kind: None,
            words: &["/errorMessage"],
        },
        ErrorRule {
            at: Where {
                frame: &[
                    Cond::Eq("/type", "message_end"),
                    Cond::Eq("/message/stopReason", "error"),
                ],
                each: None,
                unit: &[],
            },
            status: None,
            kind: None,
            words: &["/message/errorMessage"],
        },
    ],
    file_changes: None,
    session: Some(SessionId {
        at: Where {
            frame: &[Cond::Eq("/type", "session")],
            each: None,
            unit: &[],
        },
        path: "/id",
        // Not measured: S34 resumed a known id only, so what an unknown one starts is unknown.
        resumes_in_place: false,
    }),
    // One unit per assistant message, and `input` is already net of cache reads (item 7).
    usage: Some(UsageRule {
        at: Where {
            frame: &[
                Cond::Eq("/type", "message_end"),
                Cond::Eq("/message/role", "assistant"),
            ],
            each: None,
            unit: &[],
        },
        input: "/message/usage/input",
        output: "/message/usage/output",
        cache_read: Some("/message/usage/cacheRead"),
        cache_write: Some("/message/usage/cacheWrite"),
        reasoning: None,
        input_includes_cache: false,
        fold: UsageFold::Sum,
    }),
    activity: Some(ActivityRule {
        calls: &[ToolUnit {
            at: Where {
                frame: &[Cond::Eq("/type", "tool_execution_start")],
                each: None,
                unit: &[],
            },
            name: "/toolName",
            args: "/args",
            id: Some("/toolCallId"),
            shape: CallShape::Tool,
        }],
        text: &[TextUnit {
            at: Where {
                frame: &[
                    Cond::Eq("/type", "message_end"),
                    Cond::Eq("/message/role", "assistant"),
                ],
                each: Some("/message/content"),
                unit: &[Cond::Eq("/type", "text")],
            },
            path: "/text",
            joins: false,
        }],
    }),
    // Not measured: no usage-window reading was recorded for this harness.
    rate_limit: None,
};

/// Relocates `auth.json`, `models.json`, `settings.json`, `sessions/` and extension discovery.
/// **Dropped under [`crate::Auth::Inherited`]**: relocating it is what hides the operator's own
/// login.
pub const AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";

/// The MCP server alias. The model-facing tool name is `mcp__<alias>__<tool>`.
pub const MCP_ALIAS: &str = crate::spec::MCP_ALIAS;

/// `$PI_CODING_AGENT_DIR` for a canned node: a directory under marion's own config dir.
pub fn agent_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(AGENT_DIR)
}

/// Where [`models_json`] is written: `$PI_CODING_AGENT_DIR/models.json`.
pub fn models_path(config_dir: &Path) -> PathBuf {
    agent_dir(config_dir).join("models.json")
}

/// Where [`extension_source`] is written, in both modes.
pub fn extension_path(config_dir: &Path) -> PathBuf {
    config_dir.join(EXTENSION_FILE)
}

/// The canned node's `models.json`: one provider, [`PROVIDER`], speaking Chat Completions at
/// marion's `…/v1` base URL, with the one model the launch names. pi lists a model only when its
/// provider has a key, so the key is written even though the canned endpoint ignores it.
pub fn models_json(base_url: &str, api_key: &str, model: &str) -> Value {
    json!({
        "providers": {
            PROVIDER: {
                "baseUrl": base_url,
                "api": "openai-completions",
                "apiKey": api_key,
                "models": [{ "id": model }],
            }
        }
    })
}

/// The server the extension starts: the bridge, its arguments, and the bridge's environment,
/// which the extension lays over pi's own.
pub fn server_json(b: &BridgeEnv) -> Value {
    json!({
        "command": b.bridge.to_string_lossy(),
        "args": b.args,
        "env": b.env_json(),
    })
}

/// [`EXTENSION`] with this node's server and marion's spelling filled in. Both are JSON, which is
/// a JavaScript expression, so no escaping beyond serde's is needed.
pub fn extension_source(b: &BridgeEnv) -> String {
    EXTENSION
        .replace("__MARION_SERVER__", &server_json(b).to_string())
        .replace(
            "__MARION_PREFIX__",
            &Value::String(ToolSpelling::McpDoubleUnderscore.spell("")).to_string(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-report.stdout.jsonl"
    ));
    const REPORT_ISERROR: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-report-iserror.stdout.jsonl"
    ));
    const PROVIDER_500: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-provider-500.stdout.jsonl"
    ));
    const PROVIDER_401: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-provider-401.stdout.jsonl"
    ));
    const RETRY_THEN_REPORT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-retry-then-report.stdout.jsonl"
    ));
    const RESUME_TURN_1: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-resume-turn-1.stdout.jsonl"
    ));
    const RESUME_TURN_2: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-resume-turn-2.stdout.jsonl"
    ));

    use marion_core::TokenUsage;
    use marion_core::contract::AgentId;

    use crate::auth::Auth;
    use crate::invocation::Invocation;
    use crate::spec::{Axes, Fields, Shape, render};
    use crate::stream::{CallOutcome, MarionCall, StreamOutcome, json_frames};

    fn bridge() -> BridgeEnv {
        BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-child".into()),
            agent_type: "pi".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
        }
    }

    fn spec() -> Fields {
        Fields {
            cwd: "/tmp/wt".into(),
            config_dir: "/tmp/cfg".into(),
            auth: Auth::Canned,
            prompt: "do the task".into(),
            model: Some("canned-1".into()),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("sk-fake".into()),
            mcp_config: Some("/tmp/cfg/marion-pi.js".into()),
            axes: Axes {
                tools: vec!["mcp__marion__report".into()],
                allowed: vec!["mcp__marion__report".into()],
                mode: None,
            },
            ..Fields::default()
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

    fn parse(s: &str) -> StreamOutcome {
        crate::grammar::parse_stream(&STREAM, s, &prefix())
    }

    fn calls(s: &str) -> Vec<MarionCall> {
        crate::grammar::marion_calls(&STREAM, s, &prefix())
    }

    /// The headless launch opens pi's command channel: no `-p`, no prompt on argv.
    #[test]
    fn a_canned_launch_opens_the_rpc_channel_and_carries_no_prompt() {
        let inv = compile(&spec());
        assert_eq!(inv.program, "pi");
        assert_eq!(
            inv.args,
            [
                "--mode",
                "rpc",
                "--no-extensions",
                "--provider",
                "marion",
                "--model",
                "canned-1",
                "--tools",
                "mcp__marion__report",
                "-e",
                "/tmp/cfg/marion-pi.js",
            ]
        );
        assert_eq!(SPEC.surfaces.channel(), Some(&RPC));
    }

    /// The fallback row still renders the one-shot launch every json-mode capture ran.
    #[test]
    fn a_canned_launch_only_launch_is_the_measured_argv_and_relocates_the_agent_dir() {
        let inv = render(&LAUNCH_ONLY, Shape::Headless, &spec()).unwrap();
        assert_eq!(inv.program, "pi");
        assert_eq!(LAUNCH_ONLY.surfaces.channel(), None);
        assert_eq!(
            inv.args,
            [
                "-p",
                "--mode",
                "json",
                "--no-extensions",
                "--provider",
                "marion",
                "--model",
                "canned-1",
                "--tools",
                "mcp__marion__report",
                "-e",
                "/tmp/cfg/marion-pi.js",
                "do the task",
            ]
        );
        assert_eq!(
            env_of(&inv, AGENT_DIR_ENV).as_deref(),
            Some("/tmp/cfg/agent")
        );
        assert_eq!(env_of(&inv, "PI_SKIP_VERSION_CHECK").as_deref(), Some("1"));
        assert_eq!(
            models_path(Path::new("/tmp/cfg")),
            PathBuf::from("/tmp/cfg/agent/models.json")
        );
    }

    /// Live keeps the operator's agent dir, extensions and provider, and adds only the extension,
    /// `--tools`, and a model where one was named.
    #[test]
    fn live_mode_removes_the_relocation_and_the_canned_provider() {
        let f = Fields {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            model: None,
            ..spec()
        };
        let inv = compile(&f);
        assert_eq!(env_of(&inv, AGENT_DIR_ENV), None);
        for gone in ["--no-extensions", "--provider", "marion", "--model"] {
            assert!(!inv.args.iter().any(|a| a == gone), "{gone} survived live");
        }
        assert!(
            inv.args
                .windows(2)
                .any(|w| w == ["-e", "/tmp/cfg/marion-pi.js"])
        );
    }

    #[test]
    fn an_empty_tool_list_is_still_emitted_because_it_offers_nothing() {
        let mut f = spec();
        f.axes = Axes::default();
        let inv = compile(&f);
        let at = inv.args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(inv.args[at + 1], "");
    }

    #[test]
    fn a_resume_follows_the_mode_flags() {
        let mut f = spec();
        f.resume = Some("<UUID-1>".into());
        let inv = compile(&f);
        assert_eq!(&inv.args[2..4], ["--session", "<UUID-1>"]);
        let inv = render(&LAUNCH_ONLY, Shape::Headless, &f).unwrap();
        assert_eq!(&inv.args[3..5], ["--session", "<UUID-1>"]);
    }

    #[test]
    fn the_models_document_names_one_chat_completions_provider() {
        let m = models_json("http://127.0.0.1:8099/v1", "sk-fake", "canned-1");
        let p = &m["providers"]["marion"];
        assert_eq!(p["baseUrl"], json!("http://127.0.0.1:8099/v1"));
        assert_eq!(p["api"], json!("openai-completions"));
        assert_eq!(p["apiKey"], json!("sk-fake"));
        assert_eq!(p["models"], json!([{"id": "canned-1"}]));
    }

    /// The rendered extension is the template with both holes filled by JSON, and nothing else.
    #[test]
    fn the_extension_carries_this_nodes_server_and_marions_spelling() {
        let src = extension_source(&bridge());
        assert!(!src.contains("__MARION_"), "every hole is filled");
        assert!(src.contains("const PREFIX = \"mcp__marion__\";"));
        let line = src
            .lines()
            .find(|l| l.starts_with("const SERVER = "))
            .unwrap();
        let server: Value = serde_json::from_str(
            line.trim_start_matches("const SERVER = ")
                .trim_end_matches(';'),
        )
        .unwrap();
        assert_eq!(server["command"], json!("/bin/marion-supervisor"));
        assert_eq!(server["args"], json!(["mcp"]));
        assert_eq!(server["env"]["MARION_AGENT_ID"], json!("019f-child"));
        assert!(src.contains("export default async function (pi)"));
    }

    #[test]
    fn the_report_is_read_off_the_execution_frames() {
        let out = parse(REPORT);
        assert_eq!(
            out.narrative.as_deref(),
            Some("hello from pi under a canned provider")
        );
        assert_eq!(out.failure, None);
        assert_eq!(
            calls(REPORT),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered,
            }]
        );
    }

    #[test]
    fn an_is_error_result_is_a_refusal_in_the_servers_words() {
        match &calls(REPORT_ISERROR)[0].outcome {
            CallOutcome::Refused(words) => {
                assert!(words.contains("refused: not authorized"), "{words}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(
            parse(REPORT_ISERROR).failure,
            None,
            "the run itself did not fail"
        );
    }

    #[test]
    fn a_provider_fault_is_claimed_by_the_final_agent_end_only() {
        assert_eq!(
            parse(PROVIDER_500).failure.as_deref(),
            Some("500 canned failure")
        );
        assert_eq!(
            parse(PROVIDER_401).failure.as_deref(),
            Some("401 canned failure")
        );
        // A transient fault that a retry recovered from is no claim at all.
        let out = parse(RETRY_THEN_REPORT);
        assert_eq!(out.failure, None);
        assert_eq!(calls(RETRY_THEN_REPORT).len(), 1);
    }

    #[test]
    fn the_session_header_names_the_session_a_resume_repeats() {
        let one = json_frames(RESUME_TURN_1);
        let two = json_frames(RESUME_TURN_2);
        let id = crate::grammar::session_id(&STREAM, &one[0]).unwrap();
        assert!(id.starts_with("<UUID-"), "{id}");
        assert_eq!(crate::grammar::session_id(&STREAM, &two[0]), Some(id));
        assert_eq!(crate::grammar::session_id(&STREAM, &one[1]), None);
    }

    #[test]
    fn usage_is_the_sum_of_every_assistant_message() {
        let rule = STREAM.usage.as_ref().unwrap();
        // Two assistant messages at 11 in / 7 out each: the call, then the closing words.
        assert_eq!(
            crate::grammar::usage(rule, &json_frames(REPORT)),
            Some(TokenUsage {
                input: 22,
                output: 14,
                cache_read: 0,
                cache_write: 0,
                reasoning: None,
            })
        );
    }

    #[test]
    fn activity_shows_the_call_and_the_last_words() {
        let a = crate::grammar::recent_activity(
            STREAM.activity.as_ref().unwrap(),
            &json_frames(REPORT),
            5,
        );
        assert_eq!(a.calls.len(), 1);
        assert_eq!(a.calls[0].name, "mcp__marion__report");
        assert_eq!(a.text.as_deref(), Some("reported."));
    }

    const RPC_STEER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-rpc-steer-mid-tool.stdout.jsonl"
    ));
    const RPC_ABORT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s34-pi/pi-rpc-abort-mid-tool.stdout.jsonl"
    ));

    /// **The channel reads S34's rpc captures as they happened**: one turn per agent run (the steer
    /// folded into the first, the follow-up inside it, the second prompt its own), each closed by
    /// its `agent_end`; the abort capture's one turn closed by the aborted run's end.
    #[test]
    fn the_rpc_channel_reads_the_measured_turns_off_the_captures() {
        let frames = |s: &str| -> Vec<Value> {
            s.lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|v| v.get("_sent").is_none())
                .collect()
        };
        for (capture, turns) in [(RPC_STEER, 2), (RPC_ABORT, 1)] {
            let f = frames(capture);
            assert_eq!(f.iter().filter(|v| RPC.opens_turn(v)).count(), turns);
            assert_eq!(f.iter().filter(|v| RPC.closes_turn(v)).count(), turns);
        }
        // The steer capture's refusal of a bare mid-turn prompt is a reply by shape and id.
        let refused = frames(RPC_STEER)
            .into_iter()
            .find(|v| v["success"] == json!(false))
            .unwrap();
        assert!(RPC.is_reply_to(&refused, "2"));
    }

    /// Usage and the call still read off the rpc stream, and the session off the handshake's reply.
    #[test]
    fn the_rpc_stream_reads_usage_calls_and_the_session_off_get_state() {
        let calls = crate::grammar::marion_calls(&RPC_STREAM, RPC_STEER, &prefix());
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].verb, "report");
        let out = crate::grammar::parse_stream(&RPC_STREAM, RPC_STEER, &prefix());
        assert_eq!(out.narrative.as_deref(), Some("rpc report"), "{out:?}");
        let reply = json!({"id": "h", "type": "response", "command": "get_state", "success": true,
                           "data": {"sessionId": "01a0e4a7-93fd"}});
        assert_eq!(
            crate::grammar::session_id(&RPC_STREAM, &reply).as_deref(),
            Some("01a0e4a7-93fd")
        );
    }
}
