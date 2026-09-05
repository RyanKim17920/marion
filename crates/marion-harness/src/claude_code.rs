//! The Claude Code adapter (design §5.2): its launch row, its stream grammar, and its MCP
//! declaration document.
//!
//! Control is **config-time**: marion owns the launch configuration and never parses a pty. Every
//! flag here was measured against 2.1.220, and several are non-obvious enough that the tests below
//! state *why* rather than merely pinning the string.

use marion_core::harness::Harness;
use serde_json::{Value, json};

use crate::grammar::{
    Cond, Failure, Name, OnRefusedReport, Pairing, StreamGrammar, Verdict, Where,
};
pub use crate::mcp_bridge::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, AUTH_ENV, BASE_URL_ENV, BridgeEnv, DEPTH_ENV, NODE_TOKEN_ENV,
    READY_FILE_ENV,
};
use crate::spec::{Arg, Env, Field, HarnessSpec, Val, When};

/// Claude Code's row. Measured against 2.1.220 (S1, S9, S11) and re-measured on 2.1.222 for the
/// two tool axes (`tests/fixtures/s14/`); every flag below carries its reason in the doc of the
/// hand-written compile it was transcribed from, and those reasons are repeated here only where
/// the *shape* is the surprising part.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::ClaudeCode,
    program: Some("claude"),
    argv: &[
        Arg::Lit("-p"),
        Arg::Lit("--output-format"),
        Arg::Lit("stream-json"),
        Arg::Lit("--input-format"),
        Arg::Lit("stream-json"),
        // MANDATORY with `-p --output-format stream-json`: without it 2.1.220 exits 1 with "When
        // using --print, --output-format=stream-json requires --verbose" before emitting anything.
        Arg::Lit("--verbose"),
        // The availability axis, **emitted even when empty**: `--tools ""` is the documented
        // "disable all tools" spelling, and every node ran with it before the axis existed. It does
        // not gate MCP tools.
        Arg::Joined("--tools", Field::Tools),
        // The permission axis. marion's tools must be listed here or a denied `spawn` is the result.
        Arg::Joined("--allowedTools", Field::Allowed),
        // Without this a non-allowlisted call is auto-denied in-process and no `can_use_tool`
        // frame ever reaches marion. Absent from --help.
        Arg::Lit("--permission-prompt-tool"),
        Arg::Lit("stdio"),
        // Only the MCP servers marion declared; never the user's.
        Arg::Lit("--strict-mcp-config"),
        Arg::Flag("--mcp-config", Field::McpConfig),
        // No user settings, plugins or hooks leak into a node. This does NOT suppress the
        // session-title request (§5.5).
        Arg::Lit("--setting-sources"),
        Arg::Lit(""),
        Arg::Flag("--model", Field::Model),
    ],
    // The TUI: the same isolation and the same two axes, none of the protocol flags (a pane has an
    // operator in it, so the permission ask stays the harness's own dialog), and the prompt on
    // argv — seeded into the composer, not sent, so there is no turn-one race to lose.
    pane: Some(&[
        Arg::Joined("--tools", Field::Tools),
        Arg::Joined("--allowedTools", Field::Allowed),
        Arg::Lit("--strict-mcp-config"),
        Arg::Flag("--mcp-config", Field::McpConfig),
        Arg::Lit("--setting-sources"),
        Arg::Lit(""),
        Arg::Flag("--model", Field::Model),
        Arg::PosIfNonEmpty(Field::Prompt),
    ]),
    // NOT `CLAUDE_CONFIG_DIR`: isolating it breaks OAuth, because the Keychain entry is keyed to
    // the real config dir. The fileless path above is what keeps auth working, and live mode is
    // therefore *only* the removal of these three — which the neutral fields' live rule does.
    env: &[
        Env {
            key: "ANTHROPIC_BASE_URL",
            val: Val::Field(Field::BaseUrl),
            when: When::Always,
        },
        Env {
            key: "ANTHROPIC_AUTH_TOKEN",
            val: Val::Field(Field::ApiKey),
            when: When::Always,
        },
        // A non-empty key silently wins over the token (§6.4), so it is blanked beside one rather
        // than left inherited — otherwise a node would present the operator's real key to marion's
        // endpoint.
        Env {
            key: "ANTHROPIC_API_KEY",
            val: Val::Lit(""),
            when: When::Present(Field::ApiKey),
        },
    ],
    stream: Some(&STREAM),
    note: "S1/S9/S11 on 2.1.220; s14 on 2.1.222 for --tools/--allowedTools. The pane shape was \
           measured on 2.1.220 for M3 C1 (MILESTONES: the recorded manual session)",
};

/// How a `--output-format stream-json` stream is read (`tests/fixtures/s1/`, `s9/`).
///
/// A call to marion is a `tool_use` block inside an `assistant` frame's `message.content[]`, and
/// its result a `tool_result` block inside a later `user` frame, paired by `tool_use_id`. On the
/// recording the success case has **no `is_error` key at all**, which is why the verdict is an
/// error *flag*: an absent key is this harness saying the call was fine. The run's own verdict is
/// its `result` frame — `is_error` or a `subtype` other than `success` — read on this surface
/// rather than the exit code, because 2.1.220 reports its own errors in-band.
///
/// `file_changes` is `None`: Claude Code's edits arrive as `tool_use` blocks for its own built-in
/// tools, whose argument shapes are per-tool and unmeasured here, and git is the authority anyway.
pub const STREAM: StreamGrammar = StreamGrammar {
    call: Where {
        frame: &[Cond::Eq("/type", "assistant")],
        each: Some("/message/content"),
        unit: &[Cond::Eq("/type", "tool_use")],
    },
    name: Name::Prefixed("/name"),
    args: "/input",
    pairing: Pairing::Separate {
        call_id: "/id",
        result: Where {
            frame: &[Cond::Eq("/type", "user")],
            each: Some("/message/content"),
            unit: &[Cond::Eq("/type", "tool_result")],
        },
        result_id: "/tool_use_id",
        verdict: Verdict::ErrorFlag {
            path: "/is_error",
            words: &["/content"],
            fallback: "the result frame carried no message",
        },
    },
    refused_report: OnRefusedReport::Record,
    failures: &[
        Failure::NotOk {
            at: Where {
                frame: &[Cond::Eq("/type", "result")],
                each: None,
                unit: &[],
            },
            path: "/subtype",
            ok: "success",
            words: &["/result", "/subtype"],
            label: "the run's result frame reported an error",
        },
        Failure::Frame {
            at: Where {
                frame: &[Cond::Eq("/type", "result"), Cond::Eq("/is_error", "true")],
                each: None,
                unit: &[],
            },
            words: &["/result", "/subtype"],
            fallback: "the run's result frame reported an error",
        },
    ],
    file_changes: None,
};

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

/// The `--mcp-config` document declaring marion's control MCP.
///
/// The bridge is a *short-lived process the harness starts*, not one marion spawns (§5.4), so
/// everything it needs rides this declaration: which repo, which state dir, which provider, and
/// **which node it is serving**. `MARION_AGENT_ID` is what makes `TaskContract.requester` the
/// root's own `AgentId` rather than a placeholder.
///
/// This is the Claude Code adapter's config emission, so it lives here rather than in the
/// supervisor: §3.1 makes config generation part of the adapter contract, and the Codex adapter's
/// counterpart ([`crate::codex::config_toml`]) has always lived beside its own compile step. The
/// `env` block is [`BridgeEnv::env_json`], the same derivation every harness's document writes.
pub fn mcp_config_json(b: &BridgeEnv) -> Value {
    json!({
        "mcpServers": {
            "marion": {
                "type": "stdio",
                "command": b.bridge.to_string_lossy(),
                "args": b.args,
                "env": b.env_json()
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::AgentId;

    use crate::adapter::{
        ClaudeCodeAdapter, Extras, HarnessAdapter, HarnessError, LaunchSpec, McpDeclaration,
        SpawnCtx,
    };
    use crate::auth::Auth;
    use crate::invocation::Invocation;

    fn ctx() -> SpawnCtx {
        SpawnCtx {
            agent_id: AgentId("019f-root".into()),
            agent_type: "claude".into(),
            depth: 0,
            node_token: None,
            ready_file: Some("/tmp/mcp-ready".into()),
            repo: "/repo".into(),
            state_dir: "/state".into(),
            bridge: "/bin/marion-supervisor".into(),
            bridge_args: vec!["mcp".into()],
        }
    }

    /// A **root**: no credential of its own, because `marion run` mints a per-run token after
    /// `compile` so that a request log attributes traffic to one run.
    fn root() -> LaunchSpec {
        LaunchSpec {
            cwd: "/tmp/wt".into(),
            model: Some("haiku".into()),
            prompt: String::new(),
            tools: vec![],
            allowed_tools: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: None,
            auth: Auth::Canned,
            config_dir: "/tmp".into(),
            extra: Extras::default(),
        }
    }

    /// A **child** differs from a root in exactly two compiled values — the permission axis it is
    /// given and the credential it presents — and in nothing else. Every flag is the root's,
    /// because a Claude Code node has one launch shape ([`SPEC`]).
    fn child() -> LaunchSpec {
        LaunchSpec {
            model: None,
            allowed_tools: vec!["mcp__marion__report".into()],
            api_key: Some("dummy".into()),
            ..root()
        }
    }

    fn compile(spec: &LaunchSpec) -> Invocation {
        ClaudeCodeAdapter.compile(spec, &ctx()).unwrap()
    }

    #[test]
    fn verbose_is_present_because_the_cli_exits_1_without_it() {
        let inv = compile(&root());
        assert!(
            inv.args.iter().any(|a| a == "--verbose"),
            "2.1.220 refuses -p --output-format stream-json without --verbose"
        );
    }

    #[test]
    fn the_two_tool_axes_are_distinct() {
        let inv = compile(&root());
        // --tools is availability and does NOT gate MCP tools; --allowedTools is permission and
        // is what a denied mcp__marion__spawn call turns on. An empty availability axis is still
        // emitted, as the documented `""`.
        let i = inv.args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(inv.args[i + 1], "");
        let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(inv.args[i + 1], "mcp__marion__spawn,mcp__marion__status");
    }

    #[test]
    fn permission_prompt_tool_is_set_or_can_use_tool_never_fires() {
        let inv = compile(&root());
        let i = inv
            .args
            .iter()
            .position(|a| a == "--permission-prompt-tool")
            .expect("without this the CLI auto-denies in-process and marion sees nothing");
        assert_eq!(inv.args[i + 1], "stdio");
    }

    #[test]
    fn mcp_config_is_strict_so_the_users_servers_stay_out() {
        let inv = compile(&root());
        assert!(inv.args.iter().any(|a| a == "--strict-mcp-config"));
        let i = inv.args.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(
            inv.args[i + 1],
            "/tmp/mcp.json",
            "under the node's own config dir"
        );
    }

    #[test]
    fn claude_config_dir_is_never_isolated() {
        let inv = compile(&root());
        assert!(
            !inv.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
            "isolating it breaks OAuth: the Keychain entry is keyed to the real config dir"
        );
    }

    #[test]
    fn nothing_is_shell_quoted_because_nothing_reaches_a_shell() {
        let inv = compile(&root());
        assert_eq!(inv.program, "claude");
        assert!(
            inv.args
                .iter()
                .all(|a| !a.contains('\'') && !a.contains('"'))
        );
    }

    /// **The defect this shape exists to end.** A child's prompt is a frame written after the
    /// readiness gate, never argv: 2.1.220 does not hold turn one for an `--mcp-config` server, so
    /// an argv prompt takes that turn with `"tools":[]`, gets the session-title stub back, and the
    /// run exits 0 having called nothing (§6.1 step 8, §12).
    #[test]
    fn a_childs_prompt_is_never_compiled_into_argv_and_stdin_stays_typed() {
        let inv = compile(&child());
        assert!(
            inv.args.iter().any(|a| a == "--input-format"),
            "without a typed stdin there is no frame to withhold, and the gate cannot exist"
        );
        let i = inv.args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(
            inv.args.get(i + 1).map(String::as_str),
            Some("--output-format"),
            "`-p` takes no value here; a positional prompt is the toolless-turn bug"
        );
        let with_prompt = LaunchSpec {
            prompt: "do the task".into(),
            ..child()
        };
        assert!(matches!(
            ClaudeCodeAdapter.compile(&with_prompt, &ctx()),
            Err(HarnessError::MissingInput {
                harness: Harness::ClaudeCode,
                ..
            })
        ));
    }

    /// The permission axis is the one that decides whether the child's single load-bearing call
    /// happens at all, and its failure is silent: an unlisted tool is auto-denied in process.
    #[test]
    fn a_childs_marion_tools_are_allowlisted() {
        let inv = compile(&child());
        let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(inv.args[i + 1], "mcp__marion__report");
    }

    #[test]
    fn a_childs_credential_is_the_token_and_the_key_beside_it_is_blanked() {
        let inv = compile(&child());
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

    /// And a caller that mints its own per-run token after `compile` — which is what `marion run`
    /// does, so that a request log attributes traffic to one run — gets no pair pushed under it.
    #[test]
    fn a_node_with_no_credential_in_its_spec_carries_no_anthropic_pair() {
        let inv = compile(&root());
        let names: Vec<&str> = inv.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["ANTHROPIC_BASE_URL"]);
    }

    #[test]
    fn a_node_records_the_model_it_was_given_and_nothing_when_it_was_given_none() {
        assert_eq!(compile(&child()).model, None);
        let inv = compile(&LaunchSpec {
            model: Some("haiku".into()),
            ..child()
        });
        assert_eq!(inv.model.as_deref(), Some("haiku"));
        let i = inv.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(inv.args[i + 1], "haiku");
    }
}
