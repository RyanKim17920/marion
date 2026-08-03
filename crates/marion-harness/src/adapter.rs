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
use crate::stream::{ChildExit, StreamOutcome};
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

    /// Read this harness's own output stream (§6.1 step 9).
    ///
    /// Behind the seam for the same reason `compile` is: the four harnesses emit four different
    /// event vocabularies, and until this method existed the supervisor read every child as codex
    /// JSONL — so a gemini child's report was simply invisible, and its contract said `Unreported`
    /// about a run that had reported. That is the §12 silent-failure shape again, not a missing
    /// feature.
    ///
    /// `exit` is passed rather than consulted by the caller so each harness can state its own
    /// success rule. It is genuinely per-harness: gemini documents 0/1/42/53 **and** returns 0 with
    /// a JSON error body on an auth failure (S12), while opencode returns 1 with an empty stderr
    /// and its only description in-stream (S13). Nothing here is obliged to *use* it — codex does
    /// not — but nothing else is in a position to decide.
    ///
    /// A pure function of bytes: it must never block, and in particular must never wait for a
    /// terminal frame, because opencode emits none at all (`tests/fixtures/s13/`).
    fn parse_stream(&self, stdout: &str, exit: ChildExit) -> StreamOutcome;

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

    /// Which of marion's own verbs this harness's stream shows the node calling, in **marion's**
    /// vocabulary (`spawn`, `report`, …) rather than in the harness's spelling.
    ///
    /// This is §6.1 step 8's *post-hoc* readiness assertion. A surface whose prompt rides argv has
    /// no frame to withhold, so its MCP readiness cannot be gated before the turn; §6.1 says it is
    /// *"asserted post hoc from the `mcp_tool_call` items in its JSONL stream"* instead. An empty
    /// result therefore means the node never reached marion's bridge, which is the §12 failure the
    /// gate exists for: a run that ends as plain text, exit 0, with no error anywhere.
    ///
    /// It cannot be one function in the supervisor. The four harnesses put the tool's name in four
    /// different places — a `tool_use` block's `name`, an `mcp_tool_call` item's `server`/`tool`
    /// **pair**, a top-level `tool_name`, a `part.tool` — and in three different spellings, so a
    /// substring scan for any one of them is wrong on the other three (codex most of all, whose
    /// stream never contains the string `mcp__marion__` at all).
    ///
    /// Every call is counted, whatever came of it: a call the bridge refused still proves the node
    /// had marion's tools, which is the only thing being asserted.
    fn marion_tool_calls(&self, stdout: &str) -> Vec<String>;
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

    /// Whether this launch writes its prompt after the process is up.
    ///
    /// The neutral vocabulary already carries the distinction and says so:
    /// [`LaunchSpec::prompt`] is *"empty for a surface whose prompt is written after launch rather
    /// than compiled into argv"*. So this reads the spec rather than adding a mode flag beside it —
    /// two ways to say the same thing could disagree.
    fn prompt_is_written_after_launch(spec: &LaunchSpec) -> bool {
        spec.prompt.is_empty()
    }

    fn mcp_env(spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<McpEnv, HarnessError> {
        // Required on **every** Claude Code node, root or child. Its prompt is always written after
        // launch — 2.1.220 does not hold turn one for an `--mcp-config` server — so there is always
        // a frame to withhold and always something for the marker to gate (§6.1 step 8). A node
        // launched without one takes its first turn with `tools: []` and nothing anywhere reports
        // an error, which is why the absence is a refusal rather than a fallback path.
        let ready_file = ctx.ready_file.clone().ok_or(HarnessError::MissingInput {
            harness: Harness::ClaudeCode,
            what: "a headless node's prompt is written after launch, so the bridge \
                       readiness marker is required: without it the first turn goes out with \
                       tools: [] and nothing anywhere reports an error",
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

    /// **One shape, and a non-empty prompt is a refusal.**
    ///
    /// This adapter used to compile two: `--input-format stream-json` for a root whose prompt is a
    /// frame, and a positional `-p <prompt>` for a child. The second shape does not work and cannot
    /// be made to — measured on 2.1.220, a child launched that way reports
    /// `"tools":[],"mcp_servers":[{"name":"marion","status":"pending"}]` in its own `system/init`,
    /// takes turn one without marion's tools, is answered with the session-title stub, and exits
    /// **0 having called nothing**. There is no flag that makes the CLI wait; `MCP_TIMEOUT` does
    /// not change it. §6.1 step 8's remedy is the only one, and it *requires* a typed stdin, which
    /// is exactly what this adapter's [`Self::surfaces`] declares.
    ///
    /// So the argv branch is gone and a prompt that arrives here is a **typed refusal naming the
    /// cause**, never a launch that quietly loses its tools. The signal is the neutral vocabulary's
    /// own — [`LaunchSpec::prompt`] is *"empty for a surface whose prompt is written after launch"*
    /// — rather than a second mode flag beside it.
    fn compile(&self, spec: &LaunchSpec, _ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        if !Self::prompt_is_written_after_launch(spec) {
            return Err(HarnessError::MissingInput {
                harness: Harness::ClaudeCode,
                what: "this harness's prompt is written after launch as a user frame, never \
                       compiled into argv: 2.1.220 does not hold turn one for an --mcp-config \
                       server, so an argv prompt takes that turn with tools: [] and the run exits \
                       0 having called nothing. Leave LaunchSpec.prompt empty and write the frame \
                       after §6.1 step 8's readiness gate",
            });
        }
        Ok(compile_headless(&HeadlessSpec {
            cwd: spec.cwd.clone(),
            model: spec.model.clone(),
            allowed_tools: spec.allowed_tools.clone(),
            mcp_config: Self::mcp_config_path(spec),
            base_url: spec
                .base_url
                .as_deref()
                .map(claude_code::anthropic_base_url),
            api_key: spec.api_key.clone(),
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

    /// Failure comes off the run's own `result` frame; the exit code is not consulted, because on
    /// this surface a non-zero exit is already the supervisor's to record and 2.1.220 reports its
    /// own errors in-band as `is_error`.
    fn parse_stream(&self, stdout: &str, _exit: ChildExit) -> StreamOutcome {
        claude_code::parse_stream(stdout, &self.marion_tool_name("report"))
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        format!("mcp__marion__{tool}")
    }

    /// The prefix is derived from this adapter's own `marion_tool_name`, so the reader and the
    /// compiler of the name can never disagree about the spelling.
    fn marion_tool_calls(&self, stdout: &str) -> Vec<String> {
        claude_code::marion_tool_calls(stdout, &self.marion_tool_name(""))
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

    /// The pre-seam behaviour, unchanged: `report` off an `mcp_tool_call` item, `file_change`
    /// items as corroboration, and **no failure claim of its own** — codex's status has always come
    /// from the narrative and the exit code, so `exit` is deliberately unused here.
    fn parse_stream(&self, stdout: &str, _exit: ChildExit) -> StreamOutcome {
        codex::parse_stream(stdout)
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        // Flat, exactly as on Claude Code — see the trait's doc comment. The namespaced form is
        // codex's internal wire dispatch shape, not something a child types into `tools.…`.
        format!("mcp__marion__{tool}")
    }

    /// **No prefix.** codex's stream names the server and the tool as two fields, so the flat
    /// identifier above never appears in it — see [`codex::marion_tool_calls`].
    fn marion_tool_calls(&self, stdout: &str) -> Vec<String> {
        codex::marion_tool_calls(stdout)
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

    /// **The exit code is not trusted on its own here, and that is measured.** S12 recorded an auth
    /// failure exiting **0** with a JSON error body, so the stream's own error and `result` frames
    /// decide, and a non-zero exit that says nothing in-stream is left to the supervisor's existing
    /// exit-code rule. A documented code that arrives *with* a failure body is therefore recorded
    /// once, with the harness's own words, rather than twice.
    fn parse_stream(&self, stdout: &str, _exit: ChildExit) -> StreamOutcome {
        gemini::parse_stream(stdout, &self.marion_tool_name("report"))
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        // `mcp_<server>_<tool>`, single underscores — **not** Claude Code's `mcp__marion__report`.
        // §3.1 makes the mapping part of the adapter contract precisely because it differs, and
        // S12 captured this spelling in a `tool_use` frame: `"tool_name":"mcp_marion_report"`.
        format!("mcp_{}_{tool}", gemini::MCP_ALIAS)
    }

    fn marion_tool_calls(&self, stdout: &str) -> Vec<String> {
        gemini::marion_tool_calls(stdout, &self.marion_tool_name(""))
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

    /// **No terminal frame exists to wait for** (S13, `tests/fixtures/s13/`): opencode's stream
    /// ends when the session goes idle, so this is a fold over whatever arrived before stdout
    /// closed and has no concept of a last event. Failure comes from an `error` frame or from a
    /// terminal-state `tool_use` that ended in error — the exit code is not consulted, because S13
    /// measured exit 1 with an **empty stderr** and the description only in-stream.
    fn parse_stream(&self, stdout: &str, _exit: ChildExit) -> StreamOutcome {
        opencode::parse_stream(stdout, &self.marion_tool_name("report"))
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        // `<serverName>_<toolName>` — a third spelling again (S13, verified live). The JSON-RPC
        // `tools/call` opencode then makes to the bridge carries the **unprefixed** `report`: that
        // is the MCP wire layer, not the model-facing name, and conflating the two would put the
        // wrong identifier into a compiled prompt.
        format!("{}_{tool}", opencode::MCP_ALIAS)
    }

    fn marion_tool_calls(&self, stdout: &str) -> Vec<String> {
        opencode::marion_tool_calls(stdout, &self.marion_tool_name(""))
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
            api_key: None,
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

    /// **A `claude` child is a duplex node, and a prompt in argv is a refusal.**
    ///
    /// This test used to assert the opposite — that a non-empty prompt compiled to a positional
    /// `-p <prompt>` with no marker required. **Re-pointed, because that shape was measured not to
    /// work**: 2.1.220 launched that way reports `"tools":[]` with marion `pending` in its own
    /// `system/init`, takes turn one toolless, and exits 0 having called nothing (§12). What the
    /// test defends is unchanged and is asserted here directly — a `claude` child must never take a
    /// turn without marion's tools — and the only way to guarantee that is §6.1 step 8's gate,
    /// which needs the prompt withheld.
    #[test]
    fn a_claude_child_is_refused_if_its_prompt_was_compiled_into_argv() {
        let spec = LaunchSpec {
            prompt: "do the task".into(),
            api_key: Some("dummy".into()),
            ..claude_spec()
        };
        let e = ClaudeCodeAdapter.compile(&spec, &ctx()).unwrap_err();
        assert!(
            matches!(
                &e,
                HarnessError::MissingInput {
                    harness: Harness::ClaudeCode,
                    ..
                }
            ),
            "{e}"
        );
        assert!(
            e.to_string().contains("written after launch"),
            "the refusal must name the cause, not merely fail: {e}"
        );
    }

    /// The child's *working* shape: an empty prompt, a marker, and the credential the neutral spec
    /// carries — which is the one thing a child compiles differently from a root, because
    /// `marion run` mints a per-run token after `compile` instead.
    #[test]
    fn a_claude_child_compiles_the_duplex_launch_with_its_credential_and_its_marker() {
        let spec = LaunchSpec {
            prompt: String::new(),
            api_key: Some("dummy".into()),
            allowed_tools: vec!["mcp__marion__report".into()],
            ..claude_spec()
        };
        let inv = ClaudeCodeAdapter.compile(&spec, &ctx()).unwrap();
        assert!(
            inv.args.iter().any(|a| a == "--input-format"),
            "no typed stdin means no frame to withhold, hence no gate"
        );
        assert!(!inv.args.iter().any(|a| a == "do the task"));
        assert_eq!(
            inv.env,
            vec![
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "http://127.0.0.1:8099".to_string()
                ),
                ("ANTHROPIC_AUTH_TOKEN".to_string(), "dummy".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), String::new()),
            ]
        );
        // And its MCP declaration is written, carrying the marker the gate waits on.
        let files = ClaudeCodeAdapter.config_files(&spec, &ctx()).unwrap();
        assert_eq!(files[0].0, PathBuf::from("/state/x/config/mcp.json"));
        let v: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_AGENT_ID"],
            serde_json::json!("019f-root")
        );
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_READY_FILE"],
            serde_json::json!("/state/x/mcp-ready")
        );
    }

    /// The root's shape is untouched by that branch: an empty prompt still compiles the measured
    /// `--input-format stream-json` launch, marker and all.
    #[test]
    fn an_empty_prompt_still_compiles_the_roots_measured_launch() {
        let inv = ClaudeCodeAdapter.compile(&claude_spec(), &ctx()).unwrap();
        assert_eq!(
            inv,
            compile_headless(&HeadlessSpec {
                cwd: "/repo".into(),
                model: Some("haiku".into()),
                allowed_tools: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
                mcp_config: "/state/x/config/mcp.json".into(),
                base_url: Some("http://127.0.0.1:8099".into()),
                api_key: None,
            })
        );
        assert!(inv.args.iter().any(|a| a == "--input-format"));
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

    /// A gemini `stream-json` run, verbatim from `tests/fixtures/s12/` — every event type the CLI
    /// emits, in the order it emitted them, warnings and all.
    const GEMINI_STREAM: &str = concat!(
        "Warning: Basic terminal detected. Some features may not work.\n",
        r#"{"type":"init","timestamp":"<TS>","session_id":"<UUID-1>","model":"gemini-2.5-flash"}"#,
        "\n",
        r#"{"type":"message","timestamp":"<TS>","role":"user","content":"call the report tool"}"#,
        "\n[STARTUP] Phase 2\n",
        r#"{"type":"tool_use","timestamp":"<TS>","tool_name":"mcp_marion_report","tool_id":"mcp_marion_report__mcp_marion_report_1_0","parameters":{"narrative":"did the work"}}"#,
        "\n",
        r#"{"type":"tool_result","timestamp":"<TS>","tool_id":"<TOOL-ID-1>","status":"success","output":"MARION_REPORT_OK"}"#,
        "\n",
        r#"{"type":"message","timestamp":"<TS>","role":"assistant","content":"DONE_AFTER_TOOL","delta":true}"#,
        "\n",
        r#"{"type":"result","timestamp":"<TS>","status":"success","stats":{"total_tokens":16,"tool_calls":1}}"#,
        "\n",
    );

    /// An opencode `run --format json` stream, verbatim from `tests/fixtures/s13/` — and note what
    /// is *not* here: no init, no result, no usage summary, and no trailing newline.
    const OPENCODE_STREAM: &str = concat!(
        r#"{"type":"tool_use","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"type":"tool","tool":"marion_report","callID":"call_1","state":{"status":"completed","input":{"narrative":"did the work"},"output":"MCP_CALLED","metadata":{"truncated":false},"title":"","time":{}},"id":"<PART-1>","sessionID":"<SESSION-1>","messageID":"<MESSAGE-1>"}}"#,
        "\n",
        r#"{"type":"text","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"id":"<PART-2>","messageID":"<MESSAGE-1>","type":"text","text":"CANNED_OK","time":{"start":"<TS>","end":"<TS>"}}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":"<TS>","sessionID":"<SESSION-1>","part":{"reason":"stop","type":"step-finish","tokens":{"input":0,"output":0},"cost":0}}"#,
    );

    #[test]
    fn a_gemini_report_is_read_from_the_tool_use_frame_in_geminis_own_spelling() {
        let out = GeminiAdapter.parse_stream(GEMINI_STREAM, ChildExit::default());
        assert_eq!(out.narrative.as_deref(), Some("did the work"));
        assert_eq!(out.failure, None, "status was success");
        assert!(
            out.file_change_paths.is_empty(),
            "gemini has no file_change event; git is the authority and this stays empty"
        );
        // The spelling is load-bearing: codex's `mcp__marion__report` names no gemini tool.
        assert!(!GEMINI_STREAM.contains("mcp__marion__report"));
    }

    /// **S12's headline hazard**: an auth failure returned **exit 0** with a JSON error body. A
    /// reader that trusted the exit code would record a clean run that did nothing.
    #[test]
    fn a_gemini_failure_that_exits_zero_is_still_a_failure() {
        let exit_zero = ChildExit {
            code: Some(0),
            ..ChildExit::default()
        };
        // The measured body, verbatim from `tests/fixtures/s12/`.
        let out = GeminiAdapter.parse_stream(
            r#"{"error":{"type":"Error","message":"Invalid auth method selected.","code":41}}"#,
            exit_zero,
        );
        assert_eq!(
            out.failure.as_deref(),
            Some("Invalid auth method selected."),
            "exit 0 with an error body must not read as success"
        );
        // And the typed `error` frame of the stream-json event set, which is the other shape.
        let framed = GeminiAdapter.parse_stream(
            r#"{"type":"error","timestamp":"<TS>","error":{"message":"api error"}}"#,
            exit_zero,
        );
        assert_eq!(framed.failure.as_deref(), Some("api error"));
        // As is a result frame that says anything but success.
        let bad_result = GeminiAdapter.parse_stream(
            r#"{"type":"result","timestamp":"<TS>","status":"cancelled","stats":{}}"#,
            exit_zero,
        );
        assert!(
            bad_result
                .failure
                .as_deref()
                .is_some_and(|f| f.contains("cancelled"))
        );
    }

    #[test]
    fn an_opencode_report_is_read_from_the_terminal_tool_use_state() {
        let out = OpenCodeAdapter.parse_stream(OPENCODE_STREAM, ChildExit::default());
        assert_eq!(out.narrative.as_deref(), Some("did the work"));
        assert_eq!(out.failure, None);
        assert!(out.file_change_paths.is_empty(), "opencode announces none");
    }

    /// **S13's headline framing property**: the stream has no init, result or usage event and
    /// simply ends when the session goes idle. Nothing in the read may wait for a terminal frame —
    /// and the last frame may arrive with no newline after it.
    #[test]
    fn an_opencode_stream_that_just_stops_is_read_whole_anyway() {
        assert!(
            !OPENCODE_STREAM.ends_with('\n'),
            "the fixture's own shape: the stream stops mid-line"
        );
        for terminal in ["result", "\"type\":\"init\"", "usage"] {
            assert!(
                !OPENCODE_STREAM.contains(terminal),
                "s13: there is no {terminal} event to terminate on"
            );
        }
        // Truncate to *just* the report frame, unterminated: still read.
        let cut = &OPENCODE_STREAM[..OPENCODE_STREAM.find('\n').unwrap()];
        assert_eq!(
            OpenCodeAdapter
                .parse_stream(cut, ChildExit::default())
                .narrative
                .as_deref(),
            Some("did the work"),
        );
    }

    #[test]
    fn an_opencode_error_frame_is_a_failure_even_though_stderr_was_empty() {
        // S13's measured 400: one `{"type":"error"}` line on stdout, **stderr empty**, exit 1.
        let out = OpenCodeAdapter.parse_stream(
            r#"{"type":"error","timestamp":"<TS>","sessionID":"<SESSION-1>","error":{"name":"APIError","data":{"message":"bad request","statusCode":400,"isRetryable":false}}}"#,
            ChildExit {
                code: Some(1),
                ..ChildExit::default()
            },
        );
        assert_eq!(out.failure.as_deref(), Some("bad request"));
        assert_eq!(out.narrative, None);
    }

    /// A rejected marion call is not a completed one. S13 measured `permission: "ask"` turning the
    /// tool part into `{"status":"error", …}` while **the run continues and exits 0** — so without
    /// this the contract would record a clean run in which marion's tool was refused.
    #[test]
    fn an_opencode_tool_call_that_ended_in_error_is_not_a_report() {
        let out = OpenCodeAdapter.parse_stream(
            r#"{"type":"tool_use","sessionID":"s","part":{"type":"tool","tool":"marion_report","callID":"c","state":{"status":"error","error":"The user rejected permission to use this specific tool call."}}}"#,
            ChildExit {
                code: Some(0),
                ..ChildExit::default()
            },
        );
        assert_eq!(out.narrative, None, "nothing was reported");
        assert!(
            out.failure
                .as_deref()
                .is_some_and(|f| f.contains("rejected permission"))
        );
    }

    /// Each harness reads **its own** spelling and no other's. Cross-feeding is the failure this
    /// seam exists to end: before it, every child was read as codex JSONL, so a gemini report was
    /// invisible and the contract said `Unreported` about a run that had reported.
    #[test]
    fn no_adapter_can_read_another_harnesss_stream() {
        let codex = r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"marion","tool":"report","arguments":{"narrative":"did the work"}}}"#;
        let streams = [
            (Harness::Codex, codex),
            (Harness::Gemini, GEMINI_STREAM),
            (Harness::OpenCode, OPENCODE_STREAM),
        ];
        for (owner, stream) in streams {
            for h in Harness::ALL {
                let got = adapter_for(h)
                    .unwrap()
                    .parse_stream(stream, ChildExit::default())
                    .narrative;
                assert_eq!(
                    got.is_some(),
                    h == owner,
                    "{h} read a {owner} stream as {got:?}"
                );
            }
        }
    }

    #[test]
    fn a_claude_code_report_is_read_from_an_assistant_frames_tool_use_block() {
        // The frame shape `tests/fixtures/s9/` recorded off a real 2.1.220.
        let stream = concat!(
            r#"{"type":"assistant","message":{"content":[{"id":"toolu_1","input":{"narrative":"did the work"},"name":"mcp__marion__report","type":"tool_use"}],"role":"assistant","type":"message"},"session_id":"<UUID-1>"}"#,
            "\n",
            r#"{"type":"result","subtype":"success","is_error":false}"#,
            "\n",
        );
        let out = ClaudeCodeAdapter.parse_stream(stream, ChildExit::default());
        assert_eq!(out.narrative.as_deref(), Some("did the work"));
        assert_eq!(out.failure, None);

        let errored = ClaudeCodeAdapter.parse_stream(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"it broke"}"#,
            ChildExit::default(),
        );
        assert_eq!(errored.failure.as_deref(), Some("it broke"));
    }

    /// Every adapter must survive a stream that is pure noise — the framing hazard S12 recorded is
    /// a *prefix* of the stream, so a reader that panicked or gave up on the first non-JSON line
    /// would read nothing at all.
    #[test]
    fn a_stream_of_noise_is_an_empty_outcome_on_every_harness_rather_than_a_panic() {
        for h in Harness::ALL {
            let out = adapter_for(h).unwrap().parse_stream(
                "Warning: Basic terminal detected...\n[STARTUP] Phase 1\n\r\n{not json\n",
                ChildExit::default(),
            );
            assert_eq!(out, StreamOutcome::default(), "{h}");
        }
    }

    /// The compiled model, which is what `TaskContract.child.model` records. Four harnesses, and
    /// codex's is an **absence that is a measurement**: `codex exec` carries no model argument, so
    /// a contract that named one would be describing a wire that never existed.
    #[test]
    fn the_invocation_records_the_model_that_actually_went_on_the_wire() {
        let spec = LaunchSpec {
            model: Some("some-model".into()),
            ..codex_spec()
        };
        assert_eq!(
            CodexAdapter.compile(&spec, &ctx()).unwrap().model,
            None,
            "codex exec takes no model argument, however loudly it was asked for"
        );
        assert_eq!(
            ClaudeCodeAdapter
                .compile(&claude_spec(), &ctx())
                .unwrap()
                .model,
            Some("haiku".into())
        );

        // And on the two that require one, the recorded value is the string in argv — one
        // derivation, so the contract cannot name a model the child was not given.
        for (inv, flag) in [
            (GeminiAdapter.compile(&gemini_spec(), &ctx()).unwrap(), "-m"),
            (
                OpenCodeAdapter.compile(&opencode_spec(), &ctx()).unwrap(),
                "-m",
            ),
        ] {
            let i = inv.args.iter().position(|a| a == flag).unwrap();
            assert_eq!(inv.model.as_deref(), Some(inv.args[i + 1].as_str()));
        }
    }

    /// §6.1 step 8's post-hoc assertion, per harness: each adapter finds marion's verb in **its
    /// own** stream and in nobody else's. The spelling and the *field* both differ, which is why a
    /// single supervisor-side scan would be wrong on three of the four — codex most sharply, whose
    /// stream never contains the string `mcp__marion__` at all.
    #[test]
    fn each_adapter_recognises_a_marion_call_in_its_own_stream_and_no_others() {
        let codex_spawn = r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"marion","tool":"spawn","arguments":{}}}"#;
        let claude_spawn = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"mcp__marion__spawn","input":{}}]}}"#;
        let gemini_spawn = r#"{"type":"tool_use","tool_name":"mcp_marion_spawn","parameters":{}}"#;
        let opencode_spawn = r#"{"type":"tool_use","part":{"type":"tool","tool":"marion_spawn","state":{"status":"completed"}}}"#;
        let streams = [
            (Harness::ClaudeCode, claude_spawn),
            (Harness::Codex, codex_spawn),
            (Harness::Gemini, gemini_spawn),
            (Harness::OpenCode, opencode_spawn),
        ];
        assert!(
            !codex_spawn.contains("mcp__marion__"),
            "the whole reason this is behind the seam"
        );
        for (owner, stream) in streams {
            for h in Harness::ALL {
                let got = adapter_for(h).unwrap().marion_tool_calls(stream);
                assert_eq!(
                    got,
                    if h == owner {
                        vec!["spawn".to_string()]
                    } else {
                        vec![]
                    },
                    "{h} read a {owner} stream as {got:?}"
                );
            }
        }
    }

    /// The negative, which is the one that has to be right: a run that ended as plain text calls
    /// nothing, on every harness. This is the state §6.1 says must be refused rather than reported
    /// as a success.
    #[test]
    fn a_stream_with_no_marion_call_reports_none_on_every_harness() {
        for h in Harness::ALL {
            let a = adapter_for(h).unwrap();
            assert!(a.marion_tool_calls("").is_empty(), "{h}: empty stream");
            assert!(
                a.marion_tool_calls(
                    "Here is a session title.\n{\"type\":\"result\",\"subtype\":\"success\"}\n"
                )
                .is_empty(),
                "{h}: plain text plus a terminal frame is not a bridge call"
            );
            // A call to somebody *else's* MCP server is not marion's bridge either.
            assert!(
                a.marion_tool_calls(
                    r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"other","tool":"spawn"}}"#
                )
                .is_empty(),
                "{h}: another server's call"
            );
        }
        // And the measured report streams do count as having reached the bridge.
        assert_eq!(
            GeminiAdapter.marion_tool_calls(GEMINI_STREAM),
            vec!["report".to_string()]
        );
        assert_eq!(
            OpenCodeAdapter.marion_tool_calls(OPENCODE_STREAM),
            vec!["report".to_string()]
        );
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
