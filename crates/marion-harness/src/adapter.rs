//! The per-harness adapter seam (design §5.2, §3.1, §6.1 step 5).
//!
//! > *"`compile()` lives on `HarnessAdapter`, not `ControlPlane`, because `opaque` has no
//! > `ControlPlane` yet still needs an `Invocation` for `DisplayPlane::spawn_pty` — and §6.1
//! > step 5 compiles on every spawn without exception."*
//!
//! The compile step **is** the adapter contract: an agent type compiles to argv + env + config +
//! MCP injection. Until this module existed the supervisor called the free functions of one
//! harness directly, so `AgentType.harness` was a `String` nothing read — spawning a `claude`
//! agent type wrote a Codex config, ran `codex`, and then recorded `"claude-code"` in the
//! auditable contract. This is the seam that makes dispatching on the harness possible; the
//! dispatch itself is the next phase's change, not this one's.

use std::path::PathBuf;

use marion_core::contract::AgentId;
use marion_core::harness::Harness;

use crate::claude_code::{self, HeadlessSpec, McpEnv, compile_headless};
use crate::codex::{self, ExecSpec, compile_exec};
use crate::gemini;
use crate::invocation::Invocation;
use crate::opencode;
use crate::surfaces::{ExecutionSurfaces, TypedKind};

/// Whether marion's control MCP is injected into this node, and how much of it.
///
/// Not a path and not a document: *which file, in what format, with which keys* is precisely what
/// differs per harness, so the neutral spec states only the intent and each adapter emits its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpDeclaration {
    /// marion's control MCP, served by the bridge named in [`SpawnCtx`].
    Marion,
    /// No MCP server at all — the §9 fallback branch, where the return channel is a document.
    None,
}

/// The harness-specific knobs that have no neutral meaning. Deliberately a named struct rather
/// than a map: every field here is read by exactly one adapter, and a map would let a typo pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extras {
    /// Codex `--output-schema`, the §9 fallback branch. S6 proved the primary branch, so M1
    /// leaves this unset.
    pub output_schema: Option<PathBuf>,
    /// Codex `--output-last-message`.
    pub output_last_message: Option<PathBuf>,
}

/// What the agent type asked for, in marion's vocabulary. Nothing here is harness-native.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// The node's cwd — a worktree, or an inherited directory (§6.1 step 4).
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// The compiled prompt (§3.1's wrapping rules). Empty for a surface whose prompt is written
    /// after launch rather than compiled into argv.
    pub prompt: String,
    /// The **permission** axis (§3.1): tool calls allowed without a prompt, in marion's names for
    /// its own verbs. Adapters translate; §3.1's two-axis table is why this is not `tools`.
    pub allowed_tools: Vec<String>,
    pub mcp: McpDeclaration,
    /// The provider base URL in the canonical **`…/v1`** form — the one a Codex
    /// `model_providers` entry names verbatim. Claude Code wants it without the `/v1` and the
    /// adapter derives that itself ([`claude_code::anthropic_base_url`]), so no caller has to know
    /// which harness wants which spelling.
    pub base_url: Option<String>,
    /// The provider credential, where the node is meant to present one.
    ///
    /// Neutral because two harnesses need it in incompatible places and neither can be patched up
    /// afterwards: gemini takes it as `GEMINI_API_KEY` in the child's env, while opencode wants it
    /// **inside the generated config** at `provider.<id>.options.apiKey` — so the root's
    /// post-`compile` push of `ANTHROPIC_AUTH_TOKEN` (`marion-supervisor::root`) is not a pattern
    /// that generalises. `None` on the canned-provider path, which authenticates nothing.
    pub api_key: Option<String>,
    /// The node's isolated harness config dir (§6.4): `$CODEX_HOME` for Codex, the directory
    /// marion's `--mcp-config` document is written into for Claude Code, the sandbox `HOME` and
    /// XDG root for opencode. Every path in [`HarnessAdapter::config_files`] is under it.
    pub config_dir: PathBuf,
    pub extra: Extras,
}

/// What marion knows about the node it is spawning, independent of what was asked for.
#[derive(Debug, Clone)]
pub struct SpawnCtx {
    pub agent_id: AgentId,
    /// The readiness marker the bridge touches once it has answered `tools/list` (§6.1 step 8).
    /// `None` for a surface whose prompt rides argv and so has no frame to withhold.
    pub ready_file: Option<PathBuf>,
    pub repo: PathBuf,
    pub state_dir: PathBuf,
    /// The `marion-supervisor` binary the harness will start as the MCP server.
    pub bridge: PathBuf,
    pub bridge_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HarnessError {
    /// **No [`Harness`] variant returns this today** — every harness marion can name now has an
    /// adapter. It stays because §3.1's enum is open to a fifth harness, and the alternative to a
    /// typed refusal is the fallback this seam exists to end: naming a harness and running another.
    /// Deleting it would mean whoever adds that harness has to re-derive the refusal.
    #[error("no adapter for harness {0} yet")]
    Unimplemented(Harness),
    #[error("{harness}: {what}")]
    MissingInput {
        harness: Harness,
        what: &'static str,
    },
}

/// One harness's translation of marion's vocabulary into that harness's own.
///
/// Object-safe by construction — the supervisor holds `Box<dyn HarnessAdapter + Send + Sync>`, and
/// both bounds are load-bearing for the same reason §5.2 gives for `ControlPlane`: the supervisor
/// services the bridge socket, the root's stdout demux and the child's JSONL stream concurrently,
/// so an adapter is shared across threads. The assertion below is checked by the compiler rather
/// than asserted in prose.
pub trait HarnessAdapter {
    /// Which harness this is. The registry's key, and what `TaskContract.child.harness` records.
    fn harness(&self) -> Harness;

    /// The surfaces this adapter drives (§3.4). Note the two implementations sit at *different*
    /// points of the cross-product, one of which is not a preset.
    fn surfaces(&self) -> ExecutionSurfaces;

    /// §6.1 step 5: argv + env. Runs on every spawn without exception, including surfaces that
    /// have no `ControlPlane` to open.
    fn compile(&self, spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<Invocation, HarnessError>;

    /// The configuration files this harness needs, as `(absolute path, contents)`. The caller
    /// writes them; the adapter decides what and where, because "what and where" is the part that
    /// differs per harness. Paths are always under `spec.config_dir`.
    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError>;

    /// This harness's spelling of one of marion's tools, for **compiling into a prompt**
    /// (§3.1 item 1).
    ///
    /// Flat on both harnesses. That reads like a contradiction of §3.1's *"Never compile the
    /// Claude Code spelling into a Codex child's prompt"*, and it is not — the same paragraph
    /// settles it, measured in S6: Codex 0.146.0 runs **code mode**, where a model reaches marion
    /// by writing `await tools.mcp__marion__report({…})`, the flat name **as a JavaScript
    /// identifier**. §3.1 is explicit: *"For a prompt, compile the flat `mcp__marion__report`
    /// identifier"*. The `{"name":"report","namespace":"mcp__marion"}` pair is codex's internal
    /// **wire** dispatch form, carried in `client_metadata.…code_mode_tool_names` and never
    /// something a child types. So the prohibition is about the wire layer: a *`function_call`
    /// item* naming the flat string is what 0.146.0 rejects as `unsupported call` (§11 item 12's
    /// correction), not the identifier in a prompt.
    fn marion_tool_name(&self, tool: &str) -> String;
}

/// Object safety and the thread bounds, checked by the compiler. §5.2 requires both and says so of
/// `ControlPlane` in as many words; the same reasoning reaches every trait the supervisor boxes.
const _: () = {
    const fn assert_boxable<T: ?Sized + Send + Sync>() {}
    assert_boxable::<dyn HarnessAdapter + Send + Sync>();
};

/// Claude Code 2.1.220, headless (§5.2, §9). marion's root.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeCodeAdapter;

impl ClaudeCodeAdapter {
    /// The document `--mcp-config` names. Derived, not passed in, so `compile` and `config_files`
    /// cannot disagree about where it is.
    fn mcp_config_path(spec: &LaunchSpec) -> PathBuf {
        spec.config_dir.join("mcp.json")
    }

    fn mcp_env(spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<McpEnv, HarnessError> {
        let ready_file = ctx.ready_file.clone().ok_or(HarnessError::MissingInput {
            harness: Harness::ClaudeCode,
            what: "a headless node's prompt is written after launch, so the bridge readiness \
                   marker is required: without it the first turn goes out with tools: [] and \
                   nothing anywhere reports an error",
        })?;
        Ok(McpEnv {
            bridge: ctx.bridge.clone(),
            repo: ctx.repo.clone(),
            state: ctx.state_dir.clone(),
            base_url: spec.base_url.clone().unwrap_or_default(),
            agent_id: ctx.agent_id.clone(),
            ready_file,
        })
    }
}

impl HarnessAdapter for ClaudeCodeAdapter {
    fn harness(&self) -> Harness {
        Harness::ClaudeCode
    }

    fn surfaces(&self) -> ExecutionSurfaces {
        ExecutionSurfaces::headless(TypedKind::StreamJson)
    }

    fn compile(&self, spec: &LaunchSpec, _ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        Ok(compile_headless(&HeadlessSpec {
            cwd: spec.cwd.clone(),
            model: spec.model.clone(),
            allowed_tools: spec.allowed_tools.clone(),
            mcp_config: Self::mcp_config_path(spec),
            base_url: spec
                .base_url
                .as_deref()
                .map(claude_code::anthropic_base_url),
        }))
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        if spec.mcp == McpDeclaration::None {
            return Ok(Vec::new());
        }
        let json = claude_code::mcp_config_json(&Self::mcp_env(spec, ctx)?);
        Ok(vec![(
            Self::mcp_config_path(spec),
            serde_json::to_string_pretty(&json).expect("a Value always serialises"),
        )])
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        format!("mcp__marion__{tool}")
    }
}

/// Codex 0.146.0, `exec` surface (§9). marion's M1 child.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

impl CodexAdapter {
    fn config_path(spec: &LaunchSpec) -> PathBuf {
        spec.config_dir.join("config.toml")
    }
}

impl HarnessAdapter for CodexAdapter {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    /// `LaunchOnly` + `ProtocolEvents` + no display — §3.4's combination outside the four presets.
    fn surfaces(&self) -> ExecutionSurfaces {
        ExecutionSurfaces::launch_only_with_protocol_events()
    }

    fn compile(&self, spec: &LaunchSpec, _ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        Ok(compile_exec(&ExecSpec {
            cwd: spec.cwd.clone(),
            codex_home: spec.config_dir.clone(),
            prompt: spec.prompt.clone(),
            output_schema: spec.extra.output_schema.clone(),
            output_last_message: spec.extra.output_last_message.clone(),
        }))
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        let base_url = spec.base_url.as_deref().ok_or(HarnessError::MissingInput {
            harness: Harness::Codex,
            what: "a model_providers entry needs a base_url; a config pointing nowhere fails as \
                   a hang, which is the worst failure to diagnose",
        })?;
        let args: Vec<&str> = ctx.bridge_args.iter().map(String::as_str).collect();
        // TODO(phase-3): the node's identity does not reach the Codex bridge. The Claude Code path
        // carries MARION_AGENT_ID and MARION_READY_FILE in the MCP JSON's per-server `env` block,
        // but `config_toml` takes only (bridge, bridge_args, base_url) and emits no `env` at all —
        // so a Codex child's bridge falls back to "unattributed-root" for `TaskContract.requester`
        // (see `marion-supervisor::main::agent_id`). Codex TOML does support
        // `env = { … }` inside `[mcp_servers.marion]`; `ctx.agent_id` is already threaded here and
        // is deliberately unused until that is closed, which is why this is a gap and not a bug in
        // this refactor: today the supervisor never gives a Codex child an MCP identity either.
        let _ = &ctx.agent_id;
        Ok(vec![(
            Self::config_path(spec),
            codex::config_toml(&ctx.bridge.to_string_lossy(), &args, base_url),
        )])
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        // Flat, exactly as on Claude Code — see the trait's doc comment. The namespaced form is
        // codex's internal wire dispatch shape, not something a child types into `tools.…`.
        format!("mcp__marion__{tool}")
    }
}

/// Gemini CLI 0.53.0, headless `-p` (§6.4, fixture `tests/fixtures/s12/`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiAdapter;

impl GeminiAdapter {
    /// The settings document `GEMINI_CLI_SYSTEM_SETTINGS_PATH` names. Derived here, so `compile`
    /// and `config_files` cannot disagree about where it is — the same reason the Claude Code
    /// adapter derives its `--mcp-config` path.
    ///
    /// It sits *beside* `$GEMINI_CLI_HOME`, not inside `<home>/.gemini/`: the whole point of the
    /// system-settings override is that marion writes nothing under the sandbox home the CLI owns.
    fn settings_path(spec: &LaunchSpec) -> PathBuf {
        spec.config_dir.join("marion-settings.json")
    }
}

impl HarnessAdapter for GeminiAdapter {
    fn harness(&self) -> Harness {
        Harness::Gemini
    }

    /// The same point of §3.4's cross-product as codex: the prompt rides argv and the only reading
    /// is its `stream-json` NDJSON.
    fn surfaces(&self) -> ExecutionSurfaces {
        ExecutionSurfaces::launch_only_with_protocol_events()
    }

    fn compile(&self, spec: &LaunchSpec, _ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        // Refused rather than defaulted. A pinned id would be a guess marion has no basis for, and
        // S12 measured 0.53.0 rewriting even an explicit `-m gemini-2.5-flash` to `gemini-3.5-flash`
        // in the request path — so a "safe" default is not even reliably the model that runs. The
        // failure it prevents is the expensive one: with model `auto` the CLI issues a classifier
        // call to gemini-3.1-flash-lite over non-streaming `:generateContent` and hung on retry 5.
        let model = spec.model.clone().ok_or(HarnessError::MissingInput {
            harness: Harness::Gemini,
            what: "an explicit -m is mandatory: with the default model `auto` the CLI first makes \
                   a classifier call that retried 5x and hung, and marion will not guess a model",
        })?;
        if let Some(u) = &spec.base_url
            && !gemini::base_url_is_acceptable(u)
        {
            return Err(HarnessError::MissingInput {
                harness: Harness::Gemini,
                what: "GOOGLE_GEMINI_BASE_URL must be https unless the host is loopback; a \
                       non-loopback plain-http endpoint is refused by the CLI",
            });
        }
        Ok(gemini::compile_prompt(&gemini::PromptSpec {
            cwd: spec.cwd.clone(),
            model,
            prompt: spec.prompt.clone(),
            cli_home: spec.config_dir.clone(),
            settings: Self::settings_path(spec),
            base_url: spec.base_url.clone(),
            api_key: spec.api_key.clone(),
        }))
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        // Unlike Claude Code, the file is written even with no MCP server: it also carries the
        // auth selection, without which the run dies with `Invalid auth method selected.`
        let bridge = (spec.mcp == McpDeclaration::Marion).then(|| gemini::BridgeEnv {
            bridge: ctx.bridge.clone(),
            args: ctx.bridge_args.clone(),
            repo: ctx.repo.clone(),
            state: ctx.state_dir.clone(),
            base_url: spec.base_url.clone().unwrap_or_default(),
            agent_id: ctx.agent_id.clone(),
            ready_file: ctx.ready_file.clone(),
        });
        let json = gemini::settings_json(bridge.as_ref());
        Ok(vec![(
            Self::settings_path(spec),
            serde_json::to_string_pretty(&json).expect("a Value always serialises"),
        )])
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        // `mcp_<server>_<tool>`, single underscores — **not** Claude Code's `mcp__marion__report`.
        // §3.1 makes the mapping part of the adapter contract precisely because it differs, and
        // S12 captured this spelling in a `tool_use` frame: `"tool_name":"mcp_marion_report"`.
        format!("mcp_{}_{tool}", gemini::MCP_ALIAS)
    }
}

/// opencode 1.17.3, `run` surface (§6.4, fixture `tests/fixtures/s13/`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCodeAdapter;

impl OpenCodeAdapter {
    /// The `provider/model` pair, which both argv and the generated config name. Parsed once so
    /// they cannot disagree.
    fn model_ref(spec: &LaunchSpec) -> Result<opencode::ModelRef, HarnessError> {
        let m = spec.model.as_deref().ok_or(HarnessError::MissingInput {
            harness: Harness::OpenCode,
            what: "an explicit -m provider/model is mandatory: there is no OPENCODE_MODEL env var, \
                   so argv and the generated config are the only two channels",
        })?;
        opencode::ModelRef::parse(m).ok_or(HarnessError::MissingInput {
            harness: Harness::OpenCode,
            what: "the model must be in `provider/model` form, which is the only spelling `-m` \
                   accepts and the one the generated provider block has to repeat",
        })
    }
}

impl HarnessAdapter for OpenCodeAdapter {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn surfaces(&self) -> ExecutionSurfaces {
        ExecutionSurfaces::launch_only_with_protocol_events()
    }

    fn compile(&self, spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        Ok(opencode::compile_run(&opencode::RunSpec {
            cwd: spec.cwd.clone(),
            sandbox: spec.config_dir.clone(),
            model: Self::model_ref(spec)?,
            // Any stable string suppresses the title-generation call; the node's own id makes the
            // session identifiable in `opencode session list` without leaking the prompt.
            title: format!("marion-{}", ctx.agent_id.0),
            prompt: spec.prompt.clone(),
        }))
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        let base_url = spec.base_url.as_deref().ok_or(HarnessError::MissingInput {
            harness: Harness::OpenCode,
            what: "the provider block needs a baseURL; without one the child resolves no provider \
                   at all, and a provider that answers nothing is an unbounded hang (S13)",
        })?;
        let bridge = (spec.mcp == McpDeclaration::Marion).then(|| opencode::BridgeEnv {
            bridge: ctx.bridge.clone(),
            args: ctx.bridge_args.clone(),
            repo: ctx.repo.clone(),
            state: ctx.state_dir.clone(),
            base_url: base_url.to_string(),
            agent_id: ctx.agent_id.clone(),
            ready_file: ctx.ready_file.clone(),
        });
        let json = opencode::config_json(
            &opencode::ConfigSpec {
                model: Self::model_ref(spec)?,
                base_url: base_url.to_string(),
                api_key: spec.api_key.clone(),
            },
            bridge.as_ref(),
        );
        Ok(vec![(
            opencode::config_path(&spec.config_dir),
            serde_json::to_string_pretty(&json).expect("a Value always serialises"),
        )])
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        // `<serverName>_<toolName>` — a third spelling again (S13, verified live). The JSON-RPC
        // `tools/call` opencode then makes to the bridge carries the **unprefixed** `report`: that
        // is the MCP wire layer, not the model-facing name, and conflating the two would put the
        // wrong identifier into a compiled prompt.
        format!("{}_{tool}", opencode::MCP_ALIAS)
    }
}

/// The adapter registry: the one place a [`Harness`] becomes behaviour.
///
/// Naming a harness in §3.1's enum and having an adapter for it are different things, and the
/// difference is a typed error rather than a panic or a silent fallback — a fallback here would
/// reintroduce exactly the bug this seam exists to end, where a `claude` agent type quietly ran
/// `codex`.
pub fn adapter_for(h: Harness) -> Result<Box<dyn HarnessAdapter + Send + Sync>, HarnessError> {
    match h {
        Harness::ClaudeCode => Ok(Box::new(ClaudeCodeAdapter)),
        Harness::Codex => Ok(Box::new(CodexAdapter)),
        Harness::Gemini => Ok(Box::new(GeminiAdapter)),
        Harness::OpenCode => Ok(Box::new(OpenCodeAdapter)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::config_toml;
    use crate::surfaces::{ControlTransport, DisplaySurface};

    fn ctx() -> SpawnCtx {
        SpawnCtx {
            agent_id: AgentId("019f-root".into()),
            ready_file: Some("/state/x/mcp-ready".into()),
            repo: "/repo".into(),
            state_dir: "/state".into(),
            bridge: "/bin/marion-supervisor".into(),
            bridge_args: vec!["mcp".into()],
        }
    }

    fn claude_spec() -> LaunchSpec {
        LaunchSpec {
            cwd: "/repo".into(),
            model: Some("haiku".into()),
            prompt: String::new(),
            allowed_tools: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: None,
            config_dir: "/state/x/config".into(),
            extra: Extras::default(),
        }
    }

    fn codex_spec() -> LaunchSpec {
        LaunchSpec {
            cwd: "/wt".into(),
            model: None,
            prompt: "do the task".into(),
            allowed_tools: vec![],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: None,
            config_dir: "/state/x/config".into(),
            extra: Extras::default(),
        }
    }

    fn gemini_spec() -> LaunchSpec {
        LaunchSpec {
            model: Some("gemini-2.5-flash".into()),
            api_key: Some("sk-fake".into()),
            ..codex_spec()
        }
    }

    fn opencode_spec() -> LaunchSpec {
        LaunchSpec {
            model: Some("canned/canned-1".into()),
            api_key: Some("sk-fake".into()),
            ..codex_spec()
        }
    }

    /// The refactor's whole claim, stated as a test: routing through the adapter changes nothing
    /// about what gets spawned. If this drifts, the "pure refactor" claim is false.
    #[test]
    fn the_claude_adapter_compiles_exactly_what_the_free_function_did() {
        let spec = claude_spec();
        let via_adapter = ClaudeCodeAdapter.compile(&spec, &ctx()).unwrap();
        let via_free_function = compile_headless(&HeadlessSpec {
            cwd: "/repo".into(),
            model: Some("haiku".into()),
            allowed_tools: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
            mcp_config: "/state/x/config/mcp.json".into(),
            // The supervisor used to derive this itself; the adapter now does.
            base_url: Some("http://127.0.0.1:8099".into()),
        });
        assert_eq!(via_adapter, via_free_function);
    }

    #[test]
    fn the_codex_adapter_compiles_exactly_what_the_free_function_did() {
        let via_adapter = CodexAdapter.compile(&codex_spec(), &ctx()).unwrap();
        let via_free_function = compile_exec(&ExecSpec {
            cwd: "/wt".into(),
            codex_home: "/state/x/config".into(),
            prompt: "do the task".into(),
            output_schema: None,
            output_last_message: None,
        });
        assert_eq!(via_adapter, via_free_function);
    }

    #[test]
    fn the_claude_adapter_emits_byte_identical_mcp_json() {
        let spec = claude_spec();
        let files = ClaudeCodeAdapter.config_files(&spec, &ctx()).unwrap();
        let expected = serde_json::to_string_pretty(&claude_code::mcp_config_json(&McpEnv {
            bridge: "/bin/marion-supervisor".into(),
            repo: "/repo".into(),
            state: "/state".into(),
            // The `/v1` form, which is what the bridge hands a Codex grandchild.
            base_url: "http://127.0.0.1:8099/v1".into(),
            agent_id: AgentId("019f-root".into()),
            ready_file: "/state/x/mcp-ready".into(),
        }))
        .unwrap();
        assert_eq!(
            files,
            vec![(PathBuf::from("/state/x/config/mcp.json"), expected)]
        );
    }

    #[test]
    fn the_codex_adapter_emits_byte_identical_config_toml() {
        let files = CodexAdapter.config_files(&codex_spec(), &ctx()).unwrap();
        let expected = config_toml(
            "/bin/marion-supervisor",
            &["mcp"],
            "http://127.0.0.1:8099/v1",
        );
        assert_eq!(
            files,
            vec![(PathBuf::from("/state/x/config/config.toml"), expected)]
        );
    }

    /// `compile` names the same file `config_files` writes. Two derivations of one path would let
    /// `--mcp-config` point at a document nobody wrote — a silent `tools: []` first turn.
    #[test]
    fn the_mcp_config_argv_path_is_the_path_that_is_written() {
        let spec = claude_spec();
        let inv = ClaudeCodeAdapter.compile(&spec, &ctx()).unwrap();
        let i = inv.args.iter().position(|a| a == "--mcp-config").unwrap();
        let written = ClaudeCodeAdapter.config_files(&spec, &ctx()).unwrap();
        assert_eq!(PathBuf::from(&inv.args[i + 1]), written[0].0);
    }

    #[test]
    fn a_headless_node_without_a_readiness_marker_is_refused_not_silently_launched() {
        let mut c = ctx();
        c.ready_file = None;
        let e = ClaudeCodeAdapter
            .config_files(&claude_spec(), &c)
            .unwrap_err();
        assert!(matches!(
            e,
            HarnessError::MissingInput {
                harness: Harness::ClaudeCode,
                ..
            }
        ));
    }

    #[test]
    fn the_gemini_adapter_compiles_the_measured_invocation() {
        let inv = GeminiAdapter.compile(&gemini_spec(), &ctx()).unwrap();
        assert_eq!(inv.program, "gemini");
        assert_eq!(
            inv.args,
            vec![
                "-m",
                "gemini-2.5-flash",
                "--output-format",
                "stream-json",
                "-p",
                "do the task"
            ]
        );
        assert_eq!(
            inv.env,
            vec![
                ("GEMINI_CLI_HOME".to_string(), "/state/x/config".to_string()),
                (
                    "GEMINI_CLI_SYSTEM_SETTINGS_PATH".to_string(),
                    "/state/x/config/marion-settings.json".to_string()
                ),
                ("GEMINI_CLI_TRUST_WORKSPACE".to_string(), "true".to_string()),
                ("GEMINI_FORCE_FILE_STORAGE".to_string(), "true".to_string()),
                (
                    "GOOGLE_GEMINI_BASE_URL".to_string(),
                    "http://127.0.0.1:8099".to_string()
                ),
                ("GEMINI_API_KEY".to_string(), "sk-fake".to_string()),
            ]
        );
    }

    /// The settings file `compile` names is the one `config_files` writes — the gemini analogue of
    /// the `--mcp-config` invariant, and with the same failure mode if the two ever diverged: a
    /// highest-precedence settings layer pointing at a document nobody wrote, hence no MCP server,
    /// hence tools that are silently absent.
    #[test]
    fn the_settings_argv_path_is_the_path_that_is_written() {
        let spec = gemini_spec();
        let inv = GeminiAdapter.compile(&spec, &ctx()).unwrap();
        let (_, named) = inv
            .env
            .iter()
            .find(|(k, _)| k == "GEMINI_CLI_SYSTEM_SETTINGS_PATH")
            .unwrap();
        let written = GeminiAdapter.config_files(&spec, &ctx()).unwrap();
        assert_eq!(PathBuf::from(named), written[0].0);
    }

    /// §6.4/S12's MUST, asserted on the bytes the adapter actually emits: a "simplification" that
    /// dropped `trust` would pass every other test in this file and produce runs that exit 0
    /// having called nothing.
    #[test]
    fn the_gemini_settings_the_adapter_emits_declare_a_trusted_server() {
        let files = GeminiAdapter.config_files(&gemini_spec(), &ctx()).unwrap();
        assert_eq!(files.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(v["mcpServers"]["marion"]["trust"], serde_json::json!(true));
        assert_eq!(
            v["security"]["auth"]["selectedType"],
            serde_json::json!("gemini-api-key")
        );
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_AGENT_ID"],
            serde_json::json!("019f-root")
        );
    }

    /// The node identity keys are the **bridge's** contract, so gemini's `env` block and Claude
    /// Code's must carry the same names. A divergence would leave a gemini child's contract
    /// stamped `unattributed-root` with nothing failing.
    #[test]
    fn every_adapters_bridge_env_block_uses_one_set_of_key_names() {
        let claude = claude_code::mcp_config_json(&McpEnv {
            bridge: "/bin/marion-supervisor".into(),
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: "http://127.0.0.1:8099/v1".into(),
            agent_id: AgentId("019f-root".into()),
            ready_file: "/state/x/mcp-ready".into(),
        });
        let expected: Vec<&String> = claude["mcpServers"]["marion"]["env"]
            .as_object()
            .unwrap()
            .keys()
            .collect();

        let g = gemini::settings_json(Some(&gemini::BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: "http://127.0.0.1:8099/v1".into(),
            agent_id: AgentId("019f-root".into()),
            ready_file: Some("/state/x/mcp-ready".into()),
        }));
        let o = opencode::config_json(
            &opencode::ConfigSpec {
                model: opencode::ModelRef::parse("canned/canned-1").unwrap(),
                base_url: "http://127.0.0.1:8099/v1".into(),
                api_key: None,
            },
            Some(&opencode::BridgeEnv {
                bridge: "/bin/marion-supervisor".into(),
                args: vec!["mcp".into()],
                repo: "/repo".into(),
                state: "/state".into(),
                base_url: "http://127.0.0.1:8099/v1".into(),
                agent_id: AgentId("019f-root".into()),
                ready_file: Some("/state/x/mcp-ready".into()),
            }),
        );
        for block in [
            &g["mcpServers"]["marion"]["env"],
            &o["mcp"]["marion"]["environment"],
        ] {
            let got: Vec<&String> = block.as_object().unwrap().keys().collect();
            assert_eq!(got, expected, "the bridge reads one set of names");
        }
    }

    #[test]
    fn a_gemini_node_without_a_model_is_refused_rather_than_launched_on_auto() {
        let mut spec = gemini_spec();
        spec.model = None;
        let e = GeminiAdapter.compile(&spec, &ctx()).unwrap_err();
        assert!(matches!(
            e,
            HarnessError::MissingInput {
                harness: Harness::Gemini,
                ..
            }
        ));
    }

    #[test]
    fn a_non_loopback_plain_http_base_url_is_refused_at_compile_time() {
        let mut spec = gemini_spec();
        spec.base_url = Some("http://example.com/v1".into());
        assert!(GeminiAdapter.compile(&spec, &ctx()).is_err());
        spec.base_url = Some("https://example.com/v1".into());
        assert!(GeminiAdapter.compile(&spec, &ctx()).is_ok());
    }

    #[test]
    fn the_opencode_adapter_compiles_the_measured_invocation() {
        let inv = OpenCodeAdapter.compile(&opencode_spec(), &ctx()).unwrap();
        assert_eq!(inv.program, "opencode");
        assert_eq!(
            inv.args,
            vec![
                "run",
                "--pure",
                "--format",
                "json",
                "--title",
                "marion-019f-root",
                "-m",
                "canned/canned-1",
                "do the task"
            ]
        );
        let names: Vec<&str> = inv.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "XDG_STATE_HOME",
                "OPENCODE_DISABLE_CLAUDE_CODE",
                "OPENCODE_DISABLE_EXTERNAL_SKILLS",
                "OPENCODE_DISABLE_PROJECT_CONFIG",
                "OPENCODE_DISABLE_MODELS_FETCH",
                "OPENCODE_DISABLE_LSP_DOWNLOAD",
                "OPENCODE_DISABLE_AUTOUPDATE",
                "OPENCODE_DISABLE_SHARE",
                "OPENCODE_DB",
            ]
        );
    }

    #[test]
    fn the_opencode_config_lands_under_the_isolated_xdg_config_root() {
        let spec = opencode_spec();
        let files = OpenCodeAdapter.config_files(&spec, &ctx()).unwrap();
        assert_eq!(
            files[0].0,
            PathBuf::from("/state/x/config/config/opencode/opencode.json")
        );
        let inv = OpenCodeAdapter.compile(&spec, &ctx()).unwrap();
        let (_, xdg) = inv
            .env
            .iter()
            .find(|(k, _)| k == "XDG_CONFIG_HOME")
            .unwrap();
        assert!(
            files[0].0.starts_with(xdg),
            "a config outside XDG_CONFIG_HOME is never read"
        );
        let v: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(v["mcp"]["marion"]["type"], serde_json::json!("local"));
        assert_eq!(
            v["mcp"]["marion"]["command"],
            serde_json::json!(["/bin/marion-supervisor", "mcp"])
        );
        assert_eq!(v["model"], serde_json::json!("canned/canned-1"));
    }

    #[test]
    fn an_opencode_node_needs_a_provider_slash_model_and_a_base_url() {
        let mut spec = opencode_spec();
        spec.model = Some("canned-1".into());
        assert!(OpenCodeAdapter.compile(&spec, &ctx()).is_err());
        spec.model = None;
        assert!(OpenCodeAdapter.compile(&spec, &ctx()).is_err());

        let mut spec = opencode_spec();
        spec.base_url = None;
        assert!(matches!(
            OpenCodeAdapter.config_files(&spec, &ctx()).unwrap_err(),
            HarnessError::MissingInput {
                harness: Harness::OpenCode,
                ..
            }
        ));
    }

    #[test]
    fn the_two_adapters_sit_at_different_points_of_the_cross_product() {
        let claude = ClaudeCodeAdapter.surfaces();
        let codex = CodexAdapter.surfaces();
        assert_ne!(claude, codex);
        assert!(claude.has_typed_control_plane(), "stream-json is typed");
        assert_eq!(codex.control, ControlTransport::LaunchOnly);
        assert_eq!(codex.display, DisplaySurface::None);
        assert!(!claude.has_display_plane(), "headless runs over pipes");
    }

    /// §3.1: the prompt spelling is flat on both. The namespaced `{name, namespace}` pair is
    /// codex's internal wire form and never appears in a compiled prompt.
    #[test]
    fn both_harnesses_take_the_flat_tool_name_in_a_prompt() {
        assert_eq!(
            ClaudeCodeAdapter.marion_tool_name("report"),
            "mcp__marion__report"
        );
        assert_eq!(
            CodexAdapter.marion_tool_name("report"),
            "mcp__marion__report",
            "S6: under code mode a model writes await tools.mcp__marion__report({{…}})"
        );
    }

    /// §3.1 makes the mapping part of the adapter contract because the harnesses genuinely
    /// disagree. Three spellings, measured: Claude Code's double-underscore form (and codex's, for
    /// the reason above), gemini's `mcp_<server>_<tool>` (S12, captured in a `tool_use` frame) and
    /// opencode's `<server>_<tool>` (S13, verified live). Compiling one harness's spelling into
    /// another's prompt names a tool that does not exist there.
    #[test]
    fn the_harnesses_disagree_about_the_tool_name_and_the_adapters_say_so() {
        let names: Vec<String> = [
            Harness::ClaudeCode,
            Harness::Codex,
            Harness::Gemini,
            Harness::OpenCode,
        ]
        .into_iter()
        .map(|h| adapter_for(h).unwrap().marion_tool_name("report"))
        .collect();
        assert_eq!(
            names,
            vec![
                "mcp__marion__report",
                "mcp__marion__report",
                "mcp_marion_report",
                "marion_report",
            ]
        );
        // The opencode spelling is the **model-facing** one. The JSON-RPC `tools/call` that
        // opencode then sends to the bridge carries the unprefixed `report`; that is the MCP layer.
        assert_ne!(
            OpenCodeAdapter.marion_tool_name("report"),
            "report",
            "the model-facing name is prefixed even though the wire call is not"
        );
    }

    /// Now the stronger claim: `Harness::ALL` is *exhaustively* covered, and each adapter is the
    /// one it says it is. This replaces the old "Gemini and OpenCode have no adapter" assertion —
    /// they do now — and a future fifth harness fails here until it has one.
    #[test]
    fn every_named_harness_resolves_to_an_adapter_that_says_it_is_that_harness() {
        for h in Harness::ALL {
            assert_eq!(
                adapter_for(h)
                    .unwrap_or_else(|e| panic!("{h}: {e}"))
                    .harness(),
                h
            );
        }
    }

    /// The refusal itself, exercised at the type rather than through the registry: it names the
    /// harness rather than falling back onto whichever adapter happens to exist. No `Harness`
    /// produces it today, and it must stay correct for the one that eventually does.
    #[test]
    fn an_unimplemented_harness_is_a_refusal_that_names_it_not_a_fallback() {
        for h in Harness::ALL {
            let e = HarnessError::Unimplemented(h);
            assert!(e.to_string().contains(h.as_str()), "{h}: {e}");
            assert_eq!(
                e,
                HarnessError::Unimplemented(h),
                "the refusal carries the harness that was asked for, not a placeholder"
            );
        }
    }

    #[test]
    fn an_adapter_survives_being_boxed_and_shared_across_threads() {
        // The bounds §5.2 calls load-bearing, exercised rather than asserted.
        let adapters: Vec<Box<dyn HarnessAdapter + Send + Sync>> = Harness::ALL
            .into_iter()
            .map(|h| adapter_for(h).unwrap())
            .collect();
        let names: Vec<Harness> = std::thread::scope(|s| {
            s.spawn(|| adapters.iter().map(|a| a.harness()).collect())
                .join()
                .unwrap()
        });
        assert_eq!(names, Harness::ALL.to_vec());
    }
}
