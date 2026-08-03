//! The Gemini adapter — the headless `-p` surface (design §6.4, fixture `tests/fixtures/s12/`).
//!
//! Measured against **gemini CLI 0.53.0**. The surface is the same shape as `codex exec`: marion
//! writes the configuration, starts the process, and reads its NDJSON. There is no channel to
//! steer a turn, so control is `LaunchOnly` and the prompt rides argv.
//!
//! Three of the four env vars and two of the settings keys below are load-bearing in the §12 sense
//! — *omitting them produces no error anywhere*. Each one carries the measurement that says so.

use std::path::PathBuf;

use marion_core::contract::AgentId;
use serde_json::{Value, json};

// These name the **bridge's** env contract rather than Claude Code's — the bridge reads
// `MARION_AGENT_ID` whichever harness started it — so they are imported rather than respelled: a
// second spelling would put a gemini child's `TaskContract.requester` back on "unattributed-root".
use crate::claude_code::{AGENT_ID_ENV, READY_FILE_ENV};
use crate::invocation::Invocation;
use crate::stream::{StreamOutcome, first_string, json_frames};

/// Relocates the **entire** config and auth surface: `settings.json`, `oauth_creds.json`,
/// `trustedFolders.json`, extensions, sessions. The CLI appends `.gemini` itself, so this names
/// the parent of the sandbox home, not the home (S12: `GEMINI_CLI_HOME=$D` → `$D/.gemini/`).
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
pub const FORCE_FILE_STORAGE_ENV: &str = "GEMINI_FORCE_FILE_STORAGE";
/// Base-URL override for the `gemini-api-key` auth path.
pub const BASE_URL_ENV: &str = "GOOGLE_GEMINI_BASE_URL";
/// The AI Studio key, which is the auth type [`settings_json`] selects.
pub const API_KEY_ENV: &str = "GEMINI_API_KEY";

/// The MCP server alias. **It must not contain `_`**: gemini exposes MCP tools as
/// `mcp_<server>_<tool>`, and the shipped policy-engine docs warn that a fully-qualified name with
/// extra underscores is mis-parsed and **fails silently** (S12). `marion` is safe.
pub const MCP_ALIAS: &str = "marion";

/// What marion needs to compile a headless gemini invocation.
#[derive(Debug, Clone)]
pub struct PromptSpec {
    pub cwd: PathBuf,
    /// **Required, and always explicit.** With the default model `auto`, 0.53.0 first issues a
    /// classifier call to `gemini-3.1-flash-lite` over non-streaming `:generateContent`; S12
    /// measured a naive canned reply making it retry 5× and then hang. §6.4 states the MUST.
    pub model: String,
    pub prompt: String,
    /// `$GEMINI_CLI_HOME`.
    pub cli_home: PathBuf,
    /// The file [`settings_json`] is written to, named by [`SYSTEM_SETTINGS_PATH_ENV`].
    pub settings: PathBuf,
    /// Provider base URL, in marion's canonical `…/v1` form. See [`google_base_url`].
    pub base_url: Option<String>,
    pub api_key: Option<String>,
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

pub fn compile_prompt(spec: &PromptSpec) -> Invocation {
    let args: Vec<String> = vec![
        // Explicit model: see `PromptSpec::model`. Never omitted, never `auto`.
        "-m".into(),
        spec.model.clone(),
        "--output-format".into(),
        "stream-json".into(),
        // The prompt is the **argument to `-p`** — not positional (a bare positional query
        // launches the interactive UI) and not stdin (which is *prepended as context* instead).
        "-p".into(),
        spec.prompt.clone(),
    ];

    let mut env: Vec<(String, String)> = vec![
        (
            CLI_HOME_ENV.into(),
            spec.cli_home.to_string_lossy().into_owned(),
        ),
        (
            SYSTEM_SETTINGS_PATH_ENV.into(),
            spec.settings.to_string_lossy().into_owned(),
        ),
        (TRUST_WORKSPACE_ENV.into(), "true".into()),
        (FORCE_FILE_STORAGE_ENV.into(), "true".into()),
    ];
    if let Some(u) = &spec.base_url {
        env.push((BASE_URL_ENV.into(), google_base_url(u)));
    }
    if let Some(k) = &spec.api_key {
        env.push((API_KEY_ENV.into(), k.clone()));
    }

    Invocation {
        program: "gemini".into(),
        args,
        env,
        cwd: spec.cwd.clone(),
        // What `-m` above carries. S12 measured the CLI remapping it in the request path
        // (`gemini-2.5-flash` → `gemini-3.5-flash`), so this records what marion put on the wire,
        // which is the last point at which marion knows anything for certain.
        model: Some(spec.model.clone()),
    }
}

/// Parse a `gemini --output-format stream-json` stream.
///
/// The event set is closed and measured (`tests/fixtures/s12/`): `init | message | tool_use |
/// tool_result | error | result`. Three of the six carry evidence marion wants.
///
/// **The exit code is not the verdict, and that is a measurement, not caution.** S12: *"an auth
/// failure returned exit 0 with a JSON error body — so a launcher must parse the JSON and must not
/// trust the exit code alone."* So a failure claim anywhere in the stream is recorded regardless of
/// what the process exited with, and two shapes are accepted for it: the `stream-json` `error`
/// frame, and the bare `{"error":{"type":…,"message":…,"code":…}}` object that the failure S12
/// actually recorded took. The `result` frame's `status` is read the same way — S12 captured only
/// `"success"`, so anything else is treated as the harness saying so rather than as an unknown to
/// be ignored.
///
/// Two fields are left empty **because gemini has nothing to fill them with**, not because they
/// were forgotten: there is no `file_change` analogue anywhere in the event set (git remains the
/// authority for `changed_paths`), and `tool_result` carries only an opaque `output` string, so a
/// report's narrative is read off the `tool_use` frame that made the call rather than off its
/// result.
///
/// `report_tool` is the **model-facing** spelling, which is the adapter's to know: gemini's is
/// `mcp_<server>_<tool>` and is nobody else's (§3.1). Passing it in rather than rebuilding it here
/// keeps [`crate::HarnessAdapter::marion_tool_name`] the single derivation.
pub fn parse_stream(s: &str, report_tool: &str) -> StreamOutcome {
    let mut out = StreamOutcome::default();
    for v in json_frames(s) {
        match v["type"].as_str() {
            Some("tool_use") if v["tool_name"].as_str() == Some(report_tool) => {
                if let Some(n) = v["parameters"]["narrative"].as_str() {
                    out.narrative = Some(n.to_string());
                }
            }
            Some("error") => {
                out.failure = out.failure.take().or_else(|| {
                    first_string(&v, &["/error/message", "/message", "/error/type"])
                        .or_else(|| Some("the child's stream carried an error frame".into()))
                });
            }
            Some("result") => {
                if let Some(status) = v["status"].as_str()
                    && status != "success"
                {
                    out.failure = Some(format!("gemini result status: {status}"));
                }
            }
            // The exit-0 auth failure S12 recorded: an `error` object with no `type` frame around
            // it. Untyped, so it is matched here rather than in the arms above.
            _ if v.get("error").is_some() => {
                out.failure = out.failure.take().or_else(|| {
                    first_string(&v, &["/error/message", "/error/type"])
                        .or_else(|| Some("the child's stream carried an error body".into()))
                });
            }
            _ => {}
        }
    }
    out
}

/// The values [`settings_json`] writes into the MCP server declaration.
///
/// The key names in the `env` block are the **bridge's** contract, not Claude Code's, which is why
/// they are taken from [`crate::claude_code`]'s constants rather than respelled here: the bridge
/// reads `MARION_AGENT_ID` no matter which harness started it, and a second spelling would put a
/// gemini child's contract back on `"unattributed-root"`.
#[derive(Debug, Clone)]
pub struct BridgeEnv {
    pub bridge: PathBuf,
    pub args: Vec<String>,
    pub repo: PathBuf,
    pub state: PathBuf,
    pub base_url: String,
    pub agent_id: AgentId,
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
pub fn settings_json(mcp: Option<&BridgeEnv>) -> Value {
    let mut settings = json!({
        "security": { "auth": { "selectedType": "gemini-api-key" } },
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
            "MARION_BASE_URL": b.base_url,
            AGENT_ID_ENV: b.agent_id.0,
        });
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

    fn spec() -> PromptSpec {
        PromptSpec {
            cwd: "/tmp/wt".into(),
            model: "gemini-2.5-flash".into(),
            prompt: "do the task".into(),
            cli_home: "/tmp/cfg".into(),
            settings: "/tmp/cfg/marion-settings.json".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("sk-fake".into()),
        }
    }

    fn bridge() -> BridgeEnv {
        BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: "http://127.0.0.1:8099/v1".into(),
            agent_id: AgentId("019f-child".into()),
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
