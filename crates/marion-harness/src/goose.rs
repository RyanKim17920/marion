//! The goose adapter — the headless `run -t` surface (fixture `tests/fixtures/s26/`).
//!
//! Measured against **goose 1.49.0** on 2026-09-05, every probe against a canned local OpenAI
//! Chat Completions endpoint at $0.00. The shape is gemini's and copilot's: marion compiles argv +
//! env, starts the process, and reads its JSONL. There is no channel to steer a turn, so control is
//! `LaunchOnly` and the prompt rides `-t`.
//!
//! What is *not* like the other five, and is the content of this module:
//!
//! - **The declaration is one argv token, and goose persists it.** `--with-extension
//!   "<name>:<ENV=v …> <command> <args…>"` declares a stdio MCP server; the string is split on
//!   whitespace, and every `ENV=v` pair before the command is stored **verbatim** in goose's
//!   session store (`goose session list --format json` prints them back; even a `--no-session`
//!   run touches the store's WAL). So [`extension_declaration`] carries the bridge's identity and
//!   nothing secret, and the node token — where marion minted one — rides the process environment
//!   instead, which the extension child **inherits** (`goose-env-inherit.mcp.jsonl`).
//! - **The built-in extension is the availability axis, as one unit.** `--no-profile` is the only
//!   way to load nothing but marion: with an empty config dir and no flag goose still loads five
//!   extensions and offers nineteen tools. `--with-builtin developer` adds `edit`, `shell`,
//!   `write`, `tree` and `read_image` together — there is no per-tool grant — so a `write`
//!   declaration compiles the whole extension, and `read` is refused rather than answered with an
//!   extension that also carries `shell`.
//! - **Headless approval is `auto` or nothing.** `GOOSE_MODE=approve` aborts the run at exit 1
//!   after the `toolRequest` frame; `chat` withholds every call; unset behaves as `auto`. marion
//!   states `auto` rather than leaning on a default.
//! - **Neither fault is an exit code.** An `isError: true` MCP result arrives as
//!   `toolResult.status: "success"` with `value.isError: true`, the model gets another turn, exit
//!   0. A provider 500 is four retries, then an ordinary assistant `message` beginning `Ran into
//!   this error:` and a zero-token `complete`, exit 0 — the stream has no error frame at all, so
//!   the only honest reading of a provider fault is the report that never came.
//! - **`GOOSE_CONFIG_DIR` does nothing on 1.49.0.** `HOME` is the isolation: it relocates
//!   `~/.config/goose`, the sqlite session store and the request-dump logs together.

use marion_core::agent_type;
use marion_core::harness::Harness;

use crate::grammar::{Cond, Name, OnRefusedReport, Pairing, StreamGrammar, Verdict, Where};
pub use crate::mcp_bridge::BridgeEnv;
use crate::mcp_bridge::NODE_TOKEN_ENV;
use crate::spec::{
    Arg, Constraint, Env, Field, HarnessSpec, LiveDeclaration, McpRoute, McpRoutes, Spelling,
    Surfaces, ToolSpelling, UpdatePolicy, Val, When,
};

/// `$HOME`'s name under the node's config dir — one spelling for [`SPEC`]'s env row and [`home`].
const HOME_DIR: &str = "home";

/// goose's row. Measured against 1.49.0 on 2026-09-05 (`tests/fixtures/s26/`), every probe against
/// a canned local OpenAI Chat Completions endpoint at $0.00.
///
/// **Live mode is a removal.** The home relocation and the whole provider block go together: they
/// exist to point a node at marion's endpoint without touching the operator's `~/.config/goose`,
/// and a live node's premise is the opposite. The survivors — `GOOSE_MODE=auto` and a `GOOSE_MODEL`
/// where the launch names one — are launch requirements, not isolation.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Goose,
    surfaces: Surfaces::LaunchOnly,
    program: Some("goose"),
    argv: &[
        Arg::Lit("run"),
        Arg::Flag("-t", Field::Prompt),
        Arg::Lit("--output-format"),
        Arg::Lit("stream-json"),
        // Without `-q` a three-line banner precedes the JSONL **on stdout**.
        Arg::Lit("-q"),
        // No session row for a node whose id marion cannot read back anyway (see `resume`).
        Arg::Lit("--no-session"),
        // Nothing but marion, measured: the alternative is nineteen tools from five extensions.
        Arg::Lit("--no-profile"),
        // The built-in extension a `write` declaration compiles; nothing under the default mode.
        Arg::Flag("--with-builtin", Field::Mode),
        Arg::Flag("--with-extension", Field::McpConfig),
    ],
    pane: None,
    env: &[
        Env {
            key: HOME_ENV,
            val: Val::Under(HOME_DIR),
            when: When::Canned,
        },
        Env {
            key: PROVIDER_ENV,
            val: Val::Lit(PROVIDER),
            when: When::Canned,
        },
        // Present or absent: a live launch that names no model leaves the operator's own.
        Env {
            key: MODEL_ENV,
            val: Val::Field(Field::Model),
            when: When::Always,
        },
        Env {
            key: HOST_ENV,
            val: Val::Field(Field::BaseUrl),
            when: When::Canned,
        },
        // Together with the host: marion's `…/v1` base plus this is `/v1/chat/completions`.
        Env {
            key: BASE_PATH_ENV,
            val: Val::Lit(BASE_PATH),
            when: When::Canned,
        },
        Env {
            key: API_KEY_ENV,
            val: Val::Field(Field::ApiKey),
            when: When::Canned,
        },
        // `approve` aborts a headless run at exit 1; `chat` withholds every call. Stated.
        Env {
            key: MODE_ENV,
            val: Val::Lit(AUTO_MODE),
            when: When::Always,
        },
    ],
    stream: Some(&STREAM),
    // `write` → the developer extension's `write` (`goose-with-builtin-developer.provider-request-1.json`).
    // `read` is absent on purpose: the extension has no read-only tool, and the `--with-builtin`
    // it would compile carries `shell` and `write` too.
    tool_names: &[(agent_type::TOOL_WRITE, "write")],
    // `<extension>__<tool>`, a double underscore and no `mcp` prefix — measured in `tools[]` and in
    // `toolCall.value.name` (`goose-report.stdout.jsonl`).
    spelling: Spelling::Fixed(ToolSpelling::ServerDoubleUnderscoreTool),
    // One argv token in both modes; the needle is the extension's `<name>:` prefix.
    mcp: McpRoutes {
        canned: McpRoute::Argv(EXTENSION_KEY),
        live: McpRoute::Argv(EXTENSION_KEY),
    },
    live_declaration: Some(LiveDeclaration::ArgvInline {
        flag: "--with-extension",
        key: EXTENSION_KEY,
        body: extension_declaration,
    }),
    // On goose the built-in extension **is** the constraint: recorded in both states, because a
    // node that ran with no builtin ran under a real one.
    constraint: Constraint::Mode {
        prefix: "with-builtin:",
        default: NO_BUILTIN,
    },
    // `--resume -n <name>` / `--resume --session-id <id>` exist on 1.49.0, but no frame of the
    // stream carries the id (`goose-session-first.stdout.jsonl` is byte-identical to the
    // no-session run), so marion has nothing to hand back and a resume is refused by name.
    resume: None,
    // goose 1.49.0 does not update itself: `goose update` is an explicit subcommand (`--help`:
    // "Update the goose CLI version"; its strings carry the Sigstore-verified replace path and
    // nothing else), the binary contains no check-for-update, "new version" or "update
    // available" text, and its `GOOSE_*` variable list has no update entry. There is nothing to
    // switch off, and the row says so rather than inventing a variable.
    updates: UpdatePolicy::None {
        note: "1.49.0 never updates itself on `run`/`session`: no update check in the binary's \
               strings, no `GOOSE_*` update variable; `goose update` is explicit only",
    },
    note: "S26 on goose 1.49.0: the run -t surface, env-only provider selection, --no-profile \
           with --with-builtin developer as the one availability unit, the --with-extension token \
           as the declaration route with the bridge's environment inherited; harness_matrix's \
           goose cell runs this row end to end",
};

/// How a `goose run --output-format stream-json -q` stream is read (`tests/fixtures/s26/`).
///
/// Two frame types: `{"type":"message","message":{role, content[]}}` and a final
/// `{"type":"complete"}`. A call is a `toolRequest` item of an assistant message's `content[]`,
/// its result a `toolResponse` item of a later `user` message, paired by `id` — the provider's own
/// call id, quoted back. The verdict is `toolResult.value.isError`, **not** `toolResult.status`:
/// an `isError: true` answer was measured arriving under `status: "success"`.
///
/// **No failure frame exists.** A provider 500 is an assistant `message` whose text begins `Ran
/// into this error:` and a `complete` with zero tokens, exit 0; the grammar's conditions match
/// whole values, so nothing here claims a failure from prose, and a provider fault reads as the
/// report that never came (`StreamOutcome::narrative == None`).
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[Cond::Eq("/type", "message")],
        each: Some("/message/content"),
        unit: &[Cond::Eq("/type", "toolRequest")],
    },
    name: Name::Prefixed("/toolCall/value/name"),
    args: "/toolCall/value/arguments",
    pairing: Pairing::Separate {
        call_id: "/id",
        result: Where {
            frame: &[Cond::Eq("/type", "message")],
            each: Some("/message/content"),
            unit: &[Cond::Eq("/type", "toolResponse")],
        },
        result_id: "/id",
        verdict: Verdict::ErrorFlag {
            path: "/toolResult/value/isError",
            words: &["/toolResult/value/content", "/toolResult/error"],
            fallback: "the tool call failed without a message",
        },
    },
    refused_report: OnRefusedReport::Fail,
    failures: &[],
    file_changes: None,
    session: None,
};

/// Relocates `~/.config/goose` (config, `GOOSE_MODE`), the sqlite session store under
/// `~/.local/share/goose/sessions/` and the request-dump logs under `~/.local/state/goose/logs/`
/// — every one of which a run writes, `--no-session` or not. `GOOSE_CONFIG_DIR` was measured to
/// relocate nothing on 1.49.0 (`goose-info-paths.txt`). **Dropped under [`Auth::Inherited`]**:
/// relocating the home is exactly what hides the operator's own configuration.
pub const HOME_ENV: &str = "HOME";
/// Selects the provider by name; `openai` is the Chat Completions client. **Dropped under
/// [`Auth::Inherited`]**: a live node uses the operator's own.
pub const PROVIDER_ENV: &str = "GOOSE_PROVIDER";
/// The model the provider names in every request. Mandatory under canned: the `openai` provider
/// has no default, and marion will not guess one.
pub const MODEL_ENV: &str = "GOOSE_MODEL";
/// The origin the `openai` provider posts to — taken **verbatim**, with [`BASE_PATH_ENV`]
/// appended after a `/`. marion hands its `…/v1` form and a base path that completes it, rather
/// than deriving a bare origin, so the URL the provider sees is the one every other row proves.
pub const HOST_ENV: &str = "OPENAI_HOST";
/// The path appended to [`HOST_ENV`]; goose's default is `v1/chat/completions`
/// (`goose-provider-paths.txt`), which against marion's `…/v1` host would double the segment.
pub const BASE_PATH_ENV: &str = "OPENAI_BASE_PATH";
/// Sent as `Authorization: Bearer …`; never echoed by the CLI (grepped its logs). **Dropped under
/// [`Auth::Inherited`]**.
pub const API_KEY_ENV: &str = "OPENAI_API_KEY";
/// `auto` | `approve` | `chat` | `smart_approve`. Kept under **both** modes: it is what makes a
/// headless run run at all, not isolation.
pub const MODE_ENV: &str = "GOOSE_MODE";

/// See [`PROVIDER_ENV`].
pub const PROVIDER: &str = "openai";
/// See [`BASE_PATH_ENV`].
pub const BASE_PATH: &str = "chat/completions";
/// See [`MODE_ENV`].
pub const AUTO_MODE: &str = "auto";

/// The built-in extension a `write` declaration compiles: `edit`, `shell`, `write`, `tree`,
/// `read_image`, **unprefixed**, as one unit (`goose-with-builtin-developer.provider-request-1.json`).
pub const DEVELOPER_BUILTIN: &str = "developer";
/// The constraint's default: `--no-profile` and no `--with-builtin` — the model is offered
/// marion's tools and nothing else.
pub const NO_BUILTIN: &str = "none";

/// The MCP server alias, and the extension's name. The model-facing tool name is
/// `<alias>__<tool>`, so this string is literally half of `marion__report`.
pub const MCP_ALIAS: &str = crate::spec::MCP_ALIAS;

/// The prefix of the `--with-extension` token that names marion's server — the needle the argv
/// route is checked for.
pub const EXTENSION_KEY: &str = "marion:";

/// Is this goose-native tool name one the developer extension carries?
pub fn is_developer_tool(native: &str) -> bool {
    matches!(native, "write" | "edit" | "shell" | "tree" | "read_image")
}

/// `$HOME` for a node: a directory under marion's own config dir, never the operator's home.
pub fn home(config_dir: &std::path::Path) -> std::path::PathBuf {
    config_dir.join(HOME_DIR)
}

/// The `--with-extension` token: `marion:<ENV=v …> <bridge> <args…>`, whitespace-separated, which
/// is how goose reads it (`goose-report.meta.json`).
///
/// **The node token is not here.** goose stores every `ENV=v` pair of this string in its session
/// store verbatim (`goose-session-list.json`), and a capability token written to a sqlite file
/// under the node's own home is a token a later reader of that file holds. It travels on the
/// process environment instead ([`inherited_env`]), which the extension child inherits
/// (`goose-env-inherit.mcp.jsonl`). Every other pair is the node's identity and is stated here, so
/// the declaration names the node the way every other harness's document does.
///
/// Values with whitespace cannot be spelled in this grammar; the adapter refuses such a launch by
/// name before rendering ([`unspellable`]), so this function never has to.
pub fn extension_declaration(b: &BridgeEnv) -> String {
    let mut parts = vec![EXTENSION_KEY.to_string()];
    for (k, v) in declared_pairs(b) {
        parts.push(format!("{k}={v}"));
    }
    parts.push(b.bridge.to_string_lossy().into_owned());
    parts.extend(b.args.iter().cloned());
    // The name prefix joins the first pair (or the command) directly: `marion:K=V …`.
    let (head, tail) = parts.split_at(1);
    format!("{}{}", head[0], tail.join(" "))
}

/// [`BridgeEnv::pairs`] without the node token — what the declaration string carries.
pub fn declared_pairs(b: &BridgeEnv) -> Vec<(String, String)> {
    b.pairs()
        .into_iter()
        .filter(|(k, _)| k != NODE_TOKEN_ENV)
        .collect()
}

/// What rides the process environment rather than the declaration: the node token, where one was
/// minted. Present or absent, never empty — the bridge's own rule for it.
pub fn inherited_env(b: &BridgeEnv) -> Vec<(String, String)> {
    b.node_token
        .iter()
        .map(|t| (NODE_TOKEN_ENV.to_string(), t.clone()))
        .collect()
}

/// The first token of the declaration that whitespace would split in two — the bridge program,
/// one of its args, or a pair's value — or `None` when the whole string is spellable.
pub fn unspellable(b: &BridgeEnv) -> Option<String> {
    let has_space = |s: &str| s.chars().any(char::is_whitespace);
    let program = b.bridge.to_string_lossy();
    if has_space(&program) {
        return Some(program.into_owned());
    }
    if let Some(a) = b.args.iter().find(|a| has_space(a)) {
        return Some(a.clone());
    }
    declared_pairs(b)
        .into_iter()
        .find(|(k, v)| has_space(k) || has_space(v))
        .map(|(k, v)| format!("{k}={v}"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The s26 captures this module's claims rest on, verbatim (paths and ids redacted).
    const REPORT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s26/goose-report.stdout.jsonl"
    ));
    const REPORT_ISERROR: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s26/goose-report-iserror.stdout.jsonl"
    ));
    const PROVIDER_500: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s26/goose-provider-500.stdout.jsonl"
    ));
    const CHAT_MODE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s26/goose-chat-mode.stdout.jsonl"
    ));
    const APPROVE_MODE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s26/goose-approve-mode.stdout.jsonl"
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
            agent_type: "goose".into(),
            depth: 1,
            node_token: Some("tok-secret-7c1f".into()),
            ready_file: None,
        }
    }

    /// A canned node as the adapter's hooks shape it: the declaration token, the token on the
    /// process environment, no builtin. The hooks' refusals are pinned in `adapter::tests`; these
    /// tests pin **the row**.
    fn spec() -> Fields {
        let b = bridge();
        Fields {
            cwd: "/tmp/wt".into(),
            config_dir: "/tmp/cfg".into(),
            auth: Auth::Canned,
            prompt: "do the task".into(),
            model: Some("canned-1".into()),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("sk-fake".into()),
            axes: Axes::default(),
            mcp_config: Some(extension_declaration(&b)),
            extra_env: inherited_env(&b),
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
    fn the_argv_is_the_measured_run_surface() {
        let inv = compile(&spec());
        assert_eq!(inv.program, "goose");
        assert_eq!(&inv.args[..3], ["run", "-t", "do the task"]);
        let f = inv
            .args
            .iter()
            .position(|a| a == "--output-format")
            .unwrap();
        assert_eq!(inv.args[f + 1], "stream-json");
        for flag in ["-q", "--no-session", "--no-profile"] {
            assert!(inv.args.contains(&flag.to_string()), "{flag} missing");
        }
        assert!(
            !inv.args.contains(&"--with-builtin".to_string()),
            "no builtin under the default mode"
        );
        assert_eq!(inv.cwd, PathBuf::from("/tmp/wt"));
    }

    #[test]
    fn a_write_declaration_compiles_the_developer_builtin() {
        let mut f = spec();
        f.axes.mode = Some(DEVELOPER_BUILTIN.into());
        let inv = compile(&f);
        let i = inv.args.iter().position(|a| a == "--with-builtin").unwrap();
        assert_eq!(inv.args[i + 1], "developer");
    }

    #[test]
    fn the_declaration_is_one_token_naming_marion_and_the_bridge() {
        let inv = compile(&spec());
        let i = inv
            .args
            .iter()
            .position(|a| a == "--with-extension")
            .unwrap();
        let token = &inv.args[i + 1];
        assert!(token.starts_with("marion:MARION_REPO=/repo "), "{token}");
        assert!(token.ends_with(" /bin/marion-supervisor mcp"), "{token}");
        assert!(token.contains(" MARION_AGENT_ID=019f-child "), "{token}");
        assert!(
            token.contains(" MARION_BASE_URL=http://127.0.0.1:8099/v1 "),
            "{token}"
        );
        assert!(token.contains(EXTENSION_KEY));
    }

    /// The whole point of splitting the declaration: goose persists the token's pairs.
    #[test]
    fn the_node_token_never_reaches_argv_and_rides_the_environment() {
        let inv = compile(&spec());
        for a in &inv.args {
            assert!(!a.contains("tok-secret-7c1f"), "the token is on argv: {a}");
            assert!(
                !a.contains(NODE_TOKEN_ENV),
                "the token's name is on argv: {a}"
            );
        }
        assert_eq!(
            env_of(&inv, NODE_TOKEN_ENV).as_deref(),
            Some("tok-secret-7c1f")
        );
        // And where none was minted, nothing is set — never an empty string.
        let mut b = bridge();
        b.node_token = None;
        assert!(inherited_env(&b).is_empty());
        assert!(!extension_declaration(&b).contains(NODE_TOKEN_ENV));
    }

    #[test]
    fn whitespace_in_the_declaration_is_named_not_split() {
        let mut b = bridge();
        assert_eq!(unspellable(&b), None);
        b.repo = "/my repo".into();
        assert_eq!(unspellable(&b), Some("MARION_REPO=/my repo".into()));
        let mut b = bridge();
        b.bridge = "/opt/marion bin/supervisor".into();
        assert_eq!(unspellable(&b), Some("/opt/marion bin/supervisor".into()));
    }

    #[test]
    fn the_canned_provider_block_is_env_and_the_host_keeps_marions_v1() {
        let inv = compile(&spec());
        assert_eq!(env_of(&inv, HOME_ENV).as_deref(), Some("/tmp/cfg/home"));
        assert_eq!(env_of(&inv, PROVIDER_ENV).as_deref(), Some("openai"));
        assert_eq!(env_of(&inv, MODEL_ENV).as_deref(), Some("canned-1"));
        assert_eq!(
            env_of(&inv, HOST_ENV).as_deref(),
            Some("http://127.0.0.1:8099/v1")
        );
        assert_eq!(
            env_of(&inv, BASE_PATH_ENV).as_deref(),
            Some("chat/completions")
        );
        assert_eq!(env_of(&inv, API_KEY_ENV).as_deref(), Some("sk-fake"));
        assert_eq!(env_of(&inv, MODE_ENV).as_deref(), Some("auto"));
    }

    #[test]
    fn live_mode_removes_the_home_and_the_provider_block_and_keeps_the_mode() {
        let inv = compile(&live_spec());
        for k in [HOME_ENV, PROVIDER_ENV, HOST_ENV, BASE_PATH_ENV, API_KEY_ENV] {
            assert_eq!(env_of(&inv, k), None, "{k} survived live mode");
        }
        assert_eq!(env_of(&inv, MODE_ENV).as_deref(), Some("auto"));
        assert_eq!(env_of(&inv, MODEL_ENV).as_deref(), Some("canned-1"));
        assert!(
            inv.args.iter().any(|a| a.starts_with(EXTENSION_KEY)),
            "the declaration survives live mode"
        );
    }

    #[test]
    fn the_report_is_a_tool_request_and_its_verdict_a_tool_response() {
        let out = parse_stream(REPORT);
        assert_eq!(
            out.narrative.as_deref(),
            Some("hello from goose under a canned provider")
        );
        assert_eq!(out.failure, None);
        assert_eq!(
            marion_calls(REPORT),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered,
            }]
        );
    }

    /// `toolResult.status` stays `"success"`; `value.isError` is the verdict, and the run exits 0.
    #[test]
    fn an_is_error_result_is_a_refusal_whatever_status_says() {
        let calls = marion_calls(REPORT_ISERROR);
        assert_eq!(calls.len(), 1);
        match &calls[0].outcome {
            CallOutcome::Refused(words) => {
                assert!(words.contains("refused: not authorized"), "{words}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let out = parse_stream(REPORT_ISERROR);
        assert!(out.failure.is_some(), "a refused report fails the run");
    }

    /// No frame claims a failure; the report simply never comes.
    #[test]
    fn a_provider_fault_is_the_report_that_never_came() {
        let out = parse_stream(PROVIDER_500);
        assert_eq!(out.narrative, None);
        assert!(marion_calls(PROVIDER_500).is_empty());
        assert_eq!(out.failure, None, "the stream has no error frame to read");
    }

    /// `chat` mode answers the request from inside goose: a `toolResponse` with no `isError`.
    /// Read as answered by the grammar — which is why marion compiles `auto`, never `chat`.
    #[test]
    fn chat_mode_is_indistinguishable_from_success_on_the_stream() {
        assert_eq!(marion_calls(CHAT_MODE)[0].outcome, CallOutcome::Answered);
    }

    /// `approve` mode aborts after the request: the call's result never arrives.
    #[test]
    fn approve_mode_leaves_the_call_unanswered() {
        assert_eq!(marion_calls(APPROVE_MODE)[0].outcome, CallOutcome::Unknown);
    }

    #[test]
    fn no_frame_carries_a_session_id() {
        for frame in crate::stream::json_frames(REPORT) {
            assert_eq!(crate::grammar::session_id(&STREAM, &frame), None);
        }
    }
}
