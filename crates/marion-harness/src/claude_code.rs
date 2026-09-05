//! The Claude Code adapter (design §5.2).
//!
//! Control is **config-time**: marion owns the launch configuration and never parses a pty. Every
//! flag here was measured against 2.1.220, and several are non-obvious enough that the tests below
//! state *why* rather than merely pinning the string.

use std::collections::BTreeMap;
use std::path::PathBuf;

use marion_core::contract::AgentId;
use serde_json::{Value, json};

use marion_core::harness::Harness;

use crate::auth::Auth;
pub use crate::mcp_bridge::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, AUTH_ENV, BASE_URL_ENV, DEPTH_ENV, NODE_TOKEN_ENV, READY_FILE_ENV,
};
use crate::spec::{Arg, Env, Field, HarnessSpec, Val, When};
use crate::stream::{
    CallOutcome, MarionCall, StreamOutcome, first_string, json_frames, report_commits,
};

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
    stream: None,
    note: "S1/S9/S11 on 2.1.220; s14 on 2.1.222 for --tools/--allowedTools. The pane shape was \
           measured on 2.1.220 for M3 C1 (MILESTONES: the recorded manual session)",
};

/// Parse a `--output-format stream-json` stream.
///
/// The frame shapes are the ones `tests/fixtures/s1/` and `tests/fixtures/s9/` recorded off a real
/// 2.1.220: an `assistant` frame wraps `message.content[]` blocks, and a call to marion is a block
/// of `{"type":"tool_use","name":"mcp__marion__report","input":{…}}`. The run's terminal frame is
/// `{"type":"result", …}`, whose `is_error`/`subtype` is the harness's own verdict.
///
/// **This is the child path.** `marion-supervisor::duplex` drives the conversation itself and keeps
/// the whole transcript; this folds the same bytes into the `StreamOutcome` the seam requires of
/// every harness, which is what `run_spawn` turns into a `TaskContract` (§6.1 step 9). The two are
/// not duplicates: one is the live protocol, the other is the audit read.
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
                    {
                        // Both fields off the one call. Narrative stays conditional — `None` is
                        // load-bearing, it is what `build_contract` turns into `Unreported` — while
                        // commits are read whenever the call is seen, since an absent list and an
                        // empty one are the same claim.
                        if let Some(n) = block["input"]["narrative"].as_str() {
                            out.narrative = Some(n.to_string());
                        }
                        out.result_commits = report_commits(&block["input"]);
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

/// Every marion tool this stream shows the node calling, in **marion's** vocabulary, with what came
/// of each call.
///
/// `prefix` is this harness's own namespace for marion's verbs — the adapter derives it from its
/// `marion_tool_name`, so there is one spelling, not two.
///
/// **The call and its result are two frames and are paired by `tool_use_id`.** The shape is the one
/// `tests/fixtures/s9/can-use-tool-allow.stdout.jsonl` recorded off a real 2.1.220: an `assistant`
/// frame carries `{"type":"tool_use","id":"toolu_…","name":"mcp__marion__report"}` and a later
/// `user` frame carries `{"type":"tool_result","tool_use_id":"toolu_…", …}`. On that recording the
/// success case has **no `is_error` key at all**, which is why the reading is `Some(true)` ⇒
/// refused rather than "not `Some(false)`" ⇒ refused: an absent key is this harness saying the call
/// was fine, and treating it as a refusal would red-line every working run.
///
/// A call whose result frame never arrived is [`CallOutcome::Unknown`], not an answer — a run that
/// was killed mid-call leaves exactly that trace.
pub fn marion_calls(s: &str, prefix: &str) -> Vec<MarionCall> {
    let frames = json_frames(s);
    // id → what its result frame said. Built first, because a stream is read once and the results
    // trail the calls.
    let mut results: BTreeMap<String, CallOutcome> = BTreeMap::new();
    for v in &frames {
        if v["type"].as_str() != Some("user") {
            continue;
        }
        for block in v["message"]["content"].as_array().into_iter().flatten() {
            if block["type"].as_str() == Some("tool_result")
                && let Some(id) = block["tool_use_id"].as_str()
            {
                results.insert(id.to_string(), tool_result_outcome(block));
            }
        }
    }

    let mut out = Vec::new();
    for v in &frames {
        if v["type"].as_str() != Some("assistant") {
            continue;
        }
        for block in v["message"]["content"].as_array().into_iter().flatten() {
            if block["type"].as_str() == Some("tool_use")
                && let Some(tool) = block["name"].as_str().and_then(|n| n.strip_prefix(prefix))
            {
                let outcome = block["id"]
                    .as_str()
                    .and_then(|id| results.get(id).cloned())
                    .unwrap_or(CallOutcome::Unknown);
                out.push(MarionCall {
                    verb: tool.to_string(),
                    outcome,
                });
            }
        }
    }
    out
}

/// One `tool_result` block's verdict, and the words behind it.
///
/// The `content` is an array of typed blocks on the recording, so the refusal's own sentence is
/// gathered from the `text` ones. A refusal with no readable text still refuses — the fallback says
/// so rather than reporting an empty string, since "refused, and the harness gave no reason" is a
/// different thing to read than "refused: <reason>".
fn tool_result_outcome(block: &Value) -> CallOutcome {
    if block["is_error"].as_bool() != Some(true) {
        return CallOutcome::Answered;
    }
    let text: Vec<&str> = block["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect();
    match text.is_empty() {
        true => CallOutcome::Refused("the result frame carried no message".into()),
        false => CallOutcome::Refused(text.join(" ")),
    }
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
    /// `None` under [`crate::Auth::Inherited`], where the key is **omitted** rather than written
    /// empty — see [`BASE_URL_ENV`].
    pub base_url: Option<String>,
    /// Which endpoint the child this node spawns should talk to. See [`AUTH_ENV`].
    pub auth: Auth,
    pub agent_id: AgentId,
    /// The node's agent type name, in its **canonical** spelling — the alias `codex` resolves to
    /// `codex-impl` before it gets here, so the bridge re-resolves one definition and not two.
    pub agent_type: String,
    /// The node's depth, root = 0. Its `spawn` creates a node at `depth + 1`.
    pub depth: u32,
    /// §5.4's capability token for this node. `None` where the supervisor minted none — a node
    /// spawned by a path that does not own it, which since steps 5 and 6 is no production path at
    /// all: every root and every child now comes through the socket's `agent/spawn`. A node that
    /// somehow has none runs normally and cannot spawn, which its bridge reports by name. See
    /// [`NODE_TOKEN_ENV`].
    pub node_token: Option<String>,
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
    let mut env = json!({
        "MARION_REPO": node_env.repo.to_string_lossy(),
        "MARION_STATE_DIR": node_env.state.to_string_lossy(),
        // The auth mode is stated on **every** declaration, in both modes: a key that appears only
        // under `--live` would make its absence mean two things at once (canned, or an older
        // marion), which is the ambiguity `AUTH_ENV` exists to remove.
        AUTH_ENV: node_env.auth.as_wire(),
        AGENT_ID_ENV: node_env.agent_id.0,
        AGENT_TYPE_ENV: node_env.agent_type,
        // A string, because an MCP `env` block is `Record<string,string>` on every
        // harness that has one. The bridge parses it back.
        DEPTH_ENV: node_env.depth.to_string(),
        READY_FILE_ENV: node_env.ready_file.to_string_lossy(),
    });
    // Present or absent, never empty — see [`BASE_URL_ENV`].
    if let Some(u) = &node_env.base_url {
        env[BASE_URL_ENV] = json!(u);
    }
    // Same rule, and see [`NODE_TOKEN_ENV`] for why breaking it here is worse than a
    // misconfiguration.
    if let Some(t) = &node_env.node_token {
        env[NODE_TOKEN_ENV] = json!(t);
    }
    json!({
        "mcpServers": {
            "marion": {
                "type": "stdio",
                "command": node_env.bridge.to_string_lossy(),
                "args": ["mcp"],
                "env": env
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{
        ClaudeCodeAdapter, Extras, HarnessAdapter, HarnessError, LaunchSpec, McpDeclaration,
        SpawnCtx,
    };
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
