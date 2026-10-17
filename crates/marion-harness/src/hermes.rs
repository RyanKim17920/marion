//! The Hermes Agent adapter — Nous Research's `hermes`, headless `chat -q --format stream-json`.
//!
//! Read from the **hermes-agent v0.22.2** source; no capture yet, and no working release is
//! installed to take one, so every conformance cell is pending. The surface is `LaunchOnly`: the
//! prompt rides `-q`, marion reads the JSONL `hermes_cli/stream_json.py` writes.
//!
//! Three facts shape the row, each from the source:
//!
//! - **Hermes borrows other harnesses' logins unless its config says not to.** By default it
//!   adopts the Codex CLI's `~/.codex/auth.json` and Claude Code's credentials, whose OAuth
//!   refresh tokens are single-use and rotate: a child that refreshes first logs the operator's
//!   own codex or claude out. `auth.adopt_external_logins: false` stops every automatic borrow
//!   (`agent/credential_sources.py`), and the row writes it on every launch.
//! - **The switch rides the managed scope, not the operator's config.** `HERMES_MANAGED_DIR`
//!   names a directory whose `config.yaml` wins per leaf over the user's config
//!   (`hermes_cli/managed_scope.py`), so one document under the node's own directory carries the
//!   switch and marion's server (under `mcp_servers`) while every other setting stays the
//!   operator's. The variable **replaces** the default `/etc/hermes` scope, so a launch that would
//!   hide an administrator's policy, or the operator's own `HERMES_MANAGED_DIR`, is refused.
//! - **Hermes v0.22.2 runs `chat` in-process.** Later releases move execution into a
//!   per-`HERMES_HOME` gateway daemon, so the row is pinned at or below the last in-process
//!   release and a gateway release is re-measured before it is admitted.

use marion_core::harness::Harness;

use crate::adapter::{DataAdapter, Row, Serve};
use crate::env_filter::LoginEnv;
use crate::grammar::{
    ActivityRule, CallShape, Cond, ErrorRule, Failure, ModelName, Name, OnRefusedReport, Pairing,
    SessionId, StreamGrammar, TextUnit, ToolUnit, UsageFold, UsageRule, Verdict, Where,
};
use crate::profile::{ProfileCarrier, Status as ProfileStatus};
use crate::spec::{
    AbortVerb, Aborts, Advertised, Approval, Arg, AxesRule, Boot, BootDialogs, ConfigFile,
    Constraint, Deliveries, Env, Field, HarnessSpec, LiveDeclaration, McpRoute, McpRoutes,
    McpServers, ModelForm, Modes, Need, Push, ReadOnly, Readiness, Remembers, Requirement, Resume,
    Spelling, Surfaces, TokenCarrier, TokenCarriers, ToolSpelling, TurnDelivery, UpdatePolicy, Val,
    When,
};

/// The variable that names Hermes' managed scope: a directory whose `config.yaml` wins per leaf
/// over the user's config. Set on every launch, because the borrowed-login switch lives there.
pub const MANAGED_DIR_ENV: &str = "HERMES_MANAGED_DIR";
/// [`MANAGED_DIR_ENV`]'s directory under the node's config dir.
pub const MANAGED_DIR: &str = "managed";
/// The scope Hermes reads when [`MANAGED_DIR_ENV`] is unset — an administrator's, which a launch
/// naming marion's directory would hide.
pub const SYSTEM_MANAGED_DIR: &str = "/etc/hermes";
/// The key Hermes reads its MCP servers under.
pub const MCP_SERVERS_KEY: &str = "mcp_servers";
/// The switch that stops Hermes adopting the Codex CLI's and Claude Code's logins.
pub const ADOPT_EXTERNAL_LOGINS: &str = "auth.adopt_external_logins";
/// The row's hold ([`HarnessSpec::held`]). Lifted, in the commit that records it, by the
/// measurement `tasks/todo.md` plans: a traced run on sentinel codex and claude logins.
pub const HELD: &str = "Hermes' guard against adopting your codex and claude logins is \
                        read from the 0.22.2 source and not yet verified, so marion launches no \
                        Hermes until a measurement of it is recorded";

/// Hermes' row, read from the v0.22.2 source.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Hermes,
    surfaces: Surfaces::LaunchOnly,
    program: Some("hermes"),
    // Runs any vendor's model.
    vendor: None,
    // The last release that runs `chat` in-process (see the module docs).
    verified: &["0.22.2"],
    argv: &[
        // An untrusted repository's `.hermes/plugins` refuses any launch: see `repo_config`.
        Arg::RefuseUntrustedAnyAuth,
        Arg::Lit("chat"),
        // `--resume <session_id>`, the id the `result` frame names.
        Arg::Resume,
        Arg::Flag("-q", Field::Prompt),
        Arg::Lit("--format"),
        Arg::Lit("stream-json"),
        // Bypasses Hermes' dangerous-command approvals, which a headless node has no one to
        // answer. Never `--accept-hooks`: running the operator's hooks is theirs to consent to.
        Arg::Lit("--yolo"),
        // "Use 'tool' for third-party integrations that should not appear in user session lists."
        Arg::Lit("--source"),
        Arg::Lit("tool"),
        // Omitted where the launch names none: the operator's configured model.
        Arg::Flag("-m", Field::Model),
    ],
    pane: None,
    env: &[Env {
        key: MANAGED_DIR_ENV,
        val: Val::Under(MANAGED_DIR),
        when: When::Always,
    }],
    stream: Some(&STREAM),
    // No verb maps: which `-t` toolsets withhold writes is unmeasured, so the node runs on
    // Hermes' own tools and a declared verb is refused by name.
    tool_names: &[],
    // `mcp__<server>__<tool>` (`tools/mcp_tool_schema.py`: "the convention shared by Claude Code,
    // Codex and OpenCode").
    spelling: Spelling::Fixed(ToolSpelling::McpDoubleUnderscore),
    // The managed document in both modes.
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Document,
    },
    live_declaration: Some(LiveDeclaration::EnvDir {
        key: MANAGED_DIR_ENV,
        dir: MANAGED_DIR,
    }),
    // JSON is YAML. Written under every mode, declared or not, because it carries the switch.
    files: &[ConfigFile {
        path: "managed/config.yaml",
        modes: Modes::All,
        base: "{}",
        servers: Some(MCP_SERVERS),
        servers_key: MCP_SERVERS_KEY,
        borrowed_login: &[(ADOPT_EXTERNAL_LOGINS, false)],
        pretty: true,
    }],
    overlay_documents: &[],
    // v0.22.2 `hermes_cli/plugins_discovery.py`: with `HERMES_ENABLE_PROJECT_PLUGINS` set, which
    // a node inherits from the operator, every plugin under the cwd's `.hermes/plugins` loads and
    // runs, and no switch of Hermes' keeps it out, so any launch in an untrusted repository
    // holding it is refused ([`Arg::RefuseUntrustedAnyAuth`]). Read from source, not measured.
    repo_config: &[".hermes/plugins"],
    // Any provider Hermes is configured for, by its key.
    login_env: LoginEnv {
        login: &[],
        any_provider: true,
    },
    token: TokenCarriers::both(TokenCarrier::DeclaredFile {
        note: "v0.22.2 source: the managed document is merged into the config Hermes loads, and \
               the operator's HERMES_HOME keeps config backups, so the document names the 0600 \
               token file and no copy of it carries the token; a stdio server gets only a safe \
               base environment plus its configured `env` (`tools/mcp_tool_transport.py`)",
    }),
    // marion compiles no constraint at all, and says so (cline's record, for the same reason).
    constraint: Constraint::Fixed {
        prefix: "harness-default:",
        value: "unconstrained",
    },
    resume: Some(Resume::Flag("--resume")),
    prompt_specials: &[],
    updates: UpdatePolicy::Never {
        note: "v0.22.2: updates apply only via an explicit `hermes update`; `--version` runs a \
               passive cached check that never applies",
    },
    boot: Boot::Unmeasured {
        note: "no working Hermes 0.22.2 is installed, so no boot was timed here; the floor \
               applies.",
    },
    push: Push::McpLog,
    // A stdio server without `trust: untrusted` runs every tool unasked (`tools/mcp_tool.py`'s
    // trust tiers), so no grant of marion's is load-bearing; `--yolo` is for the node's own tools.
    approval: Approval::ApproveAll {
        note: "v0.22.2 source: an MCP server's tools need approval only under `trust: \
               untrusted`, and marion's entry states none; `--yolo` bypasses the \
               dangerous-command approvals of Hermes' own tools, which a headless node has no one \
               to answer",
    },
    read_only: ReadOnly::ScopeOnly {
        note: "unmeasured: which `-t` toolsets withhold writes was not measured, and no working \
               Hermes 0.22.2 is installed for s38",
    },
    client_name: None,
    stderr_boilerplate: &[],
    delivery: Deliveries {
        headless: TurnDelivery::None {
            note: "`chat --resume <id> -q …` is read from the v0.22.2 source, not measured: no \
                   working release is installed for S31",
        },
        interactive: TurnDelivery::None {
            note: "Hermes' TUI was not measured, and its native lane ships disabled",
        },
    },
    abort: Aborts {
        headless: AbortVerb::None {
            note: "launch-only: no channel after launch, and SIGINT mid-turn is unmeasured",
        },
        interactive: AbortVerb::None {
            note: "no interactive shape marion can type into was measured, so a cancel kills the \
                   node",
        },
    },
    boot_dialogs: BootDialogs {
        dialogs: &[],
        asks: &[],
        remembers: Remembers::Nothing,
        note: "Hermes' first screen was not measured, and its native lane ships disabled",
    },
    // No endpoint recipe: `--provider custom --base-url` puts its key on argv, which `ps` shows.
    wires: &[],
    // Hermes keeps its logins in `$HERMES_HOME/auth.json`; a profile is a home of its own.
    // Existence is the whole probe — the file is never opened.
    profile: Some(ProfileCarrier {
        env: "HERMES_HOME",
        clear: &[],
        status: ProfileStatus::FileExists("auth.json"),
        login_hint: "auth add <provider>",
        home_default: ".hermes",
        shared: &[],
        note: "v0.22.2 source: `hermes_cli/auth.py` keeps logins in $HERMES_HOME/auth.json",
    }),
    note: "read from the hermes-agent v0.22.2 source (hermes_cli/stream_json.py, _parser.py, \
           managed_scope.py, agent/credential_sources.py); no capture yet, and conformance is \
           pending a working 0.22.2 install. A command that reaches Hermes through a wrapper \
           (npx, uvx, python -m) is not recognised as Hermes, so it is neither held nor guarded",
    requires: &[
        // The canned route (`--provider custom --base-url <url> --api-key canned`) is not built.
        Requirement {
            modes: Modes::Overlay,
            need: Need::NoRecipe,
            why: "Hermes has no canned or endpoint provider route yet: it runs only on the \
                   operator's own login, so launch it without --canned or a provider",
        },
        Requirement {
            modes: Modes::All,
            need: Need::PathAbsent(SYSTEM_MANAGED_DIR),
            why: "your administrator's Hermes managed scope at /etc/hermes would be shadowed by \
                   marion's, which carries the switch that stops a child borrowing your codex and \
                   claude logins; add `auth: {adopt_external_logins: false}` to \
                   /etc/hermes/config.yaml, and marion cannot launch Hermes until it can name a \
                   scope that keeps yours",
        },
        Requirement {
            modes: Modes::All,
            need: Need::EnvUnset(MANAGED_DIR_ENV),
            why: "your HERMES_MANAGED_DIR would be replaced by marion's, which carries the switch \
                   that stops a child borrowing your codex and claude logins; add `auth: \
                   {adopt_external_logins: false}` to the config.yaml there, and marion cannot \
                   launch Hermes until it can name a scope that keeps yours",
        },
    ],
    held: Some(HELD),
    axes: AxesRule::Split,
    model: ModelForm::AsGiven,
    readiness: Readiness::Ungated,
    advertised: Advertised::NONE,
    // v0.22.2's default toolsets carry the file and terminal tools, and nothing marion compiles
    // withholds them.
    writes_without_grant: true,
    containment: crate::containment::ContainmentRule::ToolsOnly,
    os_sandbox: crate::os_sandbox::OsSandboxRule::Unsupported {
        why: "Hermes keeps its sessions, logs and auth lock in the operator's HERMES_HOME, and its \
              writes under marion's profile are unmeasured",
    },
    read_only_modes: &[],
};

/// The declaration's shape in the managed document: an untyped stdio entry, `command`, `args` and
/// `env` (`tools/mcp_tool_transport.py`).
pub const MCP_SERVERS: McpServers = McpServers {
    typed: false,
    nested: None,
    tools: &[],
    pretty: true,
};

/// A `tool_use` frame: a call to any tool.
const TOOL_USE: Where = Where {
    frame: &[Cond::Eq("/type", "tool_use")],
    each: None,
    unit: &[],
};
/// A `tool_result` frame: a call's answer.
const TOOL_RESULT: Where = Where {
    frame: &[Cond::Eq("/type", "tool_result")],
    each: None,
    unit: &[],
};
/// A `text` frame: one delta of the model's words.
const TEXT: Where = Where {
    frame: &[Cond::Eq("/type", "text")],
    each: None,
    unit: &[],
};
/// The one terminal `result` frame.
const RESULT: Where = Where {
    frame: &[Cond::Eq("/type", "result")],
    each: None,
    unit: &[],
};

/// How a `hermes chat --format stream-json` stream is read (v0.22.2 `hermes_cli/stream_json.py`).
///
/// `system/init {model, session_id}`, then `text` deltas, `tool_use {name, tool_call_id?,
/// input?}` and `tool_result {name, tool_call_id?, output, is_error}`, then one `result
/// {session_id, exit_code, text, tokens, error?}`. A call is paired to its result by
/// `tool_call_id`, which the emitter writes only where the agent gave one: a call without it reads
/// as unknown rather than guessed.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: TOOL_USE,
    name: Name::Prefixed("/name"),
    args: "/input",
    pairing: Pairing::Separate {
        call_id: "/tool_call_id",
        result: TOOL_RESULT,
        result_id: "/tool_call_id",
        verdict: Verdict::ErrorFlag {
            path: "/is_error",
            words: &["/output"],
            fallback: "the tool_result frame carried no output",
        },
    },
    refused_report: OnRefusedReport::Record,
    failures: &[
        Failure::Frame {
            at: Where {
                frame: &[Cond::Eq("/type", "result"), Cond::Has("/error")],
                each: None,
                unit: &[],
            },
            words: &["/error"],
            fallback: "the run's result frame reported an error",
        },
        Failure::NotOk {
            at: RESULT,
            path: "/exit_code",
            ok: "0",
            words: &["/text"],
            label: "the run's result frame reported exit code ",
        },
    ],
    errors: &[ErrorRule {
        at: Where {
            frame: &[Cond::Eq("/type", "result"), Cond::Has("/error")],
            each: None,
            unit: &[],
        },
        status: None,
        kind: None,
        words: &["/error"],
    }],
    file_changes: None,
    // The `result` frame's: `system/init` is written before the agent starts and may name none.
    session: Some(SessionId {
        at: RESULT,
        path: "/session_id",
        resumes_in_place: false,
        by_title: None,
    }),
    model: Some(ModelName {
        at: Where {
            frame: &[Cond::Eq("/type", "system"), Cond::Eq("/subtype", "init")],
            each: None,
            unit: &[],
        },
        path: "/model",
    }),
    // The `result` frame totals the run in Hermes' canonical buckets, whose `input` is uncached
    // (`agent/codex_runtime.py` subtracts the cached reads before reporting it).
    usage: Some(UsageRule {
        at: RESULT,
        input: "/tokens/input",
        output: "/tokens/output",
        cache_read: Some("/tokens/cache_read"),
        cache_write: Some("/tokens/cache_write"),
        reasoning: None,
        input_includes_cache: false,
        fold: UsageFold::Last,
        in_flight: None,
        uncounted: Some(
            "unmeasured: Hermes' auxiliary requests (session titles, background review) may sit \
             outside the result frame's tokens",
        ),
    }),
    activity: Some(ActivityRule {
        calls: &[ToolUnit {
            at: TOOL_USE,
            name: "/name",
            args: "/input",
            id: Some("/tool_call_id"),
            shape: CallShape::Tool,
            end: None,
        }],
        // Deltas: "a consumer concatenating `text` events must reproduce the answer byte for byte".
        text: &[TextUnit {
            at: TEXT,
            path: "/text",
            joins: true,
        }],
    }),
    rate_limit: None,
};

/// Hermes Agent v0.22.2, headless `chat -q --format stream-json`, served by its row alone.
pub const ADAPTER: DataAdapter = DataAdapter::of(&SPEC);

/// This row's entry in [`crate::adapter::ROWS`].
pub const ROW: Row = Row {
    spec: &SPEC,
    serve: Serve::Data,
};

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use marion_core::contract::AgentId;
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::Auth;
    use crate::invocation::Invocation;
    use crate::mcp_bridge::BridgeEnv;
    use crate::spec::{Fields, Shape, render};
    use crate::stream::{CallOutcome, MarionCall};

    fn fields() -> Fields {
        Fields {
            cwd: "/tmp/wt".into(),
            config_dir: "/tmp/cfg".into(),
            auth: Auth::Inherited,
            prompt: "do the task".into(),
            ..Fields::default()
        }
    }

    fn compile(f: &Fields) -> Invocation {
        render(&SPEC, Shape::Headless, f).unwrap()
    }

    fn bridge() -> BridgeEnv {
        BridgeEnv {
            node_token_file: None,
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: None,
            auth: Auth::Inherited,
            agent_id: AgentId("019f-child".into()),
            agent_type: "hermes".into(),
            depth: 1,
            node_token: Some("tok-secret".into()),
            ready_file: None,
        }
    }

    /// Lines in `stream_json.py`'s shape: `init`, a marion call and its result, the `result`.
    fn stream(result_is_error: bool, tail: Value) -> String {
        [
            json!({"type": "system", "subtype": "init", "model": "nous/hermes-4",
                   "session_id": ""}),
            json!({"type": "text", "text": "on it"}),
            json!({"type": "tool_use", "name": "mcp__marion__report", "tool_call_id": "c1",
                   "input": {"summary": "done"}}),
            json!({"type": "tool_result", "name": "mcp__marion__report", "tool_call_id": "c1",
                   "output": "refused: not authorized", "duration_ms": 3,
                   "is_error": result_is_error}),
            tail,
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
    }

    fn result(exit_code: i64, error: Option<&str>) -> Value {
        let mut r = json!({"type": "result", "session_id": "20261017_abc", "exit_code": exit_code,
                           "text": "all done", "tokens": {"input": 90, "output": 20, "total": 120,
                           "cache_read": 10, "cache_write": 0}, "duration_ms": 5});
        if let Some(e) = error {
            r["error"] = json!(e);
        }
        r
    }

    fn prefix() -> String {
        ToolSpelling::McpDoubleUnderscore.spell("")
    }

    #[test]
    fn the_argv_is_the_headless_stream_json_chat_with_yolo_and_the_tool_source() {
        let inv = compile(&fields());
        assert_eq!(inv.program, "hermes");
        assert_eq!(
            inv.args,
            [
                "chat",
                "-q",
                "do the task",
                "--format",
                "stream-json",
                "--yolo",
                "--source",
                "tool"
            ]
        );
        assert!(!inv.args.iter().any(|a| a == "--accept-hooks"));
        let mut f = fields();
        f.model = Some("nous/hermes-4".into());
        f.resume = Some("20261017_abc".into());
        let inv = compile(&f);
        assert_eq!(&inv.args[..3], ["chat", "--resume", "20261017_abc"]);
        assert_eq!(&inv.args[inv.args.len() - 2..], ["-m", "nous/hermes-4"]);
    }

    /// The managed scope is named on every launch, because the borrowed-login switch lives there.
    #[test]
    fn every_launch_names_the_managed_scope_under_the_nodes_own_directory() {
        let inv = compile(&fields());
        assert!(
            inv.env
                .contains(&(MANAGED_DIR_ENV.to_string(), "/tmp/cfg/managed".to_string())),
            "{:?}",
            inv.env
        );
    }

    /// One document carries the switch and marion's server, under the key Hermes reads, and names
    /// the token's file rather than the token.
    #[test]
    fn the_managed_document_stops_borrowed_logins_and_declares_marion_without_its_token() {
        let carrier = SPEC.token.for_auth(Auth::Inherited);
        let declared = carrier.declared(&bridge(), Path::new("/tmp/cfg/node-token"));
        let doc: Value =
            serde_json::from_str(&SPEC.files[0].render(Some(&declared)).unwrap()).unwrap();
        assert_eq!(doc["auth"]["adopt_external_logins"], json!(false));
        let entry = &doc[MCP_SERVERS_KEY]["marion"];
        assert_eq!(entry["command"], json!("/bin/marion-supervisor"));
        assert_eq!(entry["args"], json!(["mcp"]));
        assert_eq!(
            entry["env"]["MARION_NODE_TOKEN_FILE"],
            json!("/tmp/cfg/node-token")
        );
        assert!(!doc.to_string().contains("tok-secret"), "{doc}");
        assert!(
            doc.get("mcpServers").is_none(),
            "Hermes reads `mcp_servers`"
        );
        // Undeclared, the switch is still written.
        let bare: Value = serde_json::from_str(&SPEC.files[0].render(None).unwrap()).unwrap();
        assert_eq!(bare, json!({"auth": {"adopt_external_logins": false}}));
        assert_eq!(
            PathBuf::from(SPEC.files[0].path).parent(),
            Some(Path::new(MANAGED_DIR))
        );
    }

    #[test]
    fn a_marion_call_is_paired_to_its_result_by_the_tool_call_id() {
        let ok = stream(false, result(0, None));
        assert_eq!(
            crate::grammar::marion_calls(&STREAM, &ok, &prefix()),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered,
            }]
        );
        let refused = stream(true, result(0, None));
        match &crate::grammar::marion_calls(&STREAM, &refused, &prefix())[0].outcome {
            CallOutcome::Refused(words) => {
                assert!(words.contains("refused: not authorized"), "{words}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(
            crate::grammar::parse_stream(&STREAM, &ok, &prefix()).failure,
            None
        );
    }

    /// The `result` frame's `error` is the failure claim, ahead of its exit code.
    #[test]
    fn a_result_that_failed_is_the_runs_failure_claim() {
        let failed = stream(false, result(1, Some("HTTP 401: invalid api key")));
        let out = crate::grammar::parse_stream(&STREAM, &failed, &prefix());
        assert_eq!(out.failure.as_deref(), Some("HTTP 401: invalid api key"));
        assert_eq!(
            crate::grammar::error_lines(&STREAM, &failed),
            ["HTTP 401: invalid api key"]
        );
        let silent = stream(false, result(1, None));
        let out = crate::grammar::parse_stream(&STREAM, &silent, &prefix());
        assert_eq!(out.failure.as_deref(), Some("all done"));
    }

    #[test]
    fn the_session_model_and_usage_come_from_the_init_and_result_frames() {
        let s = stream(false, result(0, None));
        let frames = crate::stream::json_frames(&s);
        let sessions: Vec<String> = frames
            .iter()
            .filter_map(|f| crate::grammar::session_id(&STREAM, f))
            .collect();
        assert_eq!(sessions, ["20261017_abc"]);
        assert_eq!(
            crate::grammar::model_in(STREAM.model.as_ref().unwrap(), &frames[0]).as_deref(),
            Some("nous/hermes-4")
        );
        let u = crate::grammar::usage(STREAM.usage.as_ref().unwrap(), &frames).unwrap();
        assert_eq!((u.input, u.output, u.cache_read), (90, 20, 10));
    }
}
