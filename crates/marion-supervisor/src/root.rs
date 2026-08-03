//! Launching the **root node** (design §9).
//!
//! > *"marion launches the root node itself. The `claude` root is not hand-started: `marion run
//! > <agent-type> --prompt <…>` spawns it through the same §6.1 path as any child, which is what
//! > gives it an `AgentId`, an agent-dir, and a capability token — without which its `spawn` call
//! > cannot be stamped and `TaskContract.requester` has no value. **A root has no `TaskContract`**
//! > […]; `requester` for a top-level `spawn` is the root's `AgentId`."*
//!
//! So this module mints the id, writes the `--mcp-config` declaration that carries that id to the
//! bridge, compiles the root argv through [`marion_harness::compile_headless`], and drives the
//! `stream-json` conversation.
//!
//! # Why the prompt is not written the moment the process starts
//!
//! Measured on 2.1.220: MCP servers declared through `--mcp-config` are connected
//! **asynchronously and non-blockingly** — the debug log says so in as many words — and the first
//! turn is *not* held for them. Against a real endpoint that is invisible, because a model reply
//! takes seconds and the ~70 ms connect has long finished. Against the CannedProvider the reply
//! comes back in microseconds, so the root's first request goes out with `tools: []`, marion's
//! `spawn` is not among the tools, and the turn ends with plain text. **Nothing anywhere reports
//! an error** — the same class of silent failure the provider's shape-dispatch exists to prevent.
//!
//! marion owns both ends of that race, so it closes it rather than sleeping through it. The bridge
//! touches [`READY_FILE_ENV`] once it has answered `tools/list`, which is the actual event that
//! matters ("the harness has marion's tool list"), and the root's prompt is written only after
//! that file appears. A `control_request`/`control_response` round trip then follows, so the
//! prompt is written only after the harness's event loop has demonstrably run at least once since
//! the tool list was flushed to it.
//!
//! **None of that lives here.** §6.1 step 8 states the gate normatively, binding on *any* launcher
//! driving Claude Code headlessly — root or child — so the whole protocol sits in [`crate::duplex`]
//! and this module drives it. `run_spawn` drives the same code for a `Typed(_)` child, which is
//! what closed the last red cell of the harness matrix: until then it drove every child as
//! `LaunchOnly` and a `claude` child took turn one with `tools: []`.
//!
//! # Two root paths, chosen by the surface and never by the name
//!
//! Everything above is Claude-Code-specific *scaffolding*, not something a root needs in general.
//! §3.4 is explicit that code branches on `ExecutionSurfaces`, so [`launch`] dispatches on
//! `surfaces().control`:
//!
//! - **`Typed(_)`** — the path described above: pipes, a readiness marker, an `initialize` round
//!   trip, a user frame written after launch, and `can_use_tool` answered over the same channel.
//! - **`LaunchOnly`** — the prompt rides argv, so there is no frame to withhold and nothing to
//!   steer. §6.1 step 8 binds *"only surfaces whose prompt is written after launch"*, and says such
//!   a node's MCP readiness *"is asserted post hoc from the `mcp_tool_call` items in its stream"*.
//!   Permissions on those three harnesses are settled at **config** time — codex's
//!   `default_tools_approval_mode = "approve"`, gemini's `"trust": true`, opencode's allow-by-
//!   default — which is why no runtime permission channel is missing from this path rather than
//!   merely unimplemented.
//!
//! A root on either path has **no `TaskContract` and cannot `report`** (§9). Its result is its
//! stream and its exit.

use std::path::PathBuf;
use std::process::Command as SysCommand;
use std::time::Duration as StdDuration;

use marion_core::agent_type::builtin;
use marion_core::contract::AgentId;
use marion_core::harness::Harness;
use marion_core::ids::{new_agent_id, uuid_v7};
use marion_core::paths::{AgentDir, ProjectDir};
use marion_harness::{
    Extras, Invocation, LaunchSpec, McpDeclaration, SpawnCtx, adapter_for, json_frames,
};
use serde_json::Value;

use crate::duplex::{self, DuplexError, DuplexSpec};
use crate::run::{entropy, run_bounded, unix_millis};
use crate::spawn::SpawnError;

/// The typed-control-plane protocol, which a root shares with a child (`crate::duplex`).
///
/// Re-exported rather than reimplemented: §6.1 step 8's gate is normative for **any** launcher
/// driving Claude Code headlessly, so one implementation serves `marion run` and `run_spawn` both.
/// `permission_round_trip.rs` drives these directly.
pub use crate::duplex::{
    can_use_tool_request, deny_response, initialize_request, is_control_response_to, user_message,
};

// The declaration's env-var names and the document that carries them are the Claude Code adapter's
// business (§3.1: config emission is part of the adapter contract), so they live in
// `marion-harness` and are re-exported here — `marion-supervisor mcp`, the other end of the
// handshake, reads them from this module.
pub use marion_harness::claude_code::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, DEPTH_ENV, McpEnv, READY_FILE_ENV, anthropic_base_url,
    mcp_config_json,
};

/// §3.1/§6.1 step 2: *"`max_depth` … counting the root as 0"*.
///
/// Stated once, here, because the root is the only node whose depth is not derived from another's:
/// every other node's is its caller's plus one (`run::run_spawn`). A literal `0` at the call site
/// would be a magic number that reads as "unknown" quite as easily as "the root".
pub const ROOT_DEPTH: u32 = 0;

/// The permission axis for an M1 root (§9).
///
/// `spawn` is the load-bearing one; without it the root's single call is denied. The other three
/// are the only descendant verbs an M1 root can reach — `spawn` blocks and backgrounding is M2+,
/// so its child is terminal by the time the root regains control and §5.4 denies `send`/`cancel`
/// against terminal targets. `report` is rejected on a root. Omitting a *reachable* verb would
/// deny calls that then block until the root's bound expires, which is why the list is stated in
/// full rather than trimmed to what the canned script happens to use.
pub const ROOT_ALLOWED_TOOLS: [&str; 4] = [
    "mcp__marion__spawn",
    "mcp__marion__status",
    "mcp__marion__wait",
    "mcp__marion__list",
];

/// Which of the two launch paths a root takes, derived from its adapter's `ExecutionSurfaces`.
///
/// **One derivation, two names.** A root and a child face the same question — is this node's prompt
/// a frame written after launch, or an argv element? — and it has the same answer, so the enum and
/// the function both live in [`crate::duplex`] and are re-exported here under the names this
/// module has always used. Two copies of §3.4's dispatch would be exactly the drift §9 warns about.
pub use crate::duplex::{LaunchPath as RootPath, launch_path as root_path};

/// What `marion run` was asked for.
#[derive(Debug, Clone)]
pub struct RootSpec {
    /// Agent type of the root itself, e.g. `claude`.
    pub agent_type: String,
    /// The root's single turn.
    ///
    /// Held here rather than passed to [`launch`] because on a `LaunchOnly` surface it is compiled
    /// **into argv** at `prepare` time. One field, so the prompt that was compiled and the prompt
    /// that is written can never be two different strings.
    pub prompt: String,
    /// Canonical repository root. The root's cwd, and the repo its children are worktrees of.
    pub repo: PathBuf,
    /// Resolved state directory (`<state>` of §4.3), *not* the per-project subdirectory.
    pub state: PathBuf,
    /// The CannedProvider's base URL, in the `…/v1` form a Codex `model_providers` entry takes.
    pub base_url: String,
    /// Path to the `marion-supervisor` binary the harness will start as the MCP server.
    pub bridge: PathBuf,
    pub model: Option<String>,
}

/// A prepared, not-yet-started root node.
#[derive(Debug, Clone)]
pub struct RootNode {
    pub agent_id: AgentId,
    pub agent_dir: AgentDir,
    /// The first configuration document the adapter emitted — the one carrying marion's MCP server
    /// declaration on every harness.
    pub mcp_config: PathBuf,
    /// The bridge's readiness marker. `None` on a `LaunchOnly` root: its prompt is already in argv,
    /// so there is no frame to withhold and nothing to gate (§6.1 step 8).
    pub ready_file: Option<PathBuf>,
    /// The per-run bearer token (§9). Never a real credential: the endpoint is the canned server.
    pub token: String,
    pub invocation: Invocation,
    /// Which harness this root is, for the error messages that must name a cause.
    pub harness: Harness,
    pub path: RootPath,
    pub prompt: String,
}

/// What the root's run produced.
#[derive(Debug, Default)]
pub struct RootOutcome {
    pub exit_code: Option<i32>,
    /// Every frame the root emitted, parsed. `stream-json` on the duplex path, the harness's own
    /// JSONL on the `LaunchOnly` one.
    pub transcript: Vec<Value>,
    pub stderr: String,
    /// `tool_name` of each permission request marion denied on expiry of the root's bound.
    pub denied_permissions: Vec<String>,
    /// marion's verbs the root's stream shows it calling — the post-hoc readiness evidence on a
    /// `LaunchOnly` root (§6.1 step 8). Empty on the duplex path, where readiness was gated
    /// *before* the turn instead and this would only restate it.
    pub marion_tool_calls: Vec<String>,
    /// marion's own wall-clock bound expired and the root's process group was killed.
    pub timed_out: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "the harness never fetched marion's tool list within {0:?}: no {1} appeared. \
         The root would have taken its first turn without mcp__marion__spawn, so the run is \
         refused rather than allowed to end in plain text with no error anywhere."
    )]
    McpNeverReady(StdDuration, PathBuf),
    #[error("the root exited before answering marion's initialize control request")]
    DiedBeforeInitialize,
    #[error("compiling the root's launch: {0}")]
    Harness(#[from] marion_harness::HarnessError),
    #[error("unknown agent type {0}")]
    UnknownAgentType(String),
    #[error(
        "{0} drives its node through a terminal, and marion MUST NOT give a headless node a pty \
         on stdin (§5.2). There is no root launch path for that surface, so the run is refused \
         rather than pushed down one that does not fit it."
    )]
    UnsupportedRootSurface(Harness),
    #[error("running the root: {0}")]
    Run(#[from] SpawnError),
    #[error("the adapter for {0} emitted no configuration document to declare marion's bridge in")]
    NoMcpDeclaration(Harness),
    /// §6.1 step 8's post-hoc assertion, failed. **The loud error that must exist**: the alternative
    /// is a run whose turn went out without marion's tools, ended as plain text, and exited 0 with
    /// nothing anywhere reporting it (§12).
    #[error(
        "{harness}: the root never reached marion's bridge — not one marion tool call appears in \
         the {frames} frame(s) it emitted, so its turn went out without marion's tools and it \
         cannot have delegated anything. Refused rather than reported as a success, which is what \
         a plain-text run that exits 0 is indistinguishable from. child exit {exit:?}{stderr}"
    )]
    BridgeNeverReached {
        harness: Harness,
        frames: usize,
        exit: Option<i32>,
        /// Pre-formatted, so the empty case adds no dangling label.
        stderr: String,
    },
}

/// Mint the root's identity, write its configuration, and compile its argv.
pub fn prepare(spec: &RootSpec) -> Result<RootNode, RootError> {
    let agent_id = new_agent_id(unix_millis(), entropy()?);
    let project = ProjectDir::new(&spec.state, &spec.repo);
    let agent_dir = project.agent(&agent_id);
    std::fs::create_dir_all(agent_dir.config_dir())?;

    // §6.1 step 5, through the seam, **dispatched on the root's own agent type** — the root used to
    // be `Harness::ClaudeCode` by constant, which is why `marion run codex` compiled a Claude Code
    // launch and then failed somewhere else entirely.
    let agent_type = builtin(&spec.agent_type)
        .ok_or_else(|| RootError::UnknownAgentType(spec.agent_type.clone()))?;
    let harness = agent_type.harness;
    let adapter = adapter_for(harness)?;
    let path = root_path(&adapter.surfaces()).ok_or(RootError::UnsupportedRootSurface(harness))?;

    // Not one of §4.3's normative files: this is marion's own start-up handshake with a process it
    // did not spawn, so it lives beside the node's state rather than in the layout. Only the duplex
    // path has a frame to withhold, so only it has a marker to wait on (§6.1 step 8).
    let ready_file = match path {
        RootPath::Duplex => {
            let f = agent_dir.path().join("mcp-ready");
            let _ = std::fs::remove_file(&f);
            Some(f)
        }
        RootPath::LaunchOnly => None,
    };

    let token = per_run_token()?;
    // The adapter decides argv, env, and which configuration files exist. marion writes what it is
    // handed and derives none of those paths itself — one derivation, so `--mcp-config` can never
    // name a document nobody wrote.
    let launch = LaunchSpec {
        cwd: spec.repo.clone(),
        model: spec.model.clone(),
        prompt: match path {
            // Written after launch, not compiled into argv (§6.1 step 8).
            RootPath::Duplex => String::new(),
            RootPath::LaunchOnly => spec.prompt.clone(),
        },
        allowed_tools: ROOT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect(),
        mcp: McpDeclaration::Marion,
        base_url: Some(spec.base_url.clone()),
        // On the duplex path the root's credential is the per-run `ANTHROPIC_AUTH_TOKEN` pushed
        // onto the invocation below. On the other three it is **not** an env var marion can push
        // after the fact — gemini wants `GEMINI_API_KEY`, opencode wants it *inside* the generated
        // config — so it goes through the neutral field and each adapter puts it where that harness
        // reads it.
        api_key: match path {
            RootPath::Duplex => None,
            RootPath::LaunchOnly => Some(token.clone()),
        },
        config_dir: agent_dir.config_dir(),
        extra: Extras::default(),
    };
    let ctx = SpawnCtx {
        agent_id: agent_id.clone(),
        // The canonical name, not `spec.agent_type`: `marion run codex` and `marion run codex-impl`
        // are one type, and the root's own bridge has to re-resolve exactly one of them.
        agent_type: agent_type.name.clone(),
        // **§3.1: the root is depth 0, by definition.** Its `spawn` therefore creates depth 1, and
        // this is the value that makes the whole chain measurable — without it every node in the
        // tree would look like a root to its own bridge.
        depth: ROOT_DEPTH,
        ready_file: ready_file.clone(),
        repo: spec.repo.clone(),
        state_dir: spec.state.clone(),
        bridge: spec.bridge.clone(),
        bridge_args: vec!["mcp".into()],
    };
    let mut written = Vec::new();
    for (path, contents) in adapter.config_files(&launch, &ctx)? {
        // The adapter decides *what and where*; marion writes. "Where" is not always directly under
        // `config_dir`: opencode's document lands at `<config_dir>/config/opencode/opencode.json`,
        // because `$XDG_CONFIG_HOME` is a directory whose layout the harness owns. Creating only
        // `config_dir` failed the whole launch with a bare `No such file or directory` naming
        // nothing.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
        written.push(path);
    }
    let mcp_config = written
        .first()
        .cloned()
        .ok_or(RootError::NoMcpDeclaration(harness))?;

    let mut invocation = adapter.compile(&launch, &ctx)?;
    if path == RootPath::Duplex {
        // §9: `ANTHROPIC_AUTH_TOKEN=<per-run token>` and `ANTHROPIC_API_KEY=""` — a non-empty key
        // silently wins (§6.4), so it is set to empty rather than left inherited.
        invocation
            .env
            .push(("ANTHROPIC_AUTH_TOKEN".into(), token.clone()));
        invocation
            .env
            .push(("ANTHROPIC_API_KEY".into(), String::new()));
    }

    Ok(RootNode {
        agent_id,
        agent_dir,
        mcp_config,
        ready_file,
        token,
        invocation,
        harness,
        path,
        prompt: spec.prompt.clone(),
    })
}

/// A token scoped to this run and nothing else. Not a credential — the endpoint is canned — but
/// distinct per run so a request log attributes traffic to one run.
fn per_run_token() -> Result<String, RootError> {
    Ok(format!("marion-run-{}", uuid_v7(unix_millis(), entropy()?)))
}

/// Start the root and run it to completion, on whichever path its **surfaces** select (§3.4).
///
/// `bound` is the root's node-level timeout (§9), and what it bounds is derived from the surface
/// exactly like everything else here:
///
/// - on [`RootPath::Duplex`] it is §9's **per-episode `Blocked`-only** budget, consumed only while
///   marion is holding an answer the root is waiting on. Deliberately not a wall-clock ceiling —
///   marion offers a root none — so that path has no deadline of its own;
/// - on [`RootPath::LaunchOnly`] there *is* no `Blocked` state to budget: the node has no
///   permission channel to block on and no descendant hold marion can observe, so a `Blocked`-only
///   budget would bound nothing at all. The same knob is therefore the **wall-clock** bound, and it
///   is not optional: measured in S13, **opencode never exits on a provider hang** — a 500 still
///   retrying at 90 s, a connection-refused still hung at 180 s, no backoff ceiling. Without a
///   bound `marion run opencode` is an unbounded hang.
pub fn launch(
    node: &RootNode,
    bound: StdDuration,
    mcp_ready_timeout: StdDuration,
) -> Result<RootOutcome, RootError> {
    match node.path {
        RootPath::Duplex => launch_duplex(node, bound, mcp_ready_timeout),
        RootPath::LaunchOnly => launch_only(node, bound),
    }
}

/// The `LaunchOnly` root: spawn with the prompt already in argv, read the stream, assert post hoc
/// that it reached marion's bridge (§6.1 step 8).
///
/// **The bounded run and the kill are [`run_bounded`]'s, not a second copy.** It already does every
/// part of what a root needs — `process_group(0)`, `stdin(Stdio::null())`, piped stdout/stderr with
/// bounded drains, and §9's two-step group kill whose ordering is load-bearing (enumerate the
/// descendants' distinct pgids *first*, because once the parent dies its descendants reparent to
/// pid 1 and no `ps` walk recovers them). A second implementation of that kill is exactly the drift
/// §9 warns about, so there is one.
///
/// stdin is `null` rather than a pty for two independent reasons: §5.2 says marion **MUST NOT**
/// give a headless node a pty on stdin, and on this surface there is nothing to write to it.
fn launch_only(node: &RootNode, bound: StdDuration) -> Result<RootOutcome, RootError> {
    let inv = &node.invocation;
    let out = run_bounded(
        SysCommand::new(&inv.program)
            .args(&inv.args)
            .envs(inv.env.iter().cloned())
            // Codex's generated `config.toml` names this as its provider `env_key`, and a provider
            // whose key is unset refuses to start. The per-run token rather than a constant, for
            // the same reason `ANTHROPIC_AUTH_TOKEN` carries it on the duplex path: it attributes a
            // request log to one run. It is not a credential — the endpoint is the canned server.
            .env("MARION_DUMMY_KEY", &node.token)
            .current_dir(&inv.cwd),
        bound,
    )?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let outcome = RootOutcome {
        exit_code: out.code,
        transcript: json_frames(&stdout),
        marion_tool_calls: adapter_for(node.harness)?.marion_tool_calls(&stdout),
        stderr,
        denied_permissions: vec![],
        timed_out: out.timed_out,
    };
    // Evaluated **after** the transcript is assembled and before anything is returned, so the
    // refusal is the run's outcome rather than a warning printed beside a success.
    //
    // A root marion killed on its own bound also reached no bridge, trivially — but "it never got
    // marion's tools" is the wrong diagnosis for it, and the expiry is the one marion observed. So
    // the expiry wins the report, exactly as §6.7's status derivation lets `TimedOut` outrank every
    // other claim about the same run.
    if !outcome.timed_out {
        assert_reached_the_bridge(node.harness, &outcome)?;
    }
    Ok(outcome)
}

/// §6.1 step 8's post-hoc readiness assertion, as a decision over what the run produced.
///
/// A `LaunchOnly` node's MCP readiness is not observable before its turn — the prompt is already in
/// argv — so the only honest check is afterwards: **did any marion verb appear in its stream?** If
/// none did, the node took its turn without marion's tools, and §6.1 is emphatic that such a run
/// **MUST NOT** be allowed to end as plain text: a toolless turn terminates `exit 0` with no
/// diagnostic anywhere, which is indistinguishable from success. §12 records the Claude Code
/// version of exactly this bug — first request toolless, a session title, exit 0 in 63 ms.
///
/// It is deliberately *not* keyed on `spawn` specifically. The question is whether the node reached
/// marion's bridge at all; which verb it reached for is the node's business, and a check that
/// demanded one would refuse a legitimate root that only listed its children.
fn assert_reached_the_bridge(harness: Harness, outcome: &RootOutcome) -> Result<(), RootError> {
    if !outcome.marion_tool_calls.is_empty() {
        return Ok(());
    }
    Err(RootError::BridgeNeverReached {
        harness,
        frames: outcome.transcript.len(),
        exit: outcome.exit_code,
        stderr: match outcome.stderr.trim() {
            "" => String::new(),
            s => format!("; stderr: {}", s.chars().take(512).collect::<String>()),
        },
    })
}

/// The duplex root: §6.1 step 8's gate, then the prompt — **driven by [`crate::duplex`], the one
/// implementation a root and a child share.**
///
/// What is left here is only what a root *is*: it has no `TaskContract`, so its result is its
/// stream and its exit (§9); and marion offers it no wall clock, so `wall_clock` is `None` and
/// `bound` is spent only on a `Blocked` episode.
fn launch_duplex(
    node: &RootNode,
    blocked_bound: StdDuration,
    mcp_ready_timeout: StdDuration,
) -> Result<RootOutcome, RootError> {
    let ready_file = node
        .ready_file
        .clone()
        .ok_or(RootError::UnsupportedRootSurface(node.harness))?;
    let inv = &node.invocation;
    let out = duplex::run_duplex(
        SysCommand::new(&inv.program)
            .args(&inv.args)
            .envs(inv.env.iter().cloned())
            .current_dir(&inv.cwd),
        &DuplexSpec {
            ready_file: &ready_file,
            prompt: &node.prompt,
            init_id: format!("marion-init-{}", node.agent_id.0),
            mcp_ready_timeout,
            blocked_bound,
            // §9: marion offers a root no wall-clock ceiling on this path, so there is none here.
            wall_clock: None,
        },
    )
    .map_err(|e| root_error(e, mcp_ready_timeout))?;
    Ok(RootOutcome {
        exit_code: out.exit_code,
        transcript: out.transcript,
        stderr: out.stderr,
        denied_permissions: out.denied_permissions,
        // Gated *before* the turn on this path, so restating it post hoc would add nothing.
        marion_tool_calls: vec![],
        timed_out: out.timed_out,
    })
}

/// The shared driver's refusals, in the root's own words. The cause is the same event; the sentence
/// names the node it happened to, which is what a reader of `marion run`'s stderr needs.
fn root_error(e: DuplexError, mcp_ready_timeout: StdDuration) -> RootError {
    match e {
        DuplexError::Io(e) => RootError::Io(e),
        DuplexError::McpNeverReady(_, path) => RootError::McpNeverReady(mcp_ready_timeout, path),
        DuplexError::DiedBeforeInitialize => RootError::DiedBeforeInitialize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use serde_json::json;

    fn env() -> McpEnv {
        McpEnv {
            bridge: "/bin/marion-supervisor".into(),
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: "http://127.0.0.1:8099/v1".into(),
            agent_id: AgentId("019f-root".into()),
            agent_type: "claude".into(),
            depth: ROOT_DEPTH,
            ready_file: "/state/x/mcp-ready".into(),
        }
    }

    #[test]
    fn claude_gets_the_base_url_without_the_v1_it_appends_itself() {
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099/v1"),
            "http://127.0.0.1:8099",
            "leaving it on yields a request to /v1/v1/messages"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099/v1/"),
            "http://127.0.0.1:8099"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099"),
            "http://127.0.0.1:8099"
        );
    }

    #[test]
    fn the_declaration_carries_the_root_agent_id_so_requester_is_not_a_placeholder() {
        let v = mcp_config_json(&env());
        assert_eq!(
            v["mcpServers"]["marion"]["env"][AGENT_ID_ENV], "019f-root",
            "§9: requester for a top-level spawn is the root's own AgentId"
        );
        assert_eq!(v["mcpServers"]["marion"]["args"][0], "mcp");
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_BASE_URL"], "http://127.0.0.1:8099/v1",
            "the child's model_providers entry wants the /v1 form"
        );
    }

    #[test]
    fn the_declaration_names_the_readiness_marker_the_prompt_waits_on() {
        let v = mcp_config_json(&env());
        assert_eq!(
            v["mcpServers"]["marion"]["env"][READY_FILE_ENV],
            "/state/x/mcp-ready"
        );
    }

    #[test]
    fn the_root_allowlist_is_every_verb_an_m1_root_can_reach() {
        // Omitting a reachable verb denies a call that then blocks until the root's bound expires.
        assert_eq!(
            ROOT_ALLOWED_TOOLS.to_vec(),
            vec![
                "mcp__marion__spawn",
                "mcp__marion__status",
                "mcp__marion__wait",
                "mcp__marion__list"
            ]
        );
        assert!(
            !ROOT_ALLOWED_TOOLS.contains(&"mcp__marion__report"),
            "report is rejected on a node without a contract, and a root has none"
        );
    }

    /// The gate itself is `duplex::wait_for_ready`, tested there. What is asserted here is the
    /// **root's own sentence** for it: the symptom is a run that "succeeds" with plain text, so the
    /// message a `marion run` operator reads has to name the tool that was missing.
    #[test]
    fn a_ready_marker_that_never_appears_is_a_refusal_not_a_silent_first_turn() {
        let missing = std::env::temp_dir().join("marion-never");
        let e = root_error(
            crate::duplex::DuplexError::McpNeverReady(StdDuration::from_millis(30), missing),
            StdDuration::from_millis(30),
        );
        assert!(matches!(e, RootError::McpNeverReady(_, _)), "{e}");
        assert!(e.to_string().contains("without mcp__marion__spawn"));
    }

    /// A pty-only surface has no root launch path, and §5.2 forbids inventing one by handing a
    /// headless node a pty on stdin. `TerminalInput` is unreachable from today's four adapters, so
    /// this is asserted at the derivation rather than through one.
    #[test]
    fn a_terminal_input_surface_is_refused_rather_than_pushed_down_one_of_the_two_paths() {
        let e = RootError::UnsupportedRootSurface(Harness::Codex);
        assert!(e.to_string().contains("pty on stdin"));
    }

    fn root_spec(dir: &Path, agent_type: &str) -> RootSpec {
        RootSpec {
            agent_type: agent_type.into(),
            prompt: "delegate it".into(),
            repo: dir.join("repo"),
            state: dir.join("state"),
            base_url: "http://127.0.0.1:8099/v1".into(),
            bridge: "/bin/marion-supervisor".into(),
            model: builtin(agent_type).unwrap().model.clone(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("marion-root-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("repo")).unwrap();
        p
    }

    /// **Every built-in prepares** — argv compiled, configuration on disk, prompt where its surface
    /// puts it. This is the whole of `marion run <agent-type>` short of the process itself.
    ///
    /// The opencode case is a regression that was real: its document lands at
    /// `<config_dir>/config/opencode/opencode.json`, a layout `$XDG_CONFIG_HOME` gives the harness
    /// and marion does not create, so writing it failed the launch with a bare
    /// `No such file or directory` naming neither the path nor the harness.
    #[test]
    fn every_builtin_agent_type_prepares_as_a_root_with_its_config_actually_on_disk() {
        let dir = temp("prepare");
        for name in ["claude", "codex", "codex-impl", "gemini", "opencode"] {
            let node = prepare(&root_spec(&dir, name))
                .unwrap_or_else(|e| panic!("{name} cannot be a root: {e}"));
            assert!(
                node.mcp_config.is_file(),
                "{name}: the MCP declaration marion compiled a path to must exist: {}",
                node.mcp_config.display()
            );
            let in_argv = node.invocation.args.iter().any(|a| a == "delegate it");
            match node.path {
                RootPath::LaunchOnly => {
                    assert!(
                        in_argv,
                        "{name}: a LaunchOnly prompt rides argv (§6.1 step 8)"
                    );
                    assert!(node.ready_file.is_none(), "{name}: nothing to gate");
                }
                RootPath::Duplex => {
                    assert!(
                        !in_argv,
                        "{name}: a duplex prompt is a frame, not an argument"
                    );
                    assert!(node.ready_file.is_some(), "{name}: the §6.1 step 8 marker");
                }
            }
            assert_eq!(node.prompt, "delegate it", "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The root is depth 0, on every harness, in the document its own bridge will read.**
    ///
    /// The adapters are tested for carrying whatever `SpawnCtx` hands them; this is the other end
    /// of that wire — that `marion run` hands them the right thing. It is the one place a literal
    /// could be wrong while every adapter test still passed, and getting it wrong is not a cosmetic
    /// error: a root declared at any other depth would mis-measure its whole subtree, and one
    /// declared at `max_depth` could not delegate at all.
    ///
    /// Read off the bytes on disk rather than off `ctx`, because the bytes are what the bridge gets.
    #[test]
    fn every_root_declares_itself_at_depth_zero_and_names_its_own_type() {
        let dir = temp("depth");
        for name in ["claude", "codex", "codex-impl", "gemini", "opencode"] {
            let node = prepare(&root_spec(&dir, name)).unwrap();
            let doc = std::fs::read_to_string(&node.mcp_config).unwrap();
            assert!(
                doc.contains("\"0\""),
                "{name}: a root is depth 0 (§3.1), and the value must be in the document its own \
                 bridge reads:\n{doc}"
            );
            assert!(
                doc.contains(DEPTH_ENV),
                "{name}: {DEPTH_ENV} is missing:\n{doc}"
            );
            // The canonical name, so `marion run codex` and `marion run codex-impl` hand the bridge
            // one spelling and it re-resolves one definition.
            assert!(
                doc.contains(&format!("\"{}\"", builtin(name).unwrap().name)),
                "{name}: its canonical agent type name must reach the bridge, or §6.1 step 2's \
                 gates have no max_depth to read:\n{doc}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §9: the root's credential never leaks past the harness that reads it. Claude Code takes it
    /// as `ANTHROPIC_AUTH_TOKEN` pushed onto the invocation; the other three take it through the
    /// adapter, which puts it where that harness looks — so pushing the Anthropic pair onto *them*
    /// would present the root's token to a harness that ignores it and, worse, would be a second
    /// place the same decision is made.
    #[test]
    fn only_the_duplex_root_carries_the_anthropic_env_pair() {
        let dir = temp("token");
        for name in ["claude", "codex", "gemini", "opencode"] {
            let node = prepare(&root_spec(&dir, name)).unwrap();
            let has = |k: &str| node.invocation.env.iter().any(|(n, _)| n == k);
            assert_eq!(
                has("ANTHROPIC_AUTH_TOKEN"),
                node.path == RootPath::Duplex,
                "{name}"
            );
            assert_eq!(
                has("ANTHROPIC_API_KEY"),
                node.path == RootPath::Duplex,
                "{name}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_agent_type_nobody_declared_is_refused_by_name_before_anything_is_written() {
        let dir = temp("unknown");
        let mut spec = root_spec(&dir, "claude");
        spec.agent_type = "not-a-harness".into();
        spec.model = None;
        assert!(matches!(
            prepare(&spec),
            Err(RootError::UnknownAgentType(t)) if t == "not-a-harness"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ran(calls: &[&str], frames: usize, exit: Option<i32>, stderr: &str) -> RootOutcome {
        RootOutcome {
            exit_code: exit,
            transcript: vec![json!({}); frames],
            stderr: stderr.to_string(),
            marion_tool_calls: calls.iter().map(|s| s.to_string()).collect(),
            ..RootOutcome::default()
        }
    }

    /// **§6.1 step 8's post-hoc assertion, and the failure it exists for.** A `LaunchOnly` root that
    /// never reached marion's bridge exited 0 having done nothing — the §12 shape — so it must be a
    /// refusal that *names the cause*, never a success.
    #[test]
    fn a_launch_only_root_that_never_reached_the_bridge_is_a_refusal_not_an_exit_zero() {
        let clean_looking = ran(&[], 3, Some(0), "");
        let err = assert_reached_the_bridge(Harness::Codex, &clean_looking)
            .expect_err("a run with no marion call must not be reported as a success");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "it must name the harness: {msg}");
        assert!(
            msg.contains("never reached marion's bridge"),
            "it must name the cause, not just fail: {msg}"
        );
        assert!(
            msg.contains("Some(0)"),
            "and it must say that the exit code looked clean, which is the whole trap: {msg}"
        );
        assert!(matches!(
            err,
            RootError::BridgeNeverReached {
                harness: Harness::Codex,
                frames: 3,
                exit: Some(0),
                ..
            }
        ));
    }

    /// The other half, so the check above cannot pass by always failing: one call to any marion verb
    /// is proof the node had marion's tools, whatever the run then did with them.
    #[test]
    fn one_marion_call_of_any_verb_satisfies_the_post_hoc_assertion() {
        for verb in ["spawn", "status", "wait", "list", "report"] {
            assert!(
                assert_reached_the_bridge(Harness::Gemini, &ran(&[verb], 1, Some(0), "")).is_ok(),
                "{verb}: the question is whether the bridge was reached, not which verb was used"
            );
        }
        // Even a run that then failed: reaching the bridge and succeeding are different facts, and
        // conflating them would relabel every genuine child failure as a launch failure.
        assert!(
            assert_reached_the_bridge(Harness::OpenCode, &ran(&["spawn"], 2, Some(1), "it broke"))
                .is_ok()
        );
    }

    /// The refusal carries the child's own words when it had any — S13 measured an opencode failure
    /// arriving with an **empty** stderr, so the label must not be printed when there is nothing
    /// behind it.
    #[test]
    fn the_refusal_quotes_stderr_only_when_there_is_some() {
        let with = assert_reached_the_bridge(
            Harness::OpenCode,
            &ran(&[], 0, None, "  no provider configured\n"),
        )
        .unwrap_err()
        .to_string();
        assert!(with.contains("stderr: no provider configured"), "{with}");
        let without = assert_reached_the_bridge(Harness::OpenCode, &ran(&[], 0, None, "  \n"))
            .unwrap_err()
            .to_string();
        assert!(!without.contains("stderr:"), "{without}");
    }
}
