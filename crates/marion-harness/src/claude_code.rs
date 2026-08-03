//! The Claude Code adapter (design §5.2).
//!
//! Control is **config-time**: marion owns the launch configuration and never parses a pty. Every
//! flag here was measured against 2.1.220, and several are non-obvious enough that the tests below
//! state *why* rather than merely pinning the string.

use std::path::PathBuf;

use marion_core::contract::AgentId;
use serde_json::{Value, json};

use crate::invocation::Invocation;
use crate::stream::{StreamOutcome, first_string, json_frames};

/// Env var naming the file the bridge touches once it has answered `tools/list`.
pub const READY_FILE_ENV: &str = "MARION_READY_FILE";
/// Env var carrying a node's `AgentId` to the bridge, so a top-level `spawn` can stamp
/// `TaskContract.requester` with it (§9).
pub const AGENT_ID_ENV: &str = "MARION_AGENT_ID";

/// What marion needs to compile a headless root invocation.
#[derive(Debug, Clone)]
pub struct HeadlessSpec {
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// Tools the root may call **without a prompt** — the permission axis. marion's own tools go
    /// here, or the root's single load-bearing `spawn` call is denied.
    pub allowed_tools: Vec<String>,
    /// Path to the MCP server declaration marion wrote.
    pub mcp_config: PathBuf,
    /// Base URL for the canned provider.
    pub base_url: Option<String>,
}

pub fn compile_headless(spec: &HeadlessSpec) -> Invocation {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--input-format".into(),
        "stream-json".into(),
        // MANDATORY with `-p --output-format stream-json`: without it 2.1.220 exits 1 with
        // "When using --print, --output-format=stream-json requires --verbose" before emitting
        // anything. Easy to omit, because this document long explained it only as a dependency of
        // --include-partial-messages, which M1 does not use.
        "--verbose".into(),
        // Availability axis: no built-in tools. This does NOT gate MCP tools.
        "--tools".into(),
        "".into(),
        // Permission axis. marion's tools must be listed here or a denied `spawn` is the result.
        "--allowedTools".into(),
        spec.allowed_tools.join(","),
        // Without this, a non-allowlisted call is auto-denied in-process and surfaces only as an
        // is_error tool_result — no `can_use_tool` frame ever reaches marion. Absent from --help.
        "--permission-prompt-tool".into(),
        "stdio".into(),
        // Only the MCP servers marion declared; never the user's.
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        spec.mcp_config.to_string_lossy().into_owned(),
        // No user settings leak into a child. Note this does NOT suppress the session-title
        // request (§5.5).
        "--setting-sources".into(),
        "".into(),
    ];
    if let Some(m) = &spec.model {
        args.push("--model".into());
        args.push(m.clone());
    }

    let mut env = Vec::new();
    if let Some(u) = &spec.base_url {
        env.push(("ANTHROPIC_BASE_URL".to_string(), u.clone()));
    }
    // NOT CLAUDE_CONFIG_DIR: isolating it breaks OAuth, because the Keychain entry is keyed to the
    // real config dir. Config isolation and subscription auth are mutually exclusive here, and the
    // fileless path (--mcp-config + --setting-sources "") is what keeps auth working.

    Invocation {
        program: "claude".into(),
        args,
        env,
        cwd: spec.cwd.clone(),
        // Exactly what `--model` carries above, including its absence: 2.1.220 without `--model`
        // picks its own, and marion has no way to name that from here.
        model: spec.model.clone(),
    }
}

/// What marion needs to compile a **child** invocation.
///
/// The root and a child sit at different points of §3.4's cross-product, and the argv says so.
/// A root is `--input-format stream-json`: its prompt is a frame marion writes *after* launch, so
/// the launch has to withhold the first turn until the bridge is up (§6.1 step 8) and a
/// `--permission-prompt-tool` has a control plane to ask over. A child spawned by `run_spawn` has
/// neither — `LaunchOnly`, stdin closed, nobody servicing a permission ask — so its prompt rides
/// argv exactly as it does on the other three harnesses, and a permission prompt with no answerer
/// would be a hang rather than a question. The two shapes are therefore separate functions rather
/// than one with flags: every flag below differs *because* the surface differs.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// Compiled into argv, not written after launch.
    pub prompt: String,
    /// The permission axis (§3.1). marion's own tools go here or the child's `report` call is
    /// auto-denied in-process, which on this surface surfaces as *nothing at all*: no
    /// `can_use_tool` frame can be answered, so the run simply completes having reported nothing.
    pub allowed_tools: Vec<String>,
    pub mcp_config: PathBuf,
    pub base_url: Option<String>,
    /// Presented as `ANTHROPIC_AUTH_TOKEN`, with `ANTHROPIC_API_KEY` blanked beside it — §9's rule
    /// for the root, applied here so a child talking to marion's endpoint presents *marion's*
    /// placeholder rather than whatever subscription credential the environment happened to carry.
    pub api_key: Option<String>,
}

pub fn compile_child(spec: &ChildSpec) -> Invocation {
    let mut args: Vec<String> = vec![
        // `--print` takes no value: the prompt is positional beside it, as on `gemini -p` it is not.
        "-p".into(),
        spec.prompt.clone(),
        "--output-format".into(),
        "stream-json".into(),
        // Same mandate as the root's: 2.1.220 exits 1 without it under `-p --output-format
        // stream-json`.
        "--verbose".into(),
        // Availability axis: no built-in tools. Does NOT gate MCP tools.
        "--tools".into(),
        String::new(),
        // Permission axis. See `ChildSpec::allowed_tools`.
        "--allowedTools".into(),
        spec.allowed_tools.join(","),
        // Deliberately **no `--permission-prompt-tool`**: it exists to route a non-allowlisted call
        // to marion over the typed control plane, and this surface has none. Setting it here would
        // trade a fast auto-denial for a question nobody can answer.
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        spec.mcp_config.to_string_lossy().into_owned(),
        "--setting-sources".into(),
        String::new(),
    ];
    if let Some(m) = &spec.model {
        args.push("--model".into());
        args.push(m.clone());
    }

    let mut env = Vec::new();
    if let Some(u) = &spec.base_url {
        env.push(("ANTHROPIC_BASE_URL".to_string(), u.clone()));
    }
    if let Some(k) = &spec.api_key {
        env.push(("ANTHROPIC_AUTH_TOKEN".to_string(), k.clone()));
        // A non-empty key silently wins over the token (§6.4), so it is blanked rather than left
        // inherited — otherwise a child would present the operator's real key to marion's endpoint.
        env.push(("ANTHROPIC_API_KEY".to_string(), String::new()));
    }

    Invocation {
        program: "claude".into(),
        args,
        env,
        cwd: spec.cwd.clone(),
        model: spec.model.clone(),
    }
}

/// Parse a `--output-format stream-json` stream.
///
/// The frame shapes are the ones `tests/fixtures/s1/` and `tests/fixtures/s9/` recorded off a real
/// 2.1.220: an `assistant` frame wraps `message.content[]` blocks, and a call to marion is a block
/// of `{"type":"tool_use","name":"mcp__marion__report","input":{…}}`. The run's terminal frame is
/// `{"type":"result", …}`, whose `is_error`/`subtype` is the harness's own verdict.
///
/// **This is the child path, and marion has no Claude Code child yet.** `marion-supervisor::root`
/// drives the *root*'s conversation itself, because a root is steered turn by turn over a typed
/// control plane rather than parsed after the fact, and it keeps the whole transcript rather than
/// this outcome. So the two are not duplicates of one another: this is the `LaunchOnly`-shaped read
/// the seam requires of every harness, written against the same measured frames, and the day a
/// `claude` agent type is spawnable as a child it is what will read it.
///
/// `file_change_paths` stays empty: Claude Code's edits arrive as `tool_use` blocks for its own
/// built-in tools, whose argument shapes are per-tool and unmeasured here. Guessing them would put
/// invented paths into the audit record, and git is the authority for `changed_paths` anyway.
pub fn parse_stream(s: &str, report_tool: &str) -> StreamOutcome {
    let mut out = StreamOutcome::default();
    for v in json_frames(s) {
        match v["type"].as_str() {
            Some("assistant") => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"].as_str() == Some("tool_use")
                        && block["name"].as_str() == Some(report_tool)
                        && let Some(n) = block["input"]["narrative"].as_str()
                    {
                        out.narrative = Some(n.to_string());
                    }
                }
            }
            Some("result") => {
                let errored = v["is_error"].as_bool() == Some(true)
                    || v["subtype"].as_str().is_some_and(|s| s != "success");
                if errored {
                    out.failure = first_string(&v, &["/result", "/subtype"])
                        .or_else(|| Some("the run's result frame reported an error".into()));
                }
            }
            _ => {}
        }
    }
    out
}

/// Every marion tool this stream shows the node calling, in **marion's** vocabulary.
///
/// `prefix` is this harness's own namespace for marion's verbs — the adapter derives it from its
/// `marion_tool_name`, so there is one spelling, not two.
pub fn marion_tool_calls(s: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    for v in json_frames(s) {
        if v["type"].as_str() != Some("assistant") {
            continue;
        }
        for block in v["message"]["content"].as_array().into_iter().flatten() {
            if block["type"].as_str() == Some("tool_use")
                && let Some(tool) = block["name"].as_str().and_then(|n| n.strip_prefix(prefix))
            {
                out.push(tool.to_string());
            }
        }
    }
    out
}

/// `ANTHROPIC_BASE_URL` from the provider base URL marion carries.
///
/// The two harnesses disagree about the `/v1`: a Codex `model_providers` entry names the full
/// `…/v1`, while Claude Code appends `/v1/messages` to whatever it is given and would otherwise
/// request `/v1/v1/messages`. marion stores the Codex form — it is the one that appears verbatim
/// in a config file — and derives the other. This lives with the adapter that needs the
/// derivation, so the supervisor never has to know which harness wants which spelling.
pub fn anthropic_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

/// The values [`mcp_config_json`] writes into the declaration.
#[derive(Debug, Clone)]
pub struct McpEnv {
    pub bridge: PathBuf,
    pub repo: PathBuf,
    pub state: PathBuf,
    pub base_url: String,
    pub agent_id: AgentId,
    pub ready_file: PathBuf,
}

/// The `--mcp-config` document declaring marion's control MCP.
///
/// The bridge is a *short-lived process the harness starts*, not one marion spawns (§5.4), so
/// everything it needs rides this declaration: which repo, which state dir, which provider, and
/// **which node it is serving**. `MARION_AGENT_ID` is what makes `TaskContract.requester` the
/// root's own `AgentId` rather than a placeholder.
///
/// This is the Claude Code adapter's config emission, so it lives here rather than in the
/// supervisor: §3.1 makes config generation part of the adapter contract, and the Codex adapter's
/// counterpart ([`crate::codex::config_toml`]) has always lived beside its own compile step.
pub fn mcp_config_json(node_env: &McpEnv) -> Value {
    json!({
        "mcpServers": {
            "marion": {
                "type": "stdio",
                "command": node_env.bridge.to_string_lossy(),
                "args": ["mcp"],
                "env": {
                    "MARION_REPO": node_env.repo.to_string_lossy(),
                    "MARION_STATE_DIR": node_env.state.to_string_lossy(),
                    "MARION_BASE_URL": node_env.base_url,
                    AGENT_ID_ENV: node_env.agent_id.0,
                    READY_FILE_ENV: node_env.ready_file.to_string_lossy(),
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> HeadlessSpec {
        HeadlessSpec {
            cwd: "/tmp/wt".into(),
            model: Some("haiku".into()),
            allowed_tools: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
            mcp_config: "/tmp/mcp.json".into(),
            base_url: Some("http://127.0.0.1:8099".into()),
        }
    }

    fn args_of(inv: &Invocation) -> String {
        inv.args.join(" ")
    }

    #[test]
    fn verbose_is_present_because_the_cli_exits_1_without_it() {
        let inv = compile_headless(&spec());
        assert!(
            inv.args.iter().any(|a| a == "--verbose"),
            "2.1.220 refuses -p --output-format stream-json without --verbose"
        );
    }

    #[test]
    fn the_two_tool_axes_are_distinct() {
        let inv = compile_headless(&spec());
        let s = args_of(&inv);
        // --tools is availability and does NOT gate MCP tools; --allowedTools is permission and
        // is what a denied mcp__marion__spawn call turns on.
        assert!(s.contains("--tools  --allowedTools") || s.contains("--tools "));
        let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(inv.args[i + 1], "mcp__marion__spawn,mcp__marion__status");
    }

    #[test]
    fn permission_prompt_tool_is_set_or_can_use_tool_never_fires() {
        let inv = compile_headless(&spec());
        let i = inv
            .args
            .iter()
            .position(|a| a == "--permission-prompt-tool")
            .expect("without this the CLI auto-denies in-process and marion sees nothing");
        assert_eq!(inv.args[i + 1], "stdio");
    }

    #[test]
    fn mcp_config_is_strict_so_the_users_servers_stay_out() {
        let inv = compile_headless(&spec());
        assert!(inv.args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(inv.args.iter().any(|a| a == "--mcp-config"));
    }

    #[test]
    fn claude_config_dir_is_never_isolated() {
        let inv = compile_headless(&spec());
        assert!(
            !inv.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
            "isolating it breaks OAuth: the Keychain entry is keyed to the real config dir"
        );
    }

    #[test]
    fn nothing_is_shell_quoted_because_nothing_reaches_a_shell() {
        let inv = compile_headless(&spec());
        assert_eq!(inv.program, "claude");
        assert!(
            inv.args
                .iter()
                .all(|a| !a.contains('\'') && !a.contains('"'))
        );
    }

    fn child_spec() -> ChildSpec {
        ChildSpec {
            cwd: "/tmp/wt".into(),
            model: None,
            prompt: "do the task".into(),
            allowed_tools: vec!["mcp__marion__report".into()],
            mcp_config: "/tmp/mcp.json".into(),
            base_url: Some("http://127.0.0.1:8099".into()),
            api_key: Some("dummy".into()),
        }
    }

    #[test]
    fn a_childs_prompt_rides_argv_rather_than_a_frame_written_after_launch() {
        let inv = compile_child(&child_spec());
        let i = inv.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(inv.args[i + 1], "do the task");
        assert!(
            !inv.args.iter().any(|a| a == "--input-format"),
            "a LaunchOnly child has no stdin to write a frame on: stdin is closed"
        );
    }

    /// The permission axis is the one that decides whether the child's single load-bearing call
    /// happens at all, and its failure is silent: an unlisted tool is auto-denied in process.
    #[test]
    fn a_childs_marion_tools_are_allowlisted_and_no_permission_prompt_is_offered() {
        let inv = compile_child(&child_spec());
        let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(inv.args[i + 1], "mcp__marion__report");
        assert!(
            !inv.args.iter().any(|a| a == "--permission-prompt-tool"),
            "it routes a denial to marion over a control plane this surface does not have, so \
             setting it trades a fast auto-denial for a question nobody answers"
        );
    }

    #[test]
    fn a_childs_credential_is_the_token_and_the_key_beside_it_is_blanked() {
        let inv = compile_child(&child_spec());
        assert_eq!(
            inv.env,
            vec![
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "http://127.0.0.1:8099".to_string()
                ),
                ("ANTHROPIC_AUTH_TOKEN".to_string(), "dummy".to_string()),
                // Non-empty, it would silently win — and would be the operator's own key.
                ("ANTHROPIC_API_KEY".to_string(), String::new()),
            ]
        );
    }

    /// The three flags a child shares with the root, each for the same measured reason.
    #[test]
    fn a_child_keeps_the_flags_that_have_nothing_to_do_with_the_surface() {
        let inv = compile_child(&child_spec());
        for flag in [
            "--verbose",           // 2.1.220 exits 1 without it under -p stream-json
            "--strict-mcp-config", // never the user's servers
            "--setting-sources",   // never the user's settings
        ] {
            assert!(inv.args.iter().any(|a| a == flag), "{flag}");
        }
        let i = inv.args.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(inv.args[i + 1], "/tmp/mcp.json");
    }

    #[test]
    fn a_child_records_the_model_it_was_given_and_nothing_when_it_was_given_none() {
        assert_eq!(compile_child(&child_spec()).model, None);
        let inv = compile_child(&ChildSpec {
            model: Some("haiku".into()),
            ..child_spec()
        });
        assert_eq!(inv.model.as_deref(), Some("haiku"));
        let i = inv.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(inv.args[i + 1], "haiku");
    }
}
