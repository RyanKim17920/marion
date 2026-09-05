//! The Gemini adapter — the headless `-p` surface (design §6.4, fixture `tests/fixtures/s12/`).
//!
//! Measured against **gemini CLI 0.53.0**. The surface is the same shape as `codex exec`: marion
//! writes the configuration, starts the process, and reads its NDJSON. There is no channel to
//! steer a turn, so control is `LaunchOnly` and the prompt rides argv.
//!
//! Three of the four env vars and two of the settings keys below are load-bearing in the §12 sense
//! — *omitting them produces no error anywhere*. Each one carries the measurement that says so.

use std::path::{Path, PathBuf};

use marion_core::contract::AgentId;
use serde_json::{Value, json};

// These name the **bridge's** env contract rather than Claude Code's — the bridge reads
// `MARION_AGENT_ID` whichever harness started it — so they are imported rather than respelled: a
// second spelling would put a gemini child's `TaskContract.requester` back on "unattributed-root".
use marion_core::harness::Harness;

use crate::auth::Auth;
use crate::grammar::{
    Cond, Failure, Name, OnRefusedReport, Pairing, StreamGrammar, Verdict, Where,
};
use crate::mcp_bridge::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, AUTH_ENV, BASE_URL_ENV as MARION_BASE_URL_ENV, DEPTH_ENV,
    NODE_TOKEN_ENV, READY_FILE_ENV,
};
use crate::spec::{Arg, Env, Field, HarnessSpec, Val, When};

/// The settings document's name under the node's config dir — named by [`SYSTEM_SETTINGS_PATH_ENV`]
/// in [`SPEC`]'s env and written by `GeminiAdapter::config_files`, one spelling for both.
pub const SETTINGS_FILE: &str = "marion-settings.json";

/// Gemini CLI's row. Measured against 0.53.0 (`tests/fixtures/s12/`; §11 item 24 for the approval
/// mode). `LaunchOnly`: the prompt is the argument to `-p` — not positional (a bare positional
/// launches the interactive UI) and not stdin (which is *prepended as context* instead).
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Gemini,
    program: Some("gemini"),
    argv: &[
        // Never omitted and never `auto`: the adapter refuses a launch without one.
        Arg::Flag("-m", Field::Model),
        Arg::Lit("--output-format"),
        Arg::Lit("stream-json"),
        // Ahead of `-p`, and only when the declaration names an edit tool, so a node that declares
        // nothing compiles the argv it always compiled, in the order it always compiled it.
        Arg::Flag("--approval-mode", Field::Mode),
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
            when: When::Canned,
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
            when: When::Canned,
        },
        // Gated on the mode as well as on the value: a live spec handed an endpoint or a key gets
        // neither pushed, rather than an overlay that quietly outranks the operator's own resolution.
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
    ],
    stream: Some(&STREAM),
    note: "S12 on gemini CLI 0.53.0: the -p surface, the four load-bearing env vars and the \
           system-settings injection route; §11 item 24 for --approval-mode auto_edit. \
           harness_matrix's gemini cell runs this row end to end",
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
    file_changes: None,
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

/// What [`live_auth_type`] selects when the operator's real `settings.json` cannot be read or does
/// not state one.
///
/// **`oauth-personal` rather than [`CANNED_AUTH_TYPE`], and the asymmetry is the argument.** Under
/// [`Auth::Inherited`] marion sets no [`API_KEY_ENV`] at all, so selecting `gemini-api-key` names an
/// auth path with no key behind it and fails with S12's measured
/// `{"code":41,"message":"Invalid auth method selected."}` — a *guaranteed* failure. Guessing
/// `oauth-personal` instead is wrong only for an operator who authenticates by API key, and that
/// operator's `settings.json` says so and is read below. It is also the subscription login `gemini`
/// itself writes, and the value on this machine's real profile.
pub const LIVE_FALLBACK_AUTH_TYPE: &str = "oauth-personal";

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
    matches!(native, "write_file" | "replace")
}

/// The MCP server alias. **It must not contain `_`**: gemini exposes MCP tools as
/// `mcp_<server>_<tool>`, and the shipped policy-engine docs warn that a fully-qualified name with
/// extra underscores is mis-parsed and **fails silently** (S12). `marion` is safe.
pub const MCP_ALIAS: &str = "marion";

/// The operator's own gemini settings document — the **user** layer, which marion never writes.
///
/// `$HOME` rather than a home-directory crate: marion adds no dependency for this, and the CLI's own
/// `homedir()` is `process.env.GEMINI_CLI_HOME ?? os.homedir()` — under [`Auth::Inherited`] marion
/// sets no [`CLI_HOME_ENV`], so the child resolves the same path this does.
pub fn operator_settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(Path::new(&home).join(".gemini").join("settings.json"))
}

/// `security.auth.selectedType` out of a settings document, or `None` if it is not a string there.
///
/// Pure, and separated from the read so the parse is testable without touching a real profile.
/// **Malformed input is `None`, never a panic**: this reads a file the operator owns and marion has
/// no say over, so every failure — absent, unparseable, right shape with the wrong type — has to
/// land on the same defensible fallback rather than taking the run down.
pub fn selected_auth_type_from(settings: &str) -> Option<String> {
    let v: Value = serde_json::from_str(settings).ok()?;
    v.pointer("/security/auth/selectedType")?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// The `security.auth.selectedType` a **live** node must declare.
///
/// It has to match the operator's real setting or 0.53.0 refuses the run outright with
/// `{"code":41,"message":"Invalid auth method selected."}` (S12) — the settings marion writes are
/// the *system* layer and win over the user layer, so a hardcoded `gemini-api-key` would override
/// an `oauth-personal` profile with a type it has no credential for.
///
/// Reads the operator's own file to find out, and falls back to [`LIVE_FALLBACK_AUTH_TYPE`] when it
/// is absent, unreadable or malformed. The I/O sits here rather than in `marion-core`, which stays
/// I/O free.
pub fn live_auth_type() -> String {
    operator_settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .as_deref()
        .and_then(selected_auth_type_from)
        .unwrap_or_else(|| LIVE_FALLBACK_AUTH_TYPE.to_string())
}

/// `GOOGLE_GEMINI_BASE_URL` from the base URL marion carries.
///
/// The same derivation Claude Code needs, for a different reason: marion stores the Codex
/// `model_providers` spelling (`…/v1`, the one that appears verbatim in a config file), while the
/// google-genai SDK appends its own `/v1beta/models/<model>:streamGenerateContent` — so the `/v1`
/// would be doubled. Deliberately *not* borrowed from [`crate::claude_code`]: two adapters that
/// happen to need the same string edit are not a dependency between peers.
pub fn google_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// S12/§6.4: base-URL overrides **must be HTTPS unless the host is loopback**. marion's proxy is
/// on `127.0.0.1`, so it needs no TLS — but a non-loopback plain-HTTP endpoint is refused by the
/// CLI, and refusing it at compile time turns a confusing runtime failure into a launch error.
pub fn base_url_is_acceptable(base_url: &str) -> bool {
    let u = base_url.trim();
    if u.starts_with("https://") {
        return true;
    }
    let Some(rest) = u.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        // An IPv6 literal keeps its brackets: `[::1]:8099`.
        Some(r) => r
            .split_once(']')
            .map_or(String::new(), |(h, _)| format!("[{h}]")),
        None => authority.split(':').next().unwrap_or("").to_string(),
    };
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

/// The values [`settings_json`] writes into the MCP server declaration.
///
/// The key names in the `env` block are the **bridge's** contract, not Claude Code's, which is why
/// they are taken from [`crate::mcp_bridge`]'s constants rather than respelled here: the bridge
/// reads `MARION_AGENT_ID` no matter which harness started it, and a second spelling would put a
/// gemini child's contract back on `"unattributed-root"`.
#[derive(Debug, Clone)]
pub struct BridgeEnv {
    pub bridge: PathBuf,
    pub args: Vec<String>,
    pub repo: PathBuf,
    pub state: PathBuf,
    /// `None` under [`Auth::Inherited`]: the key is omitted, never written empty
    /// ([`crate::mcp_bridge::BASE_URL_ENV`]).
    pub base_url: Option<String>,
    /// Which endpoint a child this node spawns should talk to
    /// ([`crate::mcp_bridge::AUTH_ENV`]).
    pub auth: Auth,
    pub agent_id: AgentId,
    /// The node's canonical agent type name (§6.1 step 2 reads the caller's type).
    pub agent_type: String,
    /// The node's depth, root = 0 (§3.1's `max_depth`).
    pub depth: u32,
    /// §5.4's capability token for this node — `None` where the supervisor minted none. Present or
    /// absent, never empty ([`crate::mcp_bridge::NODE_TOKEN_ENV`]).
    pub node_token: Option<String>,
    /// `None` on a `LaunchOnly` surface: the prompt rides argv, so there is no first frame to
    /// withhold and nothing to wait on (§6.1 step 8).
    pub ready_file: Option<PathBuf>,
}

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
/// takes [`settings_json_with_auth`] with the operator's own selection instead.
pub fn settings_json(mcp: Option<&BridgeEnv>) -> Value {
    settings_json_with_auth(mcp, CANNED_AUTH_TYPE)
}

/// [`settings_json`] with the `security.auth.selectedType` stated rather than assumed.
///
/// **UNKNOWN, and deliberately not guessed at in code: whether gemini's system-settings layer
/// deep-merges with the user layer or replaces it per key.** S12 fixtured the *precedence* (system
/// settings win) but not the *granularity*. If the merge is per-key replacement, then for the
/// duration of one live run this document's `general` block silently replaces the operator's own
/// `general` — and, worse, a marion node's `mcpServers` replaces theirs, so a live gemini child
/// would see marion's bridge and none of the servers the operator configured. Nothing here depends
/// on which it is: marion writes the keys it needs either way and states the ambiguity rather than
/// encoding a belief about it. Resolving it is a measurement (declare a distinctive user-layer key,
/// run with a system-settings file that omits it, and read it back), not a reading of this file.
pub fn settings_json_with_auth(mcp: Option<&BridgeEnv>, selected_type: &str) -> Value {
    let mut settings = json!({
        "security": { "auth": { "selectedType": selected_type } },
        // Defaults to true and ships to Clearcut every 60 s, calling `systeminformation.graphics()`
        // on the way (S12).
        "privacy": { "usageStatisticsEnabled": false },
        // Both default true. `checkForUpdates()` was only found in the interactive chunks, so it
        // likely never runs on the `-p` path — disabled anyway, because "likely" is not a measurement.
        "general": { "enableAutoUpdate": false, "enableAutoUpdateNotification": false },
    });

    if let Some(b) = mcp {
        let mut env = json!({
            "MARION_REPO": b.repo.to_string_lossy(),
            "MARION_STATE_DIR": b.state.to_string_lossy(),
            AUTH_ENV: b.auth.as_wire(),
            AGENT_ID_ENV: b.agent_id.0,
            AGENT_TYPE_ENV: b.agent_type,
            DEPTH_ENV: b.depth.to_string(),
        });
        if let Some(u) = &b.base_url {
            env[MARION_BASE_URL_ENV] = json!(u);
        }
        // Same rule; see [`crate::mcp_bridge::NODE_TOKEN_ENV`].
        if let Some(t) = &b.node_token {
            env[NODE_TOKEN_ENV] = json!(t);
        }
        if let Some(r) = &b.ready_file {
            env[READY_FILE_ENV] = json!(r.to_string_lossy());
        }
        settings["mcpServers"] = json!({
            MCP_ALIAS: {
                "command": b.bridge.to_string_lossy(),
                "args": b.args,
                // Gemini's stdio schema carries `env` as Record<string,string>, so unlike codex's
                // `config_toml` there is no gap here: the node's identity reaches its bridge.
                "env": env,
                // Do not "simplify" this away — read the doc comment above first.
                "trust": true,
            }
        });
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{
        Extras, GeminiAdapter, HarnessAdapter, LaunchSpec, McpDeclaration, SpawnCtx,
    };
    use crate::invocation::Invocation;

    fn ctx() -> SpawnCtx {
        SpawnCtx {
            agent_id: AgentId("019f-child".into()),
            agent_type: "gemini".into(),
            depth: 1,
            node_token: None,
            ready_file: None,
            repo: "/repo".into(),
            state_dir: "/state".into(),
            bridge: "/bin/marion-supervisor".into(),
            bridge_args: vec!["mcp".into()],
        }
    }

    fn spec() -> LaunchSpec {
        LaunchSpec {
            cwd: "/tmp/wt".into(),
            model: Some("gemini-2.5-flash".into()),
            prompt: "do the task".into(),
            tools: vec![],
            allowed_tools: vec![],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("sk-fake".into()),
            auth: Auth::Canned,
            config_dir: "/tmp/cfg".into(),
            extra: Extras::default(),
        }
    }

    /// The same node under `--live`: marion names no endpoint and mints no credential.
    fn live_spec() -> LaunchSpec {
        LaunchSpec {
            base_url: None,
            api_key: None,
            auth: Auth::Inherited,
            ..spec()
        }
    }

    fn compile_prompt(spec: &LaunchSpec) -> Invocation {
        GeminiAdapter.compile(spec, &ctx()).unwrap()
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
            google_base_url("https://x.example/v1/"),
            "https://x.example"
        );
        assert_eq!(google_base_url("https://x.example"), "https://x.example");
    }

    #[test]
    fn a_missing_base_url_or_key_simply_omits_its_variable() {
        let mut s = spec();
        s.base_url = None;
        s.api_key = None;
        let inv = compile_prompt(&s);
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
            assert!(base_url_is_acceptable(ok), "{ok}");
        }
        for bad in ["http://example.com/v1", "http://10.0.0.1:8099", "ftp://x"] {
            assert!(!base_url_is_acceptable(bad), "{bad}");
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
        let inv = compile_prompt(&LaunchSpec {
            auth: Auth::Inherited,
            ..spec()
        });
        assert!(
            !inv.env
                .iter()
                .any(|(k, _)| k == BASE_URL_ENV || k == API_KEY_ENV)
        );
    }

    /// The operator's real selection is read, not assumed. A live node whose settings claim
    /// `gemini-api-key` against an `oauth-personal` profile dies with S12's code 41.
    #[test]
    fn the_operators_own_auth_selection_is_read_out_of_their_settings() {
        assert_eq!(
            selected_auth_type_from(r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#)
                .as_deref(),
            Some("oauth-personal")
        );
        // Every way the operator's file can fail marion lands on the same defensible fallback
        // rather than on a panic: this reads a document marion has no say over.
        for bad in [
            "",
            "not json at all",
            "{}",
            r#"{"security":{}}"#,
            r#"{"security":{"auth":{"selectedType":42}}}"#,
            r#"{"security":{"auth":{"selectedType":"  "}}}"#,
            "[1,2,3]",
        ] {
            assert_eq!(selected_auth_type_from(bad), None, "{bad}");
        }
        assert_eq!(
            LIVE_FALLBACK_AUTH_TYPE, "oauth-personal",
            "gemini-api-key with no GEMINI_API_KEY is a guaranteed code 41; the subscription \
             login is the only fallback that can succeed"
        );
        // And the reader never panics whatever this machine's profile looks like.
        assert!(!live_auth_type().is_empty());
    }

    #[test]
    fn a_live_settings_document_still_declares_a_trusted_bridge() {
        let v = settings_json_with_auth(Some(&bridge()), "oauth-personal");
        assert_eq!(
            v["security"]["auth"]["selectedType"],
            json!("oauth-personal")
        );
        assert_eq!(v["mcpServers"]["marion"]["trust"], json!(true));
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_AGENT_ID"],
            json!("019f-child")
        );
        // The canned emitter is the same function with the canned selection, so the two cannot
        // drift apart in anything but that one key.
        assert_eq!(
            settings_json(Some(&bridge())),
            settings_json_with_auth(Some(&bridge()), CANNED_AUTH_TYPE)
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
