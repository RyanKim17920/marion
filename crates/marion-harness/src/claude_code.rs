//! The Claude Code adapter (design §5.2).
//!
//! Control is **config-time**: marion owns the launch configuration and never parses a pty. Every
//! flag here was measured against 2.1.220, and several are non-obvious enough that the tests below
//! state *why* rather than merely pinning the string.

use std::collections::BTreeMap;
use std::path::PathBuf;

use marion_core::contract::AgentId;
use serde_json::{Value, json};

use crate::adapter::Auth;
use crate::invocation::Invocation;
use crate::stream::{
    CallOutcome, MarionCall, StreamOutcome, first_string, json_frames, report_commits,
};

/// Env var naming the file the bridge touches once it has answered `tools/list`.
pub const READY_FILE_ENV: &str = "MARION_READY_FILE";
/// Env var carrying a node's `AgentId` to the bridge, so a top-level `spawn` can stamp
/// `TaskContract.requester` with it (§9).
pub const AGENT_ID_ENV: &str = "MARION_AGENT_ID";
/// Env var carrying a node's **agent type name** to the bridge.
///
/// §6.1 step 2 says the `spawn` gates read *"the **caller's** agent type"*, and `max_depth` /
/// `max_concurrent_children` are per-type keys (§3.1). The bridge is a process the *harness*
/// starts, so it knows nothing about the node it serves beyond what this declaration tells it —
/// without this name it would have to assume [`marion_core::agent_type::DEFAULT_MAX_DEPTH`], which
/// is a constant pretending to be a lookup and would silently ignore any type that stated its own
/// bound. That is the same shape as `AgentType.harness` having once been a `String` nothing read.
pub const AGENT_TYPE_ENV: &str = "MARION_AGENT_TYPE";
/// Env var carrying the node's **auth mode** to the bridge (§6.4, `--live`).
///
/// The sixth member of the bridge's env contract, and the one that makes `--live` survive a spawn
/// hop. The bridge is a process the *harness* starts, so the per-server `env` block is marion's only
/// channel to it — and until this existed nothing in that block said "live": a live root's child was
/// silently compiled canned, against an endpoint that was not running.
///
/// **Explicit rather than inferred from an absent `MARION_BASE_URL`.** The absence of a URL is
/// already ambiguous — a declaration written by an older marion, a harness that reads no URL, a bug
/// that dropped it — and "guess live from a missing key" turns every one of those into a real
/// credential pointed somewhere marion did not choose. The mode is a decision, so it is stated. Its
/// absence still means [`crate::Auth::Canned`], which is what every pre-`--live` declaration meant.
pub const AUTH_ENV: &str = "MARION_AUTH";
/// Env var carrying the provider base URL the bridge should hand a child it spawns.
///
/// **Omitted entirely under [`crate::Auth::Inherited`]**, never written empty: `MARION_BASE_URL=""`
/// read back through `var()` is `Ok("")`, which is a base URL that names nothing and compiles into a
/// child's config as a provider pointing at the empty string. Absent is a state the reader can act
/// on; empty is one it cannot tell from a value.
pub const BASE_URL_ENV: &str = "MARION_BASE_URL";
/// Env var carrying a node's **depth** to the bridge, with the root at 0 (§3.1, §6.1 step 2).
///
/// The other half of the same problem: depth is a property of the *tree*, which only marion can
/// see, and a node's `spawn` is served by a bridge that marion did not start. Until this existed
/// nothing anywhere computed a depth, so `max_depth` was inert and a child could spawn a
/// grandchild — and that grandchild another — without bound.
pub const DEPTH_ENV: &str = "MARION_DEPTH";
/// Env var carrying the node's **per-node capability token** to its bridge — §5.4's *"a per-node
/// capability token bound to its `AgentId`"*, which until §11 item 28 step 4 was a sentence in the
/// design with nothing implementing it.
///
/// [`AGENT_ID_ENV`] tells the bridge who it serves, and since §11 item 28 step 5 the bridge states
/// that identity back over a connection nobody authenticated: `serve_conn` performs no
/// `SO_PEERCRED`/`getpeereid` check for a node's spawn, so **any process that can `connect(2)` could
/// assert another node's `AgentId`**, and §6.1 step 2's gates would be a check the caller chooses
/// whether to fail. This is what makes the identity provable rather than merely stated: the
/// supervisor minted the value, holds the `AgentId → token` binding in memory, and wrote it into
/// exactly one place — this declaration, which only the node's own bridge reads.
///
/// **In memory, so a supervisor restart invalidates it**, and that is now visible rather than
/// theoretical: the node keeps running, its bridge keeps serving, and its next `spawn` is refused
/// because the supervisor that minted this value is gone (`handler::resolve_caller` says so in the
/// refusal). §5.4 asks for a capability bound to an `AgentId`, and a secret at rest under `<state>`
/// would be readable by exactly the processes it excludes — so the cost is the restart, taken
/// deliberately (§11 item 28, open question 2).
///
/// **Present or absent, never empty**, for [`BASE_URL_ENV`]'s reason sharpened: `MARION_NODE_TOKEN=""`
/// read back through `var()` is `Ok("")`, and a capability token every process on the machine can
/// guess is worse than none, because the bridge would present it and be believed by any check that
/// only asked whether a token was stated.
pub const NODE_TOKEN_ENV: &str = "MARION_NODE_TOKEN";

/// What marion needs to compile a headless invocation — **root or child.**
///
/// There is one shape and not two. A Claude Code node's prompt is *always* a frame written after
/// launch, whatever its role in the tree: 2.1.220 connects `--mcp-config` servers asynchronously
/// and does not hold turn one for them, so a prompt in argv takes that turn with `"tools":[]` and
/// the run exits 0 having called nothing (§6.1 step 8, §12). That is a property of the harness, not
/// of being a root, which is why the child spec that used to sit beside this one is gone.
#[derive(Debug, Clone)]
pub struct HeadlessSpec {
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// Built-in tools that **exist** for this node — the availability axis (§3.1), already in
    /// Claude Code's own spelling (`Write`, …). `--tools` selects from the *built-in* set and does
    /// not gate MCP tools at all, so marion's own verbs never appear here.
    ///
    /// **Empty is the documented "no built-in tools" value and the default every node has run
    /// with**: `--help` on 2.1.222 reads *"Use `""` to disable all tools"*, and an empty list joins
    /// to exactly the `""` this argv has always carried. A node that declares nothing therefore
    /// compiles byte-identically to one compiled before this field existed.
    pub tools: Vec<String>,
    /// Tools the node may call **without a prompt** — the permission axis. marion's own tools go
    /// here, or the node's load-bearing call (`spawn` on a root, `report` on a child) is denied.
    ///
    /// **Every entry of [`Self::tools`] must appear here too**, and the adapter is what unions
    /// them: §3.1's table compiles the permission axis from *"the same list, plus marion's own
    /// `mcp__marion__*`"*. Availability without permission is not a route — §11 item 24 measured
    /// `--tools "Write"` alone sending the call to `--permission-prompt-tool stdio`, where marion
    /// has no answerer, so the child received item 22's dead-end message rather than writing.
    pub allowed_tools: Vec<String>,
    /// Path to the MCP server declaration marion wrote.
    pub mcp_config: PathBuf,
    /// Base URL for the canned provider.
    pub base_url: Option<String>,
    /// The credential the node presents, as `ANTHROPIC_AUTH_TOKEN` with `ANTHROPIC_API_KEY` blanked
    /// beside it (§9, §6.4: a non-empty key silently wins over the token, so it is blanked rather
    /// than left inherited — otherwise a node would present the operator's real key to marion's
    /// endpoint).
    ///
    /// `None` where the caller pushes the pair itself: `marion run` mints a **per-run** token after
    /// `compile`, so that one run's traffic is attributable in a request log.
    pub api_key: Option<String>,
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
        // Availability axis (§3.1): which built-in tools exist. This does NOT gate MCP tools.
        // Empty — the default — is the CLI's documented "disable all tools" value, so a node that
        // declares nothing is launched exactly as every node was before the axis existed.
        //
        // Comma-joined, matching `--allowedTools` below, which is the separator measured to work
        // on this CLI. The multi-name case is presently unreachable rather than measured:
        // `marion_core::agent_type` implements a one-word vocabulary, so at most one name is ever
        // emitted here. Whoever widens that vocabulary owes this separator a measurement.
        "--tools".into(),
        spec.tools.join(","),
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
    if let Some(k) = &spec.api_key {
        env.push(("ANTHROPIC_AUTH_TOKEN".to_string(), k.clone()));
        // A non-empty key silently wins over the token (§6.4), so it is blanked rather than left
        // inherited — otherwise a node would present the operator's real key to marion's endpoint.
        env.push(("ANTHROPIC_API_KEY".to_string(), String::new()));
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

    fn spec() -> HeadlessSpec {
        HeadlessSpec {
            cwd: "/tmp/wt".into(),
            model: Some("haiku".into()),
            tools: vec![],
            allowed_tools: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
            mcp_config: "/tmp/mcp.json".into(),
            base_url: Some("http://127.0.0.1:8099".into()),
            api_key: None,
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

    /// A **child** differs from a root in exactly two compiled values — the permission axis it is
    /// given and the credential it presents — and in nothing else. Every flag is the root's,
    /// because a Claude Code node has one launch shape (see [`HeadlessSpec`]).
    fn child_spec() -> HeadlessSpec {
        HeadlessSpec {
            model: None,
            allowed_tools: vec!["mcp__marion__report".into()],
            api_key: Some("dummy".into()),
            ..spec()
        }
    }

    /// **The defect this shape exists to end.** A child's prompt is a frame written after the
    /// readiness gate, never argv: 2.1.220 does not hold turn one for an `--mcp-config` server, so
    /// an argv prompt takes that turn with `"tools":[]`, gets the session-title stub back, and the
    /// run exits 0 having called nothing (§6.1 step 8, §12).
    #[test]
    fn a_childs_prompt_is_never_compiled_into_argv_and_stdin_stays_typed() {
        let inv = compile_headless(&child_spec());
        assert!(
            inv.args.iter().any(|a| a == "--input-format"),
            "without a typed stdin there is no frame to withhold, and the gate cannot exist"
        );
        let i = inv.args.iter().position(|a| a == "-p").unwrap();
        assert_ne!(
            inv.args.get(i + 1).map(String::as_str),
            Some("do the task"),
            "`-p` takes no value here; a positional prompt is the toolless-turn bug"
        );
    }

    /// The permission axis is the one that decides whether the child's single load-bearing call
    /// happens at all, and its failure is silent: an unlisted tool is auto-denied in process.
    #[test]
    fn a_childs_marion_tools_are_allowlisted() {
        let inv = compile_headless(&child_spec());
        let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(inv.args[i + 1], "mcp__marion__report");
    }

    #[test]
    fn a_childs_credential_is_the_token_and_the_key_beside_it_is_blanked() {
        let inv = compile_headless(&child_spec());
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
        let inv = compile_headless(&spec());
        let names: Vec<&str> = inv.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["ANTHROPIC_BASE_URL"]);
    }

    #[test]
    fn a_node_records_the_model_it_was_given_and_nothing_when_it_was_given_none() {
        assert_eq!(compile_headless(&child_spec()).model, None);
        let inv = compile_headless(&HeadlessSpec {
            model: Some("haiku".into()),
            ..child_spec()
        });
        assert_eq!(inv.model.as_deref(), Some("haiku"));
        let i = inv.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(inv.args[i + 1], "haiku");
    }
}
