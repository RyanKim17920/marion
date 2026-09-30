//! The Gemini adapter — the headless `-p` surface (design §6.4, fixture `tests/fixtures/s12/`).
//!
//! Measured against **gemini CLI 0.53.0**. The surface is the same shape as `codex exec`: marion
//! writes the configuration, starts the process, and reads its NDJSON. There is no channel to
//! steer a turn, so control is `LaunchOnly` and the prompt rides argv.
//!
//! Three of the four env vars and two of the settings keys below are load-bearing in the §12 sense
//! — *omitting them produces no error anywhere*. Each one carries the measurement that says so.

use marion_core::agent_type;
use marion_core::harness::Harness;
use marion_core::provider::Wire;
use serde_json::{Value, json};

use crate::adapter::{
    HarnessAdapter, HarnessError, LaunchSpec, McpDeclaration, Row, SpawnCtx, declared_bridge,
};
use crate::auth::Auth;
use crate::env_filter::{EnvGrant, LoginEnv};
use crate::grammar::{
    ActivityRule, CallShape, Cond, ErrorRule, Failure, ModelName, Name, OnRefusedReport, Pairing,
    SessionId, StreamGrammar, TextUnit, ToolUnit, UsageFold, UsageRule, Verdict, Where,
};
pub use crate::mcp_bridge::BridgeEnv;
use crate::profile::{ProfileCarrier, Status as ProfileStatus};
use crate::spec::{
    Advertised, Approval, Arg, AxesRule, Body, BootDialog, BootDialogs, Constraint, Deliveries,
    DialogAnswer, Env, Field, HarnessSpec, LiveDeclaration, McpRoute, McpRoutes, ModelForm, Modes,
    Need, Push, ReadOnly, Readiness, Remembers, Requirement, Spelling, Surfaces, TokenCarriers,
    ToolSpelling, TurnDelivery, UpdatePolicy, Val, When, WireRecipe,
};
use std::path::PathBuf;

/// The live node's system-settings document, as bytes: [`live_settings_json`] with the bridge,
/// which is what `GeminiAdapter::config_files` writes for an [`Auth::Inherited`] launch.
pub fn live_settings_document(b: &BridgeEnv) -> String {
    serde_json::to_string_pretty(&live_settings_json(Some(b))).expect("a Value always serialises")
}

/// The settings document's name under the node's config dir — named by [`SYSTEM_SETTINGS_PATH_ENV`]
/// in [`SPEC`]'s env and written by `GeminiAdapter::config_files`, one spelling for both.
pub const SETTINGS_FILE: &str = "marion-settings.json";

/// Gemini CLI's row. Measured against 0.53.0 (`tests/fixtures/s12/`; §11 item 24 for the approval
/// mode). `LaunchOnly`: the prompt is the argument to `-p` — not positional (a bare positional
/// launches the interactive UI) and not stdin (which is *prepended as context* instead).
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Gemini,
    surfaces: Surfaces::LaunchOnly,
    program: Some("gemini"),
    argv: &[
        // Never omitted and never `auto`: the adapter refuses a launch without one.
        Arg::Flag("-m", Field::Model),
        Arg::Lit("--output-format"),
        Arg::Lit("stream-json"),
        // Ahead of `-p`, and only when the declaration names an edit tool, so a node that declares
        // nothing compiles the argv it always compiled, in the order it always compiled it.
        Arg::Flag("--approval-mode", Field::Mode),
        // Only when the declaration names the shell ([`ALLOWED_BY_NAME`]).
        Arg::Each("--allowed-tools", Field::Allowed),
        Arg::Flag("-p", Field::Prompt),
    ],
    pane: None,
    // **Live mode is a removal, and the two survivors are not part of the isolation.** S12's
    // settings-precedence table resolves the *system settings* layer through
    // `GEMINI_CLI_SYSTEM_SETTINGS_PATH` and the *user* layer through `GEMINI_CLI_HOME` — two
    // layers, two variables — so dropping the sandbox home leaves marion's MCP injection route
    // untouched. `GEMINI_CLI_TRUST_WORKSPACE` answers the folder-trust gate on marion's worktree,
    // which is a fresh directory under either auth mode.
    env: &[
        Env {
            key: CLI_HOME_ENV,
            val: Val::Under(""),
            when: When::Overlay,
        },
        Env {
            key: SYSTEM_SETTINGS_PATH_ENV,
            val: Val::Under(SETTINGS_FILE),
            when: When::Always,
        },
        Env {
            key: TRUST_WORKSPACE_ENV,
            val: Val::Lit("true"),
            when: When::Always,
        },
        // Never over a real `~/.gemini`: see [`FORCE_FILE_STORAGE_ENV`] — the migration it can
        // trigger deletes the operator's own `oauth_creds.json` (`tests/fixtures/s12/`).
        Env {
            key: FORCE_FILE_STORAGE_ENV,
            val: Val::Lit("true"),
            when: When::Overlay,
        },
        // Gated on the mode as well as on the value: a live spec handed an endpoint or a key gets
        // neither pushed, rather than an overlay that quietly outranks the operator's own resolution.
        Env {
            key: BASE_URL_ENV,
            val: Val::Field(Field::BaseUrlRoot),
            when: When::Overlay,
        },
        Env {
            key: API_KEY_ENV,
            val: Val::Field(Field::ApiKey),
            when: When::Overlay,
        },
    ],
    stream: Some(&STREAM),
    // `write` → `write_file`, measured on 0.53.0: under [`AUTO_EDIT_APPROVAL_MODE`] it appears in
    // `functionDeclarations`, and under the default mode nowhere but the prose of the system
    // instruction (§11 item 24). `read` → `read_file`, one of the eight declarations present under
    // the **default** mode (s14), so that grant is a no-op — answered anyway, because making it
    // conditional would narrow what this harness has always been able to do. s14 also measured
    // `--allowed-tools` neither gating nor validating on 0.53.0, so there is no flag to compile.
    //
    // `edit` → `replace`, the other name `auto_edit` puts back (§11 item 24). `bash` →
    // `run_shell_command`, which neither mode offers headless until it is allowed by name: measured
    // on 0.53.0 against a capture endpoint (2026-09-28), `--allowed-tools run_shell_command` put it
    // in `functionDeclarations` under both `default` and `auto_edit`, as did a `--policy` rule
    // allowing it; without either it is absent.
    tool_names: &[
        (agent_type::TOOL_READ, "read_file"),
        (agent_type::TOOL_WRITE, "write_file"),
        (agent_type::TOOL_EDIT, "replace"),
        (agent_type::TOOL_BASH, "run_shell_command"),
    ],
    // `mcp_<server>_<tool>`, single underscores — S12 captured `"tool_name":"mcp_marion_report"`.
    spelling: Spelling::Fixed(ToolSpelling::McpSingleUnderscore),
    // A document in both modes: `GEMINI_CLI_SYSTEM_SETTINGS_PATH` is resolved independently of
    // `GEMINI_CLI_HOME`, so live mode drops the sandbox home and the injection route survives.
    mcp: McpRoutes {
        canned: McpRoute::Document,
        live: McpRoute::Document,
    },
    // The system-settings layer, which **outranks the operator's own** per key and merges
    // `mcpServers` per key — measured S30, see [`settings_json_with_auth`]. That measurement is
    // what let the native lane for this harness ship enabled.
    live_declaration: Some(LiveDeclaration::EnvDocument {
        key: SYSTEM_SETTINGS_PATH_ENV,
        file: SETTINGS_FILE,
        // The system-settings layer, whose MCP block sits beside settings no `mcpServers` shape
        // states (trust, the auth selection it must leave alone).
        body: Body::Code(live_settings_document),
    }),
    // Its Gemini or Google key, and the Vertex login its switch selects.
    overlay_documents: &[],
    login_env: LoginEnv {
        login: &[
            EnvGrant::always("GEMINI_API_KEY"),
            EnvGrant::always("GOOGLE_API_KEY"),
            EnvGrant::always("GOOGLE_GEMINI_BASE_URL"),
            EnvGrant::always("GOOGLE_GENAI_USE_VERTEXAI"),
            EnvGrant::always("GOOGLE_CLOUD_PROJECT"),
            EnvGrant::always("GOOGLE_CLOUD_LOCATION"),
            EnvGrant::always("GOOGLE_APPLICATION_CREDENTIALS"),
        ],
        any_provider: false,
    },
    token: TokenCarriers::DECLARATION,
    // On this harness the mode **is** the constraint: under `default` the mutating tools are
    // withheld from `functionDeclarations` entirely. Recorded in both states.
    constraint: Constraint::Mode {
        prefix: "approval-mode:",
        default: DEFAULT_APPROVAL_MODE,
        allowed: Some("allowed-tools:"),
    },
    // 0.53.0's `-r, --resume` takes `"latest"` or an index number, not a session id, and
    // `--session-id` **starts** a session with a given UUID. Resuming a named session is unmeasured,
    // so a launch that asks is refused rather than pointed at whichever session is "latest".
    resume: None,
    // gemini 0.53.0 has **no** environment switch; both keys are settings booleans and ride the
    // system-settings document this row already emits on every route ([`settings_json_with_auth`]
    // applies them). `enableAutoUpdateNotification=false` skips the check, the banner and the
    // install; `enableAutoUpdate=false` alone stops the install and keeps the banner. Both, and
    // since the merge is per leaf key (S30) each one lands on the operator's own `general` block
    // without disturbing its other keys: this is an npm-global install and does self-update by
    // default.
    updates: UpdatePolicy::Document {
        keys: &[
            ("general.enableAutoUpdate", false),
            ("general.enableAutoUpdateNotification", false),
        ],
        note: "0.53.0 bundle: `checkForUpdates` reads `general.enableAutoUpdate` and \
               `general.enableAutoUpdateNotification`; no `GEMINI_CLI_*` update variable exists",
    },
    // Unmeasured: MCP's own logging notification, which this harness may show or drop.
    push: Push::McpLog,
    // On marion's own `mcpServers.marion` entry: without it the tool is dropped from the request
    // body at exit 0 (S12). `--yolo` is the only other route, and an admin can veto it.
    approval: Approval::DeclarationKey {
        key: "trust",
        contest: None,
        note: "S12 on 0.53.0: `trust: true` puts marion's tool in the request body; without it \
               the tool is omitted, no prompt, exit 0",
    },
    // s38 (0.53.0): without `auto_edit` there is no `write_file` and, headless, no shell.
    read_only: ReadOnly::ToolsAxis {
        verified: true,
        note: "s38 on 0.53.0: the default approval mode registers neither `write_file` nor \
               `run_shell_command` headless; a scripted call is `tool_not_registered`, no file \
               (the `generalist` subagent's tools exclude `write_file` too)",
    },
    client_name: None,
    stderr_boilerplate: &[],
    delivery: Deliveries {
        headless: TurnDelivery::None {
            note: "gemini's --resume takes `latest` or an index, not a session id, and S31 did not \
                   probe it; gemini 0.53 --acp refuses session/new (S31 p0a)",
        },
        interactive: TurnDelivery::None {
            note: "gemini 0.53's TUI stops at Google's retired sign-in, so S31 could not probe it",
        },
    },
    // S37 first screen (0.53.0, `tests/fixtures/s37-boot-dialogs/gemini-0.53.0.raw`): folder
    // trust, selection on `1. Trust folder`. Its answer was not measured (it persists to the
    // operator's trustedFolders), and the row takes no paste, so it is held.
    boot_dialogs: BootDialogs {
        dialogs: &[BootDialog {
            needle: "Do you trust the files in this folder?",
            action: "trust {repo} in gemini once (run `gemini` there and trust the folder)",
            answer: DialogAnswer::Hold,
            note: "S37 0.53.0 folder trust; its answer persists and was not measured",
        }],
        remembers: Remembers::Nothing,
        note: "S37 0.53.0, fresh directory, the operator's login, terminal queries answered: \
               folder trust before the composer",
    },
    wires: &[WireRecipe {
        wire: Wire::Gemini,
        env: &[],
        // The gemini wire carries its key in `x-goog-api-key` whatever the provider's row says;
        // Bearer is listed because it is the default every provider states.
        keys: &[crate::spec::BEARER_BY_OVERLAY],
        note: "Gemini `generateContent` alone, through `GOOGLE_GEMINI_BASE_URL`.",
    }],
    // gemini roots `.gemini/` under `GEMINI_CLI_HOME` (the canned row's isolation variable) and
    // writes its OAuth login to `.gemini/oauth_creds.json` there; sign-in is its first screen.
    profile: Some(ProfileCarrier {
        env: CLI_HOME_ENV,
        clear: &[],
        status: ProfileStatus::FileExists(".gemini/oauth_creds.json"),
        login_hint: "",
        home_default: "",
        shared: &[],
        note: "gemini 0.53.0: GEMINI_CLI_HOME roots .gemini/, oauth_creds.json holds the login",
    }),
    note: "S12 on gemini CLI 0.53.0: the -p surface, the four load-bearing env vars and the \
           system-settings injection route; §11 item 24 for --approval-mode auto_edit. \
           harness_matrix's gemini cell runs this row end to end",
    requires: &[
        // **The model is refused rather than defaulted.** A pinned id would be a guess marion has
        // no basis for, and S12 measured 0.53.0 rewriting even an explicit `-m gemini-2.5-flash` to
        // `gemini-3.5-flash` in the request path — so a "safe" default is not even reliably the
        // model that runs. The failure it prevents is the expensive one: with model `auto` the CLI
        // issues a classifier call to gemini-3.1-flash-lite over non-streaming `:generateContent`
        // and hung on retry 5.
        Requirement {
            modes: Modes::All,
            need: Need::Model,
            why: "an explicit -m is mandatory: with the default model `auto` the CLI first makes \
                  a classifier call that retried 5x and hung, and marion will not guess a model",
        },
        Requirement {
            modes: Modes::All,
            need: Need::HttpsOrLoopback,
            why: "GOOGLE_GEMINI_BASE_URL must be https unless the host is loopback; a non-loopback \
                  plain-http endpoint is refused by the CLI",
        },
    ],
    // §3.1's availability axis, in the only form this harness has one: **a mode, not a list.**
    // `mode` is `Some(auto_edit)` exactly when the declaration names an edit tool, and `None` —
    // the default mode, which this row then compiles no flag for — otherwise. The marion →
    // gemini mapping is `tool_names` above, and which *gemini* names need the mode is
    // `EDIT_TOOLS`, so neither half is restated here.
    // The shell is past what `auto_edit` approves, so it is granted by name.
    axes: AxesRule::Mode {
        mode: AUTO_EDIT_APPROVAL_MODE,
        when_any: EDIT_TOOLS,
        by_name: ALLOWED_BY_NAME,
    },
    model: ModelForm::AsGiven,
    readiness: Readiness::Ungated,
    // Nothing measured. S12 measured 0.53.0 rewriting an explicit `-m`, and MILESTONES records
    // `gemini -p` refused by the vendor on this machine, so no capability has been observed to
    // work — including through ACP, where S20 found `session/new` refused outright.
    advertised: Advertised::NONE,
    // The default approval mode drops the mutating tools from `functionDeclarations` outright.
    writes_without_grant: false,
    containment: crate::containment::ContainmentRule::ToolsOnly,
    read_only_modes: &[],
};

/// How a `gemini --output-format stream-json` stream is read (`tests/fixtures/s12/`).
///
/// The event set is closed and measured: `init | message | tool_use | tool_result | error |
/// result`. A call is a `tool_use` frame whose `tool_name` carries the `mcp_<server>_` spelling;
/// its result is a separate `tool_result` paired by `tool_id` — a pairing S12 recorded with the two
/// ids redacted independently, so it is the only reading the field name admits and is stated here
/// so that whoever next records a gemini run knows the fixture owes an unredacted pair. Only
/// `"success"` was ever captured for `tool_result.status`, so every other spelling is a refusal.
///
/// **The exit code is not the verdict, and that is a measurement**: S12 recorded an auth failure
/// exiting **0** with a JSON error body — the bare `{"error":{…}}` object the third rule reads, an
/// untyped frame the `error` and `result` rules would miss. A report's narrative is read off the
/// `tool_use` that made the call, because `tool_result` carries only an opaque `output` string.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[Cond::Eq("/type", "tool_use")],
        each: None,
        unit: &[],
    },
    name: Name::Prefixed("/tool_name"),
    args: "/parameters",
    pairing: Pairing::Separate {
        call_id: "/tool_id",
        result: Where {
            frame: &[Cond::Eq("/type", "tool_result")],
            each: None,
            unit: &[],
        },
        result_id: "/tool_id",
        verdict: Verdict::Status {
            path: "/status",
            ok: "success",
            pending: &[],
            words: &["/output"],
        },
    },
    refused_report: OnRefusedReport::Record,
    failures: &[
        Failure::Frame {
            at: Where {
                frame: &[Cond::Eq("/type", "error")],
                each: None,
                unit: &[],
            },
            words: &["/error/message", "/message", "/error/type"],
            fallback: "the child's stream carried an error frame",
        },
        Failure::NotOk {
            at: Where {
                frame: &[Cond::Eq("/type", "result")],
                each: None,
                unit: &[],
            },
            path: "/status",
            ok: "success",
            words: &[],
            label: "gemini result status: ",
        },
        // The exit-0 auth failure S12 recorded: an `error` object with no `type` frame around it.
        Failure::Frame {
            at: Where {
                frame: &[Cond::Has("/error")],
                each: None,
                unit: &[],
            },
            words: &["/error/message", "/error/type"],
            fallback: "the child's stream carried an error body",
        },
    ],
    // S37 (`gemini-0.53.0/p-errors-401.jsonl`): the result's `error.message` quotes the
    // provider's body, status included (`{"error":{"code":401,…}}`). A 429 or 500 is retried with
    // its status on stderr only (`Attempt 1 failed with status 429`).
    errors: &[ErrorRule {
        at: Where {
            frame: &[Cond::Has("/error")],
            each: None,
            unit: &[],
        },
        status: None,
        kind: None,
        words: &["/error/message", "/message"],
    }],
    file_changes: None,
    // The first `stream-json` frame (`s12/README.md`): `init` carries `session_id`. Recorded even
    // though the row's `resume` is `None` — the id is a fact about the run, and what 0.53.0 cannot
    // take back on argv a later build may.
    // The `init` frame names the model (`s12`, `conformance/gemini-0.53.0`).
    model: Some(ModelName {
        at: Where {
            frame: &[Cond::Eq("/type", "init")],
            each: None,
            unit: &[],
        },
        path: "/model",
    }),
    session: Some(SessionId {
        at: Where {
            frame: &[Cond::Eq("/type", "init")],
            each: None,
            unit: &[],
        },
        path: "/session_id",
        resumes_in_place: false,
        by_title: None,
    }),
    // The terminal `result` frame's `stats` (`s12/README.md`) totals the run. **Unmeasured past
    // that one README line**, whose `cached` is 0: that `input_tokens` counts cached tokens (and
    // `stats.input` is the uncached remainder) is assumed from the zero-cache capture, not shown.
    usage: Some(UsageRule {
        at: Where {
            frame: &[Cond::Eq("/type", "result")],
            each: None,
            unit: &[],
        },
        input: "/stats/input_tokens",
        output: "/stats/output_tokens",
        cache_read: Some("/stats/cached"),
        cache_write: None,
        reasoning: None,
        input_includes_cache: true,
        fold: UsageFold::Last,
        in_flight: None,
    }),
    // `tool_use` frames name every tool; the assistant's words arrive as `message` frames marked
    // `"delta":true` (`s12/README.md`), so consecutive ones are one message.
    // No frame measured carrying the account's usage window.
    rate_limit: None,
    activity: Some(ActivityRule {
        calls: &[ToolUnit {
            at: Where {
                frame: &[Cond::Eq("/type", "tool_use")],
                each: None,
                unit: &[],
            },
            name: "/tool_name",
            args: "/parameters",
            id: Some("/tool_id"),
            shape: CallShape::Tool,
            end: None,
        }],
        text: &[TextUnit {
            at: Where {
                frame: &[Cond::Eq("/type", "message"), Cond::Eq("/role", "assistant")],
                each: None,
                unit: &[],
            },
            path: "/content",
            joins: true,
        }],
    }),
};

/// Relocates the **entire** config and auth surface: `settings.json`, `oauth_creds.json`,
/// `trustedFolders.json`, extensions, sessions. The CLI appends `.gemini` itself, so this names
/// the parent of the sandbox home, not the home (S12: `GEMINI_CLI_HOME=$D` → `$D/.gemini/`).
///
/// **Dropped under [`Auth::Inherited`].** Relocating everything is precisely what hides the
/// operator's own credential from a node meant to use it — S12's COPYABLE verdict is about copying
/// a credential *into* a sandbox home, and marion copies nothing. It is dropped *independently* of
/// [`SYSTEM_SETTINGS_PATH_ENV`], which the same fixture records as an override of a different layer
/// resolved by its own env var, so the MCP injection route survives the drop intact.
pub const CLI_HOME_ENV: &str = "GEMINI_CLI_HOME";
/// Points at an arbitrary settings file that **wins over all four settings layers**. There is no
/// `--settings` flag, so this is the only non-invasive injection: it writes nothing the user owns
/// and needs no project `.gemini/` directory (S12, verified end to end).
pub const SYSTEM_SETTINGS_PATH_ENV: &str = "GEMINI_CLI_SYSTEM_SETTINGS_PATH";
/// Folder trust. A fresh sandbox directory is untrusted, and the trust check can block a headless
/// run outright (S12; the alternative is `--skip-trust`).
pub const TRUST_WORKSPACE_ENV: &str = "GEMINI_CLI_TRUST_WORKSPACE";
/// Pins the file credential path unconditionally. `HybridTokenStorage` otherwise probes a native
/// keychain under a 2 s timeout and only then falls back, so which store a child uses would
/// depend on a race (S12).
///
/// **Dropped under [`Auth::Inherited`], and that is a safety matter rather than tidiness.** Against
/// a throwaway `$GEMINI_CLI_HOME` it pins an empty store and costs nothing. Against the operator's
/// **real** `~/.gemini` — which is exactly what live mode leaves in place — it risks pushing their
/// actual `oauth_creds.json` through `OAuthCredentialStorage.migrateFromFileStorage()`, which
/// `tests/fixtures/s12/` records as reading the legacy file, writing the hybrid store, and then
/// `fs.rm`-ing the original: a **one-way destructive migration** of a file marion does not own.
/// §6.4's central MUST is that marion never mutates the user's real harness config, so this
/// variable may only ever be set over a config surface marion created.
pub const FORCE_FILE_STORAGE_ENV: &str = "GEMINI_FORCE_FILE_STORAGE";
/// Base-URL override for the `gemini-api-key` auth path. **Dropped under [`Auth::Inherited`]**:
/// live mode names no endpoint.
pub const BASE_URL_ENV: &str = "GOOGLE_GEMINI_BASE_URL";
/// The AI Studio key, which is the auth type [`settings_json`] selects. **Dropped under
/// [`Auth::Inherited`]**: marion mints no credential there, and a placeholder beside the
/// operator's own login would be a second credential competing with the real one.
pub const API_KEY_ENV: &str = "GEMINI_API_KEY";

/// The auth type [`settings_json`] selects on the canned path, where marion supplies
/// `GEMINI_API_KEY` itself.
pub const CANNED_AUTH_TYPE: &str = "gemini-api-key";

/// The approval mode that makes gemini's edit tools **exist**, and the whole of this harness's
/// availability axis (§3.1).
///
/// gemini has no `--tools` flag and no per-tool permission list: under the default approval mode
/// 0.53.0 withholds `write_file`, `replace` and `run_shell_command` from `functionDeclarations`
/// entirely, so the model is never offered them and the prose of its system instruction describes
/// tools it cannot call. §11 item 24 measured this mode putting `write_file` and `replace` back.
///
/// **This is not `-y`, and §6.4's standing objection to yolo mode does not reach it.** That
/// objection is that `--yolo` auto-approves *everything* and an admin can veto it outright through
/// `security.disableYoloMode`, so it is not a foundation marion can stand on. `auto_edit` is a
/// third value beside `default` and `yolo` (0.53.0 `--help`: *"auto_edit (auto-approve edit
/// tools)"*), scoped to edit tools and outside that veto.
///
/// Emitted **only** when an edit tool is actually declared ([`is_edit_tool`]). A node whose
/// `tools:` is the default `[]` is launched in the default mode it always was.
pub const AUTO_EDIT_APPROVAL_MODE: &str = "auto_edit";

/// The mode 0.53.0 runs in when marion passes no `--approval-mode` — which is every node that
/// declares no edit tool, i.e. every node marion has ever launched until now.
///
/// Named because §6.7's `allowed_tools` has to record *it* too. A node that ran under the default
/// mode ran under a real constraint — the mutating tools were withheld from `functionDeclarations`
/// outright — and a record that mentioned the mode only when it was relaxed would be silent in
/// exactly the case a reader most wants confirmed.
pub const DEFAULT_APPROVAL_MODE: &str = "default";

/// Is this gemini-native tool name one [`AUTO_EDIT_APPROVAL_MODE`] is required for?
///
/// The two names 0.53.0 was measured to add under that mode (§11 item 24). `run_shell_command` is
/// deliberately **not** here: it is withheld under the default mode too, but item 24 did not
/// measure `auto_edit` restoring it — *"edit tools"* is what the flag documents — so claiming it
/// would be a guess about a grant, which is the direction this codebase never guesses in.
///
/// Lives here rather than in the adapter because it is knowledge about gemini, and the adapter's
/// job is only to hand marion's declaration to the harness that owns the answer.
pub fn is_edit_tool(native: &str) -> bool {
    EDIT_TOOLS.contains(&native)
}

/// The gemini-native tools that run headless only when `--allowed-tools` names them.
///
/// `run_shell_command`, which no approval mode short of yolo offers: measured on 0.53.0 against a
/// capture endpoint (2026-09-28), it is absent from `functionDeclarations` under `default` and under
/// `auto_edit`, and present under either once `--allowed-tools run_shell_command` is passed. The
/// flag is marked deprecated in favour of the policy engine (it prints one stderr line saying so);
/// a `--policy` document allowing the tool was measured to do the same, and is the route to take
/// when a gemini release drops the flag.
pub const ALLOWED_BY_NAME: &[&str] = &["run_shell_command"];

/// The gemini-native tools [`is_edit_tool`] answers for.
pub const EDIT_TOOLS: &[&str] = &["write_file", "replace"];

/// The MCP server alias. **It must not contain `_`**: gemini exposes MCP tools as
/// `mcp_<server>_<tool>`, and the shipped policy-engine docs warn that a fully-qualified name with
/// extra underscores is mis-parsed and **fails silently** (S12). `marion` is safe.
pub const MCP_ALIAS: &str = crate::spec::MCP_ALIAS;

/// The settings document `GEMINI_CLI_SYSTEM_SETTINGS_PATH` names.
///
/// **`"trust": true` is load-bearing and its omission is silent.** Measured in S12 against 0.53.0,
/// identical prompts: with `trust: true` the marion tool appears in the request body (1
/// occurrence, 39.7 KB) and is called; **without it the tool is omitted from the request body
/// entirely** (0 occurrences, 39.3 KB) — no prompt, no warning, no error, and the run exits 0
/// having done nothing. See `tests/fixtures/s12/`. `--yolo` is the only other route, and an admin
/// can veto it with `security.disableYoloMode`, so it is not a substitute.
///
/// `security.auth.selectedType` is load-bearing too, though loudly: with a `GEMINI_API_KEY` and no
/// selected type the run fails with `{"error":{…,"code":41,"message":"Invalid auth method
/// selected."}}`, and there is no env-var equivalent (S12, §6.4).
///
/// The canned emitter: [`CANNED_AUTH_TYPE`], because marion supplies the key itself. A live node
/// takes [`live_settings_json`], which selects nothing.
pub fn settings_json(mcp: Option<&BridgeEnv>) -> Value {
    settings_json_with_auth(mcp, Some(CANNED_AUTH_TYPE))
}

/// The live emitter: [`settings_json`] with **no** `security.auth.selectedType` at all.
///
/// gemini resolves its auth as `configuredAuthType || getAuthTypeFromEnv()` — the merged
/// settings' selection, else the environment (`GOOGLE_GENAI_USE_GCA`, `GOOGLE_GENAI_USE_VERTEXAI`,
/// `GOOGLE_GEMINI_BASE_URL`, `GEMINI_API_KEY`, then Cloud Shell / compute ADC). This document is
/// the system layer, which outranks the operator's own, so any value here pins every operator to
/// one route: marion used to guess `oauth-personal` when the user file was silent, which pinned an
/// operator whose only credential is `GEMINI_API_KEY` to a login Google now refuses to individual
/// accounts. Silent, the operator's own `security.auth.selectedType` still wins (the layers merge
/// leaf by leaf, S30), and an operator with none gets gemini's env resolution — measured: system
/// layer silent, empty home, a dummy key → `API key not valid`, i.e. the key route was taken.
pub fn live_settings_json(mcp: Option<&BridgeEnv>) -> Value {
    settings_json_with_auth(mcp, None)
}

/// The settings document with the `security.auth.selectedType` stated, or omitted when `None`.
///
/// **The merge granularity, measured (S30, gemini 0.53.0, 2026-09-10, `tests/fixtures/s30/`).**
/// S12 fixtured the *precedence* (system settings win); what was open was whether this document's
/// `mcpServers` sits **beside** the operator's own servers or **replaces** them for the node's
/// life. It sits beside them: with the user layer declaring `pencil` and this document declaring
/// `marion`, the TUI's `/mcp` lists both as `Ready` and its status bar counts `2 MCP servers`;
/// `gemini mcp list` connects both. The bundle's `SETTINGS_SCHEMA.mcpServers` carries
/// `mergeStrategy: "shallow_merge"` — `{...user.mcpServers, ...system.mcpServers}` — and every key
/// this document writes under `general`, `security` and `privacy` lands on the operator's block
/// leaf by leaf (`general.vimMode: true` in the user layer survived this document's two
/// `general.*` keys: the TUI came up in `[INSERT]`). The one shadow: a same-named entry — an
/// operator-owned server called `marion` — is replaced by this one for the node's life, since the
/// system layer is merged last (`s30/mcp-list.collision.txt`).
fn settings_json_with_auth(mcp: Option<&BridgeEnv>, selected_type: Option<&str>) -> Value {
    let mut settings = json!({
        // Defaults to true and ships to Clearcut every 60 s, calling `systeminformation.graphics()`
        // on the way (S12).
        "privacy": { "usageStatisticsEnabled": false },
    });
    if let Some(selected_type) = selected_type {
        settings["security"] = json!({ "auth": { "selectedType": selected_type } });
    }
    // `general.enableAutoUpdate` / `general.enableAutoUpdateNotification`, both `false`: the row's
    // [`UpdatePolicy::Document`](crate::spec::UpdatePolicy), applied here so the emitter and the
    // row cannot disagree about which keys keep gemini from updating itself.
    SPEC.updates.apply_to_json(&mut settings);

    if let Some(b) = mcp {
        settings["mcpServers"] = json!({
            MCP_ALIAS: {
                "command": b.bridge.to_string_lossy(),
                "args": b.args,
                // Gemini's stdio schema carries `env` as Record<string,string>: the bridge's
                // contract, verbatim.
                "env": b.env_json(),
                // Do not "simplify" this away — read the doc comment above first.
                "trust": true,
            }
        });
    }
    settings
}

/// Gemini CLI 0.53.0, headless `-p` (§6.4, fixture `tests/fixtures/s12/`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiAdapter;

impl GeminiAdapter {
    /// The settings document `GEMINI_CLI_SYSTEM_SETTINGS_PATH` names — [`SETTINGS_FILE`]
    /// under the config dir, which is exactly what [`SPEC`]'s env row renders, so `compile`
    /// and `config_files` cannot disagree about where it is.
    ///
    /// It sits *beside* `$GEMINI_CLI_HOME`, not inside `<home>/.gemini/`: the whole point of the
    /// system-settings override is that marion writes nothing under the sandbox home the CLI owns.
    fn settings_path(spec: &LaunchSpec) -> PathBuf {
        spec.config_dir.join(SETTINGS_FILE)
    }
}

impl HarnessAdapter for GeminiAdapter {
    fn harness(&self) -> Harness {
        Harness::Gemini
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        // Unlike Claude Code, the file is written even with no MCP server: it also carries the
        // auth selection, without which the run dies with `Invalid auth method selected.`
        let bridge = (spec.mcp == McpDeclaration::Marion).then(|| declared_bridge(self, spec, ctx));
        // The one key whose right value is not marion's to choose. Under `Canned` marion supplies
        // `GEMINI_API_KEY` and so selects `gemini-api-key`; under `Inherited` it supplies no
        // credential at all, and this document is the *system settings* layer, which outranks the
        // operator's own — so any selection here would pin every operator to one route. The live
        // document selects nothing and leaves gemini's own `user || env` resolution to run.
        let document = match (spec.auth, bridge.as_ref()) {
            (Auth::Canned | Auth::Endpoint, bridge) => {
                serde_json::to_string_pretty(&settings_json(bridge))
                    .expect("a Value always serialises")
            }
            // The row's live declaration, byte for byte: a native root gets exactly this file.
            (Auth::Inherited, Some(bridge)) => live_settings_document(bridge),
            (Auth::Inherited, None) => serde_json::to_string_pretty(&live_settings_json(None))
                .expect("a Value always serialises"),
        };
        Ok(vec![(Self::settings_path(spec), document)])
    }
}

/// This row's entry in [`crate::adapter::ROWS`].
pub const ROW: Row = Row {
    spec: &SPEC,
    adapter: |_| Ok(Box::new(GeminiAdapter)),
};

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::AgentId;

    use crate::auth::Auth;
    use crate::invocation::Invocation;
    use crate::spec::{Fields, Shape, render};

    /// A canned node as the adapter's fields hook shapes it — model required and present, the base
    /// URL already in [`google_base_url`]'s form. The hook's refusals are pinned in
    /// `adapter::tests`; these tests pin **the row**.
    fn spec() -> Fields {
        Fields {
            cwd: "/tmp/wt".into(),
            config_dir: "/tmp/cfg".into(),
            auth: Auth::Canned,
            prompt: "do the task".into(),
            model: Some("gemini-2.5-flash".into()),
            base_url: Some("http://127.0.0.1:8099".into()),
            api_key: Some("sk-fake".into()),
            ..Fields::default()
        }
    }

    /// The same node under `--live`: marion names no endpoint and mints no credential.
    fn live_spec() -> Fields {
        Fields {
            base_url: None,
            api_key: None,
            auth: Auth::Inherited,
            ..spec()
        }
    }

    fn compile_prompt(f: &Fields) -> Invocation {
        render(&SPEC, Shape::Headless, f).unwrap()
    }

    fn bridge() -> BridgeEnv {
        BridgeEnv {
            node_token_file: None,
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-child".into()),
            agent_type: "gemini".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
        }
    }

    #[test]
    fn the_model_is_always_explicit_and_the_prompt_is_the_argument_to_p() {
        let inv = compile_prompt(&spec());
        let m = inv.args.iter().position(|a| a == "-m").expect(
            "model auto makes 0.53.0 issue a classifier call that hung against a canned endpoint",
        );
        assert_eq!(inv.args[m + 1], "gemini-2.5-flash");
        let p = inv.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(inv.args[p + 1], "do the task");
        assert_eq!(inv.args.last().unwrap(), "do the task");
    }

    #[test]
    fn the_output_format_is_the_ndjson_one() {
        let inv = compile_prompt(&spec());
        let i = inv
            .args
            .iter()
            .position(|a| a == "--output-format")
            .unwrap();
        assert_eq!(inv.args[i + 1], "stream-json");
        assert_eq!(inv.program, "gemini");
    }

    #[test]
    fn the_whole_config_and_auth_surface_is_relocated_and_the_trust_check_is_answered() {
        let inv = compile_prompt(&spec());
        let get = |k: &str| {
            inv.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{k} must be set"))
        };
        assert_eq!(get(CLI_HOME_ENV), "/tmp/cfg");
        assert_eq!(
            get(SYSTEM_SETTINGS_PATH_ENV),
            "/tmp/cfg/marion-settings.json"
        );
        assert_eq!(get(TRUST_WORKSPACE_ENV), "true");
        assert_eq!(get(FORCE_FILE_STORAGE_ENV), "true");
    }

    #[test]
    fn the_base_url_loses_the_v1_the_sdk_appends_for_itself() {
        let inv = compile_prompt(&spec());
        let (_, v) = inv.env.iter().find(|(k, _)| k == BASE_URL_ENV).unwrap();
        assert_eq!(v, "http://127.0.0.1:8099");
        assert_eq!(
            crate::spec::base_url_root("https://x.example/v1/"),
            "https://x.example"
        );
        assert_eq!(
            crate::spec::base_url_root("https://x.example"),
            "https://x.example"
        );
    }

    #[test]
    fn a_missing_base_url_or_key_simply_omits_its_variable() {
        let inv = compile_prompt(&Fields {
            base_url: None,
            api_key: None,
            ..spec()
        });
        assert!(
            !inv.env
                .iter()
                .any(|(k, _)| k == BASE_URL_ENV || k == API_KEY_ENV)
        );
    }

    #[test]
    fn plain_http_is_accepted_only_on_loopback() {
        for ok in [
            "http://127.0.0.1:8099/v1",
            "http://localhost:1/v1",
            "http://[::1]:9/v1",
            "https://generativelanguage.googleapis.com",
        ] {
            assert!(crate::spec::https_or_loopback(ok), "{ok}");
        }
        for bad in ["http://example.com/v1", "http://10.0.0.1:8099", "ftp://x"] {
            assert!(!crate::spec::https_or_loopback(bad), "{bad}");
        }
    }

    /// The §12-family test: `trust: true` has no error mode, so only a test defends it. Its
    /// omission drops marion's tools from the request body with exit 0 (`tests/fixtures/s12/`).
    #[test]
    fn the_mcp_server_is_trusted_or_its_tools_are_silently_invisible() {
        let v = settings_json(Some(&bridge()));
        assert_eq!(
            v["mcpServers"]["marion"]["trust"],
            json!(true),
            "without it: 0 occurrences of the tool in the request body, no prompt, no error, exit 0"
        );
    }

    #[test]
    fn the_auth_method_is_selected_or_the_run_dies_with_code_41() {
        let v = settings_json(Some(&bridge()));
        assert_eq!(
            v["security"]["auth"]["selectedType"],
            json!("gemini-api-key"),
            "an API key alone fails: Invalid auth method selected."
        );
    }

    #[test]
    fn the_alias_carries_no_underscore_because_the_policy_engine_mis_parses_one() {
        assert!(!MCP_ALIAS.contains('_'));
        let v = settings_json(Some(&bridge()));
        assert!(v["mcpServers"].as_object().unwrap().contains_key("marion"));
    }

    #[test]
    fn the_bridge_declaration_carries_the_nodes_identity() {
        let mut b = bridge();
        b.ready_file = Some("/state/x/mcp-ready".into());
        let v = settings_json(Some(&b));
        let env = &v["mcpServers"]["marion"]["env"];
        assert_eq!(env["MARION_AGENT_ID"], json!("019f-child"));
        assert_eq!(env["MARION_READY_FILE"], json!("/state/x/mcp-ready"));
        assert_eq!(env["MARION_REPO"], json!("/repo"));
        assert_eq!(v["mcpServers"]["marion"]["args"], json!(["mcp"]));
    }

    #[test]
    fn a_launch_only_node_has_no_readiness_marker_and_that_is_not_an_error() {
        let v = settings_json(Some(&bridge()));
        assert!(v["mcpServers"]["marion"]["env"]["MARION_READY_FILE"].is_null());
    }

    /// The §9 fallback branch: no MCP at all still needs the auth and telemetry keys, so the file
    /// is not empty — it simply declares no server.
    #[test]
    fn settings_without_an_mcp_server_still_carry_the_auth_selection() {
        let v = settings_json(None);
        assert!(v["mcpServers"].is_null());
        assert_eq!(
            v["security"]["auth"]["selectedType"],
            json!("gemini-api-key")
        );
        assert_eq!(v["privacy"]["usageStatisticsEnabled"], json!(false));
        assert_eq!(v["general"]["enableAutoUpdate"], json!(false));
    }

    /// **Live mode drops three variables, and the MCP injection route is not one of them.**
    ///
    /// Asserted by *name*, because the failure being defended against is one of the three creeping
    /// back — and by presence for the two that must survive, because the tempting way to "make live
    /// mode simple" is to drop the whole env block, which would take the settings path with it and
    /// leave a live node with no bridge at all (§6.1 step 8's failure class).
    #[test]
    fn a_live_gemini_node_drops_the_sandbox_home_the_endpoint_and_the_key() {
        let inv = compile_prompt(&live_spec());
        for k in [CLI_HOME_ENV, BASE_URL_ENV, API_KEY_ENV] {
            assert!(
                !inv.env.iter().any(|(n, _)| n == k),
                "{k} must be absent under --live: relocating the config surface hides the very \
                 login the node is meant to use. env: {:?}",
                inv.env
            );
        }
        let get = |k: &str| {
            inv.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{k} must survive live mode"))
        };
        assert_eq!(
            get(SYSTEM_SETTINGS_PATH_ENV),
            "/tmp/cfg/marion-settings.json"
        );
        assert_eq!(get(TRUST_WORKSPACE_ENV), "true");
        assert_eq!(
            inv.args,
            compile_prompt(&spec()).args,
            "live differs from canned in env only"
        );
    }

    /// **A safety property, not a style choice — hence the name.**
    ///
    /// `GEMINI_FORCE_FILE_STORAGE` over the operator's **real** `~/.gemini` risks pushing their
    /// actual `oauth_creds.json` through `OAuthCredentialStorage.migrateFromFileStorage()`, which
    /// `tests/fixtures/s12/` records as reading the legacy file, writing the hybrid store, and then
    /// `fs.rm`-ing the original. That is marion performing a **one-way destructive migration** of a
    /// file it does not own, which §6.4 forbids outright. Under `Canned` the same variable points at
    /// a throwaway `$GEMINI_CLI_HOME` and destroys nothing, which is why it is a mode gate rather
    /// than a deletion.
    #[test]
    fn forcing_file_storage_over_a_real_gemini_profile_would_delete_the_operators_credential() {
        assert!(
            !compile_prompt(&live_spec())
                .env
                .iter()
                .any(|(k, _)| k == FORCE_FILE_STORAGE_ENV),
            "{FORCE_FILE_STORAGE_ENV} under --live can trigger a migration that fs.rm's the \
             operator's own oauth_creds.json"
        );
        assert!(
            compile_prompt(&spec())
                .env
                .iter()
                .any(|(k, _)| k == FORCE_FILE_STORAGE_ENV),
            "over marion's own sandbox home it is still wanted: otherwise which store a child uses \
             depends on a 2 s keychain-probe race"
        );
    }

    /// A live spec that arrived carrying an endpoint or a key gets neither overlaid. The mode is the
    /// authority, not the presence of a value.
    #[test]
    fn a_live_node_overlays_no_endpoint_even_if_one_was_handed_to_it() {
        let inv = compile_prompt(&Fields {
            auth: Auth::Inherited,
            ..spec()
        });
        assert!(
            !inv.env
                .iter()
                .any(|(k, _)| k == BASE_URL_ENV || k == API_KEY_ENV)
        );
    }

    /// The live document leaves `security.auth.selectedType` to gemini's own `user || env`
    /// resolution: the system layer outranks the operator's, so any value here would pin every
    /// operator to one route — measured: with this key absent and only `GEMINI_API_KEY` set, the
    /// env route is taken.
    #[test]
    fn a_live_settings_document_selects_no_auth_type_but_still_declares_a_trusted_bridge() {
        let v: Value = serde_json::from_str(&live_settings_document(&bridge())).unwrap();
        assert!(
            v.pointer("/security/auth/selectedType").is_none(),
            "a live document must not pin an auth route: {v}"
        );
        assert_eq!(v["mcpServers"]["marion"]["trust"], json!(true));
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_AGENT_ID"],
            json!("019f-child")
        );
        assert_eq!(v["privacy"]["usageStatisticsEnabled"], json!(false));
        assert_eq!(v["general"]["enableAutoUpdate"], json!(false));
        // The canned document still states the type marion supplies a key for.
        assert_eq!(
            settings_json(Some(&bridge()))["security"]["auth"]["selectedType"],
            json!(CANNED_AUTH_TYPE)
        );
    }

    #[test]
    fn nothing_is_shell_quoted_because_nothing_reaches_a_shell() {
        let inv = compile_prompt(&spec());
        assert!(
            inv.args
                .iter()
                .all(|a| !a.contains('\'') && !a.contains('"'))
        );
    }
}
