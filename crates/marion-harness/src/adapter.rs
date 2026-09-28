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

use marion_core::agent_type;
use marion_core::contract::{AgentId, TokenUsage};
use marion_core::harness::Harness;
use marion_core::provider::Wire;

use crate::acp;
use crate::antigravity;
pub use crate::auth::Auth;
use crate::claude_code;
use crate::cline;
use crate::codex;
use crate::copilot;
use crate::gemini;
use crate::goose;
use crate::grammar;
use crate::invocation::Invocation;
use crate::mcp_bridge::BridgeEnv;
use crate::opencode;
use crate::pi;
use crate::qwen;
use crate::spec::{self, Constraint, Spelling};
use crate::stream::{ChildExit, MarionCall, StreamOutcome};
use crate::surfaces::ExecutionSurfaces;

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

pub use crate::spec::McpRoute;

impl McpRoute {
    /// **Was the route actually taken?** §6.1 step 8, checked against the route the adapter
    /// *stated* rather than against the presence of a file.
    ///
    /// `written` is the paths [`HarnessAdapter::config_files`] produced, `inv` the compiled
    /// [`Invocation`]. `Ok(Some(path))` is a declaration found in a document, `Ok(None)` one found
    /// inline; `Err` names the route that was promised and not taken.
    ///
    /// It lives on the route rather than at either call site because there are now two, and they
    /// must not drift: `marion_supervisor::root` refuses a launch on this answer, and `marion
    /// doctor --adapter` reports it as a finding. An adapter that forgets its declaration is §12's
    /// silent-failure family — a node with no bridge, taking a turn with no marion tools, exiting
    /// 0 having called nothing — so the one thing this check must never be is two checks.
    /// `session` is [`HarnessAdapter::session_declaration`]'s answer — the post-launch request the
    /// adapter compiled, where it compiles one. It is a parameter rather than something looked up
    /// here for the reason `written` and `inv` are: this function checks values, it does not
    /// produce them, and the values must be the ones the launch will actually use.
    pub fn verify(
        self,
        written: &[PathBuf],
        inv: &Invocation,
        session: Option<&serde_json::Value>,
    ) -> Result<Option<PathBuf>, String> {
        match self {
            McpRoute::Document => written
                .first()
                .cloned()
                .map(Some)
                .ok_or_else(|| "a configuration document".to_string()),
            McpRoute::Environment(key) => inv
                .env
                .iter()
                .any(|(k, v)| k == key && !v.trim().is_empty())
                .then_some(None)
                .ok_or_else(|| format!("${key}")),
            // Checked against the compiled argv rather than waved through, because "declared on
            // the command line" is exactly as forgettable as "written to a file". The needle is the
            // config key the adapter named, so this fails if the overrides were dropped, if they
            // were built for a different server name, or if `compile` and `mcp_route` disagreed
            // about the mode.
            McpRoute::Argv(key) => inv
                .args
                .iter()
                .any(|a| a.contains(key))
                .then_some(None)
                .ok_or_else(|| format!("`-c {key}.…` on its own command line")),
            // Checked against the request the adapter built, and specifically against an entry
            // that **names marion's own server**. A non-empty array alone would pass on a
            // declaration built for somebody else's MCP server, which is a node with no marion
            // bridge wearing a green check.
            McpRoute::Session(key) => session
                .and_then(|s| s.pointer("/params")?.get(key)?.as_array())
                .is_some_and(|servers| {
                    servers.iter().any(|s| {
                        s.get("name").and_then(serde_json::Value::as_str)
                            == Some(crate::acp::MCP_SERVER_NAME)
                            && s.get("command")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|c| !c.trim().is_empty())
                    })
                })
                .then_some(None)
                .ok_or_else(|| {
                    format!(
                        "a `{key}` entry named `{}` in its own `session/new` request",
                        crate::acp::MCP_SERVER_NAME
                    )
                }),
            McpRoute::None => Err("no route at all".to_string()),
        }
    }
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
    /// **Which ACP agent**: a refinement row's [`crate::acp::Agent::id`], or the agent's own
    /// command line ([`crate::acp::Binding::resolve`]). Read only by [`AcpAdapter`].
    ///
    /// It is a launch input rather than a property of the adapter because §5.2's `acp` row is *one
    /// adapter serving many agents*, and §6.4 forbids marion choosing one for the operator. `None`
    /// is a refusal at `compile`, by name — never a default, because a default here would run some
    /// other vendor's agent than the one an agent type asked for, which is the exact bug the
    /// `HarnessAdapter` seam was introduced to end.
    pub acp_agent: Option<String>,
    /// The agent type's `approval_mode`: an ACP session mode the driver sets over the protocol.
    /// Read only by [`AcpAdapter`]; any other adapter refuses a launch carrying one, by name.
    pub approval_mode: Option<String>,
    /// The supervisor's own mode and endpoint, where this node's differs — set on an
    /// [`Auth::Endpoint`] node alone. The bridge the node starts is told these, never the node's
    /// provider: a spawn it serves is resolved afresh by the supervisor, and handing it the
    /// provider's URL would point a grandchild's canned overlay at a third party.
    pub tree_auth: Option<Auth>,
    pub tree_base_url: Option<String>,
    /// The header an [`Auth::Endpoint`] node's provider reads its key from; `None` is Bearer.
    pub key_header: Option<marion_core::provider::KeyHeader>,
    /// The profile directory the launch selected, exactly as `profiles.toml` stores it. Read by
    /// every row through its [`crate::profile::ProfileCarrier`], under live auth only.
    pub profile_dir: Option<PathBuf>,
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
    /// The **availability** axis (§3.1): which built-in tools exist for this node, in **marion's**
    /// vocabulary (`marion_core::agent_type::TOOL_WRITE`, …). The agent type's `tools:` list,
    /// verbatim; each adapter maps it to its harness's own spelling through
    /// [`HarnessAdapter::tool_name`], and refuses by name what it cannot provide.
    ///
    /// Distinct from [`Self::allowed_tools`] because §3.1 says the two axes are distinct and §11
    /// item 24 measured what conflating them costs — but **not independent of it**: an adapter
    /// that reads this must also union it into whatever permission surface its harness has, or the
    /// tool exists and every call to it is denied. `HarnessAdapter::compile` is where that union
    /// happens, so the two can never be declared apart.
    ///
    /// Empty on every node marion spawns today: no built-in agent type declares a tool.
    pub tools: Vec<String>,
    /// The **permission** axis (§3.1): tool calls allowed without a prompt, in marion's names for
    /// its own verbs. Adapters translate; §3.1's two-axis table is why this is not `tools`.
    ///
    /// Carries marion's **own** verbs only. What the node's agent type declared arrives on
    /// [`Self::tools`] and is unioned in by the adapter — a caller that appended it here instead
    /// would have granted permission without availability, which is the mirror of item 24's dead
    /// end and just as silent.
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
    pub api_key: Option<marion_core::secret::Secret>,
    /// Whether this node presents a credential marion minted, or the operator's own login.
    ///
    /// Distinct from `api_key` being `None`, which already means several things (a caller that
    /// pushes the pair itself, a harness that reads none). This states the *intent*, so an adapter
    /// can drop an overlay rather than merely omit a value it was not given.
    pub auth: Auth,
    /// The node's isolated harness config dir (§6.4): `$CODEX_HOME` for Codex, the directory
    /// marion's `--mcp-config` document is written into for Claude Code, the sandbox `HOME` and
    /// XDG root for opencode. Every path in [`HarnessAdapter::config_files`] is under it.
    ///
    /// **Under [`Auth::Inherited`] too.** Live mode relaxes what marion *overlays*; it does not
    /// relax where marion *writes*, and every path in [`HarnessAdapter::config_files`] stays under
    /// this directory on every harness in every mode.
    pub config_dir: PathBuf,
    /// The harness session this launch **resumes**, in the harness's own spelling — the id its
    /// stream named (`SessionObserved`), handed back through the row's measured resume grammar
    /// ([`crate::spec::HarnessSpec::resume`]). `None` is a fresh session. A row with no measured
    /// grammar for the shape asked for refuses the launch by name rather than starting fresh under
    /// a resumed session's id; nothing here is per harness.
    pub resume: Option<String>,
    /// The wire this node speaks to its endpoint — chosen by endpoint resolution as the first of
    /// the harness's wires the provider serves natively. `None` on canned and live nodes, whose
    /// wire is the row's own.
    pub wire: Option<Wire>,
    /// The registry id of the provider an [`Auth::Endpoint`] node talks to; `None` otherwise.
    pub provider: Option<String>,
    pub extra: Extras,
}

/// What marion knows about the node it is spawning, independent of what was asked for.
#[derive(Debug, Clone)]
pub struct SpawnCtx {
    pub agent_id: AgentId,
    /// The node's agent type, in the **canonical** name `marion_core::agent_type::builtin`
    /// resolves — not the alias a caller happened to type, so the bridge re-resolves one
    /// definition and not two.
    ///
    /// Distinct from [`LaunchSpec`]'s fields on purpose: that struct is *what was asked for*, and
    /// this is what marion knows about the node. The type name is carried here because the bridge
    /// this node will start has to re-resolve it to read §3.1's `max_depth` /
    /// `max_concurrent_children` off the **caller's** type (§6.1 step 2).
    pub agent_type: String,
    /// The node's depth in the tree, **root = 0** (§3.1). A `spawn` this node makes creates a node
    /// at `depth + 1`, and is refused when that would pass its type's `max_depth`.
    pub depth: u32,
    /// **§5.4's per-node capability token**, minted by whoever owns this node's lifecycle and
    /// written into the declaration this node's bridge reads
    /// ([`crate::mcp_bridge::NODE_TOKEN_ENV`]).
    ///
    /// It rides `SpawnCtx` rather than `LaunchSpec` for the reason [`Self::agent_type`] does: this
    /// is *what marion knows about the node*, not what was asked for. Nobody asks for a token.
    ///
    /// `None` where no owner minted one — every spawn path but the supervisor's own `agent/spawn`,
    /// until steps 5 and 6 land. A node with no token declares no key at all rather than an empty
    /// one, so its bridge states no capability rather than a worthless one.
    pub node_token: Option<marion_core::secret::Secret>,
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
    /// A value the launch needs cannot be spelled in this harness's declaration grammar — goose's
    /// `--with-extension` token is split on whitespace, so a bridge path with a space in it would
    /// be declared as two arguments. **Refused by name, quoting the value**, never split: the
    /// resulting node would start no bridge and take a turn with none of marion's tools.
    #[error("{harness}: {what}")]
    Unspellable { harness: Harness, what: String },
    /// An agent type declared a tool this harness has no mapping for (§3.1's availability axis).
    ///
    /// **Loud, at compile time, naming both halves** — never a silent drop. Dropping it is the
    /// §12 accept-and-ignore shape marion keeps finding in other harnesses, and here it would be
    /// the worst instance of it: the node launches, is offered no such tool, does no work, and
    /// persists a contract with `changed_paths: []` that is byte-identical to a child whose write
    /// escaped its worktree (§11 item 24). A caller cannot tell those apart, so marion must never
    /// produce the first by accident.
    ///
    /// The tool is a `String` and not a `&'static str` because it is a value that came from a
    /// declaration rather than from marion's own source, and the message is only useful if it
    /// quotes what was actually written.
    #[error("{harness}: no mapping for marion tool `{tool}`; this harness's adapter provides none")]
    UnsupportedTool { harness: Harness, tool: String },
    /// A run asked for a pane and this harness has no interactive shape marion can drive.
    ///
    /// **Refused, never silently downgraded to the headless shape.** A caller that asked for a
    /// pane is a caller that is about to run `marion attach`, and a node launched headlessly
    /// instead would answer that attach with *"no display plane"* — a true sentence about a node
    /// marion made headless after being told not to.
    #[error(
        "{0}: marion has no pane shape for this harness, so it cannot be run in a terminal marion \
         owns. Spawn it without a pane and watch its structured events instead"
    )]
    NoPaneSurface(Harness),
    /// The ACP agent this launch named is one marion cannot launch — either it has never heard of
    /// it, or it has heard of it and has never measured what it calls a tool.
    ///
    /// A `String` because both messages have to quote something that came from outside marion's
    /// source: the id an operator wrote, or the agent's own recorded note. And a refusal rather
    /// than a fallback to the one agent that *is* measured, which would run somebody else's agent
    /// under the name an agent type asked for.
    #[error("acp: {0}")]
    AcpAgent(String),
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

    /// This harness's row (`plan-harness-spec.md`): the launch, the stream grammar, the tool
    /// names, the spelling, the declaration routes and the audit constraint, as data. Every
    /// default below reads it, and an adapter is the row plus the measured logic no row can hold.
    fn spec(&self) -> &'static spec::HarnessSpec {
        harness_spec(self.harness())
    }

    /// The surfaces this adapter drives (§3.4) — the row's point in the cross-product.
    fn surfaces(&self) -> ExecutionSurfaces {
        self.spec().surfaces.execution()
    }

    /// §6.1 step 5: argv + env. Runs on every spawn without exception, including surfaces that
    /// have no `ControlPlane` to open.
    ///
    /// The default renders this harness's [`spec::HarnessSpec`] row over [`Self::fields`], and is
    /// the whole of `compile` for every migrated harness: the row is the launch, the hook is the
    /// judgement, and nothing else stands between a `LaunchSpec` and an argv.
    fn compile(&self, spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        let f = self.fields(spec, ctx, spec::Shape::Headless)?;
        // An approval mode is a session mode, and only a hook that has a session to set it in
        // carries it onward; anything else would launch a node ignoring the operator's choice.
        if spec.extra.approval_mode.is_some() && f.session_mode.is_none() {
            return Err(HarnessError::MissingInput {
                harness: self.harness(),
                what: "the agent type states an approval_mode, which is an ACP session mode set \
                       over the protocol; this harness has no ACP session to set it in",
            });
        }
        let mut inv = render_row(self.spec(), spec::Shape::Headless, &f)?;
        inv.env.extend(bridge_process_env(self, spec, ctx));
        Ok(inv)
    }

    /// §3.1's two axes in this harness's spelling, or the first refusal.
    ///
    /// The default is the rule the design states for the harness that has both axes: availability
    /// is the declared list mapped through [`Self::tool_name`], and permission is *"the same list,
    /// plus marion's own"* — [`LaunchSpec::allowed_tools`] with the mapped list appended. A harness
    /// whose axes are shaped otherwise (copilot's two spellings, gemini's mode) says so here, and
    /// nowhere else: `compile` and `compiled_permissions` both read this one answer, so the flag and
    /// the audit record cannot disagree.
    fn axes(&self, spec: &LaunchSpec) -> Result<spec::Axes, HarnessError> {
        let tools = self.native_tools(spec)?;
        let mut allowed = spec.allowed_tools.clone();
        allowed.extend(tools.iter().cloned());
        Ok(spec::Axes {
            tools,
            allowed,
            mode: None,
        })
    }

    /// **The measured logic hook**: everything this harness's [`spec::HarnessSpec`] row reads,
    /// derived from the launch — and every refusal the launch is owed, by name.
    ///
    /// The default is [`neutral_fields`] over [`Self::axes`]: the spec's values verbatim,
    /// with live mode's removal of the provider overlay applied. An adapter overrides it to add
    /// what its harness has measured and nothing else — a required model, a derived URL, a
    /// document path — so that the row stays a transcription and this stays the place a reader
    /// looks for a decision.
    fn fields(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
        shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        let _ = (ctx, shape);
        Ok(neutral_fields(spec, self.axes(spec)?))
    }

    /// The surfaces this harness runs under **when a run asks for a pane**, or `None` where marion
    /// has no interactive shape for it (§3.4, §9's M3).
    ///
    /// # Why a second surfaces method rather than a flag inside [`Self::surfaces`]
    ///
    /// A pane is asked for **per run**, by a client, and [`Self::surfaces`] takes no arguments
    /// because it is a fact about the harness rather than about a run. Threading the request into
    /// it would change how *every* node of that harness is launched to serve a feature most runs
    /// do not use — and on Claude Code that means moving M1's measured `stream-json` path onto a
    /// pty for runs that never attach to one.
    ///
    /// # Why the answer is §3.4's `opaque` and not `shared`
    ///
    /// `shared` — `Typed(_)` control *and* `NativePty` — is the preset a reader reaches for, and it
    /// is the wrong one, for a reason that is structural rather than aesthetic. It puts the
    /// **protocol stream on the pty**: `stdout` and `stderr` both become the slave, so the single
    /// reader of the master (`marion_supervisor::pty::PtyHost`) and the frame reader
    /// (`marion_supervisor::duplex`) are two readers of one stream, and a diagnostic written
    /// mid-line lands *inside* a JSON frame. §9's M3 criteria are not about that node anyway: they
    /// read *"a real `claude` TUI runs in a marion pane"* and *"a real `codex` TUI runs in a pane
    /// with scrollback retained across at least one resize"*. A TUI has **no frame parser at all**,
    /// so both hazards are absent by construction rather than mitigated — which is why the pane
    /// shape is a different node, not the same node with an extra fd. **Not `interactive`**
    /// either: that preset also claims `TranscriptRecords`, and marion reads no transcript a TUI
    /// writes.
    fn pane_surfaces(&self) -> Option<ExecutionSurfaces> {
        self.spec().pane.map(|_| ExecutionSurfaces::opaque())
    }

    /// argv + env for [`Self::pane_surfaces`]'s shape. Called **only** where that is `Some`.
    ///
    /// The default renders the row's `pane` argv, and a harness without a pane surface is the
    /// refusal by name — never [`Self::compile`]'s launch — so a sixth harness that declares a
    /// pane surface and writes no pane row gets a named error instead of a TUI request answered
    /// headlessly. The refusal comes **before** [`Self::fields`] runs: it is a fact about the
    /// harness, not about this launch, and must not be masked by a launch-level refusal.
    fn compile_pane(&self, spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        if self.pane_surfaces().is_none() {
            return Err(HarnessError::NoPaneSurface(self.harness()));
        }
        let f = self.fields(spec, ctx, spec::Shape::Pane)?;
        let mut inv = render_row(self.spec(), spec::Shape::Pane, &f)?;
        inv.env.extend(bridge_process_env(self, spec, ctx));
        Ok(inv)
    }

    /// How this launch's node token reaches its bridge: the row's carrier for the launch's auth
    /// mode ([`spec::HarnessSpec::token`]). Every declaration an adapter writes is of
    /// [`declared_bridge`], which this carrier has already stripped where it must, and `compile`
    /// sets what it withheld on the process environment — so no adapter handles the token itself.
    /// An adapter bound to a refinement row with a carrier of its own answers for it here.
    fn token_carrier(&self, spec: &LaunchSpec) -> spec::TokenCarrier {
        self.spec().token.for_auth(spec.auth)
    }

    /// The configuration files this harness needs, as `(absolute path, contents)`. The caller
    /// writes them; the adapter decides what and where, because "what and where" is the part that
    /// differs per harness. Paths are always under `spec.config_dir`.
    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError>;

    /// The wires this harness can be pointed at in endpoint mode, in preference order — the
    /// wires of the row's [`spec::HarnessSpec::wires`] recipes.
    fn endpoint_wires(&self) -> Vec<Wire> {
        self.spec().wires.iter().map(|r| r.wire).collect()
    }

    /// The provider's model id behind an endpoint node's compiled model — the inverse of whatever
    /// this adapter's `fields` did to spell it, so a resume can ask the provider for the same model
    /// again. The identity everywhere the compiled model *is* the provider's id.
    fn endpoint_model(&self, compiled: &str) -> String {
        compiled.to_string()
    }

    /// Which channel this launch's MCP declaration travels on — the row's [`spec::McpRoutes`] for
    /// the launch's auth mode, or [`McpRoute::None`] where no declaration was asked for.
    ///
    /// The row states both modes explicitly; there is no default a sixth harness could inherit
    /// unexamined, which is the same class of mistake as the fallback [`adapter_for`] refuses to
    /// make.
    fn mcp_route(&self, spec: &LaunchSpec) -> McpRoute {
        match (spec.mcp, spec.auth) {
            (McpDeclaration::None, _) => McpRoute::None,
            // Endpoint rides the canned route: the config dir is marion's in both.
            (McpDeclaration::Marion, Auth::Canned | Auth::Endpoint) => self.spec().mcp.canned,
            (McpDeclaration::Marion, Auth::Inherited) => self.spec().mcp.live,
        }
    }

    /// The **post-launch** request that carries this launch's declaration, where the harness has
    /// one — today only ACP's `session/new`.
    ///
    /// Defaulted to `None` rather than required, because it is genuinely absent on four of the
    /// five: their declaration is complete the moment `compile` returns. That is the opposite
    /// direction from [`Self::mcp_route`], which is required precisely because every harness has
    /// *some* route and a default would let one be inherited unexamined.
    ///
    /// It is compiled here, beside argv, rather than assembled by whoever drives the session, so
    /// that [`McpRoute::verify`] can check it before a byte is sent — the same guarantee the other
    /// four get from their route being a file or an environ.
    fn session_declaration(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Option<serde_json::Value>, HarnessError> {
        let _ = (spec, ctx);
        Ok(None)
    }

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
    ///
    /// The default reads the row's [`spec::HarnessSpec::stream`] grammar in this harness's own
    /// spelling of marion's tools. A row with no grammar and no override is **a failure in the
    /// outcome itself**, never an empty reading: a reader that finds nothing looks exactly like a
    /// run that reported nothing, and §12's silent-failure family is the one thing this seam must
    /// not admit. `exit` is unused by every row: each of the five measured harnesses states its
    /// verdict in-stream (or, codex, not at all), and the exit code stays the supervisor's to
    /// record.
    fn parse_stream(&self, stdout: &str, exit: ChildExit) -> StreamOutcome {
        let _ = exit;
        match self.spec().stream {
            Some(g) => grammar::parse_stream(g, stdout, &self.marion_tool_name("")),
            None => StreamOutcome {
                failure: Some(format!(
                    "{}: marion has no grammar for this harness's stream and read none of it",
                    self.harness()
                )),
                ..StreamOutcome::default()
            },
        }
    }

    /// The tokens the run spent, as the stream in `frames` states them — read with the row's
    /// [`grammar::StreamGrammar::usage`] rule. `None` when the row has no rule (no grammar, or a
    /// harness that reports no token count) and when the stream never reached the unit that
    /// carries one: an absent claim is not a claim of zero.
    fn usage(&self, frames: &[serde_json::Value]) -> Option<TokenUsage> {
        grammar::usage(self.usage_rule()?, frames)
    }

    /// The rule this node's usage is read by: the row's [`grammar::StreamGrammar::usage`], or
    /// `None` where it states none. A caller folding usage incrementally reads units with it
    /// ([`grammar::usage_units`]) rather than re-reading a whole stream per question.
    fn usage_rule(&self) -> Option<&'static grammar::UsageRule> {
        self.spec().stream?.usage.as_ref()
    }

    /// The harness session `frame` names, if it is the frame that names one — read with the row's
    /// [`grammar::StreamGrammar::session`] rule. `None` for every other frame and for a row with
    /// no rule. The one question the supervisor's session watch asks, so no caller reads a row's
    /// grammar for it directly.
    fn session_id(&self, frame: &serde_json::Value) -> Option<String> {
        grammar::session_id(self.spec().stream?, frame)
    }

    /// The rule a running node's recent tool calls and words are read by — the row's
    /// [`grammar::StreamGrammar::activity`] — or `None` where there is none, which a caller says
    /// rather than showing an empty peek.
    fn activity(&self) -> Option<&'static grammar::ActivityRule> {
        self.spec().stream?.activity.as_ref()
    }

    /// The stream's own failure claim, **without** `parse_stream`'s refused-`report` rule — the
    /// reading a **root** is judged by, since §9 gives a root no contract and §5.4 refuses its
    /// `report`. A row with no grammar reads as [`Self::parse_stream`] does.
    fn stream_failure(&self, stdout: &str) -> Option<String> {
        match self.spec().stream {
            Some(g) => grammar::stream_failure(g, stdout),
            None => self.parse_stream(stdout, ChildExit::default()).failure,
        }
    }

    /// **Why the run failed**, by the one classifier ([`crate::auth::failure_cause`]'s ranking),
    /// over stderr and the provider errors the row's grammar reads off the stream
    /// ([`grammar::StreamGrammar::errors`]) — per row data, never a guess at which frames are
    /// errors. A row with no grammar (ACP, whose reader is code) falls back to the error-shaped
    /// frames any JSON stream carries.
    fn failure_cause(
        &self,
        stderr: &str,
        stdout: &str,
        billing: crate::auth::Billing,
    ) -> Option<marion_core::contract::FailureCause> {
        match self.spec().stream {
            Some(g) => crate::auth::reported_failure_cause(
                stderr,
                &grammar::error_reports(g, stdout),
                billing,
            ),
            None => crate::auth::failure_cause(stderr, stdout, billing),
        }
    }

    /// **The refused credential one frame reports**, in the harness's words — `Some` only where
    /// the row's error rules read the provider's own 401 or 403 off it
    /// ([`crate::auth::refused_credential`]). No retry
    /// heals one, so a run showing it while the harness retries (claude's ten `api_retry`s) has
    /// failed already. `None` for every other frame, and on a row with no grammar.
    fn auth_refusal(&self, frame: &serde_json::Value) -> Option<String> {
        let g = self.spec().stream?;
        crate::auth::refused_credential(&grammar::frame_error_reports(
            g,
            std::slice::from_ref(frame),
        ))
    }

    /// How a message reaches this node's next turn in `shape` — the row's
    /// [`spec::delivery_for`], which an adapter bound to one agent may refine (ACP's per-agent
    /// [`spec::MidTurn`]). The one question a driver asks; no caller reads the row directly.
    fn turn_delivery(&self, shape: spec::NodeShape) -> spec::TurnDelivery {
        spec::delivery_for(self.spec(), shape)
    }

    /// Why a run that resumed session `resumed` did not continue it — the row's
    /// [`grammar::resume_refusal`], where its stream grammar measured a resume naming its own
    /// session. `None` for a fresh run, and on every row where no such check was measured.
    fn resume_refusal(&self, stdout: &str, resumed: Option<&str>) -> Option<String> {
        grammar::resume_refusal(self.spec().stream?, stdout, resumed?)
    }

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
    ///
    /// The default spells with the row's [`spec::Spelling::Fixed`] measurement. A
    /// [`spec::Spelling::PerAgent`] row is the adapter's to answer, and an adapter that has not
    /// bound an agent gets [`acp::UNBOUND_TOOL_NAME`] — a string that is a tool name in no
    /// spelling and matches nothing in any transcript — which every route to a launch refuses
    /// before a model sees it.
    fn marion_tool_name(&self, tool: &str) -> String {
        match self.spec().spelling {
            Spelling::Fixed(s) => s.spell(tool),
            Spelling::PerAgent => format!("{}{tool}", acp::UNBOUND_TOOL_NAME),
        }
    }

    /// This harness's spelling of one tool from **marion's own vocabulary**
    /// (`marion_core::agent_type::TOOL_WRITE`), for §3.1's **availability** axis.
    ///
    /// The sibling of [`Self::marion_tool_name`] on the other side of the boundary: that one names
    /// *marion's* verbs to a harness, this one names a *harness's* verbs to marion. §3.1 settles
    /// which direction the vocabulary runs — *"tool names are marion's vocabulary, and the mapping
    /// is part of the adapter contract"* — so a marion name goes in and a harness-native one comes
    /// out, and no call site anywhere else has to know either spelling.
    ///
    /// **Returns a `Result`, and the error is the point.** A name this harness cannot provide is
    /// [`HarnessError::UnsupportedTool`], naming the tool and the harness, and it aborts the
    /// launch. Silently dropping it would spawn a node that cannot do the work it was spawned for
    /// and report no error — §11 item 24's whole subject.
    ///
    /// **Answering does not imply marion compiles anything for it.** Two of the four harnesses
    /// have no per-tool availability surface at all, and for them the honest answer is §3.1's
    /// *"the harness's coarsest equivalent"*: a name they already grant unconditionally, which
    /// this method reports and `compile` then has nothing to do about. Making the grant
    /// conditional there would *narrow* what those two harnesses have always been able to do,
    /// which is a behaviour change wearing a feature's clothes.
    ///
    /// **Every** native name the verb maps to, in row order: a harness whose tool for one grant
    /// depends on the model (copilot's `create` vs `apply_patch`) lists each, and the launch offers
    /// all of them so whichever the model has is there.
    ///
    /// The default is the row's [`spec::HarnessSpec::tool_names`], and a verb it does not list is
    /// the refusal.
    fn tool_names(&self, tool: &str) -> Result<Vec<String>, HarnessError> {
        let names: Vec<String> = self
            .spec()
            .tool_names
            .iter()
            .filter(|(verb, _)| *verb == tool)
            .map(|(_, native)| native.to_string())
            .collect();
        if names.is_empty() {
            return Err(HarnessError::UnsupportedTool {
                harness: self.harness(),
                tool: tool.to_string(),
            });
        }
        Ok(names)
    }

    /// Every tool [`LaunchSpec::tools`] declares, in this harness's own spelling, or the first
    /// refusal.
    ///
    /// Provided rather than written four times: the *mapping* is per-harness ([`Self::tool_name`])
    /// and refusing on the first unmappable name is not. Every `compile` must call it — including
    /// the two harnesses that do nothing with the result — because the refusal is the part that is
    /// owed to a declaration on all four.
    fn native_tools(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        let mut out = Vec::new();
        for t in &spec.tools {
            out.extend(self.tool_names(t)?);
        }
        Ok(out)
    }

    /// §6.7's `TaskContract.allowed_tools`: **the constraint this launch actually compiled**, in
    /// this harness's own vocabulary.
    ///
    /// §3.1 defines the field exactly: *"the compiled, harness-native constraint — or the harness's
    /// coarsest equivalent where it has no per-tool allowlist at all"*. Both halves of that
    /// sentence are load-bearing, because the four harnesses sit on both sides of it: Claude Code
    /// has a real per-tool allowlist and the other three have one coarse knob or none at all.
    ///
    /// **A function of the same [`LaunchSpec`] `compile` receives, so the record cannot describe a
    /// launch that did not happen.** This is the third field to reach the contract this way, after
    /// `child.harness` and `child.model` (`32ec905`): the audit record names what *ran*, never what
    /// was *asked for*, and the way that stays true is by sourcing it from the thing that ran.
    ///
    /// **Never marion's own vocabulary.** §3.1: *"echoing marion's own vocabulary there would make
    /// the field claim a constraint that never existed"* — said of codex, whose contract records
    /// `sandbox:workspace-write` precisely *because* `apply_patch` and `shell` are not names any
    /// allowlist of codex's was ever checked against.
    ///
    /// Fallible for one reason only: on a harness that compiles the declaration, the declaration
    /// has to be mapped, and an unmappable name is [`HarnessError::UnsupportedTool`] here as it is
    /// in `compile`. A caller reaching this after a successful `compile` cannot see that error.
    ///
    /// The default reads the row's [`spec::Constraint`] over [`Self::axes`] — the same axes the
    /// row renders, so the record and the flag it describes cannot disagree — and runs the
    /// refusal on every kind, including the fixed ones nothing about a declaration could vary.
    fn compiled_permissions(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        let axes = self.axes(spec)?;
        Ok(match self.spec().constraint {
            Constraint::Allowed { prefix } => axes
                .allowed
                .iter()
                .map(|a| format!("{prefix}{a}"))
                .collect(),
            Constraint::Mode { prefix, default } => {
                vec![format!(
                    "{prefix}{}",
                    axes.mode.as_deref().unwrap_or(default)
                )]
            }
            Constraint::Fixed { prefix, value } => vec![format!("{prefix}{value}")],
        })
    }

    /// Every call to one of marion's verbs this harness's stream shows the node making, in
    /// **marion's** vocabulary (`spawn`, `report`, …) rather than in the harness's spelling, **and
    /// what the stream says came of each one**.
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
    /// stream never contains the string `mcp__marion__` at all). They disagree about the *result*
    /// just as widely: codex revises one item in place, opencode emits only terminal states,
    /// gemini and Claude Code emit a separate result frame that must be paired back to its call by
    /// id.
    ///
    /// **The outcome used to be discarded here, and that was the defect.** This read returned bare
    /// verb names on the argument that "a call the bridge refused still proves the node had
    /// marion's tools" — true, and not what §6.1 step 8 needs to know. A gemini root whose `spawn`
    /// was refused by gemini's own schema validator satisfied that gate, exited 0, and was
    /// journalled `ExitStatus::Ok` having delegated nothing (`tasks/todo.md`, owed item 0): the
    /// same silent success the gate exists to prevent, one level in. It is also why the root
    /// `report` defect fixed in `7ff470e` stayed invisible — every refusal marion started issuing
    /// still read as evidence the run had worked. So each call carries its [`CallOutcome`], and
    /// what that outcome can and cannot see is documented there.
    ///
    /// The default reads the row's grammar; a row with no grammar shows no calls, which is what
    /// §6.1 step 8's gate refuses — loudly, and by name, one layer up.
    fn marion_calls(&self, stdout: &str) -> Vec<MarionCall> {
        match self.spec().stream {
            Some(g) => grammar::marion_calls(g, stdout, &self.marion_tool_name("")),
            None => Vec::new(),
        }
    }

    /// The verbs alone, for callers that only ask *which* verbs were reached for.
    ///
    /// Derived rather than implemented per adapter: two readings of one stream is how the name and
    /// the result would drift apart, and the result is the half that matters.
    fn marion_tool_calls(&self, stdout: &str) -> Vec<String> {
        self.marion_calls(stdout)
            .into_iter()
            .map(|c| c.verb)
            .collect()
    }
}

/// The launch spec's values, verbatim, with **live mode already applied to the overlay**: under
/// [`Auth::Inherited`] no base URL and no credential are carried, on any harness, because *"live
/// is a removal"* is one rule and not five.
fn neutral_fields(spec: &LaunchSpec, axes: spec::Axes) -> spec::Fields {
    let (base_url, api_key) = match spec.auth {
        Auth::Canned | Auth::Endpoint => (spec.base_url.clone(), spec.api_key.clone()),
        Auth::Inherited => (None, None),
    };
    spec::Fields {
        cwd: spec.cwd.clone(),
        config_dir: spec.config_dir.clone(),
        auth: spec.auth,
        wire: spec.wire,
        key_header: spec.extra.key_header,
        prompt: spec.prompt.clone(),
        model: spec.model.clone(),
        base_url,
        api_key,
        axes,
        output_schema: spec.extra.output_schema.clone(),
        output_last_message: spec.extra.output_last_message.clone(),
        resume: spec.resume.clone(),
        profile_dir: spec.extra.profile_dir.clone(),
        ..spec::Fields::default()
    }
}

/// **The bridge declaration for this node**, in the neutral form every harness's document
/// serialises ([`BridgeEnv`]): what marion knows about the node ([`SpawnCtx`]) and what the launch
/// asked for ([`LaunchSpec`]), joined once so no two adapters can hand the bridge different values.
///
/// `base_url` is omitted rather than blanked under [`Auth::Inherited`] — a live node's bridge
/// declares no endpoint, and `MARION_BASE_URL: ""` once compiled a live root's child canned against
/// an endpoint spelled as the empty string, with nothing anywhere reporting it.
fn bridge_env(spec: &LaunchSpec, ctx: &SpawnCtx) -> BridgeEnv {
    BridgeEnv {
        bridge: ctx.bridge.clone(),
        args: ctx.bridge_args.clone(),
        repo: ctx.repo.clone(),
        state: ctx.state_dir.clone(),
        // An endpoint node's bridge is told the supervisor's mode and endpoint, never the node's.
        base_url: match spec.auth {
            Auth::Endpoint => spec.extra.tree_base_url.clone(),
            Auth::Canned | Auth::Inherited => spec.base_url.clone(),
        },
        auth: spec.extra.tree_auth.unwrap_or(spec.auth),
        agent_id: ctx.agent_id.clone(),
        agent_type: ctx.agent_type.clone(),
        depth: ctx.depth,
        node_token: ctx.node_token.clone(),
        ready_file: ctx.ready_file.clone(),
    }
}

/// **The bridge as this launch's declaration states it**: [`bridge_env`], with the node token
/// withheld where the launch's [`HarnessAdapter::token_carrier`] carries it on the environment
/// instead. The one bridge an adapter's `fields` and `config_files` hooks write from.
fn declared_bridge<A: HarnessAdapter + ?Sized>(
    adapter: &A,
    spec: &LaunchSpec,
    ctx: &SpawnCtx,
) -> BridgeEnv {
    adapter.token_carrier(spec).declared(&bridge_env(spec, ctx))
}

/// What the harness process's environment carries for its bridge: the node token, where the
/// launch's carrier withholds it from the declaration — and nothing where no declaration was asked
/// for, since then no bridge starts to read it.
fn bridge_process_env<A: HarnessAdapter + ?Sized>(
    adapter: &A,
    spec: &LaunchSpec,
    ctx: &SpawnCtx,
) -> Vec<(String, String)> {
    match spec.mcp {
        McpDeclaration::Marion => adapter
            .token_carrier(spec)
            .process_env(&bridge_env(spec, ctx)),
        McpDeclaration::None => Vec::new(),
    }
}

/// [`spec::render`] with its refusals named for this harness: a row with no pane shape, a launch
/// that named no program where the row expected one (an ACP adapter bound to no agent), and a
/// resume this harness has no measured flag for.
fn render_row(
    row: &spec::HarnessSpec,
    shape: spec::Shape,
    f: &spec::Fields,
) -> Result<Invocation, HarnessError> {
    spec::render(row, shape, f).map_err(|why| match why {
        spec::Refusal::NoPaneShape => HarnessError::NoPaneSurface(row.harness),
        spec::Refusal::NoProgram => HarnessError::MissingInput {
            harness: row.harness,
            what: "the launch named no program and the row carries none",
        },
        spec::Refusal::NoWireRecipe => HarnessError::MissingInput {
            harness: row.harness,
            what: "an endpoint launch must name a wire this harness has a recipe for; endpoint \
                   resolution chooses one from the row's `wires`",
        },
        spec::Refusal::NoKeyRecipe(header) => HarnessError::Unspellable {
            harness: row.harness,
            what: format!(
                "the provider reads its key from `{}`, and this harness's recipe for the wire \
                 cannot present it there",
                match header {
                    marion_core::provider::KeyHeader::Bearer => "Authorization: Bearer",
                    marion_core::provider::KeyHeader::XApiKey => "x-api-key",
                }
            ),
        },
        spec::Refusal::NoResume => HarnessError::MissingInput {
            harness: row.harness,
            what: "this harness has no measured way to name a session to resume on this shape's \
                   command line; starting it fresh under a resumed session's id would be a launch \
                   the contract misdescribes",
        },
    })
}

/// The row for a harness (`plan-harness-spec.md`). **Every harness marion names has one**; a
/// harness without a row is a compile error here, not a fallback.
pub fn harness_spec(h: Harness) -> &'static spec::HarnessSpec {
    match h {
        Harness::ClaudeCode => &claude_code::SPEC,
        Harness::Codex => &codex::SPEC,
        Harness::Gemini => &gemini::SPEC,
        Harness::OpenCode => &opencode::SPEC,
        Harness::Copilot => &copilot::SPEC,
        Harness::Goose => &goose::SPEC,
        Harness::Cline => &cline::SPEC,
        Harness::Qwen => &qwen::SPEC,
        Harness::Antigravity => &antigravity::SPEC,
        Harness::Pi => &pi::SPEC,
        Harness::Acp => &acp::SPEC,
    }
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
        spec.config_dir.join(claude_code::MCP_CONFIG_FILE)
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

    /// The bridge declaration, **with the readiness marker required.** A Claude Code node's prompt
    /// is always written after launch — 2.1.220 does not hold turn one for an `--mcp-config` server
    /// — so there is always a frame to withhold and always something for the marker to gate (§6.1
    /// step 8). A node launched without one takes its first turn with `tools: []` and nothing
    /// anywhere reports an error, which is why the absence is a refusal rather than a fallback.
    fn mcp_env(&self, spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<BridgeEnv, HarnessError> {
        if ctx.ready_file.is_none() {
            return Err(HarnessError::MissingInput {
                harness: Harness::ClaudeCode,
                what: "a headless node's prompt is written after launch, so the bridge \
                       readiness marker is required: without it the first turn goes out with \
                       tools: [] and nothing anywhere reports an error",
            });
        }
        Ok(declared_bridge(self, spec, ctx))
    }
}

impl HarnessAdapter for ClaudeCodeAdapter {
    fn harness(&self) -> Harness {
        Harness::ClaudeCode
    }

    /// **One shape, and a non-empty prompt is a refusal on it.**
    ///
    /// This adapter used to compile two headless shapes: `--input-format stream-json` for a root
    /// whose prompt is a frame, and a positional `-p <prompt>` for a child. The second does not
    /// work and cannot be made to — measured on 2.1.220, a child launched that way reports
    /// `"tools":[],"mcp_servers":[{"name":"marion","status":"pending"}]` in its own `system/init`,
    /// takes turn one without marion's tools, is answered with the session-title stub, and exits
    /// **0 having called nothing**. There is no flag that makes the CLI wait; `MCP_TIMEOUT` does
    /// not change it. §6.1 step 8's remedy is the only one, and it *requires* a typed stdin, which
    /// is exactly what this adapter's [`Self::surfaces`] declares.
    ///
    /// So a prompt on the headless shape is a **typed refusal naming the cause**, never a launch
    /// that quietly loses its tools. The signal is the neutral vocabulary's own —
    /// [`LaunchSpec::prompt`] is *"empty for a surface whose prompt is written after launch"* —
    /// rather than a second mode flag beside it. The pane shape is not refused: a TUI takes no turn
    /// until the operator presses return, so the text is seeded and there is no race to lose
    /// ([`claude_code::SPEC`]'s `pane` row).
    ///
    /// **Live is pure removal, and this harness is the case where that is literally true.**
    /// `CLAUDE_CONFIG_DIR` is already never set (isolating it breaks OAuth — the Keychain entry is
    /// keyed to the real config dir), `--strict-mcp-config --mcp-config` already keeps the MCP
    /// declaration fileless inside marion's own agent dir, and `--setting-sources ""` already
    /// excludes the user's settings, plugins and hooks. So the only thing standing between a
    /// logged-in `claude` and marion is the three env vars marion overlays, and dropping them is
    /// the whole of live mode — which [`neutral_fields`] does for every harness at once.
    fn fields(
        &self,
        spec: &LaunchSpec,
        _ctx: &SpawnCtx,
        shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        if shape == spec::Shape::Headless && !Self::prompt_is_written_after_launch(spec) {
            return Err(HarnessError::MissingInput {
                harness: Harness::ClaudeCode,
                what: "this harness's prompt is written after launch as a user frame, never \
                       compiled into argv: 2.1.220 does not hold turn one for an --mcp-config \
                       server, so an argv prompt takes that turn with tools: [] and the run exits \
                       0 having called nothing. Leave LaunchSpec.prompt empty and write the frame \
                       after §6.1 step 8's readiness gate",
            });
        }
        // §3.1's two axes come from the trait's default `axes`: availability is the mapped list,
        // permission is *"the same list, plus marion's own `mcp__marion__*`"*. This harness is the
        // one where both axes are marion's to set and where opening only the first is a measured
        // dead end (§11 item 24).
        let mut f = neutral_fields(spec, self.axes(spec)?);
        // Claude Code wants the base URL **without** the `/v1` marion stores (it appends
        // `/v1/messages` itself); the derivation is this harness's and lives beside it.
        f.base_url = f.base_url.as_deref().map(claude_code::anthropic_base_url);
        f.mcp_config = Some(Self::mcp_config_path(spec).to_string_lossy().into_owned());
        Ok(f)
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        if spec.mcp == McpDeclaration::None {
            return Ok(Vec::new());
        }
        Ok(vec![(
            Self::mcp_config_path(spec),
            claude_code::mcp_config_document(&self.mcp_env(spec, ctx)?),
        )])
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

    /// **Under `Inherited` the declaration is compiled into argv, not written to a file** — and
    /// `CODEX_HOME` is dropped, which is what makes the login visible in the first place. The two go
    /// together: unsetting the variable points codex at the operator's `~/.codex/auth.json` *and* at
    /// the operator's `~/.codex/config.toml`, and marion is forbidden to write the second (§6.4), so
    /// the `-c` overlay is the only channel left. The row's `Each("-c", Pairs)` carries it, and the
    /// `Canned`-gated `CODEX_HOME` row drops the variable.
    ///
    /// The same fields serve both shapes: `codex::SPEC`'s pane row simply names no `--output-schema`
    /// or `--output-last-message`, so the two `exec`-only outputs are ignored there structurally
    /// rather than by a second branch that sets them `None`.
    fn fields(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        // The trait's default `axes` runs the refusal owed to a `tools:` declaration on every
        // harness; nothing in the row reads the result — see `Self::tool_name` for why a codex
        // declaration compiles no flag. Discarding the names is the honest outcome.
        let mut f = neutral_fields(spec, self.axes(spec)?);
        // Canned compiles none, so every existing contract still records `None` and the canned argv
        // is byte-identical to what it was before `-m` was known to exist here.
        f.model = match spec.auth {
            Auth::Canned => None,
            // A real endpoint serves the model it is asked for, so it must be asked.
            Auth::Inherited | Auth::Endpoint => spec.model.clone(),
        };
        // A live node reads no marion-written config, so the sandbox the contract records rides
        // `-c` first, bridge or no bridge (`codex::live_sandbox_override`).
        f.pairs = match (spec.auth, spec.mcp) {
            (Auth::Canned | Auth::Endpoint, _) => Vec::new(),
            (Auth::Inherited, McpDeclaration::None) => vec![codex::live_sandbox_override()],
            (Auth::Inherited, McpDeclaration::Marion) => {
                std::iter::once(codex::live_sandbox_override())
                    .chain(codex::live_config_overrides(&declared_bridge(
                        self, spec, ctx,
                    )))
                    .collect()
            }
        };
        Ok(f)
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        // **No file at all under `Inherited`, and that is the MUST rather than a convenience.**
        // With `CODEX_HOME` unset the only `config.toml` codex reads is `~/.codex/config.toml` —
        // the operator's own — and §6.4 forbids marion mutating it. Writing marion's document
        // *anywhere else* would simply not be read, and writing it there would clobber a login
        // marion is not even running. So the declaration moves to argv (see `compile` /
        // `mcp_route`) and this route emits nothing.
        if spec.auth == Auth::Inherited {
            // No `base_url` is demanded either: the refusal below exists because a *generated*
            // `model_providers` block pointing nowhere is an unbounded hang, and a live node
            // generates none — it uses codex's own default provider and the operator's credential.
            return Ok(Vec::new());
        }
        let base_url = spec.base_url.as_deref().ok_or(HarnessError::MissingInput {
            harness: Harness::Codex,
            what: "a model_providers entry needs a base_url; a config pointing nowhere fails as \
                   a hang, which is the worst failure to diagnose",
        })?;
        // The bridge's identity reaches a codex node's `[mcp_servers.marion]` `env` — a codex
        // **root** without it answered `spawn` with `marion: MARION_REPO is not set`. Written on
        // every canned node, declaration or not: the document is the whole of the node's config.
        let bridge = declared_bridge(self, spec, ctx);
        Ok(vec![(
            Self::config_path(spec),
            codex::config_toml(&bridge, base_url),
        )])
    }
}

/// Gemini CLI 0.53.0, headless `-p` (§6.4, fixture `tests/fixtures/s12/`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiAdapter;

impl GeminiAdapter {
    /// The settings document `GEMINI_CLI_SYSTEM_SETTINGS_PATH` names — [`gemini::SETTINGS_FILE`]
    /// under the config dir, which is exactly what [`gemini::SPEC`]'s env row renders, so `compile`
    /// and `config_files` cannot disagree about where it is.
    ///
    /// It sits *beside* `$GEMINI_CLI_HOME`, not inside `<home>/.gemini/`: the whole point of the
    /// system-settings override is that marion writes nothing under the sandbox home the CLI owns.
    fn settings_path(spec: &LaunchSpec) -> PathBuf {
        spec.config_dir.join(gemini::SETTINGS_FILE)
    }
}

impl HarnessAdapter for GeminiAdapter {
    fn harness(&self) -> Harness {
        Harness::Gemini
    }

    /// §3.1's availability axis, in the only form this harness has one: **a mode, not a list.**
    /// `mode` is `Some(auto_edit)` exactly when the declaration names an edit tool, and `None` —
    /// the default mode, which [`gemini::SPEC`] then compiles no flag for — otherwise. The marion →
    /// gemini mapping is [`Self::tool_name`]'s, and which *gemini* names need the mode is
    /// [`gemini::is_edit_tool`]'s, so neither half is restated here.
    fn axes(&self, spec: &LaunchSpec) -> Result<spec::Axes, HarnessError> {
        let tools = self.native_tools(spec)?;
        let relaxed = tools.iter().any(|t| gemini::is_edit_tool(t));
        Ok(spec::Axes {
            tools,
            allowed: Vec::new(),
            mode: relaxed.then(|| gemini::AUTO_EDIT_APPROVAL_MODE.to_string()),
        })
    }

    /// The two refusals this harness owes, and the one derivation.
    ///
    /// **The model is refused rather than defaulted.** A pinned id would be a guess marion has no
    /// basis for, and S12 measured 0.53.0 rewriting even an explicit `-m gemini-2.5-flash` to
    /// `gemini-3.5-flash` in the request path — so a "safe" default is not even reliably the model
    /// that runs. The failure it prevents is the expensive one: with model `auto` the CLI issues a
    /// classifier call to gemini-3.1-flash-lite over non-streaming `:generateContent` and hung on
    /// retry 5.
    fn fields(
        &self,
        spec: &LaunchSpec,
        _ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        if spec.model.is_none() {
            return Err(HarnessError::MissingInput {
                harness: Harness::Gemini,
                what: "an explicit -m is mandatory: with the default model `auto` the CLI first makes \
                       a classifier call that retried 5x and hung, and marion will not guess a model",
            });
        }
        if let Some(u) = &spec.base_url
            && !gemini::base_url_is_acceptable(u)
        {
            return Err(HarnessError::MissingInput {
                harness: Harness::Gemini,
                what: "GOOGLE_GEMINI_BASE_URL must be https unless the host is loopback; a \
                       non-loopback plain-http endpoint is refused by the CLI",
            });
        }
        let mut f = neutral_fields(spec, self.axes(spec)?);
        // The google-genai SDK appends its own `/v1beta/...`, so the `/v1` marion stores would be
        // doubled; the derivation is this harness's ([`gemini::google_base_url`]).
        f.base_url = f.base_url.as_deref().map(gemini::google_base_url);
        Ok(f)
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
                serde_json::to_string_pretty(&gemini::settings_json(bridge))
                    .expect("a Value always serialises")
            }
            // The row's live declaration, byte for byte: a native root gets exactly this file.
            (Auth::Inherited, Some(bridge)) => gemini::live_settings_document(bridge),
            (Auth::Inherited, None) => {
                serde_json::to_string_pretty(&gemini::live_settings_json(None))
                    .expect("a Value always serialises")
            }
        };
        Ok(vec![(Self::settings_path(spec), document)])
    }
}

/// The provider-block id an opencode endpoint node's generated config uses.
const ENDPOINT_PROVIDER_BLOCK: &str = "marion";

/// opencode 1.17.3, `run` surface (§6.4, fixture `tests/fixtures/s13/`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCodeAdapter;

impl OpenCodeAdapter {
    /// The `provider/model` pair, which both argv and the generated config name — parsed once so
    /// they cannot disagree — or, on a live node that names no model of its own, none: `-m` is
    /// left off and opencode runs on the operator's configured default, as it uses their login
    /// ([`opencode::requested_model`]). A canned or endpoint node always needs one, because the
    /// provider block marion generates names it.
    fn model_ref(spec: &LaunchSpec) -> Result<Option<opencode::ModelRef>, HarnessError> {
        let Some(m) = opencode::requested_model(spec.auth, spec.model.as_deref()) else {
            return match spec.auth {
                Auth::Inherited => Ok(None),
                Auth::Canned | Auth::Endpoint => Err(HarnessError::MissingInput {
                    harness: Harness::OpenCode,
                    what: "an explicit -m provider/model is mandatory: there is no \
                           OPENCODE_MODEL env var, so argv and the generated provider block are \
                           the only two channels",
                }),
            };
        };
        // A real endpoint is asked for its model verbatim — OpenRouter's ids carry a slash of their
        // own — under a provider block of marion's name, which merges with none of opencode's
        // built-in providers.
        if spec.auth == Auth::Endpoint {
            return Ok(Some(opencode::ModelRef {
                provider: ENDPOINT_PROVIDER_BLOCK.to_string(),
                model: m.to_string(),
            }));
        }
        opencode::ModelRef::parse(m)
            .map(Some)
            .ok_or(HarnessError::MissingInput {
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

    /// The `provider/model` pair (refused where it is not one; none on a live node that names no
    /// model of its own — [`Self::model_ref`]), the session title, and the live route's inline
    /// document.
    ///
    /// **Under `Inherited` the declaration is compiled into the env, not written to a file.** S13:
    /// auth resolves through `$XDG_DATA_HOME` and config through `$XDG_CONFIG_HOME` — two
    /// variables — so marion cannot relocate the config without also having to relocate, and
    /// therefore hide, the login. `OPENCODE_CONFIG_CONTENT` is last in the merge order and merges
    /// *over* the operator's own config, which is exactly the wrong property for isolation and
    /// exactly the right one here.
    fn fields(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        // The trait's default `axes` runs the refusal owed to a `tools:` declaration; nothing in
        // the row reads the result — see `Self::tool_name` for why a declaration here compiles
        // nothing.
        let mut f = neutral_fields(spec, self.axes(spec)?);
        f.model = Self::model_ref(spec)?.map(|m| m.qualified());
        // Any stable string suppresses the title-generation call; the node's own id makes the
        // session identifiable in `opencode session list` without leaking the prompt.
        f.title = Some(format!("marion-{}", ctx.agent_id.0));
        f.inline_config = match spec.auth {
            Auth::Canned | Auth::Endpoint => None,
            Auth::Inherited => (spec.mcp == McpDeclaration::Marion)
                .then(|| opencode::live_config_document(&declared_bridge(self, spec, ctx))),
        };
        Ok(f)
    }

    /// `marion/<model>` back to `<model>`: [`Self::model_ref`] put the provider's id under marion's
    /// own block.
    fn endpoint_model(&self, compiled: &str) -> String {
        compiled
            .strip_prefix(ENDPOINT_PROVIDER_BLOCK)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(compiled)
            .to_string()
    }

    /// **No file at all under `Inherited`** — see [`HarnessAdapter::mcp_route`], which is what keeps
    /// that from reading as "this node got no bridge".
    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        if spec.auth == Auth::Inherited {
            // And no `base_url` is demanded either: the refusal below exists because a *generated*
            // provider block pointing nowhere is an unbounded hang, and a live node generates none.
            return Ok(Vec::new());
        }
        let base_url = spec.base_url.as_deref().ok_or(HarnessError::MissingInput {
            harness: Harness::OpenCode,
            what: "the provider block needs a baseURL; without one the child resolves no provider \
                   at all, and a provider that answers nothing is an unbounded hang (S13)",
        })?;
        let bridge = (spec.mcp == McpDeclaration::Marion).then(|| declared_bridge(self, spec, ctx));
        let json = opencode::config_json(
            &opencode::ConfigSpec {
                // Canned always resolves one: `model_ref` answers `None` only under `Inherited`,
                // which returned above.
                model: Self::model_ref(spec)?.ok_or(HarnessError::MissingInput {
                    harness: Harness::OpenCode,
                    what: "a canned node needs a provider/model for the generated provider block",
                })?,
                base_url: base_url.to_string(),
                api_key: spec.api_key.clone(),
                key_header: spec.extra.key_header.unwrap_or_default(),
            },
            bridge.as_ref(),
        );
        Ok(vec![(
            opencode::config_path(&spec.config_dir),
            serde_json::to_string_pretty(&json).expect("a Value always serialises"),
        )])
    }
}

/// GitHub Copilot CLI 1.0.83, headless `-p` (fixture `tests/fixtures/s24/`).
///
/// The second harness after Claude Code where **both** of §3.1's axes are marion's to set
/// (`copilot::PromptSpec::available` and `::allow`), and the first where the two axes spell the
/// same tool differently — see [`copilot`]'s module docs. Everything else is the `LaunchOnly`
/// shape gemini and opencode take: the prompt rides argv, the declaration is a document, and the
/// stream is read after the fact.
#[derive(Debug, Clone, Copy, Default)]
pub struct CopilotAdapter;

impl CopilotAdapter {
    /// Marion's verbs out of [`LaunchSpec::allowed_tools`], which arrive in **this harness's**
    /// model-facing spelling (`marion-report`) because `run_spawn` compiles them through
    /// [`HarnessAdapter::marion_tool_name`].
    ///
    /// **Refused by name when they do not**, rather than passed through: copilot is the one
    /// `LaunchOnly` harness that compiles this list into a flag, and a list in another harness's
    /// spelling would compile to `--available-tools=mcp__marion__spawn`, which names no copilot
    /// tool and leaves the node with none of marion's verbs, at exit 0. That is the shape a root
    /// would take today — `root::ROOT_VERBS` is spelled for Claude Code — and a refusal
    /// here is what keeps it from being a silent one.
    fn marion_verbs(spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        let prefix = copilot::model_tool_name(copilot::MCP_ALIAS, "");
        spec.allowed_tools
            .iter()
            .map(|t| {
                t.strip_prefix(prefix.as_str())
                    .filter(|verb| !verb.is_empty())
                    .map(str::to_string)
                    .ok_or(HarnessError::MissingInput {
                        harness: Harness::Copilot,
                        what: "allowed_tools must carry marion's verbs in this harness's own \
                               spelling (`marion-<verb>`, from `marion_tool_name`); a list in \
                               another harness's spelling would compile into --available-tools \
                               and grant the node none of marion's tools, silently",
                    })
            })
            .collect()
    }
}

impl HarnessAdapter for CopilotAdapter {
    fn harness(&self) -> Harness {
        Harness::Copilot
    }

    /// Both of §3.1's axes, in copilot's two spellings ([`copilot`]'s module docs).
    ///
    /// `tools` is `--available-tools`: what the model **sees** — marion's verbs plus the declared
    /// built-ins. `allowed` is `--allow-tool`: what runs **without a prompt** — `<alias>(<verb>)`
    /// for each of marion's verbs, plus the kind `write` when the declaration reaches a
    /// file-writing built-in. §3.1's *"the same list, plus marion's own"*, in the pattern grammar
    /// rather than the tool grammar. **`view` adds nothing here, and that is measured**: with only
    /// `marion(report)` granted, a `view` call ran (`tests/fixtures/s24/README.md`, item 4).
    fn axes(&self, spec: &LaunchSpec) -> Result<spec::Axes, HarnessError> {
        let native = self.native_tools(spec)?;
        let mut tools = spec.allowed_tools.clone();
        tools.extend(native.iter().cloned());
        let mut allowed: Vec<String> = Self::marion_verbs(spec)?
            .iter()
            .map(|verb| copilot::permission_pattern(copilot::MCP_ALIAS, verb))
            .collect();
        if native.iter().any(|t| copilot::is_write_tool(t)) {
            allowed.push(copilot::WRITE_PERMISSION.into());
        }
        Ok(spec::Axes {
            tools,
            allowed,
            mode: None,
        })
    }

    /// The three refusals this harness owes, and where argv names the declaration document.
    fn fields(
        &self,
        spec: &LaunchSpec,
        _ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        match spec.auth {
            Auth::Canned | Auth::Endpoint => {
                // Both measured as hard refusals by the CLI itself, so marion refuses first and
                // says why. Without a base URL there is no BYOK at all and the CLI goes looking
                // for a GitHub login; without a model BYOK exits 1 before any request.
                if spec.base_url.is_none() {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Copilot,
                        what: "a canned node needs a provider base URL: COPILOT_PROVIDER_BASE_URL \
                               is what selects BYOK at all, and without it the CLI requires a \
                               GitHub login",
                    });
                }
                if spec.model.is_none() {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Copilot,
                        what: "an explicit model is mandatory under BYOK: 1.0.83 exits 1 with \
                               `BYOK providers require an explicit model` before any request, \
                               and marion will not guess one",
                    });
                }
            }
            // The canned default names marion's own endpoint's plumbing, and a live node has no
            // such endpoint — the string would go to GitHub's model routing and name nothing. The
            // same refusal opencode makes for `marion/default`, for the same reason.
            Auth::Inherited => {
                if spec.model.as_deref() == Some(agent_type::COPILOT_DEFAULT_MODEL) {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Copilot,
                        what: "that model name belongs to marion's canned test provider, which a \
                               run on your own login does not use. Pass a real Copilot model \
                               (-m <model>), or none to use Copilot's own default",
                    });
                }
            }
        }
        let mut f = neutral_fields(spec, self.axes(spec)?);
        // `--additional-mcp-config @<file>`: the `@` is how the flag reads a file, and the path is
        // the one `config_files` writes.
        f.mcp_config = (spec.mcp == McpDeclaration::Marion).then(|| {
            format!(
                "@{}",
                copilot::mcp_config_path(&spec.config_dir).to_string_lossy()
            )
        });
        Ok(f)
    }

    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        if spec.mcp != McpDeclaration::Marion {
            return Ok(Vec::new());
        }
        Ok(vec![(
            copilot::mcp_config_path(&spec.config_dir),
            copilot::mcp_config_document(&declared_bridge(self, spec, ctx)),
        )])
    }
}

/// goose 1.49.0, headless `run -t` (fixture `tests/fixtures/s26/`).
///
/// The `LaunchOnly` shape gemini and copilot take, with gemini's kind of availability axis — a
/// mode, not a list: the developer extension is loaded whole or not at all — and a declaration
/// that is one argv token rather than a document. See [`goose`]'s module docs for why the bridge's
/// node token rides the process environment and not that token.
#[derive(Debug, Clone, Copy, Default)]
pub struct GooseAdapter;

impl HarnessAdapter for GooseAdapter {
    fn harness(&self) -> Harness {
        Harness::Goose
    }

    /// §3.1's availability axis, in the only form this harness has one: **a builtin, not a
    /// list.** `mode` is `Some(developer)` exactly when the declaration names a developer-extension
    /// tool, and `None` — no `--with-builtin`, nothing but marion — otherwise.
    fn axes(&self, spec: &LaunchSpec) -> Result<spec::Axes, HarnessError> {
        let tools = self.native_tools(spec)?;
        let relaxed = tools.iter().any(|t| goose::is_developer_tool(t));
        Ok(spec::Axes {
            tools,
            allowed: Vec::new(),
            mode: relaxed.then(|| goose::DEVELOPER_BUILTIN.to_string()),
        })
    }

    /// The refusals this harness owes, the declaration token, and the environment the bridge
    /// inherits.
    fn fields(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        match spec.auth {
            Auth::Canned | Auth::Endpoint => {
                // Without a host the `openai` provider posts to api.openai.com with marion's
                // placeholder key — a real vendor call on a run premised on making none.
                if spec.base_url.is_none() {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Goose,
                        what: "a canned node needs a provider base URL: without OPENAI_HOST the \
                               openai provider posts to the vendor's own endpoint",
                    });
                }
                if spec.model.is_none() {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Goose,
                        what: "an explicit model is mandatory: GOOSE_MODEL is how the openai \
                               provider is told what to name, it has no default, and marion will \
                               not guess one",
                    });
                }
            }
            // The canned default names marion's own endpoint's plumbing; a live node has none.
            Auth::Inherited => {
                if spec.model.as_deref() == Some(agent_type::GOOSE_DEFAULT_MODEL) {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Goose,
                        what: "that model name belongs to marion's canned test provider, which a \
                               run on your own login does not use. Pass a real model (-m <model>), \
                               or none to use the harness's own default",
                    });
                }
            }
        }
        let mut f = neutral_fields(spec, self.axes(spec)?);
        if spec.mcp == McpDeclaration::Marion {
            let bridge = declared_bridge(self, spec, ctx);
            // goose splits the token on whitespace; a path it would split is refused by name
            // rather than declared as two arguments.
            if let Some(token) = goose::unspellable(&bridge) {
                return Err(HarnessError::Unspellable {
                    harness: Harness::Goose,
                    what: format!(
                        "`--with-extension` is split on whitespace, and {token:?} contains some"
                    ),
                });
            }
            f.mcp_config = Some(goose::extension_declaration(&bridge));
        }
        Ok(f)
    }

    /// No file at all — see [`HarnessAdapter::mcp_route`], which is what keeps that from reading
    /// as "this node got no bridge".
    fn config_files(
        &self,
        _spec: &LaunchSpec,
        _ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        Ok(Vec::new())
    }
}

/// cline 3.0.61, headless `--json <prompt>` (fixture `tests/fixtures/s27/`).
///
/// opencode's shape: the provider is a document marion writes, the tool set is the harness's own
/// and unconstrained, and the declaration is a document too. See [`cline`]'s module docs for why
/// the provider document may sit in exactly one place and why the isolation is flags and
/// variables together.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClineAdapter;

impl ClineAdapter {
    /// The `providers.json` values, or the refusal each missing one is owed.
    fn provider(spec: &LaunchSpec) -> Result<cline::ProviderSpec, HarnessError> {
        // A document with no base URL would select the vendor's own endpoint through cline's
        // default — s27 item 14 measured a missing provider file as exactly that fallback.
        let base_url = spec.base_url.clone().ok_or(HarnessError::MissingInput {
            harness: Harness::Cline,
            what: "a canned node needs a provider base URL: providers.json is the only provider \
                   selection cline reads, and without one it falls back to its own vendor with no \
                   credential (s27 item 14)",
        })?;
        let model = spec.model.clone().ok_or(HarnessError::MissingInput {
            harness: Harness::Cline,
            what: "an explicit model is mandatory: providers.json names one, and marion will not \
                   guess it",
        })?;
        let api_key = spec.api_key.clone().ok_or(HarnessError::MissingInput {
            harness: Harness::Cline,
            what: "a canned node needs a credential: providers.json carries apiKey as a string and \
                   an absent one is not a spelling the document has",
        })?;
        Ok(cline::ProviderSpec {
            model,
            base_url,
            api_key,
        })
    }
}

impl HarnessAdapter for ClineAdapter {
    fn harness(&self) -> Harness {
        Harness::Cline
    }

    /// The refusals this harness owes. Nothing else: the row's argv reads the launch verbatim, the
    /// provider is a document, and the declaration rides an env-named document.
    fn fields(
        &self,
        spec: &LaunchSpec,
        _ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        match spec.auth {
            Auth::Canned | Auth::Endpoint => {
                Self::provider(spec)?;
            }
            // The canned default names marion's own endpoint's plumbing; a live node has none. Any
            // other model rides `-m`, measured to win over the operator's `providers.json` (s27
            // item 15); none at all leaves the operator's own.
            Auth::Inherited => {
                if spec.model.as_deref() == Some(agent_type::CLINE_DEFAULT_MODEL) {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Cline,
                        what: "that model name belongs to marion's canned test provider, which a \
                               run on your own login does not use. Pass a real model (-m <model>), \
                               or none to use providers.json's own",
                    });
                }
            }
        }
        // The trait's default `axes` runs the refusal owed to a `tools:` declaration; nothing in
        // the row reads the result — see `Self::tool_name`'s row entry for why a declaration
        // compiles nothing.
        Ok(neutral_fields(spec, self.axes(spec)?))
    }

    /// The MCP document **first** — [`McpRoute::Document`] reads the first file as the
    /// declaration — then, under `Canned`, the provider document at the one path cline reads it.
    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        let mut files = Vec::new();
        if spec.mcp == McpDeclaration::Marion {
            files.push((
                cline::mcp_settings_path(&spec.config_dir),
                cline::mcp_settings_document(&declared_bridge(self, spec, ctx)),
            ));
        }
        if spec.auth.overlays() {
            files.push((
                cline::providers_path(&spec.config_dir),
                serde_json::to_string_pretty(&cline::providers_json(&Self::provider(spec)?))
                    .expect("a Value always serialises"),
            ));
        }
        Ok(files)
    }
}

/// qwen 0.23.0, headless `-p` (fixture `tests/fixtures/s25/`).
///
/// Claude Code's shape over an OpenAI provider: both of §3.1's axes are one list — what
/// `--core-tools` names is both what the model sees and what runs under `--yolo` — and the
/// declaration is one argv token. See [`qwen`]'s module docs for the deferred-discovery switch and
/// the twelve exempt survivors.
#[derive(Debug, Clone, Copy, Default)]
pub struct QwenAdapter;

impl HarnessAdapter for QwenAdapter {
    fn harness(&self) -> Harness {
        Harness::Qwen
    }

    /// One list on both axes: marion's verbs (already in this harness's spelling) plus the declared
    /// built-ins, as `--core-tools` names them — and the audit record is that same list.
    ///
    /// **Never empty.** `--core-tools` with no names is silently no allowlist at all: the run
    /// starts, nothing warns, and the model is offered every built-in but the twelve excluded
    /// (`qwen-core-tools-empty.provider-request-1.json`). A launch with no marion verb and no
    /// declaration is refused by name rather than compiled into that.
    fn axes(&self, spec: &LaunchSpec) -> Result<spec::Axes, HarnessError> {
        let mut tools = spec.allowed_tools.clone();
        tools.extend(self.native_tools(spec)?);
        if tools.is_empty() {
            return Err(HarnessError::MissingInput {
                harness: Harness::Qwen,
                what: "a launch must name at least one tool: `--core-tools` with no names is \
                       silently no allowlist, and the model would be offered every built-in \
                       (s25 item 16)",
            });
        }
        Ok(spec::Axes {
            allowed: tools.clone(),
            tools,
            mode: None,
        })
    }

    /// The refusals this harness owes, and the declaration token.
    fn fields(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        match spec.auth {
            Auth::Canned | Auth::Endpoint => {
                // Without a base URL the OpenAI provider posts to the vendor with marion's
                // placeholder key; without a model it has nothing to name.
                if spec.base_url.is_none() {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Qwen,
                        what: "a canned node needs a provider base URL: without OPENAI_BASE_URL \
                               the provider posts to the vendor's own endpoint",
                    });
                }
                if spec.model.is_none() {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Qwen,
                        what: "an explicit model is mandatory: OPENAI_MODEL is how the provider is \
                               told what to name, and marion will not guess one",
                    });
                }
            }
            Auth::Inherited => {
                if spec.model.as_deref() == Some(agent_type::QWEN_DEFAULT_MODEL) {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Qwen,
                        what: "that model name belongs to marion's canned test provider, which a \
                               run on your own login does not use. Pass a real model (-m <model>), \
                               or none to use the harness's own default",
                    });
                }
            }
        }
        let mut f = neutral_fields(spec, self.axes(spec)?);
        // **Under `Inherited` the declaration is compiled onto argv, not written to a file** —
        // codex's arrangement, for codex's reason: a live node's settings document is the
        // operator's own, and `--mcp-config` was measured carrying the same block inline.
        f.mcp_config = (spec.auth == Auth::Inherited && spec.mcp == McpDeclaration::Marion)
            .then(|| qwen::mcp_config_document(&declared_bridge(self, spec, ctx)));
        Ok(f)
    }

    /// Under `Canned`, the settings document: the memory side turn switched off, and the
    /// declaration where one was asked for. **Nothing at all under `Inherited`** — see
    /// [`HarnessAdapter::mcp_route`], which is what keeps that from reading as "no bridge".
    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        if spec.auth == Auth::Inherited {
            return Ok(Vec::new());
        }
        let bridge = (spec.mcp == McpDeclaration::Marion).then(|| declared_bridge(self, spec, ctx));
        Ok(vec![(
            qwen::settings_path(&spec.config_dir),
            serde_json::to_string_pretty(&qwen::settings_json(bridge.as_ref()))
                .expect("a Value always serialises"),
        )])
    }
}

/// agy 1.2.8, headless `-p` (fixture `tests/fixtures/s32/`). See [`antigravity`]'s module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct AntigravityAdapter;

impl HarnessAdapter for AntigravityAdapter {
    fn harness(&self) -> Harness {
        Harness::Antigravity
    }

    /// A mode, not a list, like gemini's: `accept-edits` exactly when an edit tool is declared,
    /// and agy's default otherwise — under which a headless edit is auto-denied (s32).
    fn axes(&self, spec: &LaunchSpec) -> Result<spec::Axes, HarnessError> {
        let tools = self.native_tools(spec)?;
        let relaxed = tools.iter().any(|t| antigravity::is_edit_tool(t));
        Ok(spec::Axes {
            tools,
            allowed: Vec::new(),
            mode: relaxed.then(|| antigravity::ACCEPT_EDITS_MODE.to_string()),
        })
    }

    /// The refusal this harness owes, the root, and the working-directory preamble.
    ///
    /// **Canned and endpoint are refused by name.** agy has no measured way to point it at another
    /// provider (an API-key route would need the operator's profile relocated, which loses the
    /// keychain login), so a "canned" node would spend the operator's real quota under a mode
    /// that promises it spends nothing, and an endpoint node would bill the operator's login
    /// instead of the provider it names.
    fn fields(
        &self,
        spec: &LaunchSpec,
        _ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        if spec.auth.overlays() {
            return Err(HarnessError::MissingInput {
                harness: Harness::Antigravity,
                what: "agy has no canned or endpoint provider route: it runs only on the \
                       operator's own login, so launch it without --canned or a provider",
            });
        }
        let mut f = neutral_fields(spec, self.axes(spec)?);
        if spec.mcp == McpDeclaration::Marion {
            let root = antigravity::root_dir(&spec.config_dir);
            f.prompt = format!(
                "{}{}",
                antigravity::working_directory_preamble(&spec.cwd, &root),
                f.prompt
            );
            f.mcp_config = Some(root.to_string_lossy().into_owned());
        }
        Ok(f)
    }

    /// The root's one document, where a declaration was asked for — the row's live declaration,
    /// byte for byte.
    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        if spec.mcp == McpDeclaration::None {
            return Ok(Vec::new());
        }
        Ok(vec![(
            antigravity::root_dir(&spec.config_dir).join(antigravity::MCP_CONFIG_FILE),
            antigravity::mcp_config_document(&declared_bridge(self, spec, ctx)),
        )])
    }
}

/// pi 0.80.2, headless `-p --mode json` (fixture `tests/fixtures/s34-pi/`).
///
/// qwen's arrangement of the axes — `--tools` is both what the model is offered and all it may run,
/// since pi has no approval surface — with a declaration that is marion's own extension file. See
/// [`pi`]'s module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct PiAdapter;

impl HarnessAdapter for PiAdapter {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    /// One list on both axes: marion's verbs (already in this harness's spelling) plus the declared
    /// built-ins. An empty list is legitimate here, since `--tools ""` offers nothing
    /// (`pi-empty-tools.*`), so unlike qwen there is nothing to refuse.
    fn axes(&self, spec: &LaunchSpec) -> Result<spec::Axes, HarnessError> {
        let mut tools = spec.allowed_tools.clone();
        tools.extend(self.native_tools(spec)?);
        Ok(spec::Axes {
            allowed: tools.clone(),
            tools,
            mode: None,
        })
    }

    /// The refusals this harness owes, and the extension path.
    fn fields(
        &self,
        spec: &LaunchSpec,
        _ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        match spec.auth {
            Auth::Canned => {
                if spec.base_url.is_none() || spec.model.is_none() {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Pi,
                        what: "a canned node needs a provider base URL and an explicit model: \
                               both are fields of the models.json marion writes, and marion will \
                               not write a provider that points nowhere or names no model",
                    });
                }
            }
            Auth::Endpoint => {
                return Err(HarnessError::MissingInput {
                    harness: Harness::Pi,
                    what: "pi has no measured endpoint recipe: launch it canned or on your own \
                           login",
                });
            }
            Auth::Inherited => {
                if spec.model.as_deref() == Some(agent_type::PI_DEFAULT_MODEL) {
                    return Err(HarnessError::MissingInput {
                        harness: Harness::Pi,
                        what: "the built-in default model names marion's canned endpoint, which a \
                               --live node does not talk to. Name a real model instead \
                               (marion run --live -m …), or none to keep pi's own default",
                    });
                }
            }
        }
        let mut f = neutral_fields(spec, self.axes(spec)?);
        f.mcp_config = (spec.mcp == McpDeclaration::Marion).then(|| {
            pi::extension_path(&spec.config_dir)
                .to_string_lossy()
                .into_owned()
        });
        Ok(f)
    }

    /// The extension first, where a declaration was asked for — it is what
    /// [`McpRoute::Document`] checks — then, under `Canned`, the `models.json` the relocated agent
    /// dir reads. Both are marion's own files under marion's own directory, so live mode writes the
    /// extension too and nothing of the operator's.
    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        let mut files = Vec::new();
        if spec.mcp == McpDeclaration::Marion {
            files.push((
                pi::extension_path(&spec.config_dir),
                pi::extension_source(&declared_bridge(self, spec, ctx)),
            ));
        }
        if spec.auth == Auth::Canned {
            let missing = HarnessError::MissingInput {
                harness: Harness::Pi,
                what: "a canned models.json needs a base URL, a key and a model",
            };
            let (Some(url), Some(model)) = (spec.base_url.as_deref(), spec.model.as_deref()) else {
                return Err(missing);
            };
            // pi lists a model only when its provider has a key; the canned endpoint ignores it.
            let key = spec
                .api_key
                .as_ref()
                .map_or("marion-canned", |k| k.expose());
            files.push((
                pi::models_path(&spec.config_dir),
                serde_json::to_string_pretty(&pi::models_json(url, key, model))
                    .expect("a Value always serialises"),
            ));
        }
        Ok(files)
    }
}

/// §5.2's `acp` row: **one adapter, many agents** (§9's M5).
///
/// # What the surfaces are, and why
///
/// [`crate::acp::surfaces`] — `Typed(Acp)` control, `StructuredUi` display, `ProtocolEvents`
/// observation — and each axis is a separate decision:
///
/// * **Control is `Typed(Acp)`.** ACP is a bidirectional JSON-RPC session with `session/prompt`,
///   `session/cancel` and `session/load`, so marion can address a turn rather than merely write
///   bytes at one. That is §3.4's own definition of `Typed`, and it is what separates this from
///   opencode's `LaunchOnly` row: the same vendor's binary, one surface up, and the reason §3.3
///   keys on surfaces at all. S21 exercised the plane in both directions on a live agent — marion
///   sent three requests and *answered* the agent's own MCP traffic mid-turn.
/// * **Display is `StructuredUi`, not `NativePty`.** marion owns no terminal for an ACP agent: the
///   output arrives as `session/update` frames, which is something marion renders, not something a
///   VT grid receives. `NativePty` would mint the node a [`crate::PtyWitness`] and put it one call
///   from `spawn_pty` (§11 item 1), for a process that has no terminal at all. So
///   [`Self::pane_surfaces`] is `None` too, and a pane request is refused by name.
/// * **Observation is `ProtocolEvents` alone.** The frames are the record. There is no transcript
///   file marion knows how to find — that is per-agent, and this adapter is per-protocol — and
///   there are no terminal bytes.
///
/// # What the ceiling then permits, and what is actually claimed
///
/// Typed + `ProtocolEvents` is the most permissive point in §3.4's cross-product: its
/// [`Capabilities::ceiling`](crate::Capabilities::ceiling) is all ten. That makes the ceiling
/// **useless as a limit here**, which is precisely why ACP is the one row where §3.3's stage two
/// does the work: [`crate::advertised`] claims only what has been measured of the protocol, and
/// [`crate::AgentHandshake::refine`] narrows that to the agent that actually answered. Nothing in
/// this adapter publishes a capability; see `caps.rs`'s `Harness::Acp` arm for the five that are
/// claimed and the five that are not.
/// # Baseline and refinement, and why the struct has a field
///
/// **Any agent that speaks ACP over stdio runs through this adapter with no row naming it.** The
/// protocol supplies what a per-harness row supplies elsewhere: identity arrives in `initialize`,
/// the bridge is declared in `session/new`'s `mcpServers`, and the agent's spelling of marion's
/// verbs is read off its own `tool_call` frames ([`acp::Reading::Generic`] — four agents have been
/// watched spelling `report` four ways, and every one of them spelled the same `<server> <verb>`
/// pair). An [`acp::Agent`] row is a **refinement** over that path: a pinned spelling, a canned
/// recipe, a measured quirk. [`acp::Binding`] is the layering made a value, and it is what this
/// adapter is bound to.
///
/// The binding rides the adapter rather than the spec because [`HarnessAdapter::marion_tool_name`]
/// and the stream readers take no [`LaunchSpec`], and their answers are per agent. [`Self::unbound`]
/// is the protocol-level adapter — what [`adapter_for`] hands back from a [`Harness`] alone, enough
/// for the surface questions (`surfaces`, `mcp_route`) that have no per-agent answer — and it
/// **cannot be launched**: every method that would put marion's verbs in front of a model refuses it
/// by name. [`adapter_for_type`] is the seam that binds one.
#[derive(Debug, Clone)]
pub struct AcpAdapter {
    /// `None` on the protocol-level adapter; see the type's doc comment.
    binding: Option<acp::Binding>,
}

impl AcpAdapter {
    /// The adapter for the ACP **protocol**, bound to no agent. Unlaunchable by construction.
    pub fn unbound() -> Self {
        Self { binding: None }
    }

    /// The adapter for one refinement row.
    pub fn for_agent(agent: acp::Agent) -> Self {
        Self::bound(acp::Binding::refined(agent))
    }

    /// The adapter for one resolved binding — a row or a generic command.
    pub fn bound(binding: acp::Binding) -> Self {
        Self {
            binding: Some(binding),
        }
    }

    /// Resolve an operator's selector — a row's id or a command line — into a bound adapter. The
    /// one refusal is a selector with no program in it; see [`acp::Binding::resolve`].
    pub fn resolve(selector: &str) -> Result<Self, HarnessError> {
        acp::Binding::resolve(selector)
            .map(Self::bound)
            .map_err(|e| HarnessError::AcpAgent(e.to_string()))
    }

    /// The binding this launch names, or the refusal. **The one place the selector is resolved**,
    /// so `compile` and `session_declaration` cannot disagree about which agent is being launched.
    ///
    /// The spec's `acp_agent` and the adapter's own binding must resolve to the **same** binding.
    /// They are two routes to one answer — an agent type names a selector, and the supervisor binds
    /// an adapter from it — and a launch in which they disagree is one where marion would compile
    /// agent A's argv and read agent B's spelling out of the transcript. That is exactly the class
    /// of bug the `HarnessAdapter` seam was introduced to end, so it is a refusal rather than a
    /// precedence rule.
    fn binding(&self, spec: &LaunchSpec) -> Result<&acp::Binding, HarnessError> {
        let selector = spec
            .extra
            .acp_agent
            .as_deref()
            .ok_or(HarnessError::MissingInput {
                harness: Harness::Acp,
                what: "`acp` is a protocol, not a program: one adapter serves many agents and \
                       marion may not choose one for the operator (§6.4). Name it in the agent \
                       type's `acp_agent` — a refinement row's id, or the agent's command",
            })?;
        let named =
            acp::Binding::resolve(selector).map_err(|e| HarnessError::AcpAgent(e.to_string()))?;
        match &self.binding {
            None => Err(HarnessError::MissingInput {
                harness: Harness::Acp,
                what: "this adapter was built from the harness name alone, so it carries no ACP \
                       agent: no argv to launch and no reading for its transcript. Bind it with \
                       `adapter_for_type`",
            }),
            Some(bound) if *bound != named => Err(HarnessError::AcpAgent(format!(
                "this adapter is bound to `{}` and the launch names `{}`; marion will not compile \
                 one agent's argv and read another's tool spelling",
                bound.selector(),
                named.selector()
            ))),
            Some(bound) => Ok(bound),
        }
    }

    /// How this adapter reads a transcript: the bound agent's measured spelling, the baseline for
    /// an unmeasured or unnamed one, and `None` only on the unbound adapter.
    fn reading(&self) -> Option<acp::Reading> {
        self.binding.as_ref().map(acp::Binding::reading)
    }
}

impl HarnessAdapter for AcpAdapter {
    fn harness(&self) -> Harness {
        Harness::Acp
    }

    /// argv is the agent's own, verbatim as S20 launched it, and **nothing else is compiled into
    /// it** — [`acp::SPEC`]'s one row splices [`spec::Field::AgentArgs`] and names no program of
    /// its own, because the program is the agent's.
    ///
    /// * *No prompt.* It rides `session/prompt` after the handshake, so `spec.prompt` reaches argv
    ///   on no ACP agent — the claude-code situation, one protocol over.
    /// * *No model on argv.* S21's `session/new` result carries a `configOptions` `model`
    ///   **select**: the model is chosen inside the session, and marion has measured no argv that
    ///   sets it. The compiled [`Invocation::model`] is the requested one because the ACP driver
    ///   applies it over the protocol (`session/set_config_option`, measured on `opencode acp`
    ///   1.18.32) and refuses the run by name where the agent offers no model select or not that
    ///   model, so the record never names a model that did not run. Under the canned recipe the
    ///   config document's `model` key carries it too, and the session then already has it.
    /// * *No credential.* Under [`Auth::Canned`] marion would have to point the agent at its own
    ///   endpoint, and there is no ACP-level way to do that — it is per-agent config, and this
    ///   adapter is per-protocol. Refused by name rather than launched at the operator's real
    ///   provider while the contract records a canned one.
    fn fields(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
        _shape: spec::Shape,
    ) -> Result<spec::Fields, HarnessError> {
        let binding = self.binding(spec)?;
        // For the refusal only: ACP has no availability axis — see `Self::tool_name`.
        let mut f = neutral_fields(spec, self.axes(spec)?);
        let (program, args) = binding
            .argv()
            .split_first()
            .expect("a binding always names a program");
        f.program = Some(program.to_string());
        f.agent_args = args.to_vec();
        // **The one refinement that touches argv.** A row measured to ignore the protocol's
        // declaration channel (S25: copilot 1.0.83) gets the bridge on its own flag instead, as the
        // same document marion's copilot adapter writes for `copilot -p` — and
        // [`Self::session_declaration`] then leaves the session block empty, so a version that one
        // day honours `mcpServers` does not start a second bridge. The generic path never reaches
        // this arm: it has no row, so it has only the protocol's channel.
        if let (acp::Declaration::Argv { flag, .. }, McpDeclaration::Marion) =
            (binding.declaration(), spec.mcp)
        {
            f.agent_args.push(flag.to_string());
            f.agent_args
                .push(copilot::mcp_config_json(&declared_bridge(self, spec, ctx)).to_string());
        }
        // A resume rides `session/load` ([`Self::session_declaration`]), never argv: the row has no
        // `Arg::Resume` and the renderer would otherwise refuse the launch as one it cannot name a
        // session on. The session *is* named — on the request, which is where this protocol puts
        // it.
        f.resume = None;
        (f.extra_env, f.model) = match (spec.auth, binding.canned()) {
            // opencode's own rows, over the same binary, under **every** mode — they gate
            // themselves on auth, exactly as they do for `opencode run`: canned and endpoint, the
            // relocations, the hygiene and `PWD` (placement, not isolation — S13 measured a child
            // re-entering `$PWD` whatever it was `chdir`'d to); live, the hygiene and `PWD` only, so the
            // operator's own login and config stay where opencode finds them. The inline document
            // is the ACP one ([`opencode::acp_session_document`]): the bridge rides `session/new`,
            // and only its call timeout has to be carried beside it.
            (_, Some(acp::CannedRecipe::OpencodeConfigDocument)) => {
                f.inline_config = Some(opencode::acp_session_document());
                let mut env = spec::render_env(opencode::SPEC.env, &f);
                // And opencode's no-self-update switch, which is that row's policy rather than one
                // of its `Env` rows — the same binary, one subcommand over, updates itself the same
                // way.
                env.extend(opencode::SPEC.updates.env());
                // And, live, no session model where only the type's plumbing default was given:
                // the session keeps the operator's own.
                let model = opencode::requested_model(spec.auth, spec.model.as_deref());
                (env, model.map(str::to_string))
            }
            // The requested model rides the session, not argv: `acp_child` sets it through ACP's
            // `session/set_config_option` and refuses the run by name where the agent offers no
            // such model, so recording it here names what ran.
            (Auth::Inherited, None) => (Vec::new(), spec.model.clone()),
            // **Refused by name, per agent — and this is the one thing the baseline cannot do.**
            // The protocol has no provider channel anywhere in its handshake, so a generic agent
            // has no canned recipe by construction, and most refinement rows have none measured
            // either. Launching them anyway would point the operator's real credential at a vendor
            // while the contract records a canned run — §6.7's audit record asserting something
            // that never happened.
            (Auth::Canned | Auth::Endpoint, None) => {
                return Err(HarnessError::MissingInput {
                    harness: Harness::Acp,
                    what: "ACP names no provider, base URL or credential at any point in its \
                           handshake, and marion has never measured a way to point this \
                           particular agent at one. Run it without --canned, on your own login, \
                           or pick an agent whose canned recipe is measured",
                });
            }
        };
        // The agent type's approval mode rides the session too, set by the driver after the model.
        f.session_mode = spec.extra.approval_mode.clone();
        Ok(f)
    }

    /// The row's constraint, plus the session mode where the launch sets one: the driver refuses a
    /// run whose agent does not offer it, so the record names a mode the session really ran in.
    fn compiled_permissions(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        self.binding(spec)?;
        // The refusal owed to a `tools:` declaration, as the default runs it.
        self.axes(spec)?;
        let mut out = vec![acp::NO_TOOL_AVAILABILITY_SURFACE.to_string()];
        out.extend(
            spec.extra
                .approval_mode
                .as_ref()
                .map(|m| format!("{}{m}", acp::SESSION_MODE_PREFIX)),
        );
        Ok(out)
    }

    /// **No declaration document, on any ACP agent** — and, under a canned provider, one document
    /// that declares nothing.
    ///
    /// Where the other four write marion's *bridge* into a config file under `spec.config_dir`,
    /// ACP's only declaration channel is `session/new` ([`Self::session_declaration`]), and an
    /// empty `mcp` block here is what [`McpRoute::Session`] exists to keep from reading as *"this
    /// node got no bridge"*. That is still true of every byte below: the document this writes
    /// under [`acp::CannedRecipe::OpencodeConfigDocument`] carries a **provider**, and no `mcp`
    /// key at all, because the bridge rides the session and the endpoint cannot.
    fn config_files(
        &self,
        spec: &LaunchSpec,
        _ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError> {
        let binding = self.binding(spec)?;
        if !spec.auth.overlays() {
            return Ok(Vec::new());
        }
        match binding.canned() {
            None => Ok(Vec::new()),
            Some(acp::CannedRecipe::OpencodeConfigDocument) => {
                let model = spec
                    .model
                    .as_deref()
                    .and_then(opencode::ModelRef::parse)
                    .ok_or(HarnessError::MissingInput {
                        harness: Harness::Acp,
                        what: "this agent's canned recipe is opencode's config document, whose \
                               `model` key is the only channel that reaches an ACP session — ACP \
                               chooses the model inside the session and marion has measured no \
                               argv that sets it. Name a `provider/model` pair",
                    })?;
                let base_url = spec.base_url.clone().ok_or(HarnessError::MissingInput {
                    harness: Harness::Acp,
                    what: "a canned run points the agent at marion's own endpoint, and no base \
                           URL was given",
                })?;
                Ok(vec![(
                    opencode::config_path(&spec.config_dir),
                    format!(
                        "{:#}\n",
                        opencode::config_json(
                            &opencode::ConfigSpec {
                                model,
                                base_url,
                                api_key: spec.api_key.clone(),
                                key_header: Default::default(),
                            },
                            // **No `mcp` block.** marion's bridge is declared in `session/new`, and
                            // declaring it here as well would start a second copy of it.
                            None,
                        )
                    ),
                )])
            }
        }
    }

    /// The `session/new` request, with marion's bridge declared as a stdio MCP server.
    ///
    /// The env block is [`BridgeEnv::pairs`] — the same derivation every other declaration
    /// serialises — because the process on the other end is the same `marion-supervisor mcp`
    /// bridge reading the same variables. A second spelling here would be a second thing to keep
    /// true.
    fn session_declaration(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Option<serde_json::Value>, HarnessError> {
        self.binding(spec)?;
        // No spelling gate here any more. This method *is* marion putting its verbs in front of
        // the agent, and it used to refuse an agent nobody had watched call a tool (s14: an
        // unknown tool name is silently ignored). That gate guarded a *compiled* spelling — and
        // on this row nothing is compiled: ACP has no availability axis, the prompt rides
        // `session/prompt` verbatim, and the agent presents the bridge's tools to its model under
        // whatever name it likes. What marion has to get right is the *reading*, and
        // `acp::Reading::Generic` reads the `<server> <verb>` pair in any spelling.
        //
        // The bridge's contract, verbatim — the same pairs every document-shaped declaration
        // carries — because the process on the other end is the same `marion-supervisor mcp`.
        let servers = match (spec.mcp, self.binding(spec)?.declaration()) {
            (McpDeclaration::None, _) => Vec::new(),
            // Declared on argv by `fields`; an entry here too would be the second bridge the
            // `Declaration::Argv` doc names.
            (McpDeclaration::Marion, acp::Declaration::Argv { .. }) => Vec::new(),
            (McpDeclaration::Marion, acp::Declaration::Session) => vec![acp::McpServerDecl {
                name: acp::MCP_SERVER_NAME.into(),
                command: ctx.bridge.clone(),
                args: ctx.bridge_args.clone(),
                env: declared_bridge(self, spec, ctx).pairs(),
            }],
        };
        match &spec.resume {
            // **A resume is `session/load`, not a flag** — the row's `resume` grammar is `None`
            // for exactly this reason. The same declaration rides it, because the agent's MCP
            // processes did not survive the agent. Built even under `McpDeclaration::None`, since
            // the session to continue is something only this request can say.
            Some(session) => Ok(Some(acp::session_load_request(
                SESSION_NEW_ID,
                session,
                &spec.cwd,
                &servers,
            ))),
            None if servers.is_empty() => Ok(None),
            None => Ok(Some(acp::session_new_request(
                SESSION_NEW_ID,
                &spec.cwd,
                &servers,
            ))),
        }
    }

    /// The protocol row's carrier — the token rides `session/new` beside the node's identity —
    /// except on a binding whose refinement declares the bridge on argv, whose own carrier then
    /// withholds it. An unresolvable binding answers for the protocol; `compile` refuses it anyway.
    fn token_carrier(&self, spec: &LaunchSpec) -> spec::TokenCarrier {
        match self.binding(spec).map(acp::Binding::declaration) {
            Ok(acp::Declaration::Argv { token, .. }) => token,
            Ok(acp::Declaration::Session) | Err(_) => self.spec().token.for_auth(spec.auth),
        }
    }

    /// The row's route — `session/new`'s block — except on a binding whose refinement measured the
    /// bridge reaching the agent on argv, where it is that flag, verified against the compiled argv
    /// exactly as codex's `-c` overrides are. The unbound adapter answers for the protocol.
    fn mcp_route(&self, spec: &LaunchSpec) -> McpRoute {
        let declaration = self
            .binding
            .as_ref()
            .map(acp::Binding::declaration)
            .unwrap_or(acp::Declaration::Session);
        match (spec.mcp, declaration) {
            (McpDeclaration::None, _) => McpRoute::None,
            (McpDeclaration::Marion, acp::Declaration::Argv { flag, .. }) => McpRoute::Argv(flag),
            (McpDeclaration::Marion, acp::Declaration::Session) => {
                McpRoute::Session(acp::MCP_SERVERS_KEY)
            }
        }
    }

    fn parse_stream(&self, stdout: &str, exit: ChildExit) -> StreamOutcome {
        match self.reading() {
            Some(r) => acp::parse_stream(stdout, exit, r),
            None => StreamOutcome::default(),
        }
    }

    /// **This agent's** spelling where one was measured, and the baseline where none was.
    ///
    /// | agent | this returns |
    /// |---|---|
    /// | `opencode acp` 1.17.3 | `marion_report` |
    /// | `claude-agent-acp` 0.66.0 | `mcp__marion__report` |
    /// | `codex-acp` 1.1.14 | `mcp.marion.report` |
    /// | `copilot --acp` 1.0.83 | `marion-report` |
    /// | any other agent | [`acp::GENERIC_SPELLING`] |
    ///
    /// On this row the answer reaches no model: ACP has no availability axis
    /// ([`acp::NO_TOOL_AVAILABILITY_SURFACE`]) and the prompt rides `session/prompt`, so the string
    /// is a record and never a compiled constraint. The transcript is read with the whole
    /// [`acp::Reading`], not with this string, which is what makes a generic agent readable.
    ///
    /// The unbound adapter gets [`acp::UNBOUND_TOOL_NAME`] — a string that is not a tool name in
    /// any spelling and matches nothing in any transcript. It is unreachable from a launch:
    /// `compile` refuses an unbound adapter by name before anything is put in front of a model.
    /// The row's typed turn, with the bound agent's measured [`spec::MidTurn`] where a refinement
    /// row carries one: opencode and claude-agent-acp fold, codex-acp and copilot queue, and an
    /// unmeasured agent keeps the row's queue.
    fn turn_delivery(&self, shape: spec::NodeShape) -> spec::TurnDelivery {
        match spec::delivery_for(self.spec(), shape) {
            spec::TurnDelivery::TypedTurn { mid_turn, note } => spec::TurnDelivery::TypedTurn {
                mid_turn: self
                    .binding
                    .as_ref()
                    .and_then(acp::Binding::mid_turn)
                    .unwrap_or(mid_turn),
                note,
            },
            other => other,
        }
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        match self.reading() {
            Some(r) => r.spell(tool),
            None => format!("{}{tool}", acp::UNBOUND_TOOL_NAME),
        }
    }

    fn marion_calls(&self, stdout: &str) -> Vec<MarionCall> {
        self.reading()
            .map(|r| acp::marion_calls(stdout, r))
            .unwrap_or_default()
    }

    /// The protocol's own `session/prompt` response shape ([`acp::USAGE`]), whatever the agent and
    /// whether or not one is bound: spend is a protocol fact, not an agent refinement.
    fn usage_rule(&self) -> Option<&'static grammar::UsageRule> {
        Some(&acp::USAGE)
    }

    /// The protocol's own `session/new` answer ([`acp::SESSION`]), whatever the agent: where a
    /// session is named is a protocol fact, as spend is.
    fn session_id(&self, frame: &serde_json::Value) -> Option<String> {
        grammar::session_in(&acp::SESSION, frame)
    }

    /// The protocol's `session/update` frames ([`acp::ACTIVITY`]), whatever the agent: every
    /// agent names its calls and streams its words through the same updates.
    fn activity(&self) -> Option<&'static grammar::ActivityRule> {
        Some(&acp::ACTIVITY)
    }
}

/// The JSON-RPC id [`AcpAdapter::session_declaration`] stamps on its request. A constant so the
/// value a driver must correlate its answer against is stated once, in the module that builds it.
pub const SESSION_NEW_ID: u64 = 1;

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
        Harness::Copilot => Ok(Box::new(CopilotAdapter)),
        Harness::Goose => Ok(Box::new(GooseAdapter)),
        Harness::Cline => Ok(Box::new(ClineAdapter)),
        Harness::Qwen => Ok(Box::new(QwenAdapter)),
        Harness::Antigravity => Ok(Box::new(AntigravityAdapter)),
        Harness::Pi => Ok(Box::new(PiAdapter)),
        // The **protocol** row, bound to no agent. Enough for every question a harness name can
        // answer — the surfaces, the declaration route, the ceiling — and unlaunchable, because a
        // harness name is not enough to say what a model will call marion's verbs. See
        // [`AcpAdapter`] and [`adapter_for_type`].
        Harness::Acp => Ok(Box::new(AcpAdapter::unbound())),
    }
}

/// The adapter for a resolved **agent type**, which is what a launch actually has.
///
/// Four of the five harnesses ignore the second argument entirely: their adapter is a property of
/// the harness. ACP's is not — §5.2's `acp` row is one adapter over many agents, each with its own
/// argv and its own name for marion's verbs — so this is the seam where an agent type's
/// `acp_agent` becomes behaviour: a refinement row's id binds that row, any other command binds
/// the generic path ([`acp::Binding::resolve`]), and naming `acp` without naming an agent is refused
/// rather than defaulted. A default here would run some other vendor's agent than the one the type
/// asked for, which is the bug the whole `HarnessAdapter` seam exists to end.
pub fn adapter_for_type(
    h: Harness,
    acp_agent: Option<&str>,
) -> Result<Box<dyn HarnessAdapter + Send + Sync>, HarnessError> {
    match (h, acp_agent) {
        (Harness::Acp, Some(selector)) => Ok(Box::new(AcpAdapter::resolve(selector)?)),
        (Harness::Acp, None) => Err(HarnessError::MissingInput {
            harness: Harness::Acp,
            what: "`acp` is a protocol, not a program: one adapter serves many agents and marion \
                   may not choose one for the operator (§6.4). Name it in the agent type's \
                   `acp_agent` — a refinement row's id, or the agent's command",
        }),
        _ => adapter_for(h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::config_toml;
    use crate::mcp_bridge;
    use crate::mcp_bridge::{AGENT_TYPE_ENV, DEPTH_ENV};
    use crate::stream::CallOutcome;
    use crate::surfaces::{ControlTransport, DisplaySurface, TypedKind};

    /// One S37 P-errors run as the harness said it: its stdout (the frames it wrote, one JSON line
    /// each) and its stderr, read back from the committed transcript.
    fn p_errors_run(dir: &str, status: u16) -> Option<(String, String)> {
        let path = format!(
            "{}/../../tests/fixtures/conformance/{dir}/p-errors-{status}.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(path).ok()?;
        let (mut stdout, mut stderr) = (String::new(), String::new());
        for line in text.lines() {
            let rec: serde_json::Value = serde_json::from_str(line).unwrap();
            let msg = &rec["msg"];
            let out = match rec["dir"].as_str() {
                Some("s2c") => &mut stdout,
                Some("err") => &mut stderr,
                _ => continue,
            };
            match msg.as_str() {
                Some(s) => out.push_str(s),
                None => out.push_str(&msg.to_string()),
            }
            out.push('\n');
        }
        Some((stdout, stderr))
    }

    /// The conformance matrix's rows that ran P-errors: `(selector, fixture dir, harness)`.
    fn p_errors_rows() -> Vec<(String, String, Harness)> {
        let path = format!(
            "{}/../../tests/fixtures/conformance/matrix.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let matrix: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        matrix["harnesses"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, row)| row["probes"]["P-errors"]["status"] != "UNSUPPORTED")
            .map(|(sel, row)| {
                let name = sel.split(':').next().unwrap();
                let h = Harness::ALL
                    .into_iter()
                    .find(|h| h.as_str() == name)
                    .unwrap_or_else(|| panic!("{sel} names no harness"));
                (
                    sel.clone(),
                    row["fixtures"].as_str().unwrap().to_string(),
                    h,
                )
            })
            .collect()
    }

    /// **Every row's provider errors classify as what the provider said** (S37, the canned
    /// provider answering every turn request with 401, 429 or 500): through the row's own error
    /// rules and stderr, on an API key, a 401 is `Auth`, a 429 `RateLimit`, a 500 `Outage` — and
    /// never another cause. The runs where the harness said nothing within the probe's 45 s (it was
    /// still retrying in silence) are listed by name: nothing marion reads can classify those,
    /// and a row that starts saying something moves off the list.
    #[test]
    fn every_rows_measured_provider_errors_classify_as_the_provider_said() {
        use marion_core::contract::FailureCause;
        let silent: &[(&str, u16)] = &[
            ("acp:opencode", 429),
            ("acp:opencode", 500),
            ("opencode", 429),
            ("opencode", 500),
            ("qwen", 429),
            ("qwen", 500),
        ];
        let rows = p_errors_rows();
        assert!(rows.len() >= 8, "{rows:?}");
        for (sel, dir, h) in rows {
            let adapter = adapter_for(h).unwrap();
            for status in [401u16, 429, 500] {
                let Some((stdout, stderr)) = p_errors_run(&dir, status) else {
                    panic!("{sel}: no p-errors-{status} transcript in {dir}");
                };
                let got = adapter.failure_cause(&stderr, &stdout, crate::auth::Billing::ApiKey);
                let kind = match &got {
                    Some(FailureCause::Auth { .. }) => Some(401),
                    Some(FailureCause::RateLimit { .. }) => Some(429),
                    Some(FailureCause::Outage { .. }) => Some(500),
                    Some(FailureCause::UsageLimit { .. }) => Some(0),
                    None => None,
                };
                let want = (!silent.contains(&(sel.as_str(), status))).then_some(status);
                assert_eq!(kind, want, "{sel} {status}: {got:?}");
            }
        }
    }

    /// **Every row with a stream grammar says where its provider errors are**, unless no provider
    /// fault was ever put in front of it: the rows whose P-errors cell is UNSUPPORTED (no canned
    /// route) are the only ones allowed an empty list.
    #[test]
    fn every_row_with_a_grammar_states_its_error_rules() {
        let path = format!(
            "{}/../../tests/fixtures/conformance/matrix.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let matrix: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for h in Harness::ALL {
            let Some(g) = harness_spec(h).stream else {
                continue;
            };
            if g.errors.is_empty() {
                assert_eq!(
                    matrix["harnesses"][h.as_str()]["probes"]["P-errors"]["status"],
                    "UNSUPPORTED",
                    "{h}: a row that met provider faults states where it reports them"
                );
            }
        }
    }

    /// **Only a provider's own refusal ends a run early**: an error frame that merely says
    /// `unauthorized` about something else — an MCP server of the operator's that refused codex —
    /// is recorded, not a reason to kill the node; the refusal needs the provider's 401 or 403.
    #[test]
    fn an_unauthorized_word_without_a_provider_status_is_no_refused_credential() {
        let codex = adapter_for(Harness::Codex).unwrap();
        let mcp = serde_json::json!({"type": "error", "message": "MCP client for `github` failed to start: unauthorized"});
        assert_eq!(codex.auth_refusal(&mcp), None);
        let provider = serde_json::json!({"type": "error", "message": "Reconnecting... 1/5 (unexpected status 401 Unauthorized: bad key)"});
        assert!(codex.auth_refusal(&provider).is_some());
        let forbidden =
            serde_json::json!({"type": "error", "message": "unexpected status 403 Forbidden"});
        assert!(codex.auth_refusal(&forbidden).is_some());
    }

    /// **Which frames end a run early is row data, and only an auth failure does**: on every row
    /// with a grammar, a frame of the measured 401 run reads as a refused credential, and no frame
    /// of the 429 or 500 runs does — those can recover inside the harness's own backoff.
    #[test]
    fn only_a_measured_auth_failure_frame_is_a_refused_credential() {
        for (sel, dir, h) in p_errors_rows() {
            let adapter = adapter_for(h).unwrap();
            if adapter.spec().stream.is_none() {
                continue;
            }
            for status in [401u16, 429, 500] {
                let (stdout, _) = p_errors_run(&dir, status).unwrap();
                let refusals: Vec<String> = crate::stream::json_frames(&stdout)
                    .iter()
                    .filter_map(|f| adapter.auth_refusal(f))
                    .collect();
                assert_eq!(
                    !refusals.is_empty(),
                    status == 401,
                    "{sel} {status}: {refusals:?}"
                );
            }
        }
    }

    /// **A root is judged by its stream's own failure claims, never by the `report` rule.** The
    /// rule that a refused `report` fails the run is about a node with a contract; a root has
    /// none (§9). Measured live (2026-09-22): an opencode root called `report`, was refused, and
    /// `parse_stream` read that as "the child's marion_report call ended in error", which marked a
    /// run whose child had finished `Ok` as failed. [`HarnessAdapter::stream_failure`] is the
    /// root's reading; `parse_stream` keeps the child's.
    #[test]
    fn a_refused_report_is_a_childs_failure_and_not_a_streams() {
        let adapter = adapter_for(Harness::OpenCode).unwrap();
        let spawned = format!(
            r#"{{"type":"tool_use","part":{{"tool":"{}","state":{{"status":"completed","input":{{}}}}}}}}"#,
            adapter.marion_tool_name("spawn")
        );
        let refused_report = format!(
            r#"{{"type":"tool_use","part":{{"tool":"{}","state":{{"status":"error","input":{{"narrative":"done"}},"error":"marion: report is self only"}}}}}}"#,
            adapter.marion_tool_name("report")
        );
        let stdout = format!("{spawned}\n{refused_report}\n");
        assert!(
            adapter
                .parse_stream(&stdout, ChildExit::default())
                .failure
                .is_some_and(|f| f.contains("report call ended in error")),
            "a child's refused report still fails the child"
        );
        assert_eq!(adapter.stream_failure(&stdout), None, "{stdout}");

        let errored = format!(
            "{spawned}\n{}\n",
            r#"{"type":"error","error":{"name":"APIError","data":{"message":"model gone"}}}"#
        );
        assert_eq!(
            adapter.stream_failure(&errored).as_deref(),
            Some("model gone"),
            "the stream's own failure frame still fails a root"
        );
    }

    /// [`McpRoute::verify`]'s four branches, directly. The supervisor's launch path and `marion
    /// doctor --adapter` both hang off this one answer, so each branch is pinned here rather than
    /// only through whichever caller happens to exercise it.
    #[test]
    fn each_declaration_route_is_verified_against_the_thing_it_promised() {
        let blank = Invocation {
            program: "x".into(),
            args: vec![],
            env: vec![],
            env_remove: vec![],
            cwd: "/wt".into(),
            model: None,
            session_mode: None,
        };
        let doc: PathBuf = "/state/x/mcp.json".into();

        // Document: the first written file, or a refusal naming the document.
        assert_eq!(
            McpRoute::Document.verify(std::slice::from_ref(&doc), &blank, None),
            Ok(Some(doc.clone()))
        );
        assert_eq!(
            McpRoute::Document.verify(&[], &blank, None),
            Err("a configuration document".into())
        );

        // Environment: present **and** non-blank. A key set to whitespace is not a declaration.
        let with_env = Invocation {
            env: vec![("OPENCODE_CONFIG_CONTENT".into(), "{}".into())],
            ..blank.clone()
        };
        assert_eq!(
            McpRoute::Environment("OPENCODE_CONFIG_CONTENT").verify(&[], &with_env, None),
            Ok(None)
        );
        let blanked = Invocation {
            env: vec![("OPENCODE_CONFIG_CONTENT".into(), "  ".into())],
            ..blank.clone()
        };
        assert_eq!(
            McpRoute::Environment("OPENCODE_CONFIG_CONTENT").verify(&[], &blanked, None),
            Err("$OPENCODE_CONFIG_CONTENT".into())
        );
        // A *document* on disk does not satisfy an env route, and vice versa — the two must not be
        // interchangeable or the check would pass on an adapter that took the other route.
        assert!(
            McpRoute::Environment("OPENCODE_CONFIG_CONTENT")
                .verify(std::slice::from_ref(&doc), &blank, None)
                .is_err()
        );
        assert_eq!(
            McpRoute::Document.verify(&[], &with_env, None),
            Err("a configuration document".into())
        );

        // Argv: the adapter's own config key, somewhere in the compiled command line.
        let with_argv = Invocation {
            args: vec!["-c".into(), "mcp_servers.marion.command=\"x\"".into()],
            ..blank.clone()
        };
        assert_eq!(
            McpRoute::Argv("mcp_servers").verify(&[], &with_argv, None),
            Ok(None)
        );
        assert_eq!(
            McpRoute::Argv("mcp_servers").verify(&[], &blank, None),
            Err("`-c mcp_servers.…` on its own command line".into())
        );
        // A **non-empty** argv that does not carry the key. Without this row the check could be
        // `args.iter().any(|_| true)` and nothing here or in `root.rs` would notice, because every
        // other negative case has an empty argv.
        let wrong_argv = Invocation {
            args: vec!["-c".into(), "sandbox_mode=\"danger-full-access\"".into()],
            ..blank.clone()
        };
        assert_eq!(
            McpRoute::Argv("mcp_servers").verify(&[], &wrong_argv, None),
            Err("`-c mcp_servers.…` on its own command line".into()),
            "flags that are not the declaration are not the declaration"
        );

        // Session: the `mcpServers` entry naming **marion's own** server, in the request the
        // adapter compiled. This is the branch `89b822d` said would have nothing behind it.
        let session = acp_adapter()
            .session_declaration(&acp_spec(), &ctx())
            .unwrap()
            .expect("a Marion declaration compiles a session/new request");
        assert_eq!(
            McpRoute::Session(acp::MCP_SERVERS_KEY).verify(&[], &blank, Some(&session)),
            Ok(None)
        );
        let refusal = Err(format!(
            "a `mcpServers` entry named `{}` in its own `session/new` request",
            acp::MCP_SERVER_NAME
        ));
        assert_eq!(
            McpRoute::Session(acp::MCP_SERVERS_KEY).verify(&[], &blank, None),
            refusal,
            "no request at all is not a declaration"
        );
        // **A declaration for somebody else's server is not marion's**, and neither is one with no
        // command. Without these two rows the check could be "the array is non-empty" and every
        // other case here would still pass — which is a node with no marion bridge wearing a green
        // check, §6.1 step 8's exact failure class.
        for wrong in [
            serde_json::json!({"params": {"mcpServers": [{"name": "somebody-else", "command": "/bin/x"}]}}),
            serde_json::json!({"params": {"mcpServers": [{"name": "marion", "command": "  "}]}}),
            serde_json::json!({"params": {"mcpServers": []}}),
            serde_json::json!({"params": {}}),
        ] {
            assert_eq!(
                McpRoute::Session(acp::MCP_SERVERS_KEY).verify(&[], &blank, Some(&wrong)),
                refusal,
                "{wrong}"
            );
        }
        // And the routes stay non-interchangeable in both directions: a session request does not
        // satisfy a document route, and a written document does not satisfy a session route.
        assert!(
            McpRoute::Document
                .verify(&[], &blank, Some(&session))
                .is_err()
        );
        assert!(
            McpRoute::Session(acp::MCP_SERVERS_KEY)
                .verify(std::slice::from_ref(&doc), &with_env, None)
                .is_err()
        );

        // And an adapter that stated no route at all is a refusal, never a pass. §6.1 step 8's
        // failure class is a node that launches with no bridge, so this branch may not be lenient.
        assert_eq!(
            McpRoute::None.verify(std::slice::from_ref(&doc), &with_env, Some(&session)),
            Err("no route at all".into())
        );
    }

    fn ctx() -> SpawnCtx {
        SpawnCtx {
            agent_id: AgentId("019f-root".into()),
            agent_type: "claude".into(),
            depth: 0,
            node_token: None,
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
            tools: vec![],
            allowed_tools: vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: None,
            auth: Auth::Canned,
            config_dir: "/state/x/config".into(),
            resume: None,
            wire: None,
            provider: None,
            extra: Extras::default(),
        }
    }

    fn codex_spec() -> LaunchSpec {
        LaunchSpec {
            cwd: "/wt".into(),
            model: None,
            prompt: "do the task".into(),
            tools: vec![],
            allowed_tools: vec![],
            mcp: McpDeclaration::Marion,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: None,
            auth: Auth::Canned,
            config_dir: "/state/x/config".into(),
            resume: None,
            wire: None,
            provider: None,
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

    /// The one `LaunchOnly` spec that carries `allowed_tools`, because copilot is the one
    /// `LaunchOnly` adapter that compiles them — in its own spelling, as `run_spawn` hands them.
    fn copilot_spec() -> LaunchSpec {
        LaunchSpec {
            model: Some("canned-1".into()),
            api_key: Some("sk-fake".into()),
            allowed_tools: vec!["marion-report".into()],
            ..codex_spec()
        }
    }

    fn goose_spec() -> LaunchSpec {
        LaunchSpec {
            model: Some("canned-1".into()),
            api_key: Some("sk-fake".into()),
            allowed_tools: vec!["marion__report".into()],
            ..codex_spec()
        }
    }

    fn cline_spec() -> LaunchSpec {
        LaunchSpec {
            model: Some("canned-1".into()),
            api_key: Some("sk-fake".into()),
            allowed_tools: vec!["marion__report".into()],
            ..codex_spec()
        }
    }

    fn qwen_spec() -> LaunchSpec {
        LaunchSpec {
            model: Some("canned-1".into()),
            api_key: Some("sk-fake".into()),
            allowed_tools: vec!["mcp__marion__report".into()],
            ..codex_spec()
        }
    }

    /// agy runs only live: there is no canned route (the adapter refuses one by name).
    fn agy_spec() -> LaunchSpec {
        LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            model: Some(agent_type::AGY_DEFAULT_MODEL.into()),
            ..codex_spec()
        }
    }

    /// qwen's shape: a model because `models.json` names one, and marion's verb on the list.
    fn pi_spec() -> LaunchSpec {
        LaunchSpec {
            model: Some("canned-1".into()),
            api_key: Some("sk-fake".into()),
            allowed_tools: vec!["mcp__marion__report".into()],
            ..codex_spec()
        }
    }

    /// The two things an ACP launch needs that no other harness does: an agent named by the
    /// operator, and the operator's own login — see `AcpAdapter::compile`, which refuses both by
    /// name rather than defaulting either.
    fn acp_spec() -> LaunchSpec {
        LaunchSpec {
            auth: Auth::Inherited,
            // Present under both modes, because this spec is swept through both and the canned
            // recipe needs an endpoint and a `provider/model` pair to compile a document at all.
            // Under `Inherited` the adapter reads neither, which is what the argv test asserts.
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("marion-placeholder".into()),
            model: Some("marion/canned-1".into()),
            resume: None,
            extra: Extras {
                acp_agent: Some(acp::OPENCODE.id.into()),
                ..Extras::default()
            },
            ..codex_spec()
        }
    }

    /// The adapter [`acp_spec`] is launched through — bound to the same agent the spec names,
    /// which is the pairing `AcpAdapter::agent` refuses to let come apart.
    fn acp_adapter() -> AcpAdapter {
        AcpAdapter::for_agent(acp::OPENCODE)
    }

    /// S21's verbatim `opencode acp` transcript — a real stream, so "this reader found nothing" is
    /// a claim about the reader rather than about an empty input.
    const ACP_OPENCODE_SESSION: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s21/opencode-acp-session.jsonl"
    ));

    /// M1's child, pinned token for token: the `codex exec --json` launch S6 measured on 0.146.0,
    /// which `codex::SPEC` renders, with `-C` moved ahead of the flags because that is the one
    /// position a resume can share (s29: `exec resume` does not take `-C`, and a fresh `exec` reads
    /// its flags in any order). A canned launch compiles no `-m` however loudly one is asked for,
    /// and records `None`.
    #[test]
    fn the_codex_adapter_compiles_the_measured_m1_child() {
        assert_eq!(
            CodexAdapter.compile(&codex_spec(), &ctx()).unwrap(),
            Invocation {
                program: "codex".into(),
                args: [
                    "exec",
                    "-C",
                    "/wt",
                    "--json",
                    "--skip-git-repo-check",
                    // The row's update policy on codex's own override channel.
                    "-c",
                    "check_for_update_on_startup=false",
                    "do the task",
                ]
                .map(String::from)
                .to_vec(),
                env: vec![("CODEX_HOME".into(), "/state/x/config".into())],
                env_remove: vec![],
                cwd: "/wt".into(),
                model: None,
                session_mode: None,
            }
        );
    }

    // -----------------------------------------------------------------------------------------
    // §3.1's availability axis (§11 item 24).
    //
    // The first test is the one the whole feature stands on and the rest are the feature itself.
    // Each harness expresses availability in its **own** form and the tests say which, because a
    // single shared assertion would have to pick one harness's form and be vacuous on the other
    // three — which is how §11 item 24 came to be written in the first place.
    // -----------------------------------------------------------------------------------------

    /// Every byte a node is told: argv, env, and each generated config file. The same surface
    /// `tests/auth_mode.rs` searches, for the same reason — a flag that reached the harness
    /// reached one of these three.
    fn everything_the_node_is_told(adapter: &dyn HarnessAdapter, spec: &LaunchSpec) -> String {
        let inv = adapter.compile(spec, &ctx()).expect("the spec compiles");
        let mut blob = format!("{:?}\n{:?}\n", inv.args, inv.env);
        for (p, c) in adapter.config_files(spec, &ctx()).expect("configs derive") {
            blob.push_str(&format!("--- {}\n{c}\n", p.display()));
        }
        blob
    }

    fn adapters_and_specs() -> Vec<(&'static str, Box<dyn HarnessAdapter>, LaunchSpec)> {
        vec![
            ("claude", Box::new(ClaudeCodeAdapter), claude_spec()),
            ("codex", Box::new(CodexAdapter), codex_spec()),
            ("gemini", Box::new(GeminiAdapter), gemini_spec()),
            ("opencode", Box::new(OpenCodeAdapter), opencode_spec()),
            ("copilot", Box::new(CopilotAdapter), copilot_spec()),
        ]
    }

    /// **The property that makes this axis opt-in: a node that declares nothing is launched with
    /// the bytes marion launched it with before the axis existed.**
    ///
    /// The argv strings below were captured off `HEAD` *before* a line of this feature was
    /// written, by compiling all four adapters from a throwaway test and printing the result — the
    /// same technique `ae2a18d` used to prove the adapter seam was a pure refactor, and the reason
    /// they are pinned literally rather than derived: a derivation would move with the code it is
    /// supposed to be pinning.
    ///
    /// **`--tools ""` is the one to watch on claude.** It was a hardcoded empty string and is now
    /// `spec.tools.join(",")`; an empty `Vec` joins to exactly the same empty string, so the flag
    /// and its value are both still there and still empty. If that ever compiles to something else
    /// — a dropped flag, a `[]`, a stray separator — every claude node marion runs changes at once.
    #[test]
    fn a_node_that_declares_no_tools_compiles_the_argv_it_always_did() {
        let expected: Vec<(&str, Vec<&str>)> = vec![
            (
                "claude",
                vec![
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--input-format",
                    "stream-json",
                    "--verbose",
                    "--tools",
                    "",
                    "--allowedTools",
                    "mcp__marion__spawn,mcp__marion__status",
                    "--permission-prompt-tool",
                    "stdio",
                    "--strict-mcp-config",
                    "--mcp-config",
                    "/state/x/config/mcp.json",
                    "--setting-sources",
                    "",
                    "--settings",
                    r#"{"disableAllHooks":true}"#,
                    "--model",
                    "haiku",
                ],
            ),
            (
                "codex",
                vec![
                    "exec",
                    "-C",
                    "/wt",
                    "--json",
                    "--skip-git-repo-check",
                    "-c",
                    "check_for_update_on_startup=false",
                    "do the task",
                ],
            ),
            (
                "gemini",
                vec![
                    "-m",
                    "gemini-2.5-flash",
                    "--output-format",
                    "stream-json",
                    "-p",
                    "do the task",
                ],
            ),
            (
                "opencode",
                vec![
                    "run",
                    "--pure",
                    "--format",
                    "json",
                    "--title",
                    "marion-019f-root",
                    "-m",
                    "canned/canned-1",
                    "do the task",
                ],
            ),
            // Pinned at the adapter's birth rather than captured pre-axis: copilot arrived with both
            // axes already compiled, so this is the argv `tests/fixtures/s24/` measured against
            // 1.0.83 with an empty declaration — marion's verb on both flags and nothing else.
            (
                "copilot",
                vec![
                    "-p",
                    "do the task",
                    "--output-format",
                    "json",
                    "-C",
                    "/wt",
                    "--disable-builtin-mcps",
                    "--no-custom-instructions",
                    "--model",
                    "canned-1",
                    "--available-tools=marion-report",
                    "--allow-tool=marion(report)",
                    "--additional-mcp-config",
                    "@/state/x/config/mcp.json",
                ],
            ),
        ];
        // The env block, keys and values, in order. Captured from the same pre-axis run: the axis
        // must be provably invisible in every channel a harness is configured through, not only
        // the one it happens to compile into on claude.
        let expected_env: Vec<(&str, Vec<(&str, &str)>)> = vec![
            (
                "claude",
                vec![
                    ("ANTHROPIC_BASE_URL", "http://127.0.0.1:8099"),
                    ("DISABLE_AUTOUPDATER", "1"),
                ],
            ),
            ("codex", vec![("CODEX_HOME", "/state/x/config")]),
            (
                "gemini",
                vec![
                    ("GEMINI_CLI_HOME", "/state/x/config"),
                    (
                        "GEMINI_CLI_SYSTEM_SETTINGS_PATH",
                        "/state/x/config/marion-settings.json",
                    ),
                    ("GEMINI_CLI_TRUST_WORKSPACE", "true"),
                    ("GEMINI_FORCE_FILE_STORAGE", "true"),
                    ("GOOGLE_GEMINI_BASE_URL", "http://127.0.0.1:8099"),
                    ("GEMINI_API_KEY", "sk-fake"),
                ],
            ),
            (
                "opencode",
                vec![
                    ("HOME", "/state/x/config"),
                    ("XDG_CONFIG_HOME", "/state/x/config/config"),
                    ("XDG_DATA_HOME", "/state/x/config/data"),
                    ("XDG_CACHE_HOME", "/state/x/config/cache"),
                    ("XDG_STATE_HOME", "/state/x/config/state"),
                    ("OPENCODE_DISABLE_CLAUDE_CODE", "1"),
                    ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1"),
                    ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
                    ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
                    ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1"),
                    ("OPENCODE_DISABLE_SHARE", "1"),
                    ("OPENCODE_DB", "/state/x/config/opencode.db"),
                    ("PWD", "/wt"),
                    // The row's update policy, rendered after its `Env` rows.
                    ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
                ],
            ),
            (
                "copilot",
                vec![
                    ("COPILOT_HOME", "/state/x/config/home"),
                    ("COPILOT_PROVIDER_BASE_URL", "http://127.0.0.1:8099/v1"),
                    ("COPILOT_PROVIDER_TYPE", "openai"),
                    ("COPILOT_PROVIDER_WIRE_API", "completions"),
                    ("COPILOT_PROVIDER_API_KEY", "sk-fake"),
                    ("COPILOT_OFFLINE", "true"),
                    ("COPILOT_AUTO_UPDATE", "false"),
                ],
            ),
        ];
        for (name, adapter, spec) in adapters_and_specs() {
            assert!(
                spec.tools.is_empty(),
                "{name}: this test is about the default"
            );
            let inv = adapter.compile(&spec, &ctx()).unwrap();
            assert_eq!(
                inv.args,
                expected.iter().find(|(n, _)| *n == name).unwrap().1,
                "{name}: an empty declaration must compile the pre-axis argv byte for byte"
            );
            let env: Vec<(&str, &str)> = inv
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            assert_eq!(
                env,
                expected_env.iter().find(|(n, _)| *n == name).unwrap().1,
                "{name}: and the pre-axis env block byte for byte"
            );
        }
    }

    /// The same claim for the generated configuration, which is where **codex's** availability
    /// axis lives — `sandbox_mode` moved from a literal in `config_toml` to `codex::SANDBOX_MODE`
    /// so the adapter's `tool_name` and the file cannot disagree, and that is a refactor that has
    /// to be provably pure.
    ///
    /// Lengths are the pre-axis byte counts, captured the same way as the argv above. A length is
    /// enough here precisely because the other tests in this file already pin these documents'
    /// *contents* against their free functions: what this adds is that the availability axis did
    /// not perturb them.
    #[test]
    fn a_node_that_declares_no_tools_generates_the_config_it_always_did() {
        for (name, adapter, spec) in adapters_and_specs() {
            let files = adapter.config_files(&spec, &ctx()).unwrap();
            let len: usize = files.iter().map(|(_, c)| c.len()).sum();
            let want = match name {
                "claude" => 491,
                // 1275 pre-axis, + 31 for `omit_tools_from = ["deferred"]\n` (2026-09-27), + 3 since
                // the provider's `env_key` became `MARION_PROVIDER_KEY`.
                "codex" => 1309,
                "gemini" => 718,
                // +27: `"timeout": 86400000` on marion's server; +47: the `permission` grant for
                // marion's own tools (both s36).
                "opencode" => 1036,
                // Pinned at the adapter's birth (s24), not pre-axis — see the argv table.
                "copilot" => 529,
                _ => unreachable!(),
            };
            assert_eq!(len, want, "{name}: generated config changed size");
        }
        let codex = CodexAdapter.config_files(&codex_spec(), &ctx()).unwrap();
        assert!(
            codex[0].1.contains("sandbox_mode = \"workspace-write\""),
            "the constant must render the literal the file always carried: {}",
            codex[0].1
        );
    }

    /// A `LaunchSpec` declaring marion's one write verb.
    fn writing(spec: LaunchSpec) -> LaunchSpec {
        LaunchSpec {
            tools: vec![agent_type::TOOL_WRITE.into()],
            ..spec
        }
    }

    /// A `LaunchSpec` declaring marion's read verb.
    fn reading(spec: LaunchSpec) -> LaunchSpec {
        LaunchSpec {
            tools: vec![agent_type::TOOL_READ.into()],
            ..spec
        }
    }

    /// **Claude Code: `read` is a real grant, and it reaches both axes exactly as `write` does.**
    ///
    /// Measured, `tests/fixtures/s14/README.md`: `--tools Read` puts `Read` in the request body's
    /// tool list on 2.1.222, and marion's own `--tools ""` leaves it absent — so this is the one
    /// harness of four where the declaration buys something. The `--allowedTools` half is asserted
    /// beside it for §11 item 24's reason: availability alone sends the call to
    /// `--permission-prompt-tool stdio`, where marion has no answerer, and the node receives item
    /// 22's dead-end string instead of the file.
    ///
    /// The lowercase negative is the whole reason s14 exists: `--tools read` — marion's own word,
    /// passed through unmapped — yields `body.tools []`, exit 0, empty stderr. So the assertion is
    /// on the harness's spelling and not merely on "the flag mentions read".
    #[test]
    fn a_declared_read_reaches_claude_codes_two_axes_together() {
        assert_eq!(
            ClaudeCodeAdapter.tool_names(agent_type::TOOL_READ).unwrap(),
            ["Read"],
            "marion's word is `read` and the harness's is `Read`; passing marion's through \
             declares nothing at all and says so nowhere (s14)"
        );
        let inv = ClaudeCodeAdapter
            .compile(&reading(claude_spec()), &ctx())
            .unwrap();
        let after = |flag: &str| -> String {
            let i = inv.args.iter().position(|a| a == flag).expect("flag");
            inv.args[i + 1].clone()
        };
        assert_eq!(after("--tools"), "Read");
        assert_eq!(
            after("--allowedTools"),
            "mcp__marion__spawn,mcp__marion__status,Read",
            "permission follows availability from one declaration, or the read is item 22's dead \
             end"
        );
        // Both verbs together, which is what `claude-impl` and a granted root actually declare —
        // and the comma separator s14 paid the debt on (`--tools "Read,Bash"` declares both).
        let both = ClaudeCodeAdapter
            .compile(
                &LaunchSpec {
                    tools: agent_type::builtin("claude-impl").unwrap().tools,
                    ..claude_spec()
                },
                &ctx(),
            )
            .unwrap();
        let i = both.args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(both.args[i + 1], "Read,Write");
    }

    /// **gemini and opencode already declare a read tool, so marion compiles nothing for it.**
    ///
    /// s14 measured both: gemini 0.53.0 carries `read_file` in `functionDeclarations` under the
    /// *default* approval mode, and opencode 1.17.3 carries `read` in its default tool list. The
    /// grant is therefore a no-op on both, and the argv equality is the assertion — in particular
    /// that `read` alone must **not** move gemini into `auto_edit`, which would silently hand every
    /// reading node the write tools as well.
    #[test]
    fn a_declared_read_on_the_two_harnesses_that_already_have_one_changes_nothing() {
        assert_eq!(
            GeminiAdapter.tool_names(agent_type::TOOL_READ).unwrap(),
            ["read_file"]
        );
        assert!(
            !gemini::is_edit_tool("read_file"),
            "read is not an edit tool, or declaring it would compile auto_edit and grant writes"
        );
        assert_eq!(
            OpenCodeAdapter.tool_names(agent_type::TOOL_READ).unwrap(),
            ["read"],
            "a spelling collision with marion's own word, written out rather than defaulted"
        );
        for (name, adapter, spec) in adapters_and_specs() {
            if name != "gemini" && name != "opencode" {
                continue;
            }
            assert_eq!(
                everything_the_node_is_told(adapter.as_ref(), &reading(spec.clone())),
                everything_the_node_is_told(adapter.as_ref(), &spec),
                "{name}: already declares a read tool, so the grant must not narrow or widen it"
            );
        }
    }

    /// **codex has no read tool, so `read` is refused by name rather than answered with the shell.**
    ///
    /// s14, measured: codex 0.146.0's declaration is `apply_patch, create_goal, exec_command,
    /// get_goal, update_goal, update_plan, view_image, write_stdin` — identical under
    /// `--sandbox read-only` and `--sandbox workspace-write`. Reading a file there is
    /// `exec_command`, i.e. the shell.
    ///
    /// The arm this test forbids is the one [`TOOL_WRITE`] uses on this same harness — *satisfied
    /// rather than newly granted*. It does not transfer: `write` names a measured correspondence
    /// (`apply_patch`, under a sandbox mode marion compiles), while answering `read` with the shell
    /// would grant strictly more than was declared and make `tools: [read]` read as "read-only" on
    /// the one harness where it would not be.
    #[test]
    fn codex_has_no_read_tool_so_the_verb_is_refused_by_name() {
        let err = CodexAdapter
            .compile(&reading(codex_spec()), &ctx())
            .expect_err("codex must not launch a node promised a tool it has none of");
        let msg = err.to_string();
        assert!(
            msg.contains("read") && msg.contains("codex"),
            "the refusal names the verb and the harness: {msg}"
        );
        assert!(
            matches!(err, HarnessError::UnsupportedTool { .. }),
            "the typed refusal, not a generic one: {err}"
        );
        // And the record cannot be produced either — a caller that reached `compiled_permissions`
        // past a failed `compile` would otherwise get a contract for a launch that never happened.
        assert!(
            CodexAdapter
                .compiled_permissions(&reading(codex_spec()))
                .is_err()
        );
    }

    /// **Every built-in's declaration is satisfiable by its own harness's adapter.**
    ///
    /// This is `marion doctor`'s job, and `marion doctor` does not exist — so it is a test. Without
    /// it, `codex-impl { tools: [read] }` would compile, ship, and fail only at the moment an
    /// operator ran it. The refusal is the right runtime behaviour and a wrong committed state;
    /// this is what keeps the second from happening.
    #[test]
    fn every_builtin_declares_only_tools_its_own_harness_can_provide() {
        for name in agent_type::builtin_names() {
            let t = agent_type::builtin(name).unwrap();
            let adapter = launch_adapter(t.harness).unwrap();
            for tool in &t.tools {
                adapter.tool_names(tool).unwrap_or_else(|e| {
                    panic!("built-in `{name}` declares a tool its harness cannot provide: {e}")
                });
            }
        }
    }

    /// **Claude Code: both of §3.1's axes, from one declaration.**
    ///
    /// The harness where the whole gap was measured. `--tools` is availability and `--allowedTools`
    /// is permission; item 24 measured that opening only the first is *not* a write — the call
    /// goes to `--permission-prompt-tool stdio`, marion has no answerer, and the child receives
    /// item 22's dead-end string as its `tool_result`. So the assertion is deliberately a
    /// conjunction: `Write` in **both** flags, beside marion's own verbs, which `--tools` must
    /// never gate.
    #[test]
    fn a_declared_write_reaches_claude_codes_two_axes_together() {
        let inv = ClaudeCodeAdapter
            .compile(&writing(claude_spec()), &ctx())
            .unwrap();
        let after = |flag: &str| -> String {
            let i = inv
                .args
                .iter()
                .position(|a| a == flag)
                .expect("flag present");
            inv.args[i + 1].clone()
        };
        assert_eq!(
            after("--tools"),
            "Write",
            "availability, in claude's spelling"
        );
        assert_eq!(
            after("--allowedTools"),
            "mcp__marion__spawn,mcp__marion__status,Write",
            "permission is `the same list, plus marion's own` — availability alone is item 22's \
             dead end, not a write"
        );
    }

    /// **gemini: the availability axis is a mode, because the CLI has no per-tool flag.**
    ///
    /// Under the default approval mode 0.53.0 withholds `write_file` from `functionDeclarations`
    /// entirely, so the tool the mapping names does not exist to be called. The flag is therefore
    /// the compiled form of the declaration, and it is emitted **only** when one arrives — the
    /// negative half is asserted here too, since an unconditional `auto_edit` would widen every
    /// gemini node marion runs.
    #[test]
    fn a_declared_write_puts_gemini_in_the_approval_mode_that_declares_one() {
        assert_eq!(
            GeminiAdapter.tool_names(agent_type::TOOL_WRITE).unwrap(),
            ["write_file"]
        );
        assert!(gemini::is_edit_tool("write_file"));
        let with = GeminiAdapter
            .compile(&writing(gemini_spec()), &ctx())
            .unwrap();
        let i = with
            .args
            .iter()
            .position(|a| a == "--approval-mode")
            .expect("the declaration must compile the mode that makes write_file exist");
        assert_eq!(
            with.args[i + 1],
            "auto_edit",
            "never -y: see §6.4 and item 24"
        );
        let without = GeminiAdapter.compile(&gemini_spec(), &ctx()).unwrap();
        assert!(
            !without.args.iter().any(|a| a == "--approval-mode"),
            "a node that declared nothing must stay in the default mode"
        );
    }

    /// **codex and opencode: the declaration is *satisfied*, not compiled.**
    ///
    /// Neither has a per-tool availability surface marion drives. codex has one sandbox mode, which
    /// `config_toml` has always set to `workspace-write`; opencode declares `write` in its own
    /// default tool list with no flag from marion. So `tool_name` answers with what that harness
    /// already grants — §3.1's *"the harness's coarsest equivalent"*, whose exact string for codex
    /// that section names — and `compile` emits nothing new.
    ///
    /// **The argv equality is the point, not an omission.** Making these grants conditional on the
    /// declaration would read tidier and would silently demote every codex and opencode node
    /// marion spawns today, to close a gap on two other harnesses. This pins that they were left
    /// alone.
    #[test]
    fn a_declared_write_on_the_two_already_write_capable_harnesses_changes_nothing() {
        assert_eq!(
            CodexAdapter.tool_names(agent_type::TOOL_WRITE).unwrap(),
            [format!("sandbox:{}", codex::SANDBOX_MODE)],
            "§3.1 names this exact string for this exact harness"
        );
        assert_eq!(
            OpenCodeAdapter.tool_names(agent_type::TOOL_WRITE).unwrap(),
            ["write"]
        );
        for (name, adapter, spec) in adapters_and_specs() {
            if name != "codex" && name != "opencode" {
                continue;
            }
            assert_eq!(
                everything_the_node_is_told(adapter.as_ref(), &writing(spec.clone())),
                everything_the_node_is_told(adapter.as_ref(), &spec),
                "{name}: already grants the write, so a declaration must not narrow or widen it"
            );
        }
    }

    /// **A tool no adapter can provide is refused by name, on every harness, before anything
    /// launches.**
    ///
    /// The rule this codebase keeps re-deriving: marion refuses what it declares and does not
    /// perform (`77557e3`). Dropping an unmappable name silently would be the worst instance of it
    /// — the node launches, is offered no such tool, does no work, and persists `changed_paths: []`,
    /// which §11 item 24 records as byte-identical to a child whose write escaped its worktree.
    ///
    /// `edit` and `bash` are in the sample deliberately: they are §3.1's own example vocabulary,
    /// and item 24 says in as many words that they *"were never tried"*. Refusing a §3.1 word is
    /// the honest state, and the message has to be good enough to say so.
    ///
    /// `read` **left the sample** when it gained a measured mapping on three of the four harnesses
    /// (`tests/fixtures/s14/`), which is exactly the transition §3.1's rule describes — a verb is
    /// refused until it is measured, and not one day longer. It is still refused on codex, where
    /// there is no read tool to measure, and
    /// [`codex_has_no_read_tool_so_the_verb_is_refused_by_name`] asserts that on its own.
    #[test]
    fn a_tool_a_harness_cannot_provide_is_refused_by_name_not_dropped() {
        for (name, adapter, spec) in adapters_and_specs() {
            for unmapped in ["edit", "bash", "Write", "write_file", ""] {
                let spec = LaunchSpec {
                    tools: vec![unmapped.into()],
                    ..spec.clone()
                };
                let Err(err) = adapter.compile(&spec, &ctx()) else {
                    panic!("{name}: `{unmapped}` is not mappable and must not compile");
                };
                let msg = err.to_string();
                assert!(
                    msg.contains(unmapped) && msg.contains(&adapter.harness().to_string()),
                    "{name}/{unmapped}: the refusal must name both the tool and the harness, got \
                     `{msg}`"
                );
            }
        }
    }

    /// The refusal survives a *mixed* declaration, and it aborts rather than compiling the good
    /// half. A partial grant is the failure this axis exists to prevent, wearing a success's
    /// clothes: the node would launch able to write and unable to do the other thing it was told
    /// it could, with nothing anywhere saying which.
    #[test]
    fn one_unmappable_tool_refuses_the_whole_declaration() {
        let spec = LaunchSpec {
            tools: vec![agent_type::TOOL_WRITE.into(), "bash".into()],
            ..claude_spec()
        };
        assert!(matches!(
            ClaudeCodeAdapter.compile(&spec, &ctx()),
            Err(HarnessError::UnsupportedTool { tool, .. }) if tool == "bash"
        ));
    }

    /// **§6.7's audit record names the constraint that was compiled, per harness, in that
    /// harness's own vocabulary — and it is a different *kind* of answer on each of the four.**
    ///
    /// That variety is the whole content of §3.1's *"the compiled, harness-native constraint — or
    /// the harness's coarsest equivalent where it has no per-tool allowlist at all"*: claude has a
    /// real allowlist, codex has one sandbox mode, gemini has one approval mode, and opencode has
    /// nothing marion compiles. A single uniform answer across four harnesses is what the field
    /// carried before (`["apply_patch", "shell"]`, hardcoded) and it was wrong on all four.
    #[test]
    fn the_contract_records_the_constraint_each_harness_actually_compiled() {
        let want = |name: &str| -> Vec<String> {
            match name {
                // A real per-tool allowlist: the literal contents of `--allowedTools`.
                "claude" => vec!["mcp__marion__spawn".into(), "mcp__marion__status".into()],
                // §3.1's own worked example for this harness.
                "codex" => vec!["sandbox:workspace-write".into()],
                // The mode is the constraint, and it is recorded when withheld as well as relaxed.
                "gemini" => vec!["approval-mode:default".into()],
                "opencode" => vec![opencode::NO_COMPILED_TOOL_CONSTRAINT.into()],
                // A real allowlist again, in the pattern grammar: the `--allow-tool`s, prefixed
                // with the axis because copilot's `write` kind collides with marion's verb.
                "copilot" => vec!["allow-tool:marion(report)".into()],
                _ => unreachable!(),
            }
        };
        for (name, adapter, spec) in adapters_and_specs() {
            assert_eq!(
                adapter.compiled_permissions(&spec).unwrap(),
                want(name),
                "{name}"
            );
            assert!(
                !adapter
                    .compiled_permissions(&spec)
                    .unwrap()
                    .iter()
                    .any(|t| t == "apply_patch" || t == "shell"),
                "{name}: the pre-fix constant named marion-side tool names on every harness; §3.1 \
                 forbids echoing marion's vocabulary here even on codex, where it looks plausible"
            );
        }
    }

    /// A declaration moves the record on exactly the harnesses whose constraint it moves.
    ///
    /// **The point is that it is not uniform.** On claude the granted tool joins a real allowlist;
    /// on gemini the mode it forces is what the record names; on codex and opencode the constraint
    /// did not move, so neither does the record — which is the honest answer, not an oversight,
    /// because those two grant the write with or without a declaration.
    #[test]
    fn a_declaration_moves_the_record_exactly_where_it_moves_the_constraint() {
        let want = |name: &str| -> Vec<String> {
            match name {
                "claude" => vec![
                    "mcp__marion__spawn".into(),
                    "mcp__marion__status".into(),
                    "Write".into(),
                ],
                "codex" => vec!["sandbox:workspace-write".into()],
                "gemini" => vec!["approval-mode:auto_edit".into()],
                "opencode" => vec![opencode::NO_COMPILED_TOOL_CONSTRAINT.into()],
                // The kind `write` joins the list — not the tool name `create`, which s24 measured
                // granting nothing as a pattern.
                "copilot" => vec![
                    "allow-tool:marion(report)".into(),
                    "allow-tool:write".into(),
                ],
                _ => unreachable!(),
            }
        };
        for (name, adapter, spec) in adapters_and_specs() {
            assert_eq!(
                adapter.compiled_permissions(&writing(spec)).unwrap(),
                want(name),
                "{name}"
            );
        }
    }

    /// **The record and the argv are one derivation, not two that agree today.**
    ///
    /// This is the invariant that keeps `allowed_tools` from drifting back into fiction: whatever
    /// `compiled_permissions` reports for claude must be exactly the string `--allowedTools`
    /// carries, and whatever it reports for gemini must be the mode argv actually asked for. Both
    /// are checked against the *compiled invocation*, so a second derivation appearing in either
    /// place fails here rather than in a contract someone reads a month later.
    #[test]
    fn the_recorded_constraint_is_the_one_the_argv_carries() {
        for spec in [claude_spec(), writing(claude_spec())] {
            let inv = ClaudeCodeAdapter.compile(&spec, &ctx()).unwrap();
            let i = inv.args.iter().position(|a| a == "--allowedTools").unwrap();
            assert_eq!(
                inv.args[i + 1],
                ClaudeCodeAdapter
                    .compiled_permissions(&spec)
                    .unwrap()
                    .join(","),
                "the record must be the flag, not a parallel derivation of it"
            );
        }
        for spec in [gemini_spec(), writing(gemini_spec())] {
            let inv = GeminiAdapter.compile(&spec, &ctx()).unwrap();
            let compiled = match inv.args.iter().position(|a| a == "--approval-mode") {
                // Absent from argv is not absent from the record: no flag *is* the default mode.
                None => gemini::DEFAULT_APPROVAL_MODE.to_string(),
                Some(i) => inv.args[i + 1].clone(),
            };
            assert_eq!(
                GeminiAdapter.compiled_permissions(&spec).unwrap(),
                vec![format!("approval-mode:{compiled}")]
            );
        }
    }

    /// An unmappable name is refused by `compiled_permissions` too, on every harness.
    ///
    /// Not redundant with the `compile` refusal: this method is fallible *only* for this reason,
    /// and a caller that reached it without compiling — a future replay or a `doctor` — would
    /// otherwise be handed a record derived from a declaration marion cannot honour.
    #[test]
    fn the_record_refuses_a_tool_the_harness_cannot_provide() {
        for (name, adapter, spec) in adapters_and_specs() {
            let spec = LaunchSpec {
                tools: vec!["bash".into()],
                ..spec
            };
            let Err(err) = adapter.compiled_permissions(&spec) else {
                panic!("{name}: an unmappable tool must not yield a record");
            };
            assert!(err.to_string().contains("bash"), "{name}: {err}");
        }
    }

    #[test]
    fn the_claude_adapter_emits_byte_identical_mcp_json() {
        let spec = claude_spec();
        let files = ClaudeCodeAdapter.config_files(&spec, &ctx()).unwrap();
        let expected = serde_json::to_string_pretty(&claude_code::mcp_config_json(&BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            // The `/v1` form, which is what the bridge hands a Codex grandchild.
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-root".into()),
            agent_type: "claude".into(),
            depth: 0,
            node_token: None,
            ready_file: Some("/state/x/mcp-ready".into()),
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
            &BridgeEnv {
                bridge: "/bin/marion-supervisor".into(),
                args: vec!["mcp".into()],
                repo: "/repo".into(),
                state: "/state".into(),
                base_url: Some("http://127.0.0.1:8099/v1".into()),
                auth: Auth::Canned,
                agent_id: AgentId("019f-root".into()),
                agent_type: "claude".into(),
                depth: 0,
                node_token: None,
                ready_file: Some("/state/x/mcp-ready".into()),
            },
            "http://127.0.0.1:8099/v1",
        );
        assert_eq!(
            files,
            vec![(PathBuf::from("/state/x/config/config.toml"), expected)]
        );
    }

    /// The other half of the closed gap, asserted through the **adapter** rather than the emitter:
    /// whatever `SpawnCtx` carries has to reach the document, or a codex root's `spawn` fails with
    /// `marion: MARION_REPO is not set` and its contract names no agent-dir.
    #[test]
    fn a_codex_nodes_identity_reaches_its_bridge_through_the_adapter() {
        let files = CodexAdapter.config_files(&codex_spec(), &ctx()).unwrap();
        let toml = &files[0].1;
        assert!(toml.contains(r#"MARION_AGENT_ID = "019f-root""#), "{toml}");
        assert!(toml.contains(r#"MARION_REPO = "/repo""#), "{toml}");
        assert!(toml.contains(r#"MARION_STATE_DIR = "/state""#), "{toml}");
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
                ("DISABLE_AUTOUPDATER".to_string(), "1".to_string()),
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
    /// `--input-format stream-json` launch — **this exact argv**, pinned token for token, because
    /// it is the launch S1/S9/S11 measured on 2.1.220 and the row that renders it is a
    /// transcription that must not drift.
    #[test]
    fn an_empty_prompt_still_compiles_the_roots_measured_launch() {
        let inv = ClaudeCodeAdapter.compile(&claude_spec(), &ctx()).unwrap();
        assert_eq!(
            inv,
            Invocation {
                program: "claude".into(),
                args: [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--input-format",
                    "stream-json",
                    "--verbose",
                    "--tools",
                    "",
                    "--allowedTools",
                    "mcp__marion__spawn,mcp__marion__status",
                    "--permission-prompt-tool",
                    "stdio",
                    "--strict-mcp-config",
                    "--mcp-config",
                    "/state/x/config/mcp.json",
                    "--setting-sources",
                    "",
                    "--settings",
                    r#"{"disableAllHooks":true}"#,
                    "--model",
                    "haiku",
                ]
                .map(String::from)
                .to_vec(),
                // The supervisor used to derive the `/v1`-less form itself; the adapter does.
                env: vec![
                    ("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:8099".into()),
                    ("DISABLE_AUTOUPDATER".into(), "1".into()),
                ],
                env_remove: vec![],
                cwd: "/repo".into(),
                model: Some("haiku".into()),
                session_mode: None,
            }
        );
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
        let claude = claude_code::mcp_config_json(&BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-root".into()),
            agent_type: "claude".into(),
            depth: 0,
            node_token: None,
            ready_file: Some("/state/x/mcp-ready".into()),
        });
        let expected: Vec<&String> = claude["mcpServers"]["marion"]["env"]
            .as_object()
            .unwrap()
            .keys()
            .collect();

        let g = gemini::settings_json(Some(&BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-root".into()),
            agent_type: "gemini".into(),
            depth: 0,
            node_token: None,
            ready_file: Some("/state/x/mcp-ready".into()),
        }));
        let o = opencode::config_json(
            &opencode::ConfigSpec {
                model: opencode::ModelRef::parse("canned/canned-1").unwrap(),
                base_url: "http://127.0.0.1:8099/v1".into(),
                api_key: None,
                key_header: Default::default(),
            },
            Some(&BridgeEnv {
                bridge: "/bin/marion-supervisor".into(),
                args: vec!["mcp".into()],
                repo: "/repo".into(),
                state: "/state".into(),
                base_url: Some("http://127.0.0.1:8099/v1".into()),
                auth: Auth::Canned,
                agent_id: AgentId("019f-root".into()),
                agent_type: "opencode".into(),
                depth: 0,
                node_token: None,
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

    /// The spec each harness needs to emit its configuration, so a test can sweep `Harness::ALL`
    /// without pretending the four take the same inputs (two refuse without a model, and the two
    /// spellings are incompatible).
    fn spec_for(h: Harness) -> LaunchSpec {
        match h {
            Harness::ClaudeCode => claude_spec(),
            Harness::Codex => codex_spec(),
            Harness::Gemini => gemini_spec(),
            Harness::OpenCode => opencode_spec(),
            Harness::Copilot => copilot_spec(),
            Harness::Goose => goose_spec(),
            Harness::Cline => cline_spec(),
            Harness::Qwen => qwen_spec(),
            Harness::Antigravity => agy_spec(),
            Harness::Pi => pi_spec(),
            Harness::Acp => acp_spec(),
        }
    }

    /// **Endpoint overlays exactly what canned does**: the same isolation variables, the same
    /// documents at the same paths, the same MCP route — only where they point differs. A row that
    /// dropped an isolation variable under endpoint would run the operator's own harness config
    /// against a third-party endpoint.
    #[test]
    fn an_endpoint_launch_carries_every_overlay_a_canned_one_does() {
        for h in Harness::ALL {
            let adapter = launch_adapter(h).unwrap();
            let canned = LaunchSpec {
                auth: Auth::Canned,
                ..spec_for(h)
            };
            let endpoint = LaunchSpec {
                auth: Auth::Endpoint,
                wire: adapter.endpoint_wires().first().copied(),
                ..spec_for(h)
            };
            let keys = |spec: &LaunchSpec| -> Result<Vec<String>, HarnessError> {
                let inv = adapter.compile(spec, &ctx())?;
                let mut k: Vec<String> = inv.env.into_iter().map(|(k, _)| k).collect();
                k.sort();
                Ok(k)
            };
            let paths = |spec: &LaunchSpec| -> Result<Vec<PathBuf>, HarnessError> {
                Ok(adapter
                    .config_files(spec, &ctx())?
                    .into_iter()
                    .map(|(p, _)| p)
                    .collect())
            };
            let endpoint_only: Vec<&str> = adapter
                .spec()
                .env
                .iter()
                .filter(|e| e.when == crate::spec::When::Endpoint)
                .map(|e| e.key)
                .collect();
            if adapter.endpoint_wires().is_empty() {
                assert!(
                    keys(&endpoint).is_err(),
                    "{h}: no recipe, so no endpoint launch"
                );
                continue;
            }
            match (keys(&canned), keys(&endpoint)) {
                (Ok(c), Ok(mut e)) => {
                    e.retain(|k| !endpoint_only.contains(&k.as_str()));
                    assert_eq!(c, e, "{h}: env keys")
                }
                (Err(_), Err(_)) => continue,
                (c, e) => panic!("{h}: canned {c:?} but endpoint {e:?}"),
            }
            assert_eq!(
                paths(&canned).unwrap(),
                paths(&endpoint).unwrap(),
                "{h}: documents"
            );
            assert_eq!(
                adapter.mcp_route(&canned),
                adapter.mcp_route(&endpoint),
                "{h}: route"
            );
        }
    }

    /// **Every row's endpoint recipes are well-formed data**: a note on each, no wire twice, and
    /// no recipe variable that is also an isolation row's — a recipe selects a wire, it never moves
    /// where the harness keeps its state.
    #[test]
    fn every_rows_wire_recipes_are_noted_distinct_and_touch_no_isolation() {
        for h in Harness::ALL {
            let row = launch_adapter(h).unwrap().spec();
            let mut seen = Vec::new();
            for r in row.wires {
                assert!(!r.note.trim().is_empty(), "{h}: {:?} has no note", r.wire);
                assert!(!seen.contains(&r.wire), "{h}: {:?} twice", r.wire);
                seen.push(r.wire);
                for (k, _) in r.env {
                    let isolation = row
                        .env
                        .iter()
                        .any(|e| e.key == *k && matches!(e.val, crate::spec::Val::Under(_)));
                    assert!(!isolation, "{h}: recipe variable {k} relocates state");
                }
            }
        }
    }

    /// **An endpoint launch with no recipe for its wire is refused, never rendered half-aimed.**
    #[test]
    fn an_endpoint_launch_on_a_wire_the_row_cannot_render_is_refused() {
        for wire in [None, Some(marion_core::provider::Wire::Gemini)] {
            let spec = LaunchSpec {
                auth: Auth::Endpoint,
                wire,
                ..codex_spec()
            };
            let err = CodexAdapter.compile(&spec, &ctx()).unwrap_err().to_string();
            assert!(err.contains("recipe"), "{wire:?}: {err}");
        }
    }

    /// **Every harness but the ACP protocol row and agy can be pointed at an endpoint**, on the
    /// wire its canned overlay already renders — the row states it rather than a resolver guessing
    /// it. agy has no overlay at all (it runs only on the operator's own login), so no wire.
    #[test]
    fn every_row_but_acp_states_the_endpoint_wire_its_overlay_renders() {
        use marion_core::provider::Wire;
        for h in Harness::ALL {
            let wires = launch_adapter(h).unwrap().endpoint_wires();
            let wires = wires.as_slice();
            let want: &[Wire] = match h {
                Harness::ClaudeCode => &[Wire::AnthropicMessages],
                Harness::Codex => &[Wire::OpenAiResponses],
                Harness::Gemini => &[Wire::Gemini],
                Harness::Copilot => &[Wire::OpenAiChat, Wire::AnthropicMessages],
                Harness::OpenCode | Harness::Goose | Harness::Cline | Harness::Qwen => {
                    &[Wire::OpenAiChat]
                }
                Harness::Acp | Harness::Antigravity | Harness::Pi => &[],
            };
            assert_eq!(wires, want, "{h}");
        }
    }

    /// `h`'s endpoint launch on `wire`, presenting its key in `header`, rendered to env.
    fn keyed_env(
        h: Harness,
        wire: marion_core::provider::Wire,
        header: marion_core::provider::KeyHeader,
    ) -> Result<Vec<(String, String)>, HarnessError> {
        let mut spec = endpoint(spec_for(h), "m-1", h);
        spec.wire = Some(wire);
        spec.extra.key_header = Some(header);
        launch_adapter(h)?.compile(&spec, &ctx()).map(|i| i.env)
    }

    fn var<'e>(env: &'e [(String, String)], k: &str) -> Option<&'e str> {
        env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    /// **The provider's key header reaches the harness through the recipe, as data**: Claude
    /// Code sends `ANTHROPIC_API_KEY` as `x-api-key` and `ANTHROPIC_AUTH_TOKEN` as a Bearer
    /// token; copilot sends its API key as `x-api-key` on the anthropic type and its bearer token
    /// as `Authorization`. Each recipe renders the one the provider reads, and blanks the other.
    #[test]
    fn each_recipe_presents_the_key_in_the_header_its_provider_reads() {
        use marion_core::provider::{KeyHeader, Wire};
        let key = "sk-endpoint-test";
        let e = keyed_env(
            Harness::ClaudeCode,
            Wire::AnthropicMessages,
            KeyHeader::XApiKey,
        )
        .unwrap();
        assert_eq!(var(&e, "ANTHROPIC_API_KEY"), Some(key));
        assert_eq!(var(&e, "ANTHROPIC_AUTH_TOKEN"), Some(""));
        let e = keyed_env(
            Harness::ClaudeCode,
            Wire::AnthropicMessages,
            KeyHeader::Bearer,
        )
        .unwrap();
        assert_eq!(var(&e, "ANTHROPIC_AUTH_TOKEN"), Some(key));
        assert_eq!(var(&e, "ANTHROPIC_API_KEY"), Some(""));
        let e = keyed_env(
            Harness::Copilot,
            Wire::AnthropicMessages,
            KeyHeader::XApiKey,
        )
        .unwrap();
        assert_eq!(var(&e, copilot::PROVIDER_API_KEY_ENV), Some(key));
        assert_eq!(var(&e, copilot::PROVIDER_BEARER_TOKEN_ENV), None);
        let e = keyed_env(Harness::Copilot, Wire::AnthropicMessages, KeyHeader::Bearer).unwrap();
        assert_eq!(var(&e, copilot::PROVIDER_BEARER_TOKEN_ENV), Some(key));
        assert_eq!(var(&e, copilot::PROVIDER_API_KEY_ENV), Some(""));
        let e = keyed_env(Harness::Copilot, Wire::OpenAiChat, KeyHeader::Bearer).unwrap();
        assert_eq!(var(&e, copilot::PROVIDER_API_KEY_ENV), Some(key));
    }

    /// **A header a recipe cannot present is refused by name**, never sent in the other one.
    #[test]
    fn a_key_header_the_recipe_cannot_present_is_refused_naming_it() {
        use marion_core::provider::{KeyHeader, Wire};
        let err = keyed_env(Harness::Codex, Wire::OpenAiResponses, KeyHeader::XApiKey)
            .unwrap_err()
            .to_string();
        assert!(err.contains("x-api-key"), "{err}");
        // With no header stated, a launch presents its key the default way.
        let mut spec = endpoint(spec_for(Harness::Codex), "m-1", Harness::Codex);
        spec.extra.key_header = None;
        assert!(CodexAdapter.compile(&spec, &ctx()).is_ok());
    }

    /// **Every recipe states the headers it can present**, each noted, none twice, and Bearer —
    /// every OpenAI-compatible server's — among them.
    #[test]
    fn every_recipe_states_the_key_headers_it_can_present() {
        use marion_core::provider::KeyHeader;
        for h in Harness::ALL {
            for r in launch_adapter(h).unwrap().spec().wires {
                let headers: Vec<KeyHeader> = r.keys.iter().map(|k| k.header).collect();
                assert!(headers.contains(&KeyHeader::Bearer), "{h} {:?}", r.wire);
                let mut d = headers.clone();
                d.dedup();
                assert_eq!(d.len(), headers.len(), "{h} {:?}: a header twice", r.wire);
                for k in r.keys {
                    assert!(!k.note.trim().is_empty(), "{h} {:?} {:?}", r.wire, k.header);
                }
            }
        }
    }

    /// `spec` as an endpoint launch of `h`, on the first wire its row has a recipe for — what
    /// endpoint resolution chooses against a provider serving every wire.
    fn endpoint(spec: LaunchSpec, model: &str, h: Harness) -> LaunchSpec {
        LaunchSpec {
            auth: Auth::Endpoint,
            wire: launch_adapter(h).unwrap().endpoint_wires().first().copied(),
            base_url: Some("https://provider.example/v1".into()),
            api_key: Some("sk-endpoint-test".into()),
            model: Some(model.into()),
            ..spec
        }
    }

    fn env_of(inv: &Invocation, key: &str) -> Option<String> {
        inv.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    /// **A Claude Code endpoint node names no Claude model to a third party.** Its background
    /// calls (titles, summaries) default to a Haiku id; pointed at another provider, both the small
    /// and the default-Haiku model are the chosen one. Canned mode sets neither.
    #[test]
    fn a_claude_endpoint_node_routes_its_background_model_to_the_chosen_one() {
        let inv = ClaudeCodeAdapter
            .compile(
                &endpoint(claude_spec(), "glm-4.6", Harness::ClaudeCode),
                &ctx(),
            )
            .unwrap();
        for k in [
            "ANTHROPIC_SMALL_FAST_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        ] {
            assert_eq!(env_of(&inv, k).as_deref(), Some("glm-4.6"), "{k}");
        }
        assert_eq!(
            env_of(&inv, "ANTHROPIC_AUTH_TOKEN").as_deref(),
            Some("sk-endpoint-test")
        );
        assert_eq!(env_of(&inv, "ANTHROPIC_API_KEY").as_deref(), Some(""));
        assert_eq!(
            env_of(&inv, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://provider.example")
        );
        let canned = ClaudeCodeAdapter.compile(&claude_spec(), &ctx()).unwrap();
        assert_eq!(env_of(&canned, "ANTHROPIC_SMALL_FAST_MODEL"), None);
    }

    /// **An opencode endpoint node asks for the model verbatim**, slashes and all, under a
    /// provider block of marion's own name — a registry id like `openrouter` would merge with
    /// opencode's built-in provider of that name.
    #[test]
    fn an_opencode_endpoint_node_asks_for_the_model_verbatim_under_marions_block() {
        let spec = endpoint(
            opencode_spec(),
            "anthropic/claude-sonnet-4",
            Harness::OpenCode,
        );
        let inv = OpenCodeAdapter.compile(&spec, &ctx()).unwrap();
        assert_eq!(
            inv.model.as_deref(),
            Some("marion/anthropic/claude-sonnet-4")
        );
        let files = OpenCodeAdapter.config_files(&spec, &ctx()).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        let block = &doc["provider"]["marion"];
        assert_eq!(block["options"]["baseURL"], "https://provider.example/v1");
        assert_eq!(block["options"]["apiKey"], "sk-endpoint-test");
        assert!(
            block["models"]["anthropic/claude-sonnet-4"].is_object(),
            "{doc}"
        );
    }

    /// **A codex endpoint node names its model**, which a canned one never does: the canned
    /// server ignores it, a real endpoint serves exactly what it is asked for.
    #[test]
    fn a_codex_endpoint_node_names_its_model_and_its_key() {
        let inv = CodexAdapter
            .compile(
                &endpoint(codex_spec(), "gpt-5.1-codex", Harness::Codex),
                &ctx(),
            )
            .unwrap();
        assert_eq!(inv.model.as_deref(), Some("gpt-5.1-codex"));
        assert!(
            inv.args.windows(2).any(|w| w == ["-m", "gpt-5.1-codex"]),
            "{:?}",
            inv.args
        );
        assert_eq!(
            env_of(&inv, "MARION_PROVIDER_KEY").as_deref(),
            Some("sk-endpoint-test")
        );
    }

    /// **An endpoint node's bridge is told the supervisor's mode, never the node's provider.** A
    /// spawn the bridge serves is resolved afresh; handing it the provider's URL as
    /// `MARION_BASE_URL` would aim a grandchild's canned overlay at a third party.
    #[test]
    fn an_endpoint_nodes_bridge_declares_the_tree_mode_and_not_the_provider() {
        let provider_url = "https://provider.example/v1";
        for (tree_auth, tree_url, want_auth) in [
            (Auth::Inherited, None, "inherited"),
            (Auth::Canned, Some("http://127.0.0.1:8099/v1"), "canned"),
        ] {
            let spec = LaunchSpec {
                auth: Auth::Endpoint,
                base_url: Some(provider_url.into()),
                api_key: Some("sk-endpoint-test".into()),
                extra: Extras {
                    tree_auth: Some(tree_auth),
                    tree_base_url: tree_url.map(str::to_string),
                    ..Extras::default()
                },
                ..claude_spec()
            };
            let bridge = bridge_env(&spec, &ctx());
            assert_eq!(bridge.auth.as_wire(), want_auth);
            assert_eq!(bridge.base_url.as_deref(), tree_url);
            let pairs = bridge.pairs();
            assert!(
                !pairs
                    .iter()
                    .any(|(_, v)| v.contains("provider.example") || v.contains("sk-endpoint")),
                "{pairs:?}"
            );
        }
    }

    /// **The adapter a launch actually gets**, which is the one every sweep below must exercise.
    ///
    /// [`adapter_for`] answers from a harness name alone, and on `acp` that is deliberately
    /// unlaunchable: one protocol row serves many agents and they spell marion's verbs three
    /// different ways, so a harness name cannot say what the model will type. A sweep over
    /// [`Harness::ALL`] built on it would assert over an adapter no node is ever spawned with —
    /// and, worse, the fifth harness would answer every content question below with the
    /// [`acp::UNBOUND_TOOL_NAME`] sentinel or a refusal, i.e. be *silently exempt* from exactly the
    /// assertions these sweeps exist to make. So they bind the agent their own spec names, through
    /// [`adapter_for_type`] — the same seam the supervisor uses.
    ///
    /// Signature deliberately matches `adapter_for`'s, so a sweep reads the same either way and the
    /// only difference is which of the two is being claimed about.
    fn launch_adapter(h: Harness) -> Result<Box<dyn HarnessAdapter + Send + Sync>, HarnessError> {
        adapter_for_type(h, spec_for(h).extra.acp_agent.as_deref())
    }

    /// **§6.4's central MUST, as a sweep: marion never writes outside the node's own agent dir.**
    ///
    /// > *"marion never mutates the user's real harness config."*
    ///
    /// The caller writes whatever `config_files` hands it, unconditionally and with
    /// `create_dir_all` on the parent — so a path that escaped `config_dir` would not be caught
    /// anywhere downstream: it would simply be written, over the operator's own `~/.codex/config.toml`
    /// or `~/.gemini/settings.json`, and the first symptom would be a broken login on a harness
    /// marion was not even running.
    ///
    /// It sweeps **both auth modes**, because live mode is where this is easiest to break by
    /// accident: the tempting way to make a logged-in harness work is to stop relocating its config
    /// dir and let it read the real one, and one adapter doing that would put marion's generated
    /// document straight into `$HOME`. Live mode's premise is the opposite — the config dir stays
    /// marion's, and what is dropped is only the *overlay* of a base URL and a credential.
    ///
    /// A harness that refuses under a given mode is skipped rather than failed — but the sweep
    /// asserts that **something** was checked in each mode, and names the harnesses that must be
    /// among them, so it cannot pass by refusing everywhere. That named minimum is now
    /// [`Harness::ALL`] in both modes: every harness has a live route, so a skip is no longer a
    /// legitimate outcome anywhere and one reappearing would be a regression rather than a gap.
    ///
    /// **An adapter that emits no file is bound too, by the route it states.** A live opencode node
    /// writes nothing and carries its declaration in `OPENCODE_CONFIG_CONTENT`, a live codex node
    /// carries its own on `-c` flags; an empty `files` would otherwise let either through this sweep
    /// having proved nothing at all, which is the vacuous-pass shape the `checked` counter exists to
    /// prevent.
    /// One row's launch under one auth mode, as the escape sweep poses it.
    ///
    /// What `--live` implies: marion names no endpoint and mints no credential. The canned default
    /// names marion's own generated provider block, which a live opencode node deliberately does
    /// not write (see the refusal it earns), so that one row states its own live model.
    fn escape_spec(h: Harness, auth: Auth) -> LaunchSpec {
        let live = auth == Auth::Inherited;
        LaunchSpec {
            auth,
            base_url: spec_for(h).base_url.filter(|_| !live),
            api_key: spec_for(h).api_key.filter(|_| !live),
            model: match (h, auth) {
                (Harness::OpenCode, Auth::Inherited) => Some("anthropic/claude-sonnet-4-5".into()),
                _ => spec_for(h).model,
            },
            ..spec_for(h)
        }
    }

    /// [`McpRoute::Session`]: ACP writes no file and sets no variable — its declaration is a
    /// `session/new` request, and the check that it names marion's own server is
    /// `McpRoute::verify`'s Session branch, driven directly above.
    ///
    /// **The refusal is recorded, not skipped.** Where an agent has no measured way to reach
    /// marion's endpoint, `continue`ing past it would drop this harness out of the named minimum
    /// below, which is the vacuity that minimum exists to prevent.
    fn assert_session_route(
        h: Harness,
        auth: Auth,
        adapter: &(dyn HarnessAdapter + Send + Sync),
        spec: &LaunchSpec,
        files: &[(PathBuf, String)],
        k: &'static str,
    ) {
        assert!(
            !files.iter().any(|(_, body)| body.contains("\"mcp\"")),
            "{h} under {auth:?}: a session route whose document also declares marion's bridge \
             has two declarations and no single authority"
        );
        let inv = match adapter.compile(spec, &ctx()) {
            Err(e) => {
                assert!(
                    auth == Auth::Canned
                        && matches!(
                            e,
                            HarnessError::MissingInput {
                                harness: Harness::Acp,
                                ..
                            }
                        ),
                    "{h} under {auth:?}: {e}"
                );
                return;
            }
            Ok(inv) => inv,
        };
        let session = adapter
            .session_declaration(spec, &ctx())
            .unwrap_or_else(|e| panic!("{h} under {auth:?}: {e}"))
            .unwrap_or_else(|| panic!("{h} under {auth:?}: no session/new request"));
        assert!(
            McpRoute::Session(k)
                .verify(&[], &inv, Some(&session))
                .is_ok(),
            "{h} under {auth:?}: the request carries no marion declaration: {session}"
        );
    }

    /// [`McpRoute::Environment`]: the variable the row names carries marion's whole declaration,
    /// and the adapter wrote no document beside it.
    fn assert_environment_route(
        h: Harness,
        auth: Auth,
        adapter: &(dyn HarnessAdapter + Send + Sync),
        spec: &LaunchSpec,
        files: &[(PathBuf, String)],
        k: &str,
    ) {
        assert!(
            files.is_empty(),
            "{h} under {auth:?}: an env route that also writes files has two declarations and \
             no single authority"
        );
        let inv = adapter
            .compile(spec, &ctx())
            .unwrap_or_else(|e| panic!("{h} under {auth:?}: {e}"));
        let (_, v) = inv
            .env
            .iter()
            .find(|(n, _)| n == k)
            .unwrap_or_else(|| panic!("{h} under {auth:?}: ${k} was never set"));
        assert!(
            v.contains("\"mcp\"") && v.contains(opencode::MCP_ALIAS),
            "{h} under {auth:?}: ${k} carries no marion declaration: {v}"
        );
    }

    /// [`McpRoute::Argv`]: the declaration rides argv, and the adapter wrote no document beside it.
    fn assert_argv_route(
        h: Harness,
        auth: Auth,
        adapter: &(dyn HarnessAdapter + Send + Sync),
        spec: &LaunchSpec,
        files: &[(PathBuf, String)],
        key: &str,
    ) {
        assert!(
            files.is_empty(),
            "{h} under {auth:?}: an argv route that also writes files has two declarations and \
             no single authority"
        );
        let inv = adapter
            .compile(spec, &ctx())
            .unwrap_or_else(|e| panic!("{h} under {auth:?}: {e}"));
        assert!(
            inv.args.iter().any(|a| a.contains(key)),
            "{h} under {auth:?}: argv carries no marion declaration: {:?}",
            inv.args
        );
    }

    /// Whichever route this adapter took, it took *a* route, and the route it named is the one it
    /// actually used. One checker per [`McpRoute`].
    fn assert_route_is_the_one_taken(
        h: Harness,
        auth: Auth,
        adapter: &(dyn HarnessAdapter + Send + Sync),
        spec: &LaunchSpec,
        files: &[(PathBuf, String)],
    ) {
        match adapter.mcp_route(spec) {
            McpRoute::Session(k) => assert_session_route(h, auth, adapter, spec, files, k),
            McpRoute::Document => assert!(
                !files.is_empty(),
                "{h} under {auth:?}: names a document route and wrote none"
            ),
            McpRoute::Environment(k) => assert_environment_route(h, auth, adapter, spec, files, k),
            McpRoute::Argv(key) => assert_argv_route(h, auth, adapter, spec, files, key),
            McpRoute::None => {
                panic!("{h} under {auth:?}: asked for marion's bridge and routed nowhere")
            }
        }
    }

    /// Every path an adapter handed back lands inside the node's own agent dir, by an absolute
    /// path, so where it lands does not depend on the writer's cwd.
    fn assert_paths_stay_in_the_agent_dir(
        h: Harness,
        auth: Auth,
        spec: &LaunchSpec,
        files: &[(PathBuf, String)],
    ) {
        for (path, _) in files {
            assert!(
                path.starts_with(&spec.config_dir),
                "{h} under {auth:?} would write {} outside its agent dir {} — §6.4: marion \
                 never mutates the user's real harness config",
                path.display(),
                spec.config_dir.display()
            );
            assert!(
                path.is_absolute(),
                "{h} under {auth:?}: {} is relative, so where it lands depends on the writer's cwd",
                path.display()
            );
        }
    }

    #[test]
    fn no_config_file_any_adapter_emits_ever_escapes_marions_own_agent_dir() {
        for auth in [Auth::Canned, Auth::Inherited] {
            let mut checked = 0;
            let mut bound: Vec<Harness> = Vec::new();
            for h in Harness::ALL {
                let spec = escape_spec(h, auth);
                let adapter = launch_adapter(h).unwrap();
                let Ok(files) = adapter.config_files(&spec, &ctx()) else {
                    continue;
                };
                assert_route_is_the_one_taken(h, auth, adapter.as_ref(), &spec, &files);
                bound.push(h);
                assert_paths_stay_in_the_agent_dir(h, auth, &spec, &files);
                checked += files.len();
            }
            assert!(
                checked > 0,
                "{auth:?}: no adapter emitted a single path, so this proved nothing"
            );
            // The named minimum. Without it the sweep would silently shrink to whichever harnesses
            // happen still to compile under a mode, which is exactly how it passed vacuously for
            // gemini and opencode before they had a live route at all.
            for h in Harness::ALL {
                assert!(
                    bound.contains(&h),
                    "{h} under {auth:?} was skipped: it must be exercised in both modes, or this \
                     sweep says nothing about the mode where escaping is easiest"
                );
            }
        }
    }

    /// **Live mode is pure removal on Claude Code, and this is the list of what is removed.**
    ///
    /// Asserted by *name*, not by comparing the whole env: the failure being defended against is one
    /// of the three creeping back, and `ANTHROPIC_API_KEY` is the dangerous one — marion sets it to
    /// the empty string under `Canned` precisely so a real key cannot silently win, which is exactly
    /// the wrong thing to do to a node meant to be using that key.
    #[test]
    fn a_live_claude_node_carries_none_of_the_three_anthropic_env_vars() {
        let spec = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..claude_spec()
        };
        let inv = ClaudeCodeAdapter.compile(&spec, &ctx()).unwrap();
        for k in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(
                !inv.env.iter().any(|(n, _)| n == k),
                "{k} must be absent under --live: marion inherits the operator's login rather \
                 than overlaying one. env: {:?}",
                inv.env
            );
        }
        assert_eq!(
            inv.env,
            vec![("DISABLE_AUTOUPDATER".to_string(), "1".to_string())],
            "the three are the whole provider overlay, so a live node's env additions are the \
             row's no-self-update switch and nothing else: {:?}",
            inv.env
        );
        // And the isolation that was never an overlay is untouched.
        assert!(
            !inv.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
            "isolating it breaks OAuth, under --live most of all"
        );
    }

    /// The other half: nothing *else* changes. A live node still gets the fileless MCP declaration
    /// and still runs only marion's MCP servers; the one argv difference from canned is the
    /// settings exclusion, which a live node must not carry (the test below).
    #[test]
    fn a_live_claude_node_keeps_the_same_argv_as_a_canned_one_but_the_settings_exclusion() {
        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..claude_spec()
        };
        let canned = ClaudeCodeAdapter
            .compile(&claude_spec(), &ctx())
            .unwrap()
            .args;
        let i = canned
            .iter()
            .position(|a| a == "--setting-sources")
            .unwrap();
        let mut without_exclusion = canned.clone();
        without_exclusion.drain(i..i + 2);
        assert_eq!(
            ClaudeCodeAdapter.compile(&live, &ctx()).unwrap().args,
            without_exclusion,
            "live differs from canned in env and the settings exclusion only"
        );
        let inv = ClaudeCodeAdapter.compile(&live, &ctx()).unwrap();
        assert!(inv.args.iter().any(|a| a == "--strict-mcp-config"));
        let i = inv.args.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(
            PathBuf::from(&inv.args[i + 1]),
            ClaudeCodeAdapter.config_files(&live, &ctx()).unwrap()[0].0,
            "the declaration a live node reads is still the one marion wrote in its agent dir"
        );
    }

    /// **A live claude node reads the operator's own settings, because a settings file can be
    /// where their credential lives.** `apiKeyHelper`, `awsAuthRefresh` and the `env` block
    /// (`ANTHROPIC_API_KEY`, `CLAUDE_CODE_USE_BEDROCK`, a gateway's `ANTHROPIC_BASE_URL`) are all
    /// settings keys, and `--setting-sources ""` drops every one of them — measured on 2.1.280:
    /// a config dir whose `settings.json` alone carries the credential answers `Not logged in` with
    /// the flag and takes the settings' route with `--setting-sources=user`. Canned keeps the flag:
    /// there marion supplies the credential and the operator's settings have nothing to add.
    #[test]
    fn a_live_claude_node_loads_the_operators_own_settings_so_a_settings_credential_reaches_it() {
        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..claude_spec()
        };
        let headless = ClaudeCodeAdapter.compile(&live, &ctx()).unwrap();
        let pane = ClaudeCodeAdapter.compile_pane(&live, &ctx()).unwrap();
        for (shape, inv) in [("headless", &headless), ("pane", &pane)] {
            assert!(
                !inv.args.iter().any(|a| a == "--setting-sources"),
                "{shape}: a live node must not drop the operator's settings: {:?}",
                inv.args
            );
            assert!(
                inv.args.iter().any(|a| a == "--strict-mcp-config"),
                "{shape}: MCP isolation is not a credential source and stays: {:?}",
                inv.args
            );
        }
        for inv in [
            ClaudeCodeAdapter.compile(&claude_spec(), &ctx()).unwrap(),
            ClaudeCodeAdapter
                .compile_pane(&claude_spec(), &ctx())
                .unwrap(),
        ] {
            let i = inv
                .args
                .iter()
                .position(|a| a == "--setting-sources")
                .expect("canned keeps the operator's settings out");
            assert_eq!(inv.args[i + 1], "");
        }
    }

    /// **A claude node marion launches runs none of the operator's hooks, while still reading the
    /// settings its login may live in.** Loading the operator's settings (the test above) also
    /// loads their hooks and their plugins' hooks, and a `Stop` hook that blocks — a review gate —
    /// would hold or loop a headless node no one is watching. `--settings '{"disableAllHooks":
    /// true}'` merges one key over their layers: measured on 2.1.283 with a config dir whose
    /// `settings.json` carried an `env` credential (and, separately, an `apiKeyHelper`) plus user
    /// `SessionStart`/`UserPromptSubmit`/`Stop` hooks and an installed plugin's `SessionStart`/`Stop`
    /// hooks, no hook fired and the request still reached the endpoint with the settings' key.
    /// Control-protocol hook callbacks registered in `initialize` still fire under it.
    ///
    /// Every shape marion launches carries it, headless and pane, live and canned (where
    /// `--setting-sources ""` already keeps the hooks out, so it is redundant, not different).
    /// The native facade does not: there the operator drives their own claude, hooks and all.
    #[test]
    fn a_claude_node_marion_launches_runs_no_operator_hooks_but_the_native_facade_does() {
        use std::ffi::OsString;

        use crate::native::{NativeEnvironmentView, NativeNodeContext, native_adapter};

        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..claude_spec()
        };
        for (shape, inv) in [
            ("live headless", ClaudeCodeAdapter.compile(&live, &ctx())),
            ("live pane", ClaudeCodeAdapter.compile_pane(&live, &ctx())),
            (
                "canned headless",
                ClaudeCodeAdapter.compile(&claude_spec(), &ctx()),
            ),
            (
                "canned pane",
                ClaudeCodeAdapter.compile_pane(&claude_spec(), &ctx()),
            ),
        ] {
            let args = inv.unwrap().args;
            let i = args
                .iter()
                .position(|a| a == "--settings")
                .unwrap_or_else(|| panic!("{shape}: no hooks-off overlay: {args:?}"));
            let overlay: serde_json::Value = serde_json::from_str(&args[i + 1]).unwrap();
            assert_eq!(
                overlay,
                serde_json::json!({"disableAllHooks": true}),
                "{shape}: the overlay turns hooks off and adds nothing else"
            );
        }

        let native = native_adapter(Harness::ClaudeCode).unwrap();
        let operator_env = vec![(OsString::from("PATH"), OsString::from("/usr/bin"))];
        let injection = native
            .prepare_native(&NativeNodeContext {
                bridge: &bridge_env(&live, &ctx()),
                document_dir: &PathBuf::from("/state/agents/019f-root"),
                allowed_marion_tools: &["spawn", "wait", "status"],
                environment: NativeEnvironmentView::validate(&operator_env).unwrap(),
            })
            .unwrap();
        assert!(
            !injection.argv_prefix.iter().any(|a| a == "--settings"),
            "the operator's own claude keeps their hooks: {:?}",
            injection.argv_prefix
        );
    }

    /// **Canned mode is byte-identical to what it produced before the auth axis existed**, on all
    /// four harnesses, asserted against each harness's own emitter rather than against a snapshot —
    /// a snapshot would drift with any legitimate change, while this pins the one claim that
    /// matters: routing through `Auth::Canned` adds nothing and drops nothing.
    ///
    /// `Auth::Canned` is also the `Default`, which is what makes "today's behaviour is untouched"
    /// true of every caller that never mentions the field.
    #[test]
    fn canned_mode_emits_exactly_what_each_harnesss_own_emitter_does() {
        assert_eq!(Auth::default(), Auth::Canned);
        for h in Harness::ALL {
            let explicit = LaunchSpec {
                auth: Auth::Canned,
                ..spec_for(h)
            };
            // The unnamed side is `Auth::default()` rather than whatever `spec_for` chose, which
            // is the claim's actual subject: *naming the default changes nothing*. On four
            // harnesses these are the same spec; on `acp`, whose spec names `Inherited` because it
            // has no canned mode at all, they are the same **refusal**, which is equally the claim.
            let implicit = LaunchSpec {
                auth: Auth::default(),
                ..spec_for(h)
            };
            let a = launch_adapter(h).unwrap();
            assert_eq!(
                a.compile(&explicit, &ctx()).ok(),
                a.compile(&implicit, &ctx()).ok(),
                "{h}: naming the default must not change the compile"
            );
            assert_eq!(
                a.config_files(&explicit, &ctx()).ok(),
                a.config_files(&implicit, &ctx()).ok(),
                "{h}: naming the default must not change the configuration"
            );
        }

        // gemini and opencode, against their own emitters — the claude and codex equivalences are
        // asserted above, and these two had none, so the byte-identity claim covered half the matrix.
        let g = GeminiAdapter.config_files(&gemini_spec(), &ctx()).unwrap();
        assert_eq!(
            g,
            vec![(
                PathBuf::from("/state/x/config/marion-settings.json"),
                serde_json::to_string_pretty(&gemini::settings_json(Some(&BridgeEnv {
                    bridge: "/bin/marion-supervisor".into(),
                    args: vec!["mcp".into()],
                    repo: "/repo".into(),
                    state: "/state".into(),
                    base_url: Some("http://127.0.0.1:8099/v1".into()),
                    auth: Auth::Canned,
                    agent_id: AgentId("019f-root".into()),
                    agent_type: "claude".into(),
                    depth: 0,
                    node_token: None,
                    ready_file: Some("/state/x/mcp-ready".into()),
                })))
                .unwrap()
            )]
        );
        let o = OpenCodeAdapter
            .config_files(&opencode_spec(), &ctx())
            .unwrap();
        assert_eq!(
            o,
            vec![(
                opencode::config_path(&PathBuf::from("/state/x/config")),
                serde_json::to_string_pretty(&opencode::config_json(
                    &opencode::ConfigSpec {
                        model: opencode::ModelRef::parse("canned/canned-1").unwrap(),
                        base_url: "http://127.0.0.1:8099/v1".into(),
                        api_key: Some("sk-fake".into()),
                        key_header: Default::default(),
                    },
                    Some(&BridgeEnv {
                        bridge: "/bin/marion-supervisor".into(),
                        args: vec!["mcp".into()],
                        repo: "/repo".into(),
                        state: "/state".into(),
                        base_url: Some("http://127.0.0.1:8099/v1".into()),
                        auth: Auth::Canned,
                        agent_id: AgentId("019f-root".into()),
                        agent_type: "claude".into(),
                        depth: 0,
                        node_token: None,
                        ready_file: Some("/state/x/mcp-ready".into()),
                    })
                ))
                .unwrap()
            )]
        );
    }

    /// **Depth reaches the bridge on all four harnesses, or `max_depth` is inert on the ones it
    /// does not reach.**
    ///
    /// The bridge is a process the *harness* starts, so the per-server `env` block is the only
    /// channel marion has to tell a node where in the tree it sits. Until this was carried, nothing
    /// anywhere computed a depth: `check_spawn_gates` had no production caller, `max_depth` was a
    /// number no code read, and a child could spawn a grandchild, and that grandchild another,
    /// without bound. The failure was **silent** on three of the four — only Claude Code reads
    /// `allowed_tools`, and codex's generated config sets
    /// `default_tools_approval_mode = "approve"`, so a codex, gemini or opencode child's `spawn`
    /// was simply *served*.
    ///
    /// Asserted on the emitted bytes rather than through a struct, and format-agnostically: three
    /// harnesses emit JSON (`"7"`) and codex emits TOML (`= "7"`), and both contain the quoted
    /// value. A distinctive depth is used so the assertion cannot pass on some other field's value.
    #[test]
    fn a_nodes_depth_and_agent_type_reach_its_bridge_on_every_harness() {
        for h in Harness::ALL {
            let ctx = SpawnCtx {
                depth: 7,
                agent_type: "codex-impl".into(),
                ..ctx()
            };
            let doc = declaration_bytes(h, &spec_for(h), &ctx);
            for key in [AGENT_TYPE_ENV, DEPTH_ENV] {
                assert!(
                    doc.contains(key),
                    "{h}: {key} is not in its bridge env:\n{doc}"
                );
            }
            assert!(
                doc.contains("\"7\""),
                "{h}: the depth VALUE must be carried, not just its key:\n{doc}"
            );
            assert!(
                doc.contains("\"codex-impl\""),
                "{h}: §6.1 step 2's gates read the caller's agent type, so its name must \
                 reach the bridge:\n{doc}"
            );
        }
    }

    /// **The bytes this adapter's MCP declaration actually travels in, whichever channel carries
    /// them.**
    ///
    /// The three sweeps below assert on *content* — that a node's depth, agent type and capability
    /// token reach its bridge — and they used to read `config_files[0]`. That is the same reader
    /// for four harnesses and **no reader at all** for the fifth: ACP writes no document, so every
    /// one of those assertions would have been silently exempt on it. Which is precisely the shape
    /// `a_nodes_depth_and_agent_type_reach_its_bridge_on_every_harness` was written after finding,
    /// one harness earlier. So the sweeps ask the route.
    fn declaration_bytes(h: Harness, spec: &LaunchSpec, ctx: &SpawnCtx) -> String {
        let a = launch_adapter(h).unwrap();
        match a.mcp_route(spec) {
            McpRoute::Document => a
                .config_files(spec, ctx)
                .unwrap_or_else(|e| panic!("{h}: {e}"))
                .first()
                .map(|(_, c)| c.clone())
                .unwrap_or_else(|| panic!("{h}: claims a document route and emitted none")),
            McpRoute::Environment(k) => a
                .compile(spec, ctx)
                .unwrap_or_else(|e| panic!("{h}: {e}"))
                .env
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{h}: claims ${k} and did not set it")),
            // The argv tokens, plus the compiled environment as `K="V"` lines: on goose the
            // declaration is one whitespace-split token whose pairs are unquoted (`MARION_DEPTH=7`)
            // and whose capability token deliberately rides the process environment instead
            // (`goose::extension_declaration`), so "the bytes the bridge receives" are both. Each
            // `K=V` token is also re-spelled `K="V"` so the value-quoting assertions below read
            // the same across TOML, JSON and this grammar.
            McpRoute::Argv(_) => {
                let inv = a.compile(spec, ctx).unwrap_or_else(|e| panic!("{h}: {e}"));
                let mut bytes = inv.args.join(" ");
                for pair in inv
                    .args
                    .iter()
                    .flat_map(|arg| arg.split_whitespace())
                    .filter_map(|tok| tok.split_once('='))
                {
                    bytes.push_str(&format!("\n{}=\"{}\"", pair.0, pair.1));
                }
                for (k, v) in &inv.env {
                    bytes.push_str(&format!("\n{k}=\"{v}\""));
                }
                bytes
            }
            McpRoute::Session(_) => a
                .session_declaration(spec, ctx)
                .unwrap_or_else(|e| panic!("{h}: {e}"))
                .unwrap_or_else(|| panic!("{h}: claims a session route and compiled no request"))
                .to_string(),
            McpRoute::None => panic!("{h}: asked for marion's bridge and routed nowhere"),
        }
    }

    /// **§5.4's per-node capability token, on every harness** — the other half of the same
    /// argument, and the one that decides whether §6.1 step 2's gates bind at all.
    ///
    /// The identity above travels to the bridge and the bridge states it back over the socket. On
    /// its own that is a *claim*: `serve_conn` performs no peer-credential check, so any process
    /// that can `connect(2)` could assert `depth: 0` and spawn. The token is what makes the claim
    /// checkable — the supervisor minted it, holds the binding, and wrote it into exactly one
    /// place. A harness whose declaration dropped it would have a bridge that could never spawn,
    /// which is at least loud; the failure this asserts against is the one that is not, where three
    /// harnesses carry it and the fourth is silently exempt — precisely the shape
    /// `a_nodes_depth_and_agent_type_reach_its_bridge_on_every_harness` was written after finding.
    ///
    /// Asserted on the emitted bytes, format-agnostically, for the reason the depth test gives.
    #[test]
    fn a_nodes_capability_token_reaches_its_bridge_on_every_harness() {
        for h in Harness::ALL {
            let ctx = SpawnCtx {
                node_token: Some("MARION-TOKEN-VALUE-4e1b".into()),
                ..ctx()
            };
            let doc = declaration_bytes(h, &spec_for(h), &ctx);
            assert!(
                doc.contains(mcp_bridge::NODE_TOKEN_ENV),
                "{h}: {} is not in its bridge env:\n{doc}",
                mcp_bridge::NODE_TOKEN_ENV
            );
            assert!(
                doc.contains("\"MARION-TOKEN-VALUE-4e1b\""),
                "{h}: the token VALUE must be carried, not just its key:\n{doc}"
            );
        }
    }

    /// **Every row that declares on argv withholds the token from that declaration** — the row data
    /// half of keeping the token out of `ps`, for every harness row in each auth mode and every ACP
    /// refinement row. A carrier inside the declaration is fine on a document, a variable or
    /// marion's own ACP pipe; on argv it is the leak.
    #[test]
    fn every_argv_declaration_withholds_the_node_token() {
        use crate::spec::McpRoute;

        for h in Harness::ALL {
            let row = harness_spec(h);
            for (mode, route, carrier) in [
                ("canned", row.mcp.canned, row.token.canned),
                ("live", row.mcp.live, row.token.live),
            ] {
                if matches!(route, McpRoute::Argv(_)) {
                    assert!(
                        carrier.withholds(),
                        "{h} ({mode}): the declaration rides argv and carries the token"
                    );
                }
            }
            if let Some(declaration) = row.live_declaration {
                assert_eq!(declaration.route(), row.mcp.live, "{h}");
            }
        }
        for agent in acp::AGENTS {
            if let acp::Declaration::Argv { token, .. } = agent.declaration {
                assert!(
                    token.withholds(),
                    "acp:{}: the declaration rides argv and carries the token",
                    agent.id
                );
            }
        }
    }

    /// The bytes a launch's declaration consists of, **excluding the process environment**: every
    /// document, the declaring variable, the argv, or the `session/new` request — wherever the
    /// row's route puts it.
    fn declaration_without_env(
        a: &dyn HarnessAdapter,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
        inv: &Invocation,
    ) -> String {
        use crate::spec::McpRoute;

        match a.mcp_route(spec) {
            McpRoute::Document => a
                .config_files(spec, ctx)
                .unwrap()
                .into_iter()
                .map(|(_, c)| c)
                .collect(),
            McpRoute::Environment(k) => inv
                .env
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default(),
            McpRoute::Argv(_) => inv.args.join(" "),
            McpRoute::Session(_) => a
                .session_declaration(spec, ctx)
                .unwrap()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            McpRoute::None => String::new(),
        }
    }

    /// **No launch puts the node token on argv, and every launch still hands it to the bridge** —
    /// the compiled half, over every harness row in every auth mode and shape it compiles, and every
    /// ACP refinement row. A carrier that withholds the token leaves it out of the declaration and
    /// sets it on the environment exactly once; the declaration carrier writes it into the
    /// declaration and adds nothing to the environment. A launch the row refuses is skipped, but
    /// every harness must compile at least one, and the argv rows their live launch.
    #[test]
    fn no_launch_puts_the_node_token_on_argv_and_every_launch_delivers_it() {
        const TOKEN: &str = "tok-SENTINEL-sweep-5a0c";
        let minted = SpawnCtx {
            node_token: Some(TOKEN.into()),
            ..ctx()
        };
        let check = |who: &str, a: &dyn HarnessAdapter, spec: &LaunchSpec, inv: &Invocation| {
            assert!(
                !inv.args.iter().any(|arg| arg.contains(TOKEN)),
                "{who}: the node token is on argv: {:?}",
                inv.args
            );
            let in_env: Vec<&String> = inv
                .env
                .iter()
                .filter(|(k, _)| k == mcp_bridge::NODE_TOKEN_ENV)
                .map(|(_, v)| v)
                .collect();
            let declaration = declaration_without_env(a, spec, &minted, inv);
            if a.token_carrier(spec).withholds() {
                assert_eq!(in_env, [TOKEN], "{who}: the environment carries it once");
                assert!(
                    !declaration.contains(TOKEN),
                    "{who}: the declaration withholds it: {declaration}"
                );
            } else {
                assert!(in_env.is_empty(), "{who}: {:?}", inv.env);
                assert!(
                    declaration.contains(TOKEN),
                    "{who}: the declaration carries it: {declaration}"
                );
            }
        };
        let live_of = |h: Harness| LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            model: match h {
                Harness::OpenCode => Some("anthropic/claude-sonnet-4-5".into()),
                Harness::Copilot | Harness::Goose | Harness::Pi | Harness::Qwen => None,
                _ => spec_for(h).model,
            },
            ..spec_for(h)
        };
        for h in Harness::ALL {
            let a = launch_adapter(h).unwrap();
            let endpoint = LaunchSpec {
                auth: Auth::Endpoint,
                wire: a.endpoint_wires().first().copied(),
                ..spec_for(h)
            };
            let canned = LaunchSpec {
                auth: Auth::Canned,
                ..spec_for(h)
            };
            let mut compiled = Vec::new();
            for spec in [canned, endpoint, live_of(h)] {
                let shapes = [
                    ("headless", a.compile(&spec, &minted)),
                    ("pane", a.compile_pane(&spec, &minted)),
                ];
                for (shape, inv) in shapes {
                    let Ok(inv) = inv else { continue };
                    let who = format!("{h} {:?} {shape}", spec.auth);
                    check(&who, a.as_ref(), &spec, &inv);
                    compiled.push((spec.auth, shape));
                }
            }
            assert!(!compiled.is_empty(), "{h}: no launch compiled at all");
            let row = harness_spec(h);
            if matches!(row.mcp.live, crate::spec::McpRoute::Argv(_)) {
                assert!(
                    compiled.iter().any(|(auth, _)| *auth == Auth::Inherited),
                    "{h}: its argv route was never compiled: {compiled:?}"
                );
            }
        }
        for agent in acp::AGENTS {
            let a = AcpAdapter::for_agent(agent);
            let spec = LaunchSpec {
                extra: Extras {
                    acp_agent: Some(agent.id.into()),
                    ..Extras::default()
                },
                ..acp_spec()
            };
            let inv = a
                .compile(&spec, &minted)
                .unwrap_or_else(|e| panic!("acp:{}: {e}", agent.id));
            check(&format!("acp:{}", agent.id), &a, &spec, &inv);
        }
    }

    /// The native lane's half: `marion <harness>` puts no token on the operator's harness's argv,
    /// and hands it to the bridge through the row's live carrier — the process environment where
    /// the carrier withholds it, the declaration otherwise.
    #[test]
    fn no_native_injection_puts_the_node_token_on_argv() {
        use std::ffi::OsString;

        use crate::native::{NativeEnvironmentView, NativeNodeContext, native_adapter};

        const TOKEN: &str = "tok-SENTINEL-native-sweep-3d92";
        let minted = SpawnCtx {
            node_token: Some(TOKEN.into()),
            ..ctx()
        };
        let document_dir = PathBuf::from("/state/agents/019f-root");
        let operator_env: Vec<(OsString, OsString)> = Vec::new();
        for h in Harness::ALL {
            let Some(adapter) = native_adapter(h) else {
                continue;
            };
            let live = LaunchSpec {
                auth: Auth::Inherited,
                base_url: None,
                config_dir: document_dir.clone(),
                ..spec_for(h)
            };
            let bridge = bridge_env(&live, &minted);
            let injection = adapter
                .prepare_native(&NativeNodeContext {
                    bridge: &bridge,
                    document_dir: &document_dir,
                    allowed_marion_tools: &["spawn"],
                    environment: NativeEnvironmentView::validate(&operator_env).unwrap(),
                })
                .unwrap_or_else(|e| panic!("{h}: {e}"));
            let lossy = |v: &OsString| v.to_string_lossy().into_owned();
            assert!(
                !injection
                    .argv_prefix
                    .iter()
                    .any(|a| lossy(a).contains(TOKEN)),
                "{h}: the node token is on the native argv: {:?}",
                injection.argv_prefix
            );
            let passed: Vec<String> = injection
                .bridge_env
                .iter()
                .filter(|(k, _)| k == mcp_bridge::NODE_TOKEN_ENV)
                .map(|(_, v)| lossy(v))
                .collect();
            let declared = injection
                .documents
                .iter()
                .any(|d| String::from_utf8_lossy(&d.contents).contains(TOKEN))
                || injection
                    .env_overlay
                    .iter()
                    .any(|(_, v)| lossy(v).contains(TOKEN));
            if harness_spec(h).token.live.withholds() {
                assert_eq!(passed, [TOKEN], "{h}");
                assert!(!declared, "{h}: the declaration withholds it");
            } else {
                assert!(passed.is_empty(), "{h}");
                assert!(declared, "{h}: the declaration carries it");
            }
        }
    }

    /// **The operator's endpoint key never reaches a debug print of a launch.** `LaunchSpec` is
    /// what every refusal and every failing compile assertion has in hand, so a `{:?}` of it
    /// would otherwise print the key `marion login` stored.
    #[test]
    fn an_endpoint_key_never_appears_in_its_launch_specs_debug_form() {
        let spec = LaunchSpec {
            api_key: Some("sk-SENTINEL-launch-2b7e".into()),
            auth: Auth::Endpoint,
            ..claude_spec()
        };
        let printed = format!("{spec:?} {spec:#?}");
        assert!(!printed.contains("SENTINEL"), "{printed}");
        assert!(printed.contains("api_key"), "{printed}");
    }

    /// **A node's token never reaches a debug print of its spawn context.** `SpawnCtx` is threaded
    /// through every compile, so a `{:?}` in a refusal, a panic or a failing assertion would
    /// otherwise print the node's capability.
    #[test]
    fn a_nodes_capability_token_never_appears_in_its_spawn_contexts_debug_form() {
        let ctx = SpawnCtx {
            node_token: Some("MARION-TOKEN-VALUE-4e1b".into()),
            ..ctx()
        };
        let printed = format!("{ctx:?} {ctx:#?}");
        assert!(!printed.contains("MARION-TOKEN-VALUE"), "{printed}");
        assert!(printed.contains("node_token"), "{printed}");
    }

    /// **Present or absent, never empty** — [`mcp_bridge::BASE_URL_ENV`]'s rule, applied to the one
    /// value where breaking it is a security hole rather than a misconfiguration.
    ///
    /// `MARION_NODE_TOKEN=""` read back through `var()` is `Ok("")`, which is a capability token
    /// every process on the machine can guess. A node marion minted no token for must find no key,
    /// so its bridge states no capability rather than a worthless one that any check asking only
    /// *"was a token presented?"* would believe.
    #[test]
    fn a_node_with_no_token_declares_no_token_key_rather_than_an_empty_one() {
        for h in Harness::ALL {
            let ctx = SpawnCtx {
                node_token: None,
                ..ctx()
            };
            let doc = declaration_bytes(h, &spec_for(h), &ctx);
            assert!(
                !doc.contains(mcp_bridge::NODE_TOKEN_ENV),
                "{h}: a node with no token must declare no token key, not an empty one:\n{doc}"
            );
        }
    }

    /// A root is depth 0 and a `spawn` of its own would be at depth 1 — so a document that carried
    /// no depth at all, or carried it as anything but the node's own, would let the gate read a
    /// number marion never assigned. The zero is asserted explicitly because it is the one value
    /// that could plausibly be confused with "absent".
    #[test]
    fn a_root_declares_depth_zero_rather_than_omitting_it() {
        let v: serde_json::Value = serde_json::from_str(
            &ClaudeCodeAdapter
                .config_files(&claude_spec(), &ctx())
                .unwrap()[0]
                .1,
        )
        .unwrap();
        assert_eq!(
            v["mcpServers"]["marion"]["env"][DEPTH_ENV],
            serde_json::json!("0"),
            "a root is depth 0 by definition (§3.1), and an absent value is not the same claim"
        );
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

    /// The gemini node under `--live`, at the adapter seam: three variables gone by name, and the
    /// settings path — the *whole* MCP injection route on this harness, there being no `--settings`
    /// flag — still naming the document `config_files` writes.
    #[test]
    fn a_live_gemini_node_drops_three_env_vars_and_keeps_its_mcp_route() {
        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..gemini_spec()
        };
        let inv = GeminiAdapter.compile(&live, &ctx()).unwrap();
        for k in [
            "GEMINI_CLI_HOME",
            "GOOGLE_GEMINI_BASE_URL",
            "GEMINI_API_KEY",
        ] {
            assert!(
                !inv.env.iter().any(|(n, _)| n == k),
                "{k} must be absent under --live. env: {:?}",
                inv.env
            );
        }
        let (_, named) = inv
            .env
            .iter()
            .find(|(k, _)| k == "GEMINI_CLI_SYSTEM_SETTINGS_PATH")
            .expect("the settings path IS the MCP injection route; without it a live node has no bridge");
        let files = GeminiAdapter.config_files(&live, &ctx()).unwrap();
        assert_eq!(PathBuf::from(named), files[0].0);
        let v: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(
            v["mcpServers"]["marion"]["trust"],
            serde_json::json!(true),
            "without it the tools are omitted from the request body with no error anywhere"
        );
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_AUTH"],
            serde_json::json!("inherited"),
            "a child this node spawns must reach the same endpoint it did"
        );
    }

    /// **`GEMINI_FORCE_FILE_STORAGE`'s absence under `--live` is a safety property.** Over the
    /// operator's real `~/.gemini` it can trigger the one-way migration `tests/fixtures/s12/`
    /// records — read `oauth_creds.json`, write the hybrid store, `fs.rm` the original — which is
    /// marion destroying a file it does not own (§6.4).
    #[test]
    fn live_never_forces_file_storage_because_the_migration_deletes_the_operators_credential() {
        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..gemini_spec()
        };
        assert!(
            !GeminiAdapter
                .compile(&live, &ctx())
                .unwrap()
                .env
                .iter()
                .any(|(k, _)| k == "GEMINI_FORCE_FILE_STORAGE")
        );
    }

    /// A live node declares **no** auth selection, with or without a bridge: this document is the
    /// system-settings layer, and any `selectedType` in it pins every operator to one route —
    /// `oauth-personal` pinned an operator whose only credential is `GEMINI_API_KEY` to a login
    /// Google now refuses to individuals. Silent, gemini runs its own `user || env` resolution, and
    /// the operator's own `settings.json` still wins because the layers merge leaf by leaf (S30).
    #[test]
    fn a_live_gemini_node_leaves_the_auth_selection_to_gemini() {
        for mcp in [McpDeclaration::Marion, McpDeclaration::None] {
            let live = LaunchSpec {
                auth: Auth::Inherited,
                base_url: None,
                api_key: None,
                mcp,
                ..gemini_spec()
            };
            let files = GeminiAdapter.config_files(&live, &ctx()).unwrap();
            let v: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
            assert!(
                v.pointer("/security/auth/selectedType").is_none(),
                "{mcp:?}: a live document must not pin an auth route: {v}"
            );
        }
        // Canned is untouched and still selects the type marion supplies a key for.
        let canned: serde_json::Value =
            serde_json::from_str(&GeminiAdapter.config_files(&gemini_spec(), &ctx()).unwrap()[0].1)
                .unwrap();
        assert_eq!(
            canned["security"]["auth"]["selectedType"],
            serde_json::json!("gemini-api-key")
        );
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
                "OPENCODE_DISABLE_SHARE",
                "OPENCODE_DB",
                // Placement, not isolation: opencode resolves its project directory from the
                // environment and re-enters `$PWD`, so a node given only `cwd` works in the
                // directory marion was launched from. See
                // `opencode::tests::the_node_is_placed_in_its_own_cwd_by_pwd_too_not_only_by_chdir`.
                "PWD",
                // The row's `UpdatePolicy`, after its `Env` rows.
                "OPENCODE_DISABLE_AUTOUPDATE",
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

    /// A live opencode spec: the operator's own provider, no endpoint and no credential from marion.
    fn opencode_live_spec() -> LaunchSpec {
        LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            model: Some("anthropic/claude-sonnet-4-5".into()),
            ..opencode_spec()
        }
    }

    /// What `--live` hands a codex node: marion names no endpoint and mints no credential.
    fn codex_live_spec() -> LaunchSpec {
        LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..codex_spec()
        }
    }

    /// **§6.4's central MUST, at the one place a live codex node could break it.**
    ///
    /// > *"marion never mutates the operator's real harness config."*
    ///
    /// Once `CODEX_HOME` is unset — which is exactly what makes the operator's login visible —
    /// `$CODEX_HOME/config.toml` *is* `~/.codex/config.toml`. The caller writes whatever
    /// `config_files` returns, unconditionally and with `create_dir_all` on the parent, so a
    /// document emitted here would land on the operator's own config and the first symptom would be
    /// a broken codex login on a harness marion was not even running. The declaration goes to argv
    /// instead; this asserts the file simply is not written.
    #[test]
    fn a_live_codex_node_writes_no_file_because_the_only_one_it_could_write_is_the_operators_own() {
        assert!(
            CodexAdapter
                .config_files(&codex_live_spec(), &ctx())
                .unwrap()
                .is_empty(),
            "the only config.toml a CODEX_HOME-less codex reads is ~/.codex/config.toml"
        );
    }

    /// **The key codex's generated provider names is the row's, from the launch's own credential**
    /// — one variable, compiled into the invocation, rather than a push each launch path had to
    /// remember after `compile`. The config's `env_key` and the row's variable are the same name.
    #[test]
    fn a_canned_codex_node_carries_its_credential_in_the_variable_its_config_names() {
        let spec = LaunchSpec {
            api_key: Some("per-run-token".into()),
            ..codex_spec()
        };
        let inv = CodexAdapter.compile(&spec, &ctx()).unwrap();
        assert!(
            inv.env
                .iter()
                .any(|(k, v)| k == "MARION_PROVIDER_KEY" && v == "per-run-token"),
            "{:?}",
            inv.env
        );
        let files = CodexAdapter.config_files(&spec, &ctx()).unwrap();
        assert!(
            files[0].1.contains("env_key = \"MARION_PROVIDER_KEY\""),
            "{}",
            files[0].1
        );
    }

    /// **Live mode is removal here too, and this is the list of what is removed** — asserted by
    /// name, because the failure defended against is one of the two creeping back. `CODEX_HOME`
    /// pointed anywhere but the operator's home hides the very `auth.json` the node exists to use,
    /// and `MARION_PROVIDER_KEY` is a minted placeholder standing beside a real credential.
    #[test]
    fn a_live_codex_node_carries_neither_codex_home_nor_the_minted_key() {
        let inv = CodexAdapter.compile(&codex_live_spec(), &ctx()).unwrap();
        for k in ["CODEX_HOME", "MARION_PROVIDER_KEY"] {
            assert!(
                !inv.env.iter().any(|(n, _)| n == k),
                "{k} must be absent, not blank: {:?}",
                inv.env
            );
            assert!(
                !inv.args.iter().any(|a| a.contains(k)),
                "{k} must not reach argv either: {:?}",
                inv.args
            );
        }
        // And no canned provider was declared: a live node uses codex's own default.
        assert!(!inv.args.iter().any(|a| a.contains("model_provider")));
    }

    /// The three things a live node's argv **must** carry, restated at the adapter seam so the
    /// wiring between `compile` and `codex::live_config_overrides` is covered and not just the
    /// builder in isolation.
    #[test]
    fn a_live_codex_nodes_argv_carries_the_bridge_its_approval_mode_and_the_plugin_kill() {
        let inv = CodexAdapter.compile(&codex_live_spec(), &ctx()).unwrap();
        let joined = inv.args.join(" ");
        for needle in [
            r#"-c mcp_servers.marion.command="/bin/marion-supervisor""#,
            r#"-c mcp_servers.marion.args=["mcp"]"#,
            r#"-c mcp_servers.marion.default_tools_approval_mode="approve""#,
            "-c features.plugins=false",
            // The env block, whose absence is silent in exactly the way §12 keeps recording: a
            // codex root whose bridge got none of these answers `MARION_REPO is not set`.
            r#"-c mcp_servers.marion.env.MARION_REPO="/repo""#,
            r#"-c mcp_servers.marion.env.MARION_STATE_DIR="/state""#,
            r#"-c mcp_servers.marion.env.MARION_AGENT_ID="019f-root""#,
            r#"-c mcp_servers.marion.env.MARION_AGENT_TYPE="claude""#,
            r#"-c mcp_servers.marion.env.MARION_DEPTH="0""#,
            r#"-c mcp_servers.marion.env.MARION_AUTH="inherited""#,
            r#"-c mcp_servers.marion.env.MARION_READY_FILE="/state/x/mcp-ready""#,
        ] {
            assert!(
                joined.contains(needle),
                "missing `{needle}` from:\n{joined}"
            );
        }
        // `--live` names no endpoint, so the key is omitted rather than written empty.
        assert!(!joined.contains("MARION_BASE_URL"), "{joined}");
    }

    /// **A live codex node's token is never on its argv**, where `ps` shows it to every user: it
    /// rides codex's environment, and the `-c` pairs name its variable in `env_vars` because codex
    /// hands a stdio server only an allowlist of its own environment. A canned node's token stays
    /// in its 0600 `config.toml`, and nothing is added to its environment.
    #[test]
    fn a_live_codex_nodes_token_rides_its_environment_and_env_vars_names_it() {
        let minted = SpawnCtx {
            node_token: Some("tok-SENTINEL-codex-4b1d".into()),
            ..ctx()
        };
        let token_env = |inv: &Invocation| -> Vec<String> {
            inv.env
                .iter()
                .filter(|(k, _)| k == mcp_bridge::NODE_TOKEN_ENV)
                .map(|(_, v)| v.clone())
                .collect()
        };

        let live = CodexAdapter.compile(&codex_live_spec(), &minted).unwrap();
        let joined = live.args.join(" ");
        assert!(
            !joined.contains("SENTINEL"),
            "the token is on argv: {joined}"
        );
        assert!(
            !joined.contains(&format!(".env.{}", mcp_bridge::NODE_TOKEN_ENV)),
            "the declaration names no token value: {joined}"
        );
        assert!(
            joined.contains(r#"-c mcp_servers.marion.env_vars=["MARION_NODE_TOKEN"]"#),
            "codex is told to pass the variable on: {joined}"
        );
        assert_eq!(token_env(&live), ["tok-SENTINEL-codex-4b1d"]);

        let canned = CodexAdapter.compile(&codex_spec(), &minted).unwrap();
        assert!(token_env(&canned).is_empty(), "{:?}", canned.env);
        let files = CodexAdapter.config_files(&codex_spec(), &minted).unwrap();
        assert!(
            files[0].1.contains("tok-SENTINEL-codex-4b1d"),
            "the canned document carries it"
        );
    }

    /// **A live qwen node's `--mcp-config` document is argv, so it carries no token**: qwen hands
    /// its MCP server its own environment, and the token rides that. A canned node's token stays
    /// in its 0600 `settings.json`.
    #[test]
    fn a_live_qwen_nodes_token_rides_its_environment_and_not_its_inline_document() {
        let minted = SpawnCtx {
            node_token: Some("tok-SENTINEL-qwen-0c55".into()),
            ..ctx()
        };
        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            model: None,
            ..qwen_spec()
        };
        let inv = QwenAdapter.compile(&live, &minted).unwrap();
        let joined = inv.args.join(" ");
        assert!(joined.contains("--mcp-config"), "{joined}");
        assert!(
            !joined.contains("SENTINEL"),
            "the token is on argv: {joined}"
        );
        assert!(!joined.contains(mcp_bridge::NODE_TOKEN_ENV), "{joined}");
        assert!(
            inv.env
                .iter()
                .any(|(k, v)| k == mcp_bridge::NODE_TOKEN_ENV && v == "tok-SENTINEL-qwen-0c55"),
            "{:?}",
            inv.env
        );

        let canned = QwenAdapter.compile(&qwen_spec(), &minted).unwrap();
        assert!(
            !canned
                .env
                .iter()
                .any(|(k, _)| k == mcp_bridge::NODE_TOKEN_ENV),
            "{:?}",
            canned.env
        );
        let files = QwenAdapter.config_files(&qwen_spec(), &minted).unwrap();
        assert!(
            files
                .iter()
                .any(|(_, doc)| doc.contains("tok-SENTINEL-qwen-0c55"))
        );
    }

    /// The native lane reads the same live carrier: `marion codex` puts the same `-c` pairs in
    /// front of the operator's own flags, and the token reaches the assembled environment — past
    /// the rule that strips every `MARION_` name the operator's own environment carried.
    #[test]
    fn a_native_codex_nodes_token_rides_its_environment_and_never_its_argv() {
        use std::ffi::OsString;

        use crate::native::{
            NativeEnvironmentView, NativeNodeContext, NativeProcessBase, NativeTerminalGeometry,
            assemble_native, native_adapter,
        };

        let minted = SpawnCtx {
            node_token: Some("tok-SENTINEL-native-77e2".into()),
            ..ctx()
        };
        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            ..codex_spec()
        };
        let bridge = bridge_env(&live, &minted);
        let operator_env = vec![(
            OsString::from(mcp_bridge::NODE_TOKEN_ENV),
            OsString::from("operator-forged"),
        )];
        let injection = native_adapter(Harness::Codex)
            .unwrap()
            .prepare_native(&NativeNodeContext {
                bridge: &bridge,
                document_dir: std::path::Path::new("/state/agents/019f-root"),
                allowed_marion_tools: &["spawn"],
                environment: NativeEnvironmentView::validate(&operator_env).unwrap(),
            })
            .unwrap();
        let prepared = assemble_native(
            NativeProcessBase {
                program: "codex".into(),
                user_argv: vec![],
                env: operator_env.clone(),
                cwd: "/repo".into(),
                geometry: NativeTerminalGeometry {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                },
            },
            injection,
        )
        .unwrap();
        let argv: Vec<String> = prepared
            .invocation
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !argv.iter().any(|a| a.contains("SENTINEL")),
            "the token is on argv: {argv:?}"
        );
        assert!(
            argv.iter()
                .any(|a| a == r#"mcp_servers.marion.env_vars=["MARION_NODE_TOKEN"]"#),
            "{argv:?}"
        );
        let token: Vec<&OsString> = prepared
            .invocation
            .env
            .iter()
            .filter(|(k, _)| k == mcp_bridge::NODE_TOKEN_ENV)
            .map(|(_, v)| v)
            .collect();
        assert_eq!(token, [&OsString::from("tok-SENTINEL-native-77e2")]);
        let printed = format!("{prepared:?}");
        assert!(!printed.contains("SENTINEL"), "{printed}");
    }

    /// **The contract's `sandbox:workspace-write` is a claim about the launch, so a live launch must
    /// carry it.** A canned node gets it from the generated `config.toml`; a live node reads the
    /// operator's `~/.codex/config.toml`, where an untrusted worktree resolves to `read-only`
    /// (measured 0.155.1, the live matrix's D9: `turn_context.sandbox_policy` read `read-only`
    /// while the contract said `workspace-write`). Asserted with and without marion's declaration,
    /// because the constraint is recorded on every node and the bridge is not what grants it.
    #[test]
    fn a_live_codex_node_runs_in_the_sandbox_its_contract_records() {
        for mcp in [McpDeclaration::Marion, McpDeclaration::None] {
            let spec = LaunchSpec {
                mcp,
                ..codex_live_spec()
            };
            let inv = CodexAdapter.compile(&spec, &ctx()).unwrap();
            let want = format!("sandbox_mode=\"{}\"", codex::SANDBOX_MODE);
            assert!(
                inv.args.windows(2).any(|w| w[0] == "-c" && w[1] == want),
                "{mcp:?}: missing `-c {want}` from {:?}",
                inv.args
            );
            assert_eq!(
                CodexAdapter.compiled_permissions(&spec).unwrap(),
                vec![format!("sandbox:{}", codex::SANDBOX_MODE)],
                "the recorded constraint and the compiled one are one value"
            );
        }
    }

    /// **A grant is every tool the harness may expose for it, not the one a canned run happened to
    /// see.** copilot's file-editing tool depends on the model: the canned BYOK run (1.0.83) was
    /// offered `create`, and the live `auto` model (1.0.87, gpt-5.6-luna) is offered only
    /// `apply_patch` out of `view,create,edit,apply_patch,str_replace,str_replace_editor` — so a
    /// `write` that compiled to `create` alone left the live matrix's copilot-impl children with no
    /// edit tool (D5). `--available-tools` names what may be offered and copilot drops the names a
    /// model does not have; `--allow-tool=write` is the one kind granting all of them.
    #[test]
    fn a_copilot_write_grant_offers_every_file_editing_tool_copilot_may_expose() {
        let spec = LaunchSpec {
            tools: vec![agent_type::TOOL_READ.into(), agent_type::TOOL_WRITE.into()],
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            model: Some("auto".into()),
            ..copilot_spec()
        };
        let inv = CopilotAdapter.compile(&spec, &ctx()).unwrap();
        let offered = inv
            .args
            .iter()
            .find_map(|a| a.strip_prefix("--available-tools="))
            .unwrap_or_else(|| panic!("no --available-tools in {:?}", inv.args));
        let offered: Vec<&str> = offered.split(',').collect();
        for tool in ["view", "create", "edit", "apply_patch"] {
            assert!(offered.contains(&tool), "`{tool}` missing from {offered:?}");
        }
        assert_eq!(
            inv.args
                .iter()
                .filter(|a| *a == "--allow-tool=write")
                .count(),
            1,
            "one kind grants every file tool: {:?}",
            inv.args
        );
    }

    /// A live node reaches a real vendor, so the model is the operator's to choose and `-m` carries
    /// it — verified present on the installed 0.146.0's `codex exec --help`.
    #[test]
    fn a_live_codex_node_takes_the_model_it_was_asked_for() {
        let inv = CodexAdapter
            .compile(
                &LaunchSpec {
                    model: Some("gpt-5-codex".into()),
                    ..codex_live_spec()
                },
                &ctx(),
            )
            .unwrap();
        assert_eq!(
            inv.args.windows(2).find(|w| w[0] == "-m").map(|w| &w[1]),
            Some(&"gpt-5-codex".to_string())
        );
        assert_eq!(inv.model.as_deref(), Some("gpt-5-codex"));
    }

    /// **Canned is untouched by all of the above**, including the newly-discovered `-m`: a canned
    /// launch compiles no override and no model however loudly one was asked for, so every contract
    /// this harness has ever written still records `None` and the argv is the measured one.
    #[test]
    fn a_canned_codex_launch_gained_neither_a_c_flag_nor_a_model() {
        let inv = CodexAdapter
            .compile(
                &LaunchSpec {
                    model: Some("gpt-5-codex".into()),
                    ..codex_spec()
                },
                &ctx(),
            )
            .unwrap();
        // The one `-c` a canned launch carries is the row's update policy, which is not an
        // override of anything canned configures; every other pair would be a live override.
        let pairs: Vec<&String> = inv
            .args
            .windows(2)
            .filter(|w| w[0] == "-c")
            .map(|w| &w[1])
            .collect();
        assert_eq!(
            pairs,
            vec!["check_for_update_on_startup=false"],
            "{:?}",
            inv.args
        );
        assert!(!inv.args.iter().any(|a| a == "-m"), "{:?}", inv.args);
        assert_eq!(inv.model, None);
        assert_eq!(
            inv.env,
            vec![("CODEX_HOME".to_string(), "/state/x/config".to_string())]
        );
    }

    /// **Live mode stops relocating five variables and keeps eight**, and the split is not
    /// arbitrary: the five hid the operator's login (auth.json under `$XDG_DATA_HOME`, `~/.claude`
    /// and `~/.opencode` under `HOME`), while the eight were always hygiene.
    #[test]
    fn a_live_opencode_node_drops_home_and_the_xdg_roots_and_keeps_the_hygiene() {
        let inv = OpenCodeAdapter
            .compile(&opencode_live_spec(), &ctx())
            .unwrap();
        for k in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
        ] {
            assert!(
                !inv.env.iter().any(|(n, _)| n == k),
                "{k} must be absent under --live. env: {:?}",
                inv.env
            );
        }
        // The two that matter MORE live, not less: under `--live` the `HOME` an unsevered child
        // would read `~/.claude/CLAUDE.md` and `~/.claude/skills/**` from is the operator's real one.
        for k in [
            "OPENCODE_DISABLE_CLAUDE_CODE",
            "OPENCODE_DISABLE_EXTERNAL_SKILLS",
            "OPENCODE_DB",
        ] {
            assert!(
                inv.env.iter().any(|(n, _)| n == k),
                "{k} survives live mode"
            );
        }
    }

    /// **A live opencode node loads the operator's plugins and project config**, because both can
    /// be where their credential comes from. 1.18.32 gates the user's plugin list on `--pure`
    /// (`Q.pure ? [] : plugin_origins`, internal plugins untouched), and an auth plugin supplies
    /// the provider's `auth` hook; a project `opencode.json` / `.opencode/` can carry the provider
    /// block and its key reference. Canned keeps both switches: there marion owns the provider.
    #[test]
    fn a_live_opencode_node_loads_the_operators_plugins_and_project_config() {
        let live = OpenCodeAdapter
            .compile(&opencode_live_spec(), &ctx())
            .unwrap();
        assert!(
            !live.args.iter().any(|a| a == "--pure"),
            "a live node must not skip the operator's plugins: {:?}",
            live.args
        );
        assert!(
            !live
                .env
                .iter()
                .any(|(k, _)| k == "OPENCODE_DISABLE_PROJECT_CONFIG"),
            "a live node must not skip the project config: {:?}",
            live.env
        );
        let canned = OpenCodeAdapter.compile(&opencode_spec(), &ctx()).unwrap();
        assert!(canned.args.iter().any(|a| a == "--pure"));
        assert!(
            canned
                .env
                .iter()
                .any(|(k, v)| k == "OPENCODE_DISABLE_PROJECT_CONFIG" && v == "1")
        );
    }

    /// The live route, end to end at the seam: **no file at all**, the declaration in
    /// `OPENCODE_CONFIG_CONTENT`, and the adapter saying so rather than the supervisor guessing.
    #[test]
    fn a_live_opencode_node_declares_its_bridge_in_the_environment_and_writes_no_file() {
        let live = opencode_live_spec();
        assert_eq!(
            OpenCodeAdapter.mcp_route(&live),
            McpRoute::Environment("OPENCODE_CONFIG_CONTENT"),
            "an empty config_files must never be *inferred* to mean 'fileless'"
        );
        assert!(
            OpenCodeAdapter
                .config_files(&live, &ctx())
                .unwrap()
                .is_empty(),
            "a file under an isolated $XDG_CONFIG_HOME would isolate away the login"
        );
        let inv = OpenCodeAdapter.compile(&live, &ctx()).unwrap();
        let (_, content) = inv
            .env
            .iter()
            .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
            .expect("the live node's only bridge route");
        let v: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(
            v["mcp"]["marion"]["command"],
            serde_json::json!(["/bin/marion-supervisor", "mcp"])
        );
        assert_eq!(
            v["mcp"]["marion"]["environment"]["MARION_AUTH"],
            serde_json::json!("inherited")
        );
        assert!(
            v["provider"].is_null() && v["small_model"].is_null(),
            "the inline text MERGES over the operator's config, so either key would shadow their \
             real provider: {content}"
        );
        // Canned is untouched: a document, in the isolated config root, exactly as before.
        assert_eq!(
            OpenCodeAdapter.mcp_route(&opencode_spec()),
            McpRoute::Document
        );
        assert!(
            !OpenCodeAdapter
                .compile(&opencode_spec(), &ctx())
                .unwrap()
                .env
                .iter()
                .any(|(k, _)| k == "OPENCODE_CONFIG_CONTENT"),
            "the canned route writes a file and carries no inline config"
        );
    }

    /// **The canned-default refusals speak to a person**: no spec sections, and no `--live` flag,
    /// which no longer exists as a choice (real auth is the default). opencode is not among them:
    /// its plumbing default on a live node now runs the operator's own default model instead.
    #[test]
    fn the_live_canned_default_refusals_name_no_spec_section_and_no_live_flag() {
        let cases: Vec<(Box<dyn HarnessAdapter>, LaunchSpec)> = vec![
            (
                Box::new(CopilotAdapter),
                LaunchSpec {
                    auth: Auth::Inherited,
                    base_url: None,
                    api_key: None,
                    model: Some(agent_type::COPILOT_DEFAULT_MODEL.into()),
                    ..copilot_spec()
                },
            ),
            (
                Box::new(GooseAdapter),
                LaunchSpec {
                    auth: Auth::Inherited,
                    base_url: None,
                    api_key: None,
                    model: Some(agent_type::GOOSE_DEFAULT_MODEL.into()),
                    ..goose_spec()
                },
            ),
            (
                Box::new(ClineAdapter),
                LaunchSpec {
                    auth: Auth::Inherited,
                    base_url: None,
                    api_key: None,
                    model: Some(agent_type::CLINE_DEFAULT_MODEL.into()),
                    ..cline_spec()
                },
            ),
            (
                Box::new(QwenAdapter),
                LaunchSpec {
                    auth: Auth::Inherited,
                    base_url: None,
                    api_key: None,
                    model: Some(agent_type::QWEN_DEFAULT_MODEL.into()),
                    ..qwen_spec()
                },
            ),
        ];
        for (adapter, spec) in cases {
            let e = adapter.compile(&spec, &ctx()).unwrap_err().to_string();
            assert!(!e.contains("--live"), "{}: {e}", adapter.harness());
            assert!(!e.contains('§'), "{}: {e}", adapter.harness());
            assert!(
                e.contains("-m"),
                "{}: it must say what to pass: {e}",
                adapter.harness()
            );
        }
    }

    /// **A live node that names no model of its own runs on the operator's configured default**,
    /// as a live claude or codex node does — `-m` is left off argv. `marion/default`, the type's
    /// default, names marion's own generated plumbing, which a live node does not generate (S13
    /// measured `-m marion/default` there as `Error: {"name":"UnknownError",…}`, exit 1), so it
    /// counts as naming none. s36 measured `opencode run` with no `-m` taking the config's `model`
    /// (`model-omitted/`). Canned is untouched: that default is what its provider block is for,
    /// and a canned launch still names one.
    #[test]
    fn a_live_node_that_names_no_model_of_its_own_runs_on_the_operators_default() {
        for model in [
            None,
            Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL.into()),
        ] {
            let live = LaunchSpec {
                model: model.clone(),
                ..opencode_live_spec()
            };
            let inv = OpenCodeAdapter
                .compile(&live, &ctx())
                .unwrap_or_else(|e| panic!("{model:?}: {e}"));
            assert!(
                !inv.args.iter().any(|a| a == "-m"),
                "{model:?}: no -m, so the operator's own default model runs: {:?}",
                inv.args
            );
            assert_eq!(
                inv.model, None,
                "{model:?}: the record names no model marion chose"
            );
        }
        // A real provider/model is still passed, and still checked for its form.
        let named = OpenCodeAdapter
            .compile(&opencode_live_spec(), &ctx())
            .unwrap();
        assert!(named.args.windows(2).any(|w| w[0] == "-m"));
        assert!(
            OpenCodeAdapter
                .compile(
                    &LaunchSpec {
                        model: Some("no-slash".into()),
                        ..opencode_live_spec()
                    },
                    &ctx()
                )
                .is_err()
        );
        // Canned: the default reaches argv and its provider block; no model at all is refused.
        let canned = OpenCodeAdapter
            .compile(
                &LaunchSpec {
                    model: Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL.into()),
                    ..opencode_spec()
                },
                &ctx(),
            )
            .unwrap();
        assert!(canned.args.iter().any(|a| a == "marion/default"));
        assert!(
            OpenCodeAdapter
                .compile(
                    &LaunchSpec {
                        model: None,
                        ..opencode_spec()
                    },
                    &ctx()
                )
                .is_err()
        );
    }

    /// The same for `opencode acp`: the type's plumbing default is not set on a live session, so
    /// the session keeps the operator's own default rather than being refused as offering no
    /// `marion/default`.
    #[test]
    fn a_live_opencode_acp_node_leaves_the_plumbing_default_off_its_session() {
        let live = LaunchSpec {
            model: Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL.into()),
            ..acp_spec()
        };
        assert_eq!(acp_adapter().compile(&live, &ctx()).unwrap().model, None);
        let canned = LaunchSpec {
            model: Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL.into()),
            ..canned_acp_spec()
        };
        assert_eq!(
            acp_adapter()
                .compile(&canned, &ctx())
                .unwrap()
                .model
                .as_deref(),
            Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL)
        );
    }

    /// **The route is stated, never inferred — and every adapter states one.** This is what keeps
    /// `RootError::NoMcpDeclaration` from becoming a hole: "declares by another route" and "declared
    /// nothing at all" are different answers here, and only the first is legal.
    #[test]
    fn every_adapter_states_the_route_its_mcp_declaration_travels_on() {
        for h in Harness::ALL {
            let a = launch_adapter(h).unwrap();
            let spec = spec_for(h);
            match a.mcp_route(&spec) {
                McpRoute::Session(k) => assert!(
                    McpRoute::Session(k)
                        .verify(
                            &[],
                            &a.compile(&spec, &ctx()).unwrap(),
                            a.session_declaration(&spec, &ctx()).unwrap().as_ref()
                        )
                        .is_ok(),
                    "{h}: claims a session/new declaration and compiled none"
                ),
                McpRoute::Document => assert!(
                    !a.config_files(&spec, &ctx()).unwrap().is_empty(),
                    "{h}: claims a document and emitted none"
                ),
                McpRoute::Environment(k) => assert!(
                    a.compile(&spec, &ctx())
                        .unwrap()
                        .env
                        .iter()
                        .any(|(n, _)| n == k),
                    "{h}: claims ${k} and did not set it"
                ),
                McpRoute::Argv(key) => assert!(
                    a.compile(&spec, &ctx())
                        .unwrap()
                        .args
                        .iter()
                        .any(|arg| arg.contains(key)),
                    "{h}: claims argv carries {key} and it does not appear there"
                ),
                McpRoute::None => {
                    panic!("{h}: asked for McpDeclaration::Marion and routed nowhere")
                }
            }
            // And a node that asked for no bridge says so, rather than looking like a failure.
            assert_eq!(
                a.mcp_route(&LaunchSpec {
                    mcp: McpDeclaration::None,
                    ..spec
                }),
                McpRoute::None,
                "{h}: §9's fallback branch is not the same fact as a missing declaration"
            );
        }
    }

    // ------------------------------------------------------------------------------------------
    // The pane shape (§9's M3): asked for per run, never the default
    // ------------------------------------------------------------------------------------------

    /// **A node that did not ask for a pane must not get one**, and this is the assertion that
    /// says so at the layer the decision is made.
    ///
    /// `surfaces()` is the shape every spawn compiles unless a client asked otherwise, so a
    /// display plane appearing here would move *every* node of that harness onto a pty — which on
    /// Claude Code is M1's measured `stream-json` path, with `stdout` and `stderr` collapsed onto
    /// one file description and a diagnostic able to land inside a frame. The pane shape lives on
    /// [`HarnessAdapter::pane_surfaces`] precisely so that this stays true.
    ///
    /// Mutation: make `ClaudeCodeAdapter::surfaces` return `pane_surfaces().unwrap()`, or
    /// `ExecutionSurfaces::shared(TypedKind::StreamJson)`. This fails.
    #[test]
    fn the_default_shape_of_every_harness_declares_no_display_plane() {
        for h in Harness::ALL {
            let a = launch_adapter(h).unwrap();
            assert!(
                a.surfaces().display_plane().is_none(),
                "{h}: a run that asked for no pane would now be launched under a pty"
            );
            assert_ne!(
                a.surfaces().control,
                ControlTransport::TerminalInput,
                "{h}: the default shape is not a shape marion types keystrokes into"
            );
        }
    }

    /// The positive half, so the test above cannot pass by there being no pane shape at all.
    #[test]
    fn the_pane_shape_is_a_pty_marion_types_into_and_parses_nothing_from() {
        let panes: Vec<Harness> = Harness::ALL
            .into_iter()
            .filter(|h| launch_adapter(*h).unwrap().pane_surfaces().is_some())
            .collect();
        assert!(
            !panes.is_empty(),
            "no harness has a pane shape, so `marion attach` has nothing to reach"
        );
        for h in panes {
            let s = launch_adapter(h).unwrap().pane_surfaces().unwrap();
            assert!(s.display_plane().is_some(), "{h}: a pane needs a pty");
            assert_eq!(
                s.control,
                ControlTransport::TerminalInput,
                "{h}: a pane is driven by keystrokes"
            );
            // **The load-bearing half.** A pane surface that also claimed `ProtocolEvents` would
            // be asserting that marion parses frames off the pty — two readers of one stream, and
            // stderr interleaved into them. `TerminalBytes` alone is what makes the hazard absent
            // rather than mitigated.
            assert_eq!(
                s.observations,
                std::collections::BTreeSet::from([crate::ObservationSource::TerminalBytes]),
                "{h}: nothing parses a frame off a pane, and the surfaces must say so"
            );
        }
    }

    /// **Which harnesses declare a pane is a measured list, not a count.**
    ///
    /// The test above only asks that *some* harness has a pane shape, so it stays green with codex
    /// back at the trait default — and §9's M3 criterion C2 is specifically *"a real **codex** TUI
    /// runs in a pane"*. Claude Code cannot stand in for it: it enters its own alternate screen
    /// seconds after boot, and an alternate screen has no scrollback to retain, so a `CSI 3J`
    /// assertion made against claude would pass because there was nothing to lose.
    ///
    /// gemini and opencode are `None` for a reason of the same kind, in the other direction:
    /// neither has a TUI shape marion has measured, and a `Some` here would put a run that asked
    /// for a pane onto a pty with an unmeasured harness on it rather than refusing by name.
    ///
    /// Mutation: revert `CodexAdapter::pane_surfaces` to the trait default. This fails, and so does
    /// `marion-supervisor`'s `a_real_codex_tui_keeps_its_scrollback_across_a_resize_in_a_marion_pane`.
    #[test]
    fn codex_declares_a_pane_because_c2_names_that_harness_and_no_other_can_stand_in() {
        let with_panes: Vec<Harness> = Harness::ALL
            .into_iter()
            .filter(|h| launch_adapter(*h).unwrap().pane_surfaces().is_some())
            .collect();
        assert_eq!(
            with_panes,
            vec![Harness::ClaudeCode, Harness::Codex],
            "the set of harnesses marion can pane moved. Adding one is a measurement (§5.3); \
             losing codex is losing C2, since it is the only harness of the four that keeps its \
             session on the main screen and therefore the only one with scrollback to retain"
        );
    }

    /// The codex pane is the **interactive** command, isolated exactly as the `exec` shape is.
    ///
    /// Mutation: point `CodexAdapter::compile_pane` at `codex::compile_exec`. This fails on `exec`.
    #[test]
    fn the_codex_pane_argv_is_the_tui_and_carries_the_exec_shapes_isolation() {
        let spec = LaunchSpec {
            prompt: "look at this".into(),
            ..spec_for(Harness::Codex)
        };
        let inv = CodexAdapter
            .compile_pane(&spec, &ctx())
            .expect("codex has a pane shape");
        assert_eq!(inv.program, "codex");
        for forbidden in ["exec", "--json", "--skip-git-repo-check"] {
            assert!(
                !inv.args.iter().any(|a| a == forbidden),
                "the codex pane carries {forbidden}, which is the exec shape's: {:?}",
                inv.args
            );
        }
        assert_eq!(inv.args.last().map(String::as_str), Some("look at this"));
        assert!(
            inv.env
                .iter()
                .any(|(k, v)| k == "CODEX_HOME" && std::path::Path::new(v) == spec.config_dir),
            "a paned codex without CODEX_HOME reads and writes the operator's own ~/.codex: {:?}",
            inv.env
        );
    }

    /// A harness with no pane shape refuses by name rather than compiling the headless one.
    ///
    /// Mutation: default `compile_pane` to `self.compile(spec, ctx)`. This fails.
    #[test]
    fn a_harness_with_no_pane_shape_refuses_rather_than_launching_the_headless_one() {
        for h in Harness::ALL {
            let a = launch_adapter(h).unwrap();
            if a.pane_surfaces().is_some() {
                continue;
            }
            let err = a
                .compile_pane(&spec_for(h), &ctx())
                .expect_err("compiled a pane for a harness that declares none");
            assert!(
                matches!(err, HarnessError::NoPaneSurface(got) if got == h),
                "{h}: {err}"
            );
        }
    }

    /// The pane argv is a TUI's, and the isolation is the headless shape's.
    ///
    /// Mutation: leave `-p` in `compile_pane`, or drop `--setting-sources ""`. Either fails.
    #[test]
    fn the_pane_argv_is_a_tui_with_the_headless_shape_isolation() {
        let inv = ClaudeCodeAdapter
            .compile_pane(&claude_spec(), &ctx())
            .expect("claude has a pane shape");
        for forbidden in [
            "-p",
            "--print",
            "--output-format",
            "--input-format",
            "--verbose",
            // A pane has an operator in it: §9's M3 asks for the *harness's* permission prompt,
            // and `stdio` would send `can_use_tool` to a marion with no answerer instead.
            "--permission-prompt-tool",
        ] {
            assert!(
                !inv.args.iter().any(|a| a == forbidden),
                "the pane argv carries {forbidden}, which is the headless shape's: {:?}",
                inv.args
            );
        }
        for required in ["--strict-mcp-config", "--mcp-config", "--setting-sources"] {
            assert!(
                inv.args.iter().any(|a| a == required),
                "the pane argv dropped {required}, so a pane inherits what §9 measured out: {:?}",
                inv.args
            );
        }
        // Both §3.1 axes, so a pane is not a widening.
        assert!(inv.args.iter().any(|a| a == "--tools"));
        assert!(inv.args.iter().any(|a| a == "--allowedTools"));
    }

    /// **A pane compiles the grant its `LaunchSpec` carries — on *both* axes — and still names no
    /// `--permission-prompt-tool`.**
    ///
    /// The test above uses a spec whose `tools` is empty, so it can only see that the two flags are
    /// *present*. That left a false premise standing in three places at once: that
    /// `compile_pane` hardcodes `--tools ""` and a pane therefore has nothing to ask permission
    /// for. It does not. It passes `spec.tools` through, and the empty axis those readings saw
    /// belongs to the **`claude` agent type**, which declares no tool. `claude-impl` declares
    /// `[read, write]`, `marion run <type> --pane` resolves any built-in by name, and
    /// `root::availability_axis` runs *before* the pane branch — so `marion run claude-impl --pane`
    /// over a recorded repository compiles a real grant into a TUI.
    ///
    /// **The second half is what corrects the C1 story rather than confirming it.**
    /// [`Self::permission_axis`] is one derivation shared with [`Self::compile`], so the same names
    /// land in `--allowedTools` — *"Comma or space-separated list of tool names to allow"*, i.e.
    /// pre-approval. §3.1's two axes come from one declaration and cannot be declared apart, so
    /// **no `marion run` can reach the state a permission dialog needs**: a tool that is available
    /// and not already allowed. That is asserted here rather than left as prose, because it is the
    /// precondition M3 C1's *"permission prompt correct"* clause is really waiting on.
    ///
    /// Mutations, each of which this catches and the test above does not: hardcode `--tools ""` in
    /// [`claude_code::compile_pane`]; drop the native names from `permission_axis`; add
    /// `--permission-prompt-tool stdio` to the pane (which would convert an operator's answerable
    /// question into §11 item 22's queue entry awaiting a UI that does not exist).
    #[test]
    fn a_paned_node_compiles_its_grant_on_both_axes_and_names_no_permission_prompt_tool() {
        let spec = LaunchSpec {
            tools: vec![agent_type::TOOL_READ.into(), agent_type::TOOL_WRITE.into()],
            ..claude_spec()
        };
        let value = |inv: &Invocation, flag: &str| -> String {
            let i = inv
                .args
                .iter()
                .position(|a| a == flag)
                .unwrap_or_else(|| panic!("{flag} is always compiled: {:?}", inv.args));
            inv.args[i + 1].clone()
        };

        let pane = ClaudeCodeAdapter
            .compile_pane(&spec, &ctx())
            .expect("claude has a pane shape");
        assert_eq!(
            value(&pane, "--tools"),
            "Read,Write",
            "a pane compiles the declared grant in claude's own spelling, not \"\": {:?}",
            pane.args
        );
        assert_eq!(
            value(&pane, "--allowedTools"),
            "mcp__marion__spawn,mcp__marion__status,Read,Write",
            "permission carries the same names beside marion's verbs, so every tool a paned node \
             has is one it is already allowed to use — there is nothing left to ask about: {:?}",
            pane.args
        );
        assert!(
            !pane.args.iter().any(|a| a == "--permission-prompt-tool"),
            "the ask a pane cannot make must still not be routed to a marion with no answerer: \
             {:?}",
            pane.args
        );

        // And a pane is not a widening: the two shapes compile the same two axes from the same
        // declaration. A branch that opened either one further would show up here as a difference.
        let headless = ClaudeCodeAdapter
            .compile(&spec, &ctx())
            .expect("the headless shape compiles the same spec");
        assert_eq!(value(&pane, "--tools"), value(&headless, "--tools"));
        assert_eq!(
            value(&pane, "--allowedTools"),
            value(&headless, "--allowedTools")
        );
    }

    /// The prompt rides argv on the pane shape and is refused on the headless one — the same
    /// `LaunchSpec`, two answers, which is the whole reason these are two compiles.
    #[test]
    fn a_prompt_is_a_positional_on_the_pane_shape_and_a_refusal_on_the_headless_one() {
        let spec = LaunchSpec {
            prompt: "fix the test".into(),
            ..claude_spec()
        };
        assert!(
            ClaudeCodeAdapter.compile(&spec, &ctx()).is_err(),
            "an argv prompt on --print takes turn one with tools: []"
        );
        let inv = ClaudeCodeAdapter.compile_pane(&spec, &ctx()).unwrap();
        assert_eq!(
            inv.args.last().map(String::as_str),
            Some("fix the test"),
            "a TUI takes no turn until the operator presses return: {:?}",
            inv.args
        );
        // And an empty prompt is a TUI opened at its prompt, not an empty positional.
        let bare = ClaudeCodeAdapter
            .compile_pane(&claude_spec(), &ctx())
            .unwrap();
        assert!(
            !bare.args.iter().any(String::is_empty)
                || bare.args.iter().filter(|a| a.is_empty()).count() == 2,
            "only --tools and --setting-sources carry an empty string: {:?}",
            bare.args
        );
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
            Harness::Copilot,
        ]
        .into_iter()
        .map(|h| launch_adapter(h).unwrap().marion_tool_name("report"))
        .collect();
        assert_eq!(
            names,
            vec![
                "mcp__marion__report",
                "mcp__marion__report",
                "mcp_marion_report",
                "marion_report",
                // A hyphen: the fifth spelling (s24, in `tools[]` and `toolName` alike).
                "marion-report",
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

    /// **Every built-in agent type resolves to a launchable adapter, ACP included.**
    ///
    /// This is the seam `adapter_for` cannot answer and the one a binary actually crosses:
    /// `run_spawn` and `root::prepare` have an [`agent_type::AgentType`], not a [`Harness`], and on
    /// the ACP row the type carries the second half of the selection. A built-in naming
    /// `Harness::Acp` whose `acp_agent` were absent, misspelt, or pointed at an agent nobody has
    /// watched call a tool would compile here and refuse at launch — and the symptom would be a
    /// harness that simply never worked from any binary, which is the condition this row exists to
    /// end.
    ///
    /// So each built-in is bound the way a launch binds it, and the ACP ones are additionally
    /// required to name an agent that opened a session (S33), bound to its measured spelling where
    /// one was watched and to the generic reading otherwise. `gemini --acp` refused `session/new`,
    /// so a built-in pointed there would be a type that cannot run.
    #[test]
    fn every_builtin_agent_type_binds_an_adapter_a_launch_could_use() {
        let mut acp = 0;
        for name in agent_type::builtin_names() {
            let t = agent_type::builtin(name).unwrap_or_else(|| panic!("{name} must resolve"));
            let a = adapter_for_type(t.harness, t.acp_agent.as_deref())
                .unwrap_or_else(|e| panic!("`{name}` names no adapter a launch could use: {e}"));
            assert_eq!(a.harness(), t.harness, "{name}");
            if t.harness != Harness::Acp {
                continue;
            }
            acp += 1;
            let id = t.acp_agent.as_deref().expect("checked in marion-core");
            let agent = acp::agent(id).unwrap_or_else(|| {
                panic!(
                    "`{name}` names ACP agent `{id}`, which is not in the \
                                           registry — the type and the registry have drifted"
                )
            });
            // Every ACP built-in's agent opened a session when probed (S33); a guessed spelling is
            // never compiled for it.
            assert!(
                matches!(agent.reach, acp::Reach::Opened { .. }),
                "`{name}` names `{id}`, which never opened a session: a type for it could not run"
            );
            // The adapter it bound spells *that* agent's measured name, not a neighbour's — or,
            // where no call was watched, reads it generically and spells the baseline, which on
            // ACP reaches no model (the prompt rides `session/prompt`; `Reading::Generic`).
            assert_eq!(
                a.marion_tool_name("report"),
                agent.tools.unwrap_or(acp::GENERIC_SPELLING).spell("report"),
                "`{name}`"
            );
        }
        assert!(
            acp > 0,
            "no built-in reaches the ACP row, so this asserts nothing"
        );
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

    /// A copilot `-p --output-format json` run, verbatim from `tests/fixtures/s24/` — the write
    /// tool granted and used, then marion's report answered, then the model's closing text.
    const COPILOT_STREAM: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s24/copilot-write-then-report.stdout.jsonl"
    ));

    /// A goose `run --output-format stream-json -q` run, verbatim from `tests/fixtures/s26/` —
    /// marion's report requested and answered, then the model's closing text.
    const GOOSE_STREAM: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s26/goose-report.stdout.jsonl"
    ));

    /// A cline `--json` run, verbatim from `tests/fixtures/s27/` — marion's report called and
    /// answered, then the model's closing text.
    const CLINE_STREAM: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s27/cline-report-ok.stdout.jsonl"
    ));

    #[test]
    fn a_copilot_report_is_read_from_the_execution_start_frame_in_copilots_own_spelling() {
        let out = CopilotAdapter.parse_stream(COPILOT_STREAM, ChildExit::default());
        assert_eq!(
            out.narrative.as_deref(),
            Some("Wrote the matrix marker under src/ and reported back.")
        );
        assert_eq!(out.failure, None);
        assert!(out.file_change_paths.is_empty(), "git is the authority");
        // The spelling is load-bearing: no other harness's name for the tool appears.
        for other in ["mcp__marion__report", "mcp_marion_report", "marion_report"] {
            assert!(!COPILOT_STREAM.contains(other), "{other}");
        }
    }

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

    /// **`result_commits` reaches marion on all four wires — one derivation, four spellings.**
    ///
    /// §6.7 calls this the one field the child owns outright, and it was dropped in transit:
    /// `build_contract` hardcoded an empty list, so a child that committed its work and reported
    /// the oids got a contract asserting it had committed nothing. Each harness wraps the same
    /// `report` arguments under a different key (`input`, `arguments`, `parameters`,
    /// `part.state.input`), so a single wire quietly failing to read the field would be invisible
    /// behind the three that still did — which is why this drives all four rather than one.
    #[test]
    fn every_harness_carries_the_commits_its_child_reported() {
        const A: &str = "1111111111111111111111111111111111111111";
        const B: &str = "2222222222222222222222222222222222222222";
        for h in Harness::ALL {
            let adapter = launch_adapter(h).unwrap();
            let tool = adapter.marion_tool_name("report");
            let args = format!(r#"{{"narrative":"did the work","result_commits":["{A}","{B}"]}}"#);
            // Codex dispatches on the bare verb beside `server: "marion"`, not on the flat
            // code-mode identifier — the same wire fact `spawn_tool` records in the matrix.
            let stream = match h {
                Harness::ClaudeCode => format!(
                    r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"{tool}","input":{args}}}]}}}}"#
                ),
                Harness::Codex => format!(
                    r#"{{"type":"item.completed","item":{{"type":"mcp_tool_call","server":"marion","tool":"report","arguments":{args}}}}}"#
                ),
                Harness::Gemini => {
                    format!(r#"{{"type":"tool_use","tool_name":"{tool}","parameters":{args}}}"#)
                }
                Harness::OpenCode => format!(
                    r#"{{"type":"tool_use","part":{{"type":"tool","tool":"{tool}","state":{{"status":"completed","input":{args}}}}}}}"#
                ),
                // s24's `tool.execution_start`: the arguments arrive parsed under `data`.
                Harness::Copilot => format!(
                    r#"{{"type":"tool.execution_start","data":{{"toolCallId":"c1","toolName":"{tool}","arguments":{args}}}}}"#
                ),
                // S26's `toolRequest` item: the arguments arrive parsed under `toolCall.value`.
                Harness::Goose => format!(
                    r#"{{"type":"message","message":{{"role":"assistant","content":[{{"type":"toolRequest","id":"c1","toolCall":{{"status":"success","value":{{"name":"{tool}","arguments":{args}}}}}}}]}}}}"#
                ),
                // S27's `content_start`: the arguments arrive parsed under `event.input`.
                Harness::Cline => format!(
                    r#"{{"type":"agent_event","event":{{"type":"content_start","contentType":"tool","toolName":"{tool}","toolCallId":"c1","input":{args}}}}}"#
                ),
                // s32: a `call_mcp_tool` step, the arguments under `parameters.Arguments`.
                Harness::Antigravity => format!(
                    r#"{{"event":"step_update","step_update":{{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"call_mcp_tool","tool_info":{{"parameters":{{"ServerName":"marion","ToolName":"report","Arguments":{args}}},"output":"ok"}}}}}}"#
                ),
                // S25: Claude Code's `assistant` `tool_use`, frame for frame.
                Harness::Qwen => format!(
                    r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"c1","name":"{tool}","input":{args}}}]}}}}"#
                ),
                // S34's `tool_execution_start`: the arguments arrive parsed under `args`.
                Harness::Pi => format!(
                    r#"{{"type":"tool_execution_start","toolCallId":"c1","toolName":"{tool}","args":{args}}}"#
                ),
                // Two frames, because ACP is the one wire where the verb and the arguments never
                // arrive together: S21's opening `tool_call` carries the title and an empty
                // `rawInput`, and the closing update carries the arguments and an empty title.
                Harness::Acp => format!(
                    "{}\n{}",
                    format_args!(
                        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"tool_call","toolCallId":"c1","title":"{tool}","status":"pending","rawInput":{{}}}}}}}}"#
                    ),
                    format_args!(
                        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"tool_call_update","toolCallId":"c1","title":"","status":"completed","rawInput":{args}}}}}}}"#
                    ),
                ),
            };
            let out = adapter.parse_stream(&stream, ChildExit::default());
            assert_eq!(
                out.narrative.as_deref(),
                Some("did the work"),
                "{h}: the premise — this frame must be read as a report at all"
            );
            assert_eq!(
                out.result_commits,
                vec![A.to_string(), B.to_string()],
                "{h}: the child named two commits and marion must carry both, in order"
            );
        }
    }

    /// **An absent list and a null one are the same claim: the child named no commits.**
    ///
    /// Not a tolerance for sloppiness — codex's `--output-schema` cannot express an optional key
    /// under `strict: true`, so §9 has marion spell optionality as *nullability* and a conforming
    /// codex child sends `"result_commits": null` verbatim. A reader that treated null as anything
    /// other than "none" would turn the schema marion itself writes into a parse failure.
    #[test]
    fn a_report_with_no_commits_or_a_null_list_yields_none_rather_than_failing() {
        for args in [
            r#"{"narrative":"did the work"}"#,
            r#"{"narrative":"did the work","result_commits":null}"#,
            r#"{"narrative":"did the work","result_commits":[]}"#,
        ] {
            let stream = format!(
                r#"{{"type":"item.completed","item":{{"type":"mcp_tool_call","server":"marion","tool":"report","arguments":{args}}}}}"#
            );
            let out = CodexAdapter.parse_stream(&stream, ChildExit::default());
            assert_eq!(out.narrative.as_deref(), Some("did the work"), "{args}");
            assert!(out.result_commits.is_empty(), "{args}");
        }
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
            (Harness::Copilot, COPILOT_STREAM),
            (Harness::Goose, GOOSE_STREAM),
            (Harness::Cline, CLINE_STREAM),
        ];
        for (owner, stream) in streams {
            for h in Harness::ALL {
                let got = launch_adapter(h)
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
            let out = launch_adapter(h).unwrap().parse_stream(
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
        let copilot_spawn = r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"marion-spawn","arguments":{}}}"#;
        let streams = [
            (Harness::ClaudeCode, claude_spawn),
            (Harness::Codex, codex_spawn),
            (Harness::Gemini, gemini_spawn),
            (Harness::OpenCode, opencode_spawn),
            (Harness::Copilot, copilot_spawn),
        ];
        assert!(
            !codex_spawn.contains("mcp__marion__"),
            "the whole reason this is behind the seam"
        );
        for (owner, stream) in streams {
            for h in Harness::ALL {
                let got = launch_adapter(h).unwrap().marion_tool_calls(stream);
                // qwen's headless stream **is** Claude Code's, frame for frame and spelling for
                // spelling (s25 item 2), so its row reads `claude_code::STREAM`; the one pair that
                // legitimately reads each other's stream, and stated here so it stays the only one.
                let reads = h == owner || (owner == Harness::ClaudeCode && h == Harness::Qwen);
                assert_eq!(
                    got,
                    if reads {
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
            let a = launch_adapter(h).unwrap();
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

    /// **What became of each call, per harness — the half the old reader discarded.**
    ///
    /// Each harness answers a call somewhere different: codex revises its own item in place,
    /// opencode carries the verdict on the tool part, gemini and Claude Code emit a separate frame
    /// that must be paired back by id. A single supervisor-side reading would be wrong on three of
    /// the four, exactly as it would be for the verb's name.
    ///
    /// **Provenance, stated because it is uneven.** The answered rows are the recorded shapes
    /// (s6 for codex, s9 for Claude Code, s12 for gemini, s13 for opencode). Of the refused rows
    /// **only opencode's is a recording**; codex's `"status":"failed"` and gemini's non-success
    /// `status` are the obvious complements of what was captured, and nothing in this tree has
    /// watched either arrive. They are pinned so that a harness that starts spelling refusal some
    /// other way fails here rather than passing silently as an answer.
    #[test]
    fn each_adapter_reads_what_became_of_a_marion_call_in_its_own_stream() {
        let answered = |verb: &str| MarionCall {
            verb: verb.to_string(),
            outcome: CallOutcome::Answered,
        };
        let cases: [(Harness, &str, &str, MarionCall); 10] = [
            (
                Harness::ClaudeCode,
                "answered",
                concat!(
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__marion__spawn","input":{}}]}}"#,
                    "\n",
                    // s9's success block carries no `is_error` key at all.
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"child spawned"}]}]}}"#,
                ),
                answered("spawn"),
            ),
            (
                Harness::ClaudeCode,
                "refused",
                concat!(
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__marion__report","input":{}}]}}"#,
                    "\n",
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":[{"type":"text","text":"marion: §5.4 rejects report on a root"}]}]}}"#,
                ),
                MarionCall {
                    verb: "report".into(),
                    outcome: CallOutcome::Refused("marion: §5.4 rejects report on a root".into()),
                },
            ),
            (
                Harness::Codex,
                "answered",
                r#"{"type":"item.completed","item":{"id":"i0","type":"mcp_tool_call","server":"marion","tool":"spawn","result":{},"error":null,"status":"completed"}}"#,
                answered("spawn"),
            ),
            (
                Harness::Codex,
                "refused",
                r#"{"type":"item.completed","item":{"id":"i0","type":"mcp_tool_call","server":"marion","tool":"spawn","result":null,"error":"bad arguments","status":"failed"}}"#,
                MarionCall {
                    verb: "spawn".into(),
                    outcome: CallOutcome::Refused("failed: bad arguments".into()),
                },
            ),
            (
                Harness::Gemini,
                "answered",
                concat!(
                    r#"{"type":"tool_use","tool_name":"mcp_marion_spawn","tool_id":"g1","parameters":{}}"#,
                    "\n",
                    r#"{"type":"tool_result","tool_id":"g1","status":"success","output":"ok"}"#,
                ),
                answered("spawn"),
            ),
            (
                Harness::Gemini,
                "refused",
                concat!(
                    r#"{"type":"tool_use","tool_name":"mcp_marion_spawn","tool_id":"g1","parameters":{}}"#,
                    "\n",
                    r#"{"type":"tool_result","tool_id":"g1","status":"error","output":"invalid arguments"}"#,
                ),
                MarionCall {
                    verb: "spawn".into(),
                    outcome: CallOutcome::Refused("error: invalid arguments".into()),
                },
            ),
            (
                Harness::OpenCode,
                "answered",
                r#"{"type":"tool_use","part":{"type":"tool","tool":"marion_spawn","state":{"status":"completed"}}}"#,
                answered("spawn"),
            ),
            (
                Harness::OpenCode,
                "refused",
                // S13's recorded shape, verbatim.
                r#"{"type":"tool_use","part":{"type":"tool","tool":"marion_spawn","state":{"status":"error","error":"The user rejected permission to use this specific tool call."}}}"#,
                MarionCall {
                    verb: "spawn".into(),
                    outcome: CallOutcome::Refused(
                        "The user rejected permission to use this specific tool call.".into(),
                    ),
                },
            ),
            (
                Harness::Copilot,
                "answered",
                concat!(
                    r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"marion-spawn","arguments":{}}}"#,
                    "\n",
                    r#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","success":true,"result":{"content":"ok"}}}"#,
                ),
                answered("spawn"),
            ),
            (
                Harness::Copilot,
                "refused",
                // s24's recorded shape for a call with no `--allow-tool` grant, verbatim.
                concat!(
                    r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"marion-spawn","arguments":{}}}"#,
                    "\n",
                    r#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","success":false,"error":{"message":"Permission denied and could not request permission from user","code":"denied"}}}"#,
                ),
                MarionCall {
                    verb: "spawn".into(),
                    outcome: CallOutcome::Refused(
                        "Permission denied and could not request permission from user".into(),
                    ),
                },
            ),
        ];
        for (h, label, stream, want) in cases {
            assert_eq!(
                launch_adapter(h).unwrap().marion_calls(stream),
                vec![want],
                "{h}: {label}"
            );
        }
    }

    /// **A call whose result never arrived is `Unknown`, on every harness that can express one.**
    ///
    /// Not `Answered`, which is the whole point: a stream that showed a call starting and never
    /// showed it ending is a run that stopped mid-call, and reading that as success is the defect
    /// class `root::assert_a_verb_was_answered` exists to close.
    #[test]
    fn a_call_with_no_result_frame_is_unknown_and_not_an_answer() {
        let unanswered: [(Harness, &str); 4] = [
            (
                Harness::ClaudeCode,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__marion__spawn","input":{}}]}}"#,
            ),
            // copilot's `tool.execution_start`, which s24 records ahead of every completion.
            (
                Harness::Copilot,
                r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"marion-spawn","arguments":{}}}"#,
            ),
            // codex's `item.started`, which s6 records ahead of every completion.
            (
                Harness::Codex,
                r#"{"type":"item.started","item":{"id":"i0","type":"mcp_tool_call","server":"marion","tool":"spawn","result":null,"error":null,"status":"in_progress"}}"#,
            ),
            (
                Harness::Gemini,
                r#"{"type":"tool_use","tool_name":"mcp_marion_spawn","tool_id":"g1","parameters":{}}"#,
            ),
        ];
        for (h, stream) in unanswered {
            let got = launch_adapter(h).unwrap().marion_calls(stream);
            assert_eq!(
                got,
                vec![MarionCall {
                    verb: "spawn".into(),
                    outcome: CallOutcome::Unknown
                }],
                "{h}: a call with no result is not an answered call"
            );
            assert!(!got[0].outcome.is_answered(), "{h}");
        }
        // opencode is the exception and it is a property of the harness, not a gap here: S13
        // measured `tool_use` firing **only** on terminal states, so there is no in-flight frame
        // for this harness to emit and nothing that could arrive without a verdict on it.
    }

    /// **One codex call is two frames, and it used to count as two calls.**
    ///
    /// `tests/fixtures/s6/exec-mcp-report.stream.jsonl` carries `item.started` then
    /// `item.completed` for the same `id`. The reader this replaced filtered on the item's `type`
    /// and `server` alone, so a codex node that called `report` once appeared to have called it
    /// twice — invisible to an is-empty check, wrong for anything that counts, and fixed by keying
    /// on the id and letting the later frame revise the earlier.
    #[test]
    fn a_codex_call_revised_by_a_later_frame_is_one_call_and_not_two() {
        let s = concat!(
            r#"{"type":"item.started","item":{"id":"i0","type":"mcp_tool_call","server":"marion","tool":"report","status":"in_progress"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"i0","type":"mcp_tool_call","server":"marion","tool":"report","status":"completed"}}"#,
        );
        assert_eq!(
            CodexAdapter.marion_calls(s),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered
            }],
            "the completion revises the start; it does not add a second call"
        );
        assert_eq!(
            CodexAdapter.marion_tool_calls(s),
            vec!["report".to_string()]
        );
    }

    #[test]
    fn an_adapter_survives_being_boxed_and_shared_across_threads() {
        // The bounds §5.2 calls load-bearing, exercised rather than asserted.
        let adapters: Vec<Box<dyn HarnessAdapter + Send + Sync>> = Harness::ALL
            .into_iter()
            .map(|h| launch_adapter(h).unwrap())
            .collect();
        let names: Vec<Harness> = std::thread::scope(|s| {
            s.spawn(|| adapters.iter().map(|a| a.harness()).collect())
                .join()
                .unwrap()
        });
        assert_eq!(names, Harness::ALL.to_vec());
    }

    /// **§6.6's occupancy predicate, checked against what the adapters actually compile.**
    ///
    /// [`Harness::writes_without_a_declaration`] lives in `marion-core`, because §6.6's rule does;
    /// the evidence for it lives here, in four `compiled_permissions` implementations. Two files
    /// holding one fact is exactly the drift this repo refuses elsewhere, so this is the join: for
    /// each harness, compile a spec that declares **no tools at all** and assert the constraint
    /// marion emits is still the one the classification was read off.
    ///
    /// The mapping from a compiled string to "can it write" is *stated per harness rather than
    /// derived*, deliberately — deriving it would re-encode the same judgement in a second place
    /// and this test would then agree with itself for free. What it catches is a change in the
    /// **evidence**: relax gemini's default approval mode, give codex a per-tool knob, teach the
    /// opencode adapter to compile a real constraint, and this fails naming the harness, rather
    /// than §6.6's guard quietly going wrong about which nodes write.
    /// What the compiled constraint has to look like for the classification beside it to hold.
    /// Two shapes, because the four harnesses give evidence in two different ways: three name the
    /// mode they run under, and Claude Code's evidence is an **absence** from a real allowlist.
    enum Evidence {
        /// This exact string is among the compiled constraints.
        Names(&'static str),
        /// No compiled constraint mentions this — the tool was never granted.
        Withholds(&'static str),
    }

    #[test]
    fn the_harnesses_that_write_without_a_grant_are_the_ones_that_compile_no_constraint() {
        use Evidence::*;
        let cases: [(Harness, Evidence, bool); 6] = [
            // A per-tool allowlist with `write` not in it: the mutating tool is simply absent.
            (Harness::ClaudeCode, Withholds("Write"), false),
            // The same shape one harness over: the `write` kind is absent from `--allow-tool`, and
            // s24 measured what an ungranted `create` gets — `denied`, at exit 0.
            (
                Harness::Copilot,
                Withholds(crate::copilot::WRITE_PERMISSION),
                false,
            ),
            // One coarse knob, and it is set to the writing value on every node marion configures.
            (Harness::Codex, Names("sandbox:workspace-write"), true),
            // The default mode drops the mutating tools from `functionDeclarations` outright, so
            // here the *presence* of the default mode is what proves the withholding.
            (Harness::Gemini, Names("approval-mode:default"), false),
            // marion compiles nothing here, and nothing is not a constraint.
            (
                Harness::OpenCode,
                Names(crate::opencode::NO_COMPILED_TOOL_CONSTRAINT),
                true,
            ),
            // The protocol has no tool-availability surface at all, so marion compiles nothing
            // here either — and, unlike opencode, could not have compiled anything if it wanted to.
            (
                Harness::Acp,
                Names(crate::acp::NO_TOOL_AVAILABILITY_SURFACE),
                true,
            ),
        ];
        for (harness, evidence, writes) in cases {
            let adapter =
                launch_adapter(harness).unwrap_or_else(|e| panic!("{harness} has an adapter: {e}"));
            let mut spec = spec_for(harness);
            spec.tools = vec![];
            let compiled = adapter
                .compiled_permissions(&spec)
                .unwrap_or_else(|e| panic!("{harness} compiles an empty tools list: {e}"));
            match evidence {
                Names(e) => assert!(
                    compiled.iter().any(|c| c == e),
                    "{harness}'s classification rests on it compiling {e:?} for a spec that \
                     declares no tools, and it no longer does — the classification must be \
                     re-measured, not re-asserted. Compiled: {compiled:?}"
                ),
                Withholds(e) => assert!(
                    !compiled.iter().any(|c| c.contains(e)),
                    "{harness} is classified as withholding the write tool without a grant, and it \
                     now compiles {e:?} for a spec that declares none. Compiled: {compiled:?}"
                ),
            }
            assert_eq!(
                harness.writes_without_a_declaration(),
                writes,
                "{harness}: marion-core's classification and this harness's compiled constraint \
                 have come apart, which is §6.6's guard going wrong about which nodes write"
            );
        }
    }
    /// **Every marion verb is refused by name on ACP, and the refusal names both halves.**
    ///
    /// The alternative — answering `write` because two other harnesses do — would claim a mapping
    /// onto a name marion has never seen this protocol use, and §11 item 24's whole subject is what
    /// a launch that quietly has no such tool costs.
    #[test]
    fn acp_refuses_every_marion_verb_by_name_because_the_protocol_has_no_such_axis() {
        for tool in [agent_type::TOOL_READ, agent_type::TOOL_WRITE, "invented"] {
            let e = acp_adapter().tool_names(tool).unwrap_err();
            assert_eq!(
                e,
                HarnessError::UnsupportedTool {
                    harness: Harness::Acp,
                    tool: tool.to_string(),
                }
            );
            assert!(
                e.to_string().contains(tool) && e.to_string().contains("acp"),
                "the refusal must name the tool and the harness: {e}"
            );
            // And it aborts the launch rather than being dropped: a declared tool reaches
            // `compile` through `native_tools`, which is the only reason that call is there.
            let spec = LaunchSpec {
                tools: vec![tool.to_string()],
                ..acp_spec()
            };
            assert!(acp_adapter().compile(&spec, &ctx()).is_err(), "{tool}");
            assert!(acp_adapter().compiled_permissions(&spec).is_err(), "{tool}");
        }
        // The empty declaration every node marion spawns today still compiles, or the refusal above
        // would be a refusal of everything.
        assert!(acp_adapter().compile(&acp_spec(), &ctx()).is_ok());
    }

    /// **The copilot refinement: the bridge on argv, the session block empty.** S25 measured copilot
    /// 1.0.83 ignoring `session/new`'s `mcpServers` and starting the server named on its own
    /// `--additional-mcp-config`, so the row declares there — as the same document the copilot
    /// adapter writes for `copilot -p`, env included — and declares nothing on the session, so a
    /// version that honours the protocol channel does not start two bridges. The route says argv,
    /// and verifies against argv. The same program named as a *command* has no row and gets the
    /// protocol's channel, which on this version is the difference between a report and silence.
    #[test]
    fn the_copilot_refinements_argv_document_carries_no_token_and_its_environment_does() {
        let row = AcpAdapter::for_agent(acp::COPILOT);
        let spec = LaunchSpec {
            extra: Extras {
                acp_agent: Some(acp::COPILOT.id.into()),
                ..Extras::default()
            },
            ..acp_spec()
        };
        let minted = SpawnCtx {
            node_token: Some("tok-SENTINEL-acp-9f30".into()),
            ..ctx()
        };
        let inv = row.compile(&spec, &minted).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&inv.args[2]).expect("one JSON document");
        let env = &doc["mcpServers"]["marion"]["env"];
        assert_eq!(env[mcp_bridge::AGENT_ID_ENV], "019f-root", "{doc}");
        assert!(env.get(mcp_bridge::NODE_TOKEN_ENV).is_none(), "{doc}");
        assert!(
            !inv.args.iter().any(|a| a.contains("SENTINEL")),
            "the token is on argv: {:?}",
            inv.args
        );
        assert!(
            inv.env
                .iter()
                .any(|(k, v)| k == mcp_bridge::NODE_TOKEN_ENV && v == "tok-SENTINEL-acp-9f30"),
            "{:?}",
            inv.env
        );
        // The protocol's own channel is marion's pipe, so a session-declared agent keeps the token
        // in its `session/new` block and adds nothing to its environment.
        let session = acp_adapter().compile(&acp_spec(), &minted).unwrap();
        assert!(
            !session
                .env
                .iter()
                .any(|(k, _)| k == mcp_bridge::NODE_TOKEN_ENV)
        );
        let decl = acp_adapter()
            .session_declaration(&acp_spec(), &minted)
            .unwrap()
            .unwrap();
        assert!(decl.to_string().contains("tok-SENTINEL-acp-9f30"));
    }

    #[test]
    fn the_copilot_refinement_declares_the_bridge_on_argv_and_nothing_on_the_session() {
        let row = AcpAdapter::for_agent(acp::COPILOT);
        let spec = LaunchSpec {
            extra: Extras {
                acp_agent: Some(acp::COPILOT.id.into()),
                ..Extras::default()
            },
            ..acp_spec()
        };
        let inv = row.compile(&spec, &ctx()).unwrap();
        assert_eq!(inv.program, "copilot");
        assert_eq!(inv.args[..2], ["--acp", acp::COPILOT_MCP_FLAG]);
        let doc: serde_json::Value = serde_json::from_str(&inv.args[2]).expect("one JSON document");
        assert_eq!(
            doc,
            copilot::mcp_config_json(&bridge_env(&spec, &ctx())),
            "the document copilot's own adapter writes, and no other"
        );
        assert_eq!(
            doc["mcpServers"]["marion"]["command"],
            "/bin/marion-supervisor"
        );
        assert_eq!(
            doc["mcpServers"]["marion"]["tools"],
            serde_json::json!(["*"])
        );
        assert_eq!(inv.args.len(), 3, "nothing else is added: {:?}", inv.args);

        // Nothing to declare on the session — the driver opens a plain `session/new` with an empty
        // `mcpServers`, which is what a second copy of the bridge is kept out of. A resume still
        // compiles its `session/load`, with the same empty block.
        assert_eq!(row.session_declaration(&spec, &ctx()).unwrap(), None);
        let load = row
            .session_declaration(
                &LaunchSpec {
                    resume: Some("ses_prev".into()),
                    ..spec.clone()
                },
                &ctx(),
            )
            .unwrap()
            .expect("a resume names its session");
        assert_eq!(load["method"], acp::SESSION_LOAD_METHOD);
        assert_eq!(load["params"][acp::MCP_SERVERS_KEY], serde_json::json!([]));
        assert_eq!(row.mcp_route(&spec), McpRoute::Argv(acp::COPILOT_MCP_FLAG));
        assert!(row.mcp_route(&spec).verify(&[], &inv, None).is_ok());
        // And with no declaration asked for, no flag, no route.
        let bare = LaunchSpec {
            mcp: McpDeclaration::None,
            ..spec.clone()
        };
        assert_eq!(row.compile(&bare, &ctx()).unwrap().args, vec!["--acp"]);
        assert_eq!(row.mcp_route(&bare), McpRoute::None);

        // The same binary as a command: no row, the protocol's channel, the baseline reading.
        let command = AcpAdapter::resolve("copilot --acp").unwrap();
        let as_command = LaunchSpec {
            extra: Extras {
                acp_agent: Some("copilot --acp".into()),
                ..Extras::default()
            },
            ..acp_spec()
        };
        assert_eq!(
            command.compile(&as_command, &ctx()).unwrap().args,
            vec!["--acp"]
        );
        assert_eq!(
            command.mcp_route(&as_command),
            McpRoute::Session(acp::MCP_SERVERS_KEY)
        );
        assert_eq!(
            command.marion_tool_name("report"),
            acp::GENERIC_SPELLING.spell("report")
        );
        assert_eq!(row.marion_tool_name("report"), "marion-report");
    }

    /// **An ACP resume is `session/load` carrying the same bridge declaration**, on any binding —
    /// the protocol's own resume, which is why the row's argv `resume` grammar is `None` and the
    /// generic path resumes exactly as a refinement row does. `McpRoute::Session` verifies it the
    /// way it verifies a fresh launch, and a resume with no declaration still names its session.
    #[test]
    fn an_acp_resume_is_a_session_load_with_the_same_declaration() {
        for adapter in [acp_adapter(), AcpAdapter::resolve("zed --acp").unwrap()] {
            let fresh = LaunchSpec {
                extra: Extras {
                    acp_agent: Some(adapter.binding.as_ref().unwrap().selector().into()),
                    ..Extras::default()
                },
                ..acp_spec()
            };
            let resumed = LaunchSpec {
                resume: Some("ses_prev".into()),
                ..fresh.clone()
            };
            let new = adapter
                .session_declaration(&fresh, &ctx())
                .unwrap()
                .unwrap();
            let load = adapter
                .session_declaration(&resumed, &ctx())
                .unwrap()
                .unwrap();
            assert_eq!(new["method"], acp::SESSION_NEW_METHOD);
            assert_eq!(load["method"], acp::SESSION_LOAD_METHOD);
            assert_eq!(
                load["id"], new["id"],
                "one id, stamped once, for the driver to wait on"
            );
            assert_eq!(load["params"]["sessionId"], "ses_prev");
            assert_eq!(
                load["params"][acp::MCP_SERVERS_KEY],
                new["params"][acp::MCP_SERVERS_KEY],
                "the bridge is declared again: the agent's MCP processes died with it"
            );
            let inv = adapter.compile(&resumed, &ctx()).unwrap();
            assert!(
                !inv.args.iter().any(|a| a.contains("ses_prev")),
                "nothing about the session reaches argv: {:?}",
                inv.args
            );
            assert!(
                McpRoute::Session(acp::MCP_SERVERS_KEY)
                    .verify(&[], &inv, Some(&load))
                    .is_ok()
            );
            // With no declaration asked for, a fresh launch compiles no request and a resume still
            // compiles the load, with an empty `mcpServers`.
            let bare = LaunchSpec {
                mcp: McpDeclaration::None,
                ..resumed
            };
            let load = adapter
                .session_declaration(&bare, &ctx())
                .unwrap()
                .expect("the session to continue is named only here");
            assert_eq!(load["params"]["sessionId"], "ses_prev");
            assert_eq!(load["params"][acp::MCP_SERVERS_KEY], serde_json::json!([]));
            assert_eq!(
                adapter
                    .session_declaration(
                        &LaunchSpec {
                            resume: None,
                            ..bare
                        },
                        &ctx()
                    )
                    .unwrap(),
                None
            );
        }
    }

    /// **What an ACP launch is refused for, and — the larger half — what it is not.** The two
    /// refusals are the ones the protocol cannot get past: no agent named (§6.4: marion may not
    /// pick one) and a selector with no program in it. An agent marion has never heard of is *not*
    /// one of them: it binds the generic path, compiles as the command the operator wrote, and gets
    /// the bridge declared over `session/new` like any measured row — that is the baseline every
    /// refinement sits on. And a known-but-unmeasured row (`gemini --acp`) is no longer refused a
    /// declaration either: the s14 gate guarded a compiled spelling, and this row compiles none.
    #[test]
    fn an_acp_launch_is_refused_only_for_what_the_protocol_cannot_express() {
        // 1. No agent named.
        let unnamed = LaunchSpec {
            resume: None,
            extra: Extras::default(),
            ..acp_spec()
        };
        assert!(matches!(
            acp_adapter().compile(&unnamed, &ctx()),
            Err(HarnessError::MissingInput {
                harness: Harness::Acp,
                ..
            })
        ));
        // **And it is not silently served by the one agent that is measured**, which is the
        // fallback this seam exists to end.
        assert_ne!(
            acp_adapter().compile(&unnamed, &ctx()).ok(),
            acp_adapter().compile(&acp_spec(), &ctx()).ok()
        );

        // 2. A selector with no program. The refusal lists the rows and names the command shape.
        for empty in ["", "   "] {
            let e = adapter_for_type(Harness::Acp, Some(empty)).err().unwrap();
            assert!(
                matches!(&e, HarnessError::AcpAgent(m) if m.contains("no program") && m.contains("opencode")),
                "{empty:?}: got {e}"
            );
        }

        // 3. **Not refused**: an agent marion has never heard of. The whole command is the argv,
        // nothing of marion's is added, the declaration is the protocol-standard one, and the
        // transcript is read generically.
        let unknown = LaunchSpec {
            resume: None,
            extra: Extras {
                acp_agent: Some("zed --acp --flag".into()),
                ..Extras::default()
            },
            ..acp_spec()
        };
        let zed = adapter_for_type(Harness::Acp, Some("zed --acp --flag")).unwrap();
        let inv = zed
            .compile(&unknown, &ctx())
            .expect("the generic path launches");
        assert_eq!(inv.program, "zed");
        assert_eq!(inv.args, vec!["--acp", "--flag"]);
        let session = zed
            .session_declaration(&unknown, &ctx())
            .unwrap()
            .expect("the bridge is declared");
        assert!(
            McpRoute::Session(acp::MCP_SERVERS_KEY)
                .verify(&[], &inv, Some(&session))
                .is_ok()
        );
        assert_eq!(
            zed.marion_tool_name("report"),
            acp::GENERIC_SPELLING.spell("report"),
            "the baseline spelling, which reaches no model on this row"
        );
        // Generic reading: every measured transcript's report is found, whichever spelling.
        assert_eq!(
            zed.parse_stream(ACP_OPENCODE_SESSION, ChildExit::default())
                .narrative
                .as_deref(),
            Some("hello from acp")
        );

        // 4. **Not refused**: a known row whose tool spelling has never been measured. It compiles
        // its measured argv and gets a declaration; its transcript is read generically.
        let unmeasured = LaunchSpec {
            resume: None,
            extra: Extras {
                acp_agent: Some(acp::GEMINI.id.into()),
                ..Extras::default()
            },
            ..acp_spec()
        };
        assert!(
            acp::GEMINI.tools.is_none(),
            "the premise: this row is the unmeasured one"
        );
        let gemini = AcpAdapter::for_agent(acp::GEMINI);
        let inv = gemini.compile(&unmeasured, &ctx()).unwrap();
        assert_eq!(inv.program, "gemini");
        assert_eq!(
            inv.args,
            vec!["--acp"],
            "the row's measured argv, not the id"
        );
        assert!(
            gemini
                .session_declaration(&unmeasured, &ctx())
                .unwrap()
                .is_some()
        );
        assert_eq!(
            gemini.marion_tool_name("report"),
            acp::GENERIC_SPELLING.spell("report")
        );
        // And the measured row keeps its own spelling: the refinement, layered over the baseline.
        assert_eq!(acp_adapter().marion_tool_name("report"), "marion_report");
        assert!(
            acp_adapter()
                .session_declaration(&acp_spec(), &ctx())
                .unwrap()
                .is_some()
        );
    }

    /// A canned ACP spec: an isolated config dir, marion's endpoint, and a `provider/model` pair
    /// for the document's `model` key — the shape S13 measured for `opencode run`, which is where
    /// this recipe is inferred from.
    fn canned_acp_spec() -> LaunchSpec {
        LaunchSpec {
            auth: Auth::Canned,
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            api_key: Some("marion-placeholder".into()),
            model: Some("marion/canned-1".into()),
            config_dir: "/state/agent".into(),
            ..acp_spec()
        }
    }

    /// **A canned provider is reached per agent, and refused per agent** — the distinction the
    /// blanket refusal used to flatten.
    ///
    /// ACP itself names no provider, base URL or credential anywhere in its handshake, and that
    /// fact has not changed. What changed is the conclusion drawn from it: the *agent* behind the
    /// protocol may have a channel marion already compiles, and `opencode acp` is the same binary
    /// as `opencode run`, reading the same config document under the same `$XDG_CONFIG_HOME` that
    /// S13 measured taking a whole turn against marion's canned provider at $0.00.
    ///
    /// **That last step is an inference, and it is the only one in the row.** S13's capture is of
    /// the `run` subcommand; no capture yet exists of the `acp` subcommand under this document. The
    /// recipe is therefore one inference deep — opencode's config loading being
    /// subcommand-independent — and what this test pins is the *shape* marion compiles, not that a
    /// live `opencode acp` accepted it. An agent with no recipe at all is still refused, by name.
    #[test]
    fn a_canned_provider_is_reached_only_by_an_agent_with_a_recipe_for_reaching_one() {
        // The measured one: a launch, an isolated home, and the document that points it at marion.
        let inv = acp_adapter().compile(&canned_acp_spec(), &ctx()).unwrap();
        assert_eq!(inv.program, "opencode");
        assert_eq!(inv.args, vec!["acp"], "still the agent's own argv");
        let env: std::collections::BTreeMap<&str, &str> = inv
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            env.get("XDG_CONFIG_HOME").copied(),
            Some(
                opencode::xdg_config_home(std::path::Path::new("/state/agent"))
                    .to_string_lossy()
                    .as_ref()
            ),
            "the document is only read if the agent is pointed at it: {env:?}"
        );
        assert_eq!(env.get("HOME").copied(), Some("/state/agent"));
        assert_eq!(
            env.get("PWD").copied(),
            Some(canned_acp_spec().cwd.to_string_lossy().as_ref()),
            "S13: `cwd` alone does not place an opencode node"
        );

        let files = acp_adapter()
            .config_files(&canned_acp_spec(), &ctx())
            .unwrap();
        assert_eq!(files.len(), 1, "one document, and it is not a declaration");
        assert_eq!(
            files[0].0,
            opencode::config_path(std::path::Path::new("/state/agent"))
        );
        let doc: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(doc["model"], "marion/canned-1");
        assert_eq!(
            doc["provider"]["marion"]["options"]["baseURL"],
            "http://127.0.0.1:8099/v1"
        );
        assert!(
            doc.get("mcp").is_none(),
            "marion's bridge rides `session/new`; a second copy here would start a second process"
        );

        // The unmeasured ones: refused by name, not launched at a real vendor while the contract
        // records a canned run.
        for agent in [acp::CLAUDE_ACP, acp::CODEX_ACP] {
            assert!(agent.canned.is_none(), "the premise for `{}`", agent.id);
            let spec = LaunchSpec {
                resume: None,
                extra: Extras {
                    acp_agent: Some(agent.id.into()),
                    ..Extras::default()
                },
                ..canned_acp_spec()
            };
            let e = AcpAdapter::for_agent(agent)
                .compile(&spec, &ctx())
                .unwrap_err();
            assert!(
                matches!(
                    &e,
                    HarnessError::MissingInput {
                        harness: Harness::Acp,
                        ..
                    }
                ),
                "`{}`: got {e}",
                agent.id
            );
            let said = e.to_string();
            assert!(
                !said.contains("--live") && !said.contains('§'),
                "`{}`: the refusal names no retired flag and no spec section: {said}",
                agent.id
            );
            assert!(
                said.contains("--canned"),
                "`{}`: says what to drop: {said}",
                agent.id
            );
            // And live mode is not refused for the same agent, or this would be a refusal of the
            // agent rather than of the mode.
            let live = LaunchSpec {
                auth: Auth::Inherited,
                ..spec
            };
            assert!(AcpAdapter::for_agent(agent).compile(&live, &ctx()).is_ok());
        }
    }

    /// **An `opencode acp` node carries what an `opencode run` node carries, under both modes** —
    /// the row's hygiene, its no-self-update switch and its placement, each gated by the row
    /// itself, so a live node keeps the operator's `HOME` and `XDG_*` roots and loses only what
    /// `--live` removes. And the MCP timeout: s36 measured `opencode acp` 1.18.32 abandoning a
    /// `tools/call` to a `session/new`-declared server at 60 s, and completing a 75 s one once
    /// `experimental.mcp_timeout` was set — the one channel that reaches a server the config does
    /// not itself declare. It rides `OPENCODE_CONFIG_CONTENT`, which merges over whatever config
    /// the node reads, and it shadows none of the operator's provider settings.
    #[test]
    fn an_opencode_acp_node_carries_the_run_rows_env_and_outlasts_the_sixty_second_call_limit() {
        for (mode, spec) in [("live", acp_spec()), ("canned", canned_acp_spec())] {
            let inv = acp_adapter().compile(&spec, &ctx()).unwrap();
            let env: std::collections::BTreeMap<&str, &str> = inv
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            for (k, v) in [
                ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
                ("OPENCODE_DISABLE_CLAUDE_CODE", "1"),
                ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1"),
                ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
                ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1"),
                ("OPENCODE_DISABLE_SHARE", "1"),
            ] {
                assert_eq!(env.get(k).copied(), Some(v), "{mode}: {k} in {env:?}");
            }
            assert_eq!(
                env.get("PWD").copied(),
                Some(spec.cwd.to_string_lossy().as_ref()),
                "{mode}: placement, not isolation"
            );
            let content: serde_json::Value = serde_json::from_str(
                env.get(opencode::CONFIG_CONTENT_ENV)
                    .unwrap_or_else(|| panic!("{mode}: no inline config in {env:?}")),
            )
            .unwrap();
            assert_eq!(
                content["experimental"]["mcp_timeout"],
                serde_json::json!(opencode::MCP_TIMEOUT_MS),
                "{mode}: a spawn or wait longer than 60 s would fail at opencode's side"
            );
            for shadowing in ["provider", "model", "small_model", "mcp"] {
                assert!(content.get(shadowing).is_none(), "{mode}: {shadowing}");
            }
        }
        let live = acp_adapter().compile(&acp_spec(), &ctx()).unwrap();
        for relocation in [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "OPENCODE_DISABLE_PROJECT_CONFIG",
        ] {
            assert!(
                !live.env.iter().any(|(k, _)| k == relocation),
                "live: {relocation} would hide the operator's login or config"
            );
        }
    }

    /// The canned document's `model` key is **the only channel that reaches an ACP session**, so a
    /// canned launch without one is refused rather than run at whatever the agent defaults to.
    ///
    /// Without this the recipe would silently produce a document with a `null` model and the
    /// session would pick the operator's own default — a run marion recorded as canned, against a
    /// model marion did not choose and possibly at a vendor marion is not paying.
    #[test]
    fn a_canned_acp_launch_without_a_model_or_an_endpoint_is_refused_by_name() {
        for (what, spec) in [
            (
                "model",
                LaunchSpec {
                    model: None,
                    ..canned_acp_spec()
                },
            ),
            (
                "base url",
                LaunchSpec {
                    base_url: None,
                    ..canned_acp_spec()
                },
            ),
            (
                "a `provider/model` pair rather than a bare id",
                LaunchSpec {
                    model: Some("canned-1".into()),
                    ..canned_acp_spec()
                },
            ),
        ] {
            let e = acp_adapter().config_files(&spec, &ctx()).unwrap_err();
            assert!(
                matches!(
                    &e,
                    HarnessError::MissingInput {
                        harness: Harness::Acp,
                        ..
                    }
                ),
                "missing {what}: got {e}"
            );
        }
        assert!(
            acp_adapter()
                .config_files(&canned_acp_spec(), &ctx())
                .is_ok(),
            "the complete spec still compiles, or the rows above refuse everything"
        );
    }

    /// The compiled launch is the agent's own argv and **nothing else** — no prompt, no model, no
    /// credential. Each absence is a decision `AcpAdapter::compile` argues, and each would be
    /// invisible without a row here.
    /// **An agent type's `approval_mode` is a session mode, set over the protocol.** It reaches the
    /// driver on the compiled [`Invocation`] (never argv), and the contract's constraint names it,
    /// because the driver refuses a run whose agent does not offer it — so the record describes a
    /// mode the session really ran in.
    #[test]
    fn an_acp_approval_mode_rides_the_session_and_is_recorded_as_the_constraint() {
        let spec = LaunchSpec {
            extra: Extras {
                approval_mode: Some("acceptEdits".into()),
                ..acp_spec().extra
            },
            ..acp_spec()
        };
        let inv = acp_adapter().compile(&spec, &ctx()).unwrap();
        assert_eq!(inv.session_mode.as_deref(), Some("acceptEdits"));
        assert!(
            !inv.args.iter().any(|a| a.contains("acceptEdits")),
            "{:?}",
            inv.args
        );
        assert_eq!(
            acp_adapter().compiled_permissions(&spec).unwrap(),
            vec![
                acp::NO_TOOL_AVAILABILITY_SURFACE.to_string(),
                format!("{}acceptEdits", acp::SESSION_MODE_PREFIX),
            ]
        );
        // And without one, nothing changes.
        assert_eq!(
            acp_adapter()
                .compile(&acp_spec(), &ctx())
                .unwrap()
                .session_mode,
            None
        );
    }

    /// A harness with no ACP session has nowhere to set a session mode, and says so by name rather
    /// than launching a node that ignores the operator's approval choice.
    #[test]
    fn an_approval_mode_off_acp_is_refused_by_name() {
        let spec = LaunchSpec {
            extra: Extras {
                approval_mode: Some("auto".into()),
                ..Extras::default()
            },
            ..codex_live_spec()
        };
        let e = CodexAdapter.compile(&spec, &ctx()).unwrap_err();
        assert!(
            matches!(e, HarnessError::MissingInput { harness: Harness::Codex, what } if what.contains("approval_mode")),
            "{e}"
        );
    }

    #[test]
    fn an_acp_launch_carries_the_agents_argv_and_nothing_marion_added() {
        let spec = LaunchSpec {
            prompt: "do the task".into(),
            model: Some("anthropic/claude-opus-5".into()),
            api_key: Some("sk-fake".into()),
            ..acp_spec()
        };
        let inv = acp_adapter().compile(&spec, &ctx()).unwrap();
        assert_eq!(inv.program, "opencode");
        assert_eq!(inv.args, vec!["acp"]);
        assert_eq!(
            inv.model.as_deref(),
            Some("anthropic/claude-opus-5"),
            "the model rides the session (`session/set_config_option`, which the driver refuses \
             the run without), so the record names it while argv does not"
        );
        assert!(
            !inv.env.iter().any(|(k, _)| is_credential_or_relocation(k)),
            "no credential and no relocation — only the opencode row's hygiene, update switch, \
             placement and call timeout: {:?}",
            inv.env
        );
        assert!(
            !inv.args.iter().any(|a| a.contains("do the task")),
            "the prompt rides `session/prompt`, not argv"
        );
        assert_eq!(inv.cwd, spec.cwd);
        assert!(
            acp_adapter()
                .config_files(&spec, &ctx())
                .unwrap()
                .is_empty(),
            "ACP declares marion's bridge in `session/new`, so a live node writes no document"
        );
    }

    /// **An adapter bound to one agent will not launch another's spec.**
    ///
    /// There are two routes to "which ACP agent is this" — the agent type's id, and the adapter the
    /// supervisor bound from it — and they meet here. If they were allowed to disagree, marion
    /// would compile one agent's argv and read the other's tool spelling out of the transcript: a
    /// node that ran, called `report`, and was recorded as having called nothing (s14's shape, with
    /// marion on the producing end). A precedence rule would pick a winner silently; this refuses.
    #[test]
    fn an_adapter_bound_to_one_agent_refuses_another_agents_launch() {
        let spec = LaunchSpec {
            resume: None,
            extra: Extras {
                acp_agent: Some(acp::CODEX_ACP.id.into()),
                ..Extras::default()
            },
            ..acp_spec()
        };
        let e = acp_adapter().compile(&spec, &ctx()).unwrap_err();
        assert!(
            matches!(&e, HarnessError::AcpAgent(m) if m.contains("opencode") && m.contains("codex-acp")),
            "the refusal must name both agents: {e}"
        );
        // The two spellings this would have crossed are genuinely different, or the refusal would
        // be guarding nothing.
        assert_ne!(
            acp_adapter().marion_tool_name("report"),
            AcpAdapter::for_agent(acp::CODEX_ACP).marion_tool_name("report")
        );
        assert!(
            AcpAdapter::for_agent(acp::CODEX_ACP)
                .compile(&spec, &ctx())
                .is_ok(),
            "the matching pair launches"
        );
    }

    /// **The unbound adapter cannot be launched, and its spelling is not a tool name.**
    ///
    /// `adapter_for` answers from a [`Harness`] alone, which is enough for the surface questions
    /// and not enough to say what a model will type. So the protocol-level adapter refuses every
    /// route by which marion's verbs reach a model, and the sentinel it answers `marion_tool_name`
    /// with is unreachable rather than merely unlikely.
    #[test]
    fn the_unbound_acp_adapter_refuses_every_route_to_a_model() {
        let unbound = AcpAdapter::unbound();
        for e in [
            unbound.compile(&acp_spec(), &ctx()).unwrap_err(),
            unbound
                .session_declaration(&acp_spec(), &ctx())
                .unwrap_err(),
            unbound.config_files(&acp_spec(), &ctx()).unwrap_err(),
        ] {
            assert!(
                matches!(
                    &e,
                    HarnessError::MissingInput {
                        harness: Harness::Acp,
                        ..
                    }
                ),
                "got {e}"
            );
        }
        // The surface questions still answer, which is the whole reason the unbound adapter exists.
        assert_eq!(unbound.harness(), Harness::Acp);
        assert_eq!(unbound.surfaces(), acp::surfaces());

        // And the sentinel is not a tool name: no MCP tool name may carry a colon, so it can match
        // nothing the three measured agents ever emit.
        let sentinel = unbound.marion_tool_name("report");
        assert!(sentinel.starts_with(acp::UNBOUND_TOOL_NAME) && sentinel.contains(':'));
        for a in acp::AGENTS.iter().filter_map(|a| a.tools) {
            assert_ne!(a.spell("report"), sentinel);
        }
        // A reader with no spelling reports no calls rather than somebody else's.
        assert!(unbound.marion_calls(ACP_OPENCODE_SESSION).is_empty());
        assert_eq!(
            unbound
                .parse_stream(ACP_OPENCODE_SESSION, ChildExit::default())
                .narrative,
            None
        );
    }

    /// §3.4's axes for the ACP row, and the one that is a safety property: a node with no terminal
    /// must not be one call from the pty launcher (§11 item 1).
    #[test]
    fn the_acp_surfaces_are_typed_over_pipes_with_no_pane_shape() {
        let s = acp_adapter().surfaces();
        assert_eq!(s.control, ControlTransport::Typed(TypedKind::Acp));
        assert_eq!(s.display, DisplaySurface::StructuredUi);
        assert!(s.display_plane().is_none(), "no pty, so no witness");
        assert!(s.has_typed_control_plane());
        assert!(acp_adapter().pane_surfaces().is_none());
        // Refused by name, never downgraded to the headless shape.
        assert_eq!(
            acp_adapter().compile_pane(&acp_spec(), &ctx()),
            Err(HarnessError::NoPaneSurface(Harness::Acp))
        );
    }

    /// **The declarative spec compiles what the adapter that measured it compiles — byte for byte,
    /// on every harness, both shapes, both auth modes.**
    ///
    /// This is the migration's invariant, stated before the migration: `spec::render` over the
    /// harness's `HarnessSpec` row must equal the hand-written `compile` / `compile_pane` it is
    /// about to replace, and must refuse exactly where that refused. The row is data
    /// (`Arg`s and `Env`s); everything that is genuinely a measured *decision* — a refusal, a model
    /// rule, a derived URL — stays in the adapter's `fields` hook, which is the other half of the
    /// comparison. A row that renders one token differently from the code it was transcribed from
    /// is a guess wearing a table's clothes, which is the risk `plan-harness-spec.md` names.
    ///
    /// Every harness is a row now; the sweep grew one harness per commit behind a `MIGRATED` gate
    /// and the gate is gone with the last `todo!()`.
    #[test]
    fn spec_render_matches_the_adapter_that_measured_it() {
        use crate::spec::{Shape, render};
        for h in Harness::ALL {
            let a = launch_adapter(h).unwrap();
            let row = harness_spec(h);
            assert_eq!(
                row.harness, h,
                "the row names the harness it was measured on"
            );
            assert!(
                !row.note.trim().is_empty(),
                "{h}: a row without its measurement is a guess"
            );
            let canned = spec_for(h);
            let live = LaunchSpec {
                auth: Auth::Inherited,
                base_url: None,
                api_key: None,
                ..spec_for(h)
            };
            let live = match h {
                // The two harnesses whose live mode refuses the canned default model by name.
                Harness::OpenCode => opencode_live_spec(),
                Harness::Copilot => LaunchSpec {
                    model: Some("gpt-5".into()),
                    ..live
                },
                _ => live,
            };
            for (mode, launch) in [("canned", &canned), ("live", &live)] {
                for shape in [Shape::Headless, Shape::Pane] {
                    let expected = match shape {
                        Shape::Headless => a.compile(launch, &ctx()),
                        Shape::Pane => a.compile_pane(launch, &ctx()),
                    };
                    let got = a.fields(launch, &ctx(), shape).and_then(|f| {
                        render(row, shape, &f).map_err(|_| HarnessError::NoPaneSurface(h))
                    });
                    assert_eq!(
                        got, expected,
                        "{h} ({mode}, {shape:?}): the row must render exactly what the adapter \
                         compiled, and refuse exactly where it refused"
                    );
                }
            }
        }
    }

    /// **Every committed stream fixture reads the same through the grammar as it read through the
    /// hand-written reader it replaces** — pinned as the literal outcome the old reader produced,
    /// so the readings survive the readers.
    ///
    /// `tests/fixtures/s6` (codex), `s9` (claude) and `s24` (copilot) carry real captured stdout;
    /// `s12` (gemini) and `s13` (opencode) carry only their README, so those two rows are pinned by
    /// the synthetic frames in their module tests and by `harness_matrix`, which reads a live
    /// stream of each. Every reading below was produced by the adapter's reader on this tree
    /// before the grammar existed, and the adapter is asserted beside the row so the two cannot
    /// come apart while both exist.
    #[test]
    fn every_committed_fixture_reads_the_same_through_the_grammar() {
        use crate::grammar;
        macro_rules! fixture {
            ($p:literal) => {
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../tests/fixtures/",
                    $p
                ))
            };
        }
        let answered = |verb: &str| {
            vec![MarionCall {
                verb: verb.into(),
                outcome: CallOutcome::Answered,
            }]
        };
        let refused = |verb: &str, why: &str| {
            vec![MarionCall {
                verb: verb.into(),
                outcome: CallOutcome::Refused(why.into()),
            }]
        };
        let outcome = |narrative: Option<&str>, failure: Option<&str>| StreamOutcome {
            narrative: narrative.map(str::to_string),
            failure: failure.map(str::to_string),
            ..StreamOutcome::default()
        };
        let cases: Vec<(Harness, &str, &str, StreamOutcome, Vec<MarionCall>)> = vec![
            (
                Harness::Codex,
                "s6/exec-codemode-apply-patch.stream.jsonl",
                fixture!("s6/exec-codemode-apply-patch.stream.jsonl"),
                StreamOutcome {
                    file_change_paths: vec![
                        "<HOME>/Desktop/CODING/marion/spikes/s6/wt/a.rs".into(),
                        "<HOME>/Desktop/CODING/marion/spikes/s6/wt/a.rs".into(),
                    ],
                    ..StreamOutcome::default()
                },
                vec![],
            ),
            (
                Harness::Codex,
                "s6/exec-mcp-report.stream.jsonl",
                fixture!("s6/exec-mcp-report.stream.jsonl"),
                outcome(Some("s6 probe: reporting via MCP"), None),
                answered("report"),
            ),
            (
                Harness::ClaudeCode,
                "s9/can-use-tool-allow.stdout.jsonl",
                fixture!("s9/can-use-tool-allow.stdout.jsonl"),
                outcome(Some("s9 probe: a verb the root may not use"), None),
                answered("report"),
            ),
            (
                Harness::ClaudeCode,
                "s9/can-use-tool-builtin-deny.stdout.jsonl",
                fixture!("s9/can-use-tool-builtin-deny.stdout.jsonl"),
                outcome(None, None),
                vec![],
            ),
            (
                Harness::ClaudeCode,
                "s9/can-use-tool-deny.stdout.jsonl",
                fixture!("s9/can-use-tool-deny.stdout.jsonl"),
                outcome(Some("s9 probe: a verb the root may not use"), None),
                // The recording carries the refusal as a **string** `content`, which the reader
                // this grammar replaced read past (it joined array blocks only) and reported as
                // "the result frame carried no message". The grammar reads the words.
                refused(
                    "report",
                    "marion: no permission answerer in M1; the root's Blocked bound expired",
                ),
            ),
            (
                Harness::Copilot,
                "s24/copilot-allow-all-baseline.stdout.jsonl",
                fixture!("s24/copilot-allow-all-baseline.stdout.jsonl"),
                outcome(
                    Some("Wrote the matrix marker under src/ and reported back."),
                    None,
                ),
                answered("report"),
            ),
            (
                Harness::Copilot,
                "s24/copilot-create-denied-without-grant.stdout.jsonl",
                fixture!("s24/copilot-create-denied-without-grant.stdout.jsonl"),
                outcome(
                    Some("Wrote the matrix marker under src/ and reported back."),
                    Some(
                        "the child's marion-report call ended in error: Permission denied and \
                         could not request permission from user",
                    ),
                ),
                refused(
                    "report",
                    "Permission denied and could not request permission from user",
                ),
            ),
            (
                Harness::Copilot,
                "s24/copilot-provider-500.stdout.jsonl",
                fixture!("s24/copilot-provider-500.stdout.jsonl"),
                outcome(
                    None,
                    Some(
                        "Failed to get response from the AI model; retried 5 times (total retry \
                         wait time: 30.30 seconds) Last error: 500 canned provider failure",
                    ),
                ),
                vec![],
            ),
            (
                Harness::Copilot,
                "s24/copilot-report-iserror.stdout.jsonl",
                fixture!("s24/copilot-report-iserror.stdout.jsonl"),
                outcome(
                    Some("Wrote the matrix marker under src/ and reported back."),
                    Some(
                        "the child's marion-report call ended in error: MCP server 'marion': \
                         refused: not authorized",
                    ),
                ),
                refused("report", "MCP server 'marion': refused: not authorized"),
            ),
            (
                Harness::Copilot,
                "s24/copilot-write-then-report.stdout.jsonl",
                fixture!("s24/copilot-write-then-report.stdout.jsonl"),
                outcome(
                    Some("Wrote the matrix marker under src/ and reported back."),
                    None,
                ),
                answered("report"),
            ),
            (
                Harness::Goose,
                "s26/goose-report.stdout.jsonl",
                fixture!("s26/goose-report.stdout.jsonl"),
                outcome(Some("hello from goose under a canned provider"), None),
                answered("report"),
            ),
            (
                Harness::Goose,
                "s26/goose-report-iserror.stdout.jsonl",
                fixture!("s26/goose-report-iserror.stdout.jsonl"),
                outcome(
                    Some("hello from goose under a canned provider"),
                    Some("the child's marion__report call ended in error: refused: not authorized"),
                ),
                refused("report", "refused: not authorized"),
            ),
            // The fault is an assistant text block opening `Ran into this error: ` and a
            // zero-token `complete`, at exit 0: S37 measured it as goose's only report of a
            // provider fault, so the row reads it as the stream's failure claim.
            (
                Harness::Goose,
                "s26/goose-provider-500.stdout.jsonl",
                fixture!("s26/goose-provider-500.stdout.jsonl"),
                outcome(
                    None,
                    Some(
                        "Ran into this error: Server error: Server error (500 Internal Server \
                         Error) at http://127.0.0.1:<PORT>/v1/chat/completions: canned 500 from \
                         s26.\n\nPlease retry if you think this is a transient or recoverable \
                         error.",
                    ),
                ),
                vec![],
            ),
            // `approve` aborts after the request: a call with no result is `Unknown`, and the
            // narrative it carried is still read off the request.
            (
                Harness::Goose,
                "s26/goose-approve-mode.stdout.jsonl",
                fixture!("s26/goose-approve-mode.stdout.jsonl"),
                outcome(Some("hello from goose under a canned provider"), None),
                vec![MarionCall {
                    verb: "report".into(),
                    outcome: CallOutcome::Unknown,
                }],
            ),
            (
                Harness::Cline,
                "s27/cline-report-ok.stdout.jsonl",
                fixture!("s27/cline-report-ok.stdout.jsonl"),
                outcome(Some("hello from cline under a canned provider"), None),
                answered("report"),
            ),
            (
                Harness::Cline,
                "s27/cline-report-iserror.stdout.jsonl",
                fixture!("s27/cline-report-iserror.stdout.jsonl"),
                outcome(
                    Some("hello from cline under a canned provider"),
                    Some("the child's marion__report call ended in error: refused: not authorized"),
                ),
                refused("report", "refused: not authorized"),
            ),
            (
                Harness::Cline,
                "s27/cline-provider-500.stdout.jsonl",
                fixture!("s27/cline-provider-500.stdout.jsonl"),
                outcome(None, Some("canned failure")),
                vec![],
            ),
            (
                Harness::Qwen,
                "s25/qwen-write-then-report.stdout.jsonl",
                fixture!("s25/qwen-write-then-report.stdout.jsonl"),
                outcome(Some("hello from qwen under a canned provider"), None),
                answered("report"),
            ),
            // Claude Code's grammar records a refused report on the call and lets the result frame
            // decide the run — and qwen's result frame says `success` at exit 0 (s25 item 7).
            (
                Harness::Qwen,
                "s25/qwen-report-iserror.stdout.jsonl",
                fixture!("s25/qwen-report-iserror.stdout.jsonl"),
                outcome(Some("hello from qwen under a canned provider"), None),
                refused(
                    "report",
                    "MCP tool 'report' reported tool error for function call: \
                     {\"name\":\"report\",\"args\":{\"narrative\":\"hello from qwen under a canned \
                     provider\"}} with response: [{\"functionResponse\":{\"name\":\"report\",\
                     \"response\":{\"error\":{\"content\":[{\"type\":\"text\",\"text\":\"refused: \
                     not authorized\"}],\"isError\":true},\"content\":[{\"type\":\"text\",\"text\":\
                     \"refused: not authorized\"}]}}}]",
                ),
            ),
            // 28 retries, then `result.subtype: "success"` whose text is the API error: S37
            // measured it as qwen's only report of the fault, so it is the failure claim.
            (
                Harness::Qwen,
                "s25/qwen-provider-500.stdout.jsonl",
                fixture!("s25/qwen-provider-500.stdout.jsonl"),
                outcome(None, Some("[API Error: 500 canned failure]")),
                vec![],
            ),
            (
                Harness::Qwen,
                "s25/qwen-no-auth.stdout.jsonl",
                fixture!("s25/qwen-no-auth.stdout.jsonl"),
                // The subtype, not the message: qwen's `error.message` is a field Claude Code's
                // frame does not carry, and the shared grammar reads `result`/`subtype`.
                outcome(None, Some("error_during_execution")),
                vec![],
            ),
            // Declined inside cline: `output.error` and no `isError`, so the call reads as
            // answered, and the `agent_event error` that follows is what fails the run.
            (
                Harness::Cline,
                "s27/cline-report-denied-no-auto-approve.stdout.jsonl",
                fixture!("s27/cline-report-denied-no-auto-approve.stdout.jsonl"),
                outcome(
                    Some("hello from cline under a canned provider"),
                    Some(
                        "1 tool call(s) failed: [marion__report] {\"error\":\"Tool \
                         \\\"marion__report\\\" requires approval in a TTY session -- NOT a tool or \
                         system failure. Clarify with user before proceeding.\"}",
                    ),
                ),
                answered("report"),
            ),
        ];
        for (h, name, stdout, expected_outcome, expected_calls) in cases {
            let a = adapter_for(h).unwrap();
            assert_eq!(
                a.parse_stream(stdout, ChildExit::default()),
                expected_outcome,
                "{h} {name}: the adapter's reading"
            );
            assert_eq!(
                a.marion_calls(stdout),
                expected_calls,
                "{h} {name}: the adapter's calls"
            );
            let g = harness_spec(h)
                .stream
                .unwrap_or_else(|| panic!("{h}: no stream grammar on its row"));
            let prefix = a.marion_tool_name("");
            assert_eq!(
                grammar::parse_stream(g, stdout, &prefix),
                expected_outcome,
                "{h} {name}: the grammar's reading"
            );
            assert_eq!(
                grammar::marion_calls(g, stdout, &prefix),
                expected_calls,
                "{h} {name}: the grammar's calls"
            );
        }
        // The two harnesses with no captured stream: their rows must still exist, so the sweep
        // above is not silently narrower than the registry.
        for h in [Harness::Gemini, Harness::OpenCode] {
            assert!(
                harness_spec(h).stream.is_some(),
                "{h}: no stream grammar on its row"
            );
        }
    }

    /// **One bridge declaration, five documents.** Every harness's declaration of marion's bridge
    /// is a serialisation of the same [`BridgeEnv`] — the `env` block of four JSON shapes, the TOML
    /// table and `-c` pairs of codex, the `session/new` env array of ACP — so no adapter can hand
    /// the bridge a different set of variables than another. Four field-for-field copies of the
    /// struct, each with its own env-block builder, were how that could have happened.
    #[test]
    fn one_bridge_env_feeds_every_declaration_document() {
        let expected = bridge_env(&claude_spec(), &ctx());
        assert_eq!(
            expected
                .pairs()
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            [
                "MARION_REPO",
                "MARION_STATE_DIR",
                "MARION_AUTH",
                "MARION_AGENT_ID",
                "MARION_AGENT_TYPE",
                "MARION_DEPTH",
                "MARION_BASE_URL",
                "MARION_READY_FILE",
            ],
            "the bridge's contract, in one order, with the optionals present because the spec \
             carries them"
        );
        let env_block = |files: Vec<(PathBuf, String)>, ptr: &str| -> serde_json::Value {
            let v: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
            v.pointer(ptr)
                .cloned()
                .unwrap_or_else(|| panic!("{ptr} in {v:#}"))
        };
        assert_eq!(
            env_block(
                ClaudeCodeAdapter
                    .config_files(&claude_spec(), &ctx())
                    .unwrap(),
                "/mcpServers/marion/env"
            ),
            expected.env_json()
        );
        for (h, ptr) in [
            (Harness::Gemini, "/mcpServers/marion/env"),
            (Harness::OpenCode, "/mcp/marion/environment"),
            (Harness::Copilot, "/mcpServers/marion/env"),
        ] {
            let spec = spec_for(h);
            assert_eq!(
                env_block(
                    launch_adapter(h)
                        .unwrap()
                        .config_files(&spec, &ctx())
                        .unwrap(),
                    ptr
                ),
                bridge_env(&spec, &ctx()).env_json(),
                "{h}"
            );
        }
        let toml = CodexAdapter.config_files(&codex_spec(), &ctx()).unwrap()[0]
            .1
            .clone();
        for (k, v) in bridge_env(&codex_spec(), &ctx()).pairs() {
            assert!(toml.contains(&format!("{k} = \"{v}\"")), "{k} in\n{toml}");
        }
        let live = codex_live_spec();
        let argv = CodexAdapter.compile(&live, &ctx()).unwrap().args.join(" ");
        for (k, v) in bridge_env(&live, &ctx()).pairs() {
            assert!(
                argv.contains(&format!("mcp_servers.marion.env.{k}=\"{v}\"")),
                "{k}"
            );
        }
        let session = acp_adapter()
            .session_declaration(&acp_spec(), &ctx())
            .unwrap()
            .unwrap();
        let declared: Vec<(String, String)> = session["params"]["mcpServers"][0]["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().to_string(),
                    e["value"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(declared, bridge_env(&acp_spec(), &ctx()).pairs());
    }

    /// **A native node is the operator's own harness plus marion's MCP server, and nothing else**
    /// — for every row, from the row alone.
    ///
    /// The native adapter sees only a `NativeNodeContext` and answers with a prefix, an overlay
    /// and documents. This sweep holds that answer to the **managed live launch of the same row**:
    /// whatever `config_files`/`compile` put on the live route under `Auth::Inherited` is exactly
    /// what the native lane injects — same file name, same bytes, same `-c` pairs, same inline
    /// value — and *only* that. Every argv token is the carrier flag or the value it carries; no
    /// overlay key is one of the row's isolation rows; no document lands outside the node's own
    /// directory; and the declaration names marion's server and the bridge program. Then the
    /// negative: a row with no launch-time channel has no adapter, and that is ACP and only ACP.
    ///
    /// Mutation: add `--strict-mcp-config` to claude's declaration, relocate `HOME` in the
    /// overlay, spell the file name differently from `config_files`, drop a `-c` pair, or return
    /// an adapter for ACP.
    /// The native lane's answer for one row, in the strings the comparisons below read it as —
    /// the documents it wants written, its argv prefix, and its environment overlay.
    struct Injected {
        documents: Vec<crate::native::NativeDocument>,
        prefix: Vec<String>,
        overlay: Vec<(String, String)>,
    }

    /// The managed live launch of the same row: what it compiled, what it wrote, and where.
    struct Managed {
        inv: Invocation,
        files: Vec<(PathBuf, String)>,
        document_dir: PathBuf,
    }

    /// [`LiveDeclaration::ArgvDocument`]: the same file, byte for byte, named on argv the same way.
    fn assert_argv_document(
        h: Harness,
        d: &Injected,
        m: &Managed,
        flag: &str,
        file: &str,
        at: &str,
    ) {
        let path = m.document_dir.join(file);
        let body = String::from_utf8(d.documents[0].contents.clone()).unwrap();
        assert_eq!(
            m.files,
            vec![(path.clone(), body)],
            "{h}: the native document is not the live launch's"
        );
        assert_eq!(d.documents.len(), 1, "{h}");
        assert_eq!(d.documents[0].path, path, "{h}");
        assert_eq!(
            d.prefix,
            vec![flag.to_string(), format!("{at}{}", path.display())],
            "{h}"
        );
        assert!(
            d.overlay.is_empty(),
            "{h}: a document carried on argv sets no variable"
        );
        assert!(
            m.inv.args.windows(2).any(|w| w == d.prefix.as_slice()),
            "{h}: the live launch does not carry `{flag}` the same way: {:?}",
            m.inv.args
        );
    }

    /// [`LiveDeclaration::ArgvRoot`]: the same document under the same root, and the root named
    /// on argv the way the live launch names it.
    fn assert_argv_root(h: Harness, d: &Injected, m: &Managed, flag: &str, root: &str, file: &str) {
        let dir = m.document_dir.join(root);
        let path = dir.join(file);
        let body = String::from_utf8(d.documents[0].contents.clone()).unwrap();
        assert_eq!(
            m.files,
            vec![(path.clone(), body)],
            "{h}: the native document is not the live launch's"
        );
        assert_eq!(d.documents.len(), 1, "{h}");
        assert_eq!(d.documents[0].path, path, "{h}");
        assert_eq!(
            d.prefix,
            vec![flag.to_string(), dir.display().to_string()],
            "{h}"
        );
        assert!(d.overlay.iter().all(|(k, _)| k.starts_with("AGY_")), "{h}");
        assert!(
            m.inv.args.windows(2).any(|w| w == d.prefix.as_slice()),
            "{h}: the live launch does not carry `{flag}` the same way: {:?}",
            m.inv.args
        );
    }

    /// [`LiveDeclaration::EnvDocument`]: the same file, byte for byte, named by the same variable.
    fn assert_env_document(h: Harness, d: &Injected, m: &Managed, key: &str, file: &str) {
        let path = m.document_dir.join(file);
        let body = String::from_utf8(d.documents[0].contents.clone()).unwrap();
        assert_eq!(
            m.files,
            vec![(path.clone(), body)],
            "{h}: the native document is not the live launch's"
        );
        assert_eq!(d.documents.len(), 1, "{h}");
        assert_eq!(d.documents[0].path, path, "{h}");
        assert_eq!(
            d.overlay,
            vec![(key.to_string(), path.display().to_string())],
            "{h}"
        );
        assert!(
            d.prefix.is_empty(),
            "{h}: a document carried by env adds no argv"
        );
        assert!(
            m.inv.env.contains(&d.overlay[0]),
            "{h}: the live launch's env differs: {:?}",
            m.inv.env
        );
    }

    /// [`LiveDeclaration::EnvInline`]: the same inline value on the same variable, no file at all.
    fn assert_env_inline(h: Harness, d: &Injected, m: &Managed, key: &str) {
        assert!(m.files.is_empty() && d.documents.is_empty(), "{h}");
        assert!(d.prefix.is_empty(), "{h}");
        let live_value = m
            .inv
            .env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{h}: the live launch sets no ${key}"));
        assert_eq!(d.overlay, vec![(key.to_string(), live_value)], "{h}");
    }

    /// [`LiveDeclaration::ArgvPairs`]: the same `-c k=v` pairs the live launch carries, and every
    /// argv token is one of them.
    fn assert_argv_pairs(h: Harness, d: &Injected, m: &Managed, flag: &str, key: &str) {
        assert!(m.files.is_empty() && d.documents.is_empty(), "{h}");
        assert!(d.overlay.is_empty(), "{h}");
        // The managed launch's sandbox is the constraint its contract records — a managed flag,
        // which a native session (the operator's own codex) must not carry.
        let (sk, sv) = codex::live_sandbox_override();
        let sandbox = format!("{sk}={sv}");
        let live_pairs: Vec<String> = m
            .inv
            .args
            .windows(2)
            .filter(|w| w[0] == flag && w[1] != sandbox)
            .map(|w| w[1].clone())
            .collect();
        assert!(
            !d.prefix.contains(&sandbox),
            "{h}: a native session keeps the operator's sandbox"
        );
        let native_pairs: Vec<String> = d
            .prefix
            .chunks(2)
            .map(|c| {
                assert_eq!(c[0], flag, "{h}: a token that is not `{flag} k=v`");
                c[1].clone()
            })
            .collect();
        assert!(!native_pairs.is_empty(), "{h}: no pairs at all");
        assert_eq!(
            native_pairs, live_pairs,
            "{h}: the pairs are not the live launch's"
        );
        assert!(
            native_pairs.iter().filter(|p| p.starts_with(key)).count() >= 3,
            "{h}: command, args and env must all sit under `{key}`: {native_pairs:?}"
        );
    }

    /// [`LiveDeclaration::ArgvInline`]: `<flag> <token>` and nothing else, the same way the live
    /// launch carries it.
    fn assert_argv_inline(h: Harness, d: &Injected, m: &Managed, flag: &str, key: &str) {
        assert!(m.files.is_empty() && d.documents.is_empty(), "{h}");
        assert!(
            d.overlay.is_empty(),
            "{h}: a native root's bridge has no token, so nothing rides the env"
        );
        assert_eq!(d.prefix.len(), 2, "{h}: `{flag} <token>` and nothing else");
        assert_eq!(d.prefix[0], flag, "{h}");
        assert!(d.prefix[1].contains(key), "{h}: {}", d.prefix[1]);
        assert!(
            m.inv.args.windows(2).any(|w| w == d.prefix.as_slice()),
            "{h}: the live launch does not carry `{flag}` the same way: {:?}",
            m.inv.args
        );
    }

    /// **The injection is the live launch's declaration, byte for byte.** One checker per
    /// [`LiveDeclaration`] channel.
    fn assert_injection_is_the_live_declaration(
        h: Harness,
        declaration: crate::spec::LiveDeclaration,
        d: &Injected,
        m: &Managed,
    ) {
        use crate::spec::LiveDeclaration;
        match declaration {
            LiveDeclaration::ArgvDocument {
                flag,
                file,
                prefix: at,
                ..
            } => assert_argv_document(h, d, m, flag, file, at),
            LiveDeclaration::EnvDocument { key, file, .. } => {
                assert_env_document(h, d, m, key, file)
            }
            LiveDeclaration::EnvInline { key, .. } => assert_env_inline(h, d, m, key),
            LiveDeclaration::ArgvPairs { flag, key, .. } => assert_argv_pairs(h, d, m, flag, key),
            LiveDeclaration::ArgvInline { flag, key, .. } => assert_argv_inline(h, d, m, flag, key),
            LiveDeclaration::ArgvRoot {
                flag, root, file, ..
            } => assert_argv_root(h, d, m, flag, root, file),
        }
    }

    /// **Nothing managed:** no isolation row of this harness appears in the overlay, nothing
    /// reserved does either, and no document lands outside the node's own directory.
    fn assert_nothing_managed_reached_the_node(
        h: Harness,
        row: &crate::spec::HarnessSpec,
        d: &Injected,
        document_dir: &std::path::Path,
    ) {
        let isolation: Vec<&str> = row
            .env
            .iter()
            .filter(|e| e.when == crate::spec::When::Overlay)
            .map(|e| e.key)
            .collect();
        for (k, _) in &d.overlay {
            assert!(
                !isolation.contains(&k.as_str()),
                "{h}: isolation `{k}` reached a native node"
            );
            assert!(
                !k.starts_with("MARION_"),
                "{h}: reserved `{k}` in the overlay"
            );
        }
        for doc in &d.documents {
            assert!(
                doc.path.starts_with(document_dir),
                "{h}: {} escaped the node dir",
                doc.path.display()
            );
        }
    }

    /// **It declares marion's server, by name, pointing at the bridge** — wherever the channel
    /// puts it: a document, a variable, or argv.
    fn assert_it_declares_marions_server(h: Harness, ctx: &SpawnCtx, d: &Injected) {
        let body: String = d
            .documents
            .iter()
            .map(|doc| String::from_utf8_lossy(&doc.contents).into_owned())
            .chain(d.overlay.iter().map(|(_, v)| v.clone()))
            .chain(d.prefix.iter().cloned())
            .collect();
        assert!(
            body.contains(crate::spec::MCP_ALIAS),
            "{h}: no `marion` server in {body}"
        );
        assert!(
            body.contains(&ctx.bridge.to_string_lossy().into_owned()),
            "{h}: the bridge program is not named"
        );
        assert!(
            body.contains(&ctx.agent_id.0),
            "{h}: the node's identity is not on the declaration"
        );
    }

    #[test]
    fn every_native_row_injects_only_marions_mcp_server_and_no_managed_flags() {
        use std::ffi::OsString;

        use crate::native::{NativeEnvironmentView, NativeNodeContext, native_adapter};
        use crate::spec::McpRoute;

        let document_dir = PathBuf::from("/state/agents/019f-root");
        let operator_env = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from("HOME"), OsString::from("/home/operator")),
        ];
        // One context for both sides, so the comparison is of the *channel* and not of the node's
        // identity: whatever `BridgeEnv` says, the native lane and the managed launch say it the
        // same way. (The factory's own bridge for a native root — no readiness file, no minted
        // token — is pinned in `marion-supervisor`'s factory tests.)
        let ctx = ctx();
        let mut covered = Vec::new();
        for h in Harness::ALL {
            let row = harness_spec(h);
            let Some(adapter) = native_adapter(h) else {
                assert!(
                    row.live_declaration.is_none(),
                    "{h}: the row states a launch-time declaration channel and has no native adapter"
                );
                assert!(
                    matches!(row.mcp.live, McpRoute::Session(_)),
                    "{h}: a row without a launch-time channel must declare over the session"
                );
                continue;
            };
            let declaration = row
                .live_declaration
                .unwrap_or_else(|| panic!("{h}: an adapter without a declaration"));
            assert_eq!(
                declaration.route(),
                row.mcp.live,
                "{h}: the row disagrees with itself"
            );
            covered.push(h);

            let live = LaunchSpec {
                auth: Auth::Inherited,
                base_url: None,
                api_key: None,
                model: match h {
                    Harness::OpenCode => Some("anthropic/claude-sonnet-4-5".into()),
                    _ => spec_for(h).model,
                },
                config_dir: document_dir.clone(),
                ..spec_for(h)
            };
            let bridge = bridge_env(&live, &ctx);
            let injection = adapter
                .prepare_native(&NativeNodeContext {
                    bridge: &bridge,
                    document_dir: &document_dir,
                    allowed_marion_tools: &["spawn", "wait", "status"],
                    environment: NativeEnvironmentView::validate(&operator_env).unwrap(),
                })
                .unwrap_or_else(|e| panic!("{h}: {e}"));

            let managed = launch_adapter(h).unwrap();
            // The TUI shape where the row has one: that is the launch a native node is a peer of.
            let inv = match row.pane {
                Some(_) => managed.compile_pane(&live, &ctx),
                None => managed.compile(&live, &ctx),
            }
            .unwrap_or_else(|e| panic!("{h}: {e}"));
            let managed = Managed {
                inv,
                files: managed.config_files(&live, &ctx).unwrap(),
                document_dir: document_dir.clone(),
            };
            let lossy = |v: &OsString| v.to_string_lossy().into_owned();
            let mut overlay: Vec<(String, String)> = injection
                .env_overlay
                .iter()
                .map(|(k, v)| (lossy(k), lossy(v)))
                .collect();
            // The row's no-self-update switch is the one variable beside the declaration a native
            // node carries (pinned per row by
            // `every_row_states_its_update_policy_and_renders_it_into_every_launch_shape`); what
            // follows compares the declaration channel alone.
            if let Some(hygiene) = row.updates.env() {
                let before = overlay.len();
                overlay.retain(|e| *e != hygiene);
                assert_eq!(
                    overlay.len() + 1,
                    before,
                    "{h}: the switch is set exactly once"
                );
            }
            // The row's completion-push argv leads the prefix where the row has one (pinned per
            // row by `every_row_states_its_push_strategy_and_renders_its_argv_only_into_
            // interactive_shapes`); what follows compares the declaration channel alone.
            let mut prefix: Vec<String> = injection.argv_prefix.iter().map(lossy).collect();
            let push: Vec<String> = row.push.argv().iter().map(|s| s.to_string()).collect();
            assert!(
                prefix.starts_with(&push),
                "{h}: the push argv leads the native prefix, so the declaration flag closes it"
            );
            prefix.drain(..push.len());
            let injected = Injected {
                prefix,
                overlay,
                documents: injection.documents,
            };

            // 1. The injection is the live launch's declaration, byte for byte.
            assert_injection_is_the_live_declaration(h, declaration, &injected, &managed);
            // 2. Nothing managed reached the node.
            assert_nothing_managed_reached_the_node(h, row, &injected, &document_dir);
            // 3. It declares marion's server, by name, pointing at the bridge.
            assert_it_declares_marions_server(h, &ctx, &injected);
        }
        let mut expected: Vec<Harness> = Harness::ALL
            .into_iter()
            .filter(|h| *h != Harness::Acp)
            .collect();
        expected.sort();
        covered.sort();
        assert_eq!(
            covered, expected,
            "every harness but ACP has a native adapter"
        );
    }

    /// An environment variable a live node must never be handed: a credential, an endpoint, a
    /// provider or auth-route selection, or a relocation of the directory a login lives in.
    /// marion's own `MARION_*` bridge variables are none of these.
    fn is_credential_or_relocation(key: &str) -> bool {
        const PARTS: &[&str] = &[
            "KEY",
            "TOKEN",
            "AUTH",
            "SECRET",
            "PASSWORD",
            "CREDENTIAL",
            "BASE_URL",
            "HOST",
            "PROVIDER",
            "HOME",
            "STORAGE",
            "OFFLINE",
            "VERTEX",
            "USE_GCA",
        ];
        !key.starts_with("MARION_")
            && (key.starts_with("XDG_")
                || key.ends_with("_DIR")
                || PARTS.iter().any(|p| key.contains(p)))
    }

    /// Switches that keep the operator's own configuration — and so, possibly, their login — out
    /// of a node. Canned-only on every row that has one.
    const CONFIG_EXCLUDING_FLAGS: &[&str] = &[
        "--setting-sources",
        "--settings",
        "--pure",
        "--config",
        "--data-dir",
    ];

    /// The one value a live launch may pass a [`CONFIG_EXCLUDING_FLAGS`] switch, allowed
    /// deliberately: claude's `--settings` *merges* over the operator's layers, and this overlay
    /// adds only `disableAllHooks`. Turning hooks off hides no credential — measured on 2.1.283,
    /// a settings `env` key and an `apiKeyHelper` both still reached the endpoint under it.
    /// Spelled out rather than borrowed from the row, so widening the row's overlay fails here.
    const HOOKS_ONLY_OVERLAY: (&str, &str) = ("--settings", r#"{"disableAllHooks":true}"#);

    /// Keys that select an auth route, a provider or a credential, in any declaration document or
    /// inline override marion writes.
    const AUTH_SELECTING_KEYS: &[&str] = &[
        "selectedType",
        "enforcedType",
        "apiKey",
        "api_key",
        "env_key",
        "model_provider",
        "preferred_auth_method",
        "forced_login_method",
        "baseURL",
        "base_url",
    ];

    fn assert_leaves_the_auth_source_alone(who: &str, args: &[String], env: &[(String, String)]) {
        for (k, v) in env {
            assert!(
                !is_credential_or_relocation(k),
                "{who}: a live launch set `{k}`, which selects or hides a credential: {env:?}"
            );
            assert!(
                !v.is_empty(),
                "{who}: a live launch blanked `{k}`, which scrubs whatever the operator set: {env:?}"
            );
        }
        for flag in CONFIG_EXCLUDING_FLAGS {
            let excluding = |(i, a): (usize, &String)| {
                a == flag
                    && (*flag, args.get(i + 1).map(String::as_str))
                        != (HOOKS_ONLY_OVERLAY.0, Some(HOOKS_ONLY_OVERLAY.1))
            };
            assert!(
                !args.iter().enumerate().any(excluding),
                "{who}: a live launch carries {flag}, which hides the operator's own \
                 configuration: {args:?}"
            );
        }
        let blob = args.join(" ");
        for key in AUTH_SELECTING_KEYS {
            assert!(
                !blob.contains(key),
                "{who}: a live launch's argv names `{key}`: {args:?}"
            );
        }
    }

    fn assert_selects_no_auth(who: &str, document: &str) {
        for key in AUTH_SELECTING_KEYS {
            assert!(
                !document.contains(key),
                "{who}: a live declaration names `{key}`:\n{document}"
            );
        }
    }

    /// **Live mode writes no auth selection and removes no credential source — every row, every
    /// shape it renders, and the native lane.** Under [`Auth::Inherited`] a node runs on whatever
    /// the operator already configured for that harness (OAuth, an API-key variable, the keychain,
    /// a config file), so nothing marion hands it may set, blank or relocate a credential, name a
    /// provider or auth type, or switch off a configuration layer a login can live in.
    ///
    /// The spec it launches from still *carries* a base URL and a key, as a caller might hand
    /// one: live mode must drop them, not merely never have been given them.
    ///
    /// Mutation: make any `HOME`/`XDG_*`/`CODEX_HOME`/`*_API_KEY` row `When::Always`, restore
    /// gemini's `selectedType` fallback, blank `ANTHROPIC_API_KEY` beside no token, or turn
    /// claude's `--setting-sources` / opencode's `--pure` back into `Arg::Lit`.
    #[test]
    fn no_live_launch_selects_an_auth_route_or_hides_a_credential_source() {
        use std::ffi::OsString;

        use crate::native::{NativeEnvironmentView, NativeNodeContext, native_adapter};

        let ctx = ctx();
        for h in Harness::ALL {
            let row = harness_spec(h);
            // A row with no canned route (agy) carries no endpoint of its own; it is handed the
            // codex spec's, so every row is seen dropping one.
            let live = LaunchSpec {
                auth: Auth::Inherited,
                model: match h {
                    Harness::OpenCode => Some("anthropic/claude-sonnet-4-5".into()),
                    _ => spec_for(h).model,
                },
                base_url: spec_for(h).base_url.or(codex_spec().base_url),
                api_key: spec_for(h).api_key.or(codex_spec().api_key),
                ..spec_for(h)
            };
            assert!(
                live.base_url.is_some(),
                "{h}: the sweep hands every row an endpoint so live mode is seen dropping it"
            );
            let adapter = launch_adapter(h).unwrap();
            let mut shapes = vec![("headless", adapter.compile(&live, &ctx))];
            if row.pane.is_some() {
                shapes.push(("pane", adapter.compile_pane(&live, &ctx)));
            }
            for (shape, inv) in shapes {
                let inv = inv.unwrap_or_else(|e| panic!("{h} {shape}: {e}"));
                assert_leaves_the_auth_source_alone(&format!("{h} {shape}"), &inv.args, &inv.env);
            }
            for (path, document) in adapter.config_files(&live, &ctx).unwrap() {
                assert_selects_no_auth(&format!("{h} {}", path.display()), &document);
            }
            if let Some(session) = adapter.session_declaration(&live, &ctx).unwrap() {
                assert_selects_no_auth(&format!("{h} session/new"), &session.to_string());
            }

            let Some(native) = native_adapter(h) else {
                continue;
            };
            let operator_env = vec![(OsString::from("PATH"), OsString::from("/usr/bin"))];
            let document_dir = PathBuf::from("/state/agents/019f-root");
            let injection = native
                .prepare_native(&NativeNodeContext {
                    bridge: &bridge_env(&live, &ctx),
                    document_dir: &document_dir,
                    allowed_marion_tools: &["spawn", "wait", "status"],
                    environment: NativeEnvironmentView::validate(&operator_env).unwrap(),
                })
                .unwrap_or_else(|e| panic!("{h} native: {e}"));
            let lossy = |v: &OsString| v.to_string_lossy().into_owned();
            let overlay: Vec<(String, String)> = injection
                .env_overlay
                .iter()
                .map(|(k, v)| (lossy(k), lossy(v)))
                .collect();
            let prefix: Vec<String> = injection.argv_prefix.iter().map(lossy).collect();
            assert_leaves_the_auth_source_alone(&format!("{h} native"), &prefix, &overlay);
            for doc in &injection.documents {
                assert_selects_no_auth(
                    &format!("{h} native {}", doc.path.display()),
                    &String::from_utf8_lossy(&doc.contents),
                );
            }
        }
    }

    /// The sweep's filter is not vacuous: every credential and relocation variable a **canned**
    /// row sets is one it recognises, and the flags it forbids live are ones canned rows carry.
    #[test]
    fn the_live_auth_sweep_recognises_every_canned_credential_and_relocation() {
        for key in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CODEX_HOME",
            "GEMINI_CLI_HOME",
            "GEMINI_FORCE_FILE_STORAGE",
            "GOOGLE_GEMINI_BASE_URL",
            "GEMINI_API_KEY",
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "COPILOT_HOME",
            "COPILOT_PROVIDER_BASE_URL",
            "COPILOT_PROVIDER_TYPE",
            "COPILOT_PROVIDER_API_KEY",
            "COPILOT_OFFLINE",
            "GOOSE_PROVIDER",
            "OPENAI_HOST",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "CLINE_DIR",
            "CLINE_DATA_DIR",
            "QWEN_HOME",
            "CLAUDE_CONFIG_DIR",
            "GOOGLE_GENAI_USE_VERTEXAI",
        ] {
            assert!(is_credential_or_relocation(key), "{key}");
        }
        for key in [
            "MARION_NODE_TOKEN",
            "MARION_AUTH",
            "DISABLE_AUTOUPDATER",
            "OPENCODE_DISABLE_AUTOUPDATE",
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH",
            "CLINE_MCP_SETTINGS_PATH",
            "OPENCODE_CONFIG_CONTENT",
            "GOOSE_MODEL",
            "OPENAI_MODEL",
            "PWD",
        ] {
            assert!(!is_credential_or_relocation(key), "{key}");
        }
        let canned: Vec<String> = [Harness::ClaudeCode, Harness::OpenCode, Harness::Cline]
            .into_iter()
            .flat_map(|h| {
                launch_adapter(h)
                    .unwrap()
                    .compile(&spec_for(h), &ctx())
                    .unwrap()
                    .args
            })
            .collect();
        for flag in ["--setting-sources", "--pure", "--config", "--data-dir"] {
            assert!(canned.iter().any(|a| a == flag), "{flag} in {canned:?}");
        }
    }

    /// **Every row states how its harness is kept from updating itself under marion, and every
    /// launch shape carries that switch.** A harness binary that self-updates mid-run (opencode
    /// 1.17.3 printed `Updating to v1.18.29...` on launch; codex 0.147.0's TUI offered
    /// `Update available -> 0.153.4` and an Enter installed it; claude updates in the background)
    /// changes the program under a running node and interrupts a native facade session, so the
    /// switch is row **data** ([`spec::UpdatePolicy`]), measured on the installed binary, and
    /// rendered by the one renderer into the headless shape, the pane shape and the native
    /// overlay alike — never a per-harness branch, never a duplicate `Env` row beside it.
    ///
    /// A row with no measured switch says so **explicitly**: `UpdatePolicy::None` with a note
    /// naming what was searched. Silence is the one answer the sweep refuses.
    /// One launch the row rendered: its name, the invocation, the documents beside it.
    type Launch = (String, Invocation, Vec<(PathBuf, String)>);

    /// One `OsString` as the comparisons below read it.
    fn lossy(v: &std::ffi::OsString) -> String {
        v.to_string_lossy().into_owned()
    }

    /// Every launch this row can render, under both auth modes: headless, and the pane shape
    /// where the row has one.
    fn rendered_launches(h: Harness, document_dir: &std::path::Path) -> Vec<Launch> {
        let row = harness_spec(h);
        let a = launch_adapter(h).unwrap();
        let mut launches: Vec<Launch> = Vec::new();
        for auth in [Auth::Canned, Auth::Inherited] {
            // agy has no canned route, and refuses one by name
            // (`agy_refuses_a_canned_launch_by_name`): there is no canned launch to render.
            if h == Harness::Antigravity && auth == Auth::Canned {
                continue;
            }
            let launch = LaunchSpec {
                auth,
                model: match h {
                    Harness::OpenCode => Some("anthropic/claude-sonnet-4-5".into()),
                    _ => spec_for(h).model,
                },
                config_dir: document_dir.to_path_buf(),
                ..spec_for(h)
            };
            let files = a.config_files(&launch, &ctx()).unwrap();
            launches.push((
                format!("{auth:?} headless"),
                a.compile(&launch, &ctx())
                    .unwrap_or_else(|e| panic!("{h}: {e}")),
                files.clone(),
            ));
            if row.pane.is_some() {
                launches.push((
                    format!("{auth:?} pane"),
                    a.compile_pane(&launch, &ctx())
                        .unwrap_or_else(|e| panic!("{h}: {e}")),
                    files,
                ));
            }
        }
        launches
    }

    /// The native injection, where the row has a native lane.
    fn native_injection(
        h: Harness,
        document_dir: &std::path::Path,
    ) -> Option<crate::native::NativeInjection> {
        use crate::native::{NativeEnvironmentView, NativeNodeContext, native_adapter};

        let live = LaunchSpec {
            auth: Auth::Inherited,
            config_dir: document_dir.to_path_buf(),
            ..spec_for(h)
        };
        let bridge = bridge_env(&live, &ctx());
        let operator = vec![(
            std::ffi::OsString::from("PATH"),
            std::ffi::OsString::from("/usr/bin"),
        )];
        native_adapter(h).map(|adapter| {
            adapter
                .prepare_native(&NativeNodeContext {
                    bridge: &bridge,
                    document_dir,
                    allowed_marion_tools: &["spawn", "wait", "status"],
                    environment: NativeEnvironmentView::validate(&operator).unwrap(),
                })
                .unwrap_or_else(|e| panic!("{h}: {e}"))
        })
    }

    /// [`UpdatePolicy::Env`]: the variable is set on every shape's env and on the native overlay,
    /// and it is **not** also an ordinary `Env` row, where it would be set twice.
    fn assert_update_env(
        h: Harness,
        launches: &[Launch],
        native: Option<&crate::native::NativeInjection>,
        key: &str,
        value: &str,
    ) {
        assert!(
            harness_spec(h).env.iter().all(|e| e.key != key),
            "{h}: `{key}` is the update policy and must not also be an `Env` row"
        );
        let pair = (key.to_string(), value.to_string());
        for (shape, inv, _) in launches {
            assert!(
                inv.env.contains(&pair),
                "{h} {shape}: no `{key}={value}` in {:?}",
                inv.env
            );
        }
        if let Some(native) = native {
            let overlay: Vec<(String, String)> = native
                .env_overlay
                .iter()
                .map(|(k, v)| (lossy(k), lossy(v)))
                .collect();
            assert!(
                overlay.contains(&pair),
                "{h} native: no `{key}={value}` in the overlay {overlay:?}"
            );
        }
    }

    /// [`UpdatePolicy::Pair`]: the `key=value` token rides every shape's argv and the native
    /// prefix.
    fn assert_update_pair(
        h: Harness,
        launches: &[Launch],
        native: Option<&crate::native::NativeInjection>,
        key: &str,
        value: &str,
    ) {
        let token = format!("{key}={value}");
        for (shape, inv, _) in launches {
            assert!(
                inv.args.contains(&token),
                "{h} {shape}: no `{token}` on argv {:?}",
                inv.args
            );
        }
        if let Some(native) = native {
            let prefix: Vec<String> = native.argv_prefix.iter().map(lossy).collect();
            assert!(
                prefix.contains(&token),
                "{h} native: no `{token}` in the prefix {prefix:?}"
            );
        }
    }

    /// [`UpdatePolicy::Document`]: every shape writes a document carrying the keys, and so does
    /// the native injection.
    fn assert_update_document(
        h: Harness,
        launches: &[Launch],
        native: Option<&crate::native::NativeInjection>,
        keys: &[(&str, bool)],
    ) {
        /// The value at a dotted path of a JSON document, if the document is one.
        fn at<'a>(doc: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
            key.split('.').try_fold(doc, |v, k| v.get(k))
        }
        assert!(!keys.is_empty(), "{h}: a document policy with no keys");
        let carries = |text: &str| -> bool {
            let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
                return false;
            };
            keys.iter()
                .all(|(k, v)| at(&doc, k) == Some(&serde_json::Value::Bool(*v)))
        };
        for (shape, _, files) in launches {
            assert!(
                files.iter().any(|(_, body)| carries(body)),
                "{h} {shape}: no document carries {keys:?}: {files:?}"
            );
        }
        if let Some(native) = native {
            assert!(
                native
                    .documents
                    .iter()
                    .any(|d| carries(std::str::from_utf8(&d.contents).unwrap())),
                "{h} native: no injected document carries {keys:?}"
            );
        }
    }

    /// [`UpdatePolicy::None`]: the row does not know a switch — so it must also not smuggle one in
    /// as an ordinary `Env` row, where nothing would check it against the binary, and its note
    /// must say what was searched and not found.
    fn assert_update_none(h: Harness, note: &str) {
        assert!(
            !harness_spec(h)
                .env
                .iter()
                .any(|e| e.key.to_ascii_uppercase().contains("UPDATE")),
            "{h}: an update-shaped `Env` row on a row that states no update policy"
        );
        assert!(
            note.contains("no") || note.contains("never"),
            "{h}: `None` must say what was searched and not found: {note}"
        );
    }

    #[test]
    fn every_row_states_its_update_policy_and_renders_it_into_every_launch_shape() {
        use crate::spec::UpdatePolicy;

        let document_dir = PathBuf::from("/state/agents/019f-root");
        for h in Harness::ALL {
            let updates = harness_spec(h).updates;
            let note = match updates {
                UpdatePolicy::Env { note, .. }
                | UpdatePolicy::Pair { note, .. }
                | UpdatePolicy::Document { note, .. }
                | UpdatePolicy::None { note } => note,
            };
            assert!(
                !note.trim().is_empty(),
                "{h}: an update policy without the measurement behind it"
            );
            let launches = rendered_launches(h, &document_dir);
            let native = native_injection(h, &document_dir);
            let native = native.as_ref();
            match updates {
                UpdatePolicy::Env { key, value, .. } => {
                    assert_update_env(h, &launches, native, key, value)
                }
                UpdatePolicy::Pair { key, value, .. } => {
                    assert_update_pair(h, &launches, native, key, value)
                }
                UpdatePolicy::Document { keys, .. } => {
                    assert_update_document(h, &launches, native, keys)
                }
                UpdatePolicy::None { note } => assert_update_none(h, note),
            }
        }
    }

    /// **Every row states how a backgrounded child's end is pushed to its parent, and the argv
    /// that enables it reaches only the shapes where it was measured to work.** The one measured
    /// strategy is Claude Code's `notifications/claude/channel` (2.1.268), and it has three
    /// conditions: interactive mode, the `--dangerously-load-development-channels server:marion`
    /// flag, and never `-p` — headless never enqueues the event at all, and the flag is variadic,
    /// so on a headless launch it would buy nothing and swallow a trailing positional. So the
    /// flag is row **data** ([`spec::Push`]) rendered by the one renderer into the pane shape and
    /// the native prefix, **first**, so the row's own next flag closes the variadic; the headless
    /// shape never carries it; and a row that pushes over the channel names the `clientInfo.name`
    /// a bridge started by that harness itself will see, or the bridge could not tell.
    ///
    /// A row with no measurement says [`spec::Push::McpLog`] — MCP's own `notifications/message`,
    /// which every client may ignore — and ACP, which has no launch-time channel at all, says
    /// [`spec::Push::None`]. Silence is the one answer the sweep refuses.
    #[test]
    fn every_row_states_its_push_strategy_and_renders_its_argv_only_into_interactive_shapes() {
        use crate::spec::{MCP_ALIAS, Push};

        assert_eq!(
            Push::ClaudeChannel.argv(),
            ["--dangerously-load-development-channels", "server:marion"],
            "the measured flag, byte for byte"
        );
        assert_eq!(
            Push::ClaudeChannel.argv()[1],
            format!("server:{MCP_ALIAS}"),
            "the channel is keyed by the server name every declaration uses"
        );
        assert!(Push::McpLog.argv().is_empty() && Push::None.argv().is_empty());
        assert_eq!(harness_spec(Harness::ClaudeCode).push, Push::ClaudeChannel);
        assert_eq!(harness_spec(Harness::Acp).push, Push::None);

        let document_dir = PathBuf::from("/state/agents/019f-root");
        for h in Harness::ALL {
            let row = harness_spec(h);
            let tokens: Vec<String> = row.push.argv().iter().map(|s| s.to_string()).collect();
            if row.push == Push::ClaudeChannel {
                assert!(
                    row.client_name.is_some(),
                    "{h}: a row that pushes over the channel must name the client it serves"
                );
            }
            for (name, inv, _) in rendered_launches(h, &document_dir) {
                let carries = !tokens.is_empty() && inv.args.starts_with(&tokens);
                let interactive = name.ends_with("pane");
                assert_eq!(
                    carries,
                    interactive && !tokens.is_empty(),
                    "{h} ({name}): the push argv belongs to the interactive shape alone: {:?}",
                    inv.args
                );
                assert!(
                    !inv.args.iter().any(|a| a.contains("development-channels")) || carries,
                    "{h} ({name}): the flag reached a shape by another route: {:?}",
                    inv.args
                );
            }
            if let Some(native) = native_injection(h, &document_dir) {
                let prefix: Vec<String> = native.argv_prefix.iter().map(lossy).collect();
                assert_eq!(
                    prefix.starts_with(&tokens) && !tokens.is_empty(),
                    !tokens.is_empty(),
                    "{h}: the native prefix carries the push argv iff the row has one: {prefix:?}"
                );
            }
        }
    }

    /// A [`spec::TurnDelivery`]'s variant, as a word, so a truth table reads as one.
    fn delivery_kind(d: crate::spec::TurnDelivery) -> &'static str {
        use crate::spec::TurnDelivery;
        match d {
            TurnDelivery::TypedTurn { .. } => "typed",
            TurnDelivery::Continuation { .. } => "continuation",
            TurnDelivery::McpChannel { .. } => "channel",
            TurnDelivery::TerminalPaste { .. } => "paste",
            TurnDelivery::None { .. } => "none",
        }
    }

    /// **How a message reaches each harness's next turn, per shape — the S31 truth table.**
    ///
    /// One resolver (`spec::delivery_for`) reads the row; this pins what every row says, so a row
    /// that changes its strategy has to change this table too, beside the fixture that justifies
    /// it (`tests/fixtures/s31-turn-delivery/`). The paste rows are pinned to the measured values:
    /// bracketed paste, then `\r` after 50 ms (0 ms measured on all four TUIs, 50 for margin), with
    /// 1500 ms of output quiet as the idle signal (busy spinners repaint at ≤ 454 ms).
    #[test]
    fn every_row_resolves_one_turn_delivery_per_shape_as_s31_measured() {
        use crate::spec::{BootSignal, IdleSignal, NodeShape, TurnDelivery, delivery_for};
        let table = [
            (Harness::ClaudeCode, "typed", "channel"),
            (Harness::Codex, "continuation", "paste"),
            (Harness::Gemini, "none", "none"),
            (Harness::OpenCode, "continuation", "paste"),
            (Harness::Copilot, "continuation", "paste"),
            (Harness::Goose, "none", "none"),
            (Harness::Cline, "none", "none"),
            (Harness::Qwen, "continuation", "none"),
            // s32: `--conversation <id>`, and a bracketed paste on the (dark) native lane.
            (Harness::Antigravity, "continuation", "paste"),
            // S34 item 12: `--mode rpc` folds a steer into the running turn.
            (Harness::Pi, "typed", "paste"),
            (Harness::Acp, "typed", "none"),
        ];
        assert_eq!(
            table.len(),
            Harness::ALL.len(),
            "every harness has a row here"
        );
        // Each paste row's boot mark. Only a TUI measured drawing a provisional composer (codex
        // 0.155.1's startup draft) waits for its window title.
        let boots = [
            (Harness::Codex, BootSignal::WindowTitle),
            (Harness::OpenCode, BootSignal::FirstDraw),
            (Harness::Copilot, BootSignal::FirstDraw),
            (Harness::Antigravity, BootSignal::FirstDraw),
            (Harness::Pi, BootSignal::FirstDraw),
        ];
        for (h, headless, interactive) in table {
            let row = harness_spec(h);
            assert_eq!(
                delivery_kind(delivery_for(row, NodeShape::Headless)),
                headless,
                "{h} headless"
            );
            assert_eq!(
                delivery_kind(delivery_for(row, NodeShape::Interactive)),
                interactive,
                "{h} interactive"
            );
            assert_eq!(
                delivery_for(row, NodeShape::Headless),
                row.delivery.headless
            );
            assert_eq!(
                delivery_for(row, NodeShape::Interactive),
                row.delivery.interactive
            );
            if let TurnDelivery::TerminalPaste {
                idle,
                boot,
                submit,
                submit_delay_ms,
                ..
            } = row.delivery.interactive
            {
                let measured = boots.iter().find(|(b, _)| *b == h).map(|(_, m)| *m);
                assert_eq!(
                    Some(boot),
                    measured,
                    "{h}: the boot mark its measurement names"
                );
                assert_eq!(idle, IdleSignal::OutputQuiet { ms: 1500 }, "{h}");
                assert_eq!(submit, b"\r", "{h}: CR after the bracketed paste");
                assert_eq!(submit_delay_ms, 50, "{h}");
            }
        }
        let TurnDelivery::McpChannel { note } =
            harness_spec(Harness::ClaudeCode).delivery.interactive
        else {
            unreachable!("pinned above")
        };
        assert!(
            note.contains("claude.ai") && note.contains("API"),
            "the channel's auth limit is stated where the strategy is: {note}"
        );
    }

    /// **Every row's turn delivery is one its surfaces can carry, and says what measured it.**
    ///
    /// The same shape of sweep as the push one above: a strategy is row data, and each variant has
    /// exactly one precondition on the rest of the row, so a row cannot claim a channel it has no
    /// way to take:
    ///
    /// * `TypedTurn` needs a typed control channel (`Surfaces::Headless(_)` or a row's JSONL
    ///   channel), and is headless only.
    /// * `Continuation` needs a launch-only row with a resume grammar in its headless argv — the
    ///   next turn is a relaunch of the same session.
    /// * `McpChannel` needs the row to push over Claude Code's channel, and is interactive only
    ///   (headless `-p` never enqueues a channel event).
    /// * `TerminalPaste` needs a terminal marion can type into — a pane shape or a native lane —
    ///   and is interactive only, with a non-empty submit sequence.
    /// * Every note is non-empty, `None` included: an absence says what was searched.
    #[test]
    fn every_row_states_a_turn_delivery_its_surfaces_can_carry() {
        use crate::spec::{Arg, NodeShape, Push, Surfaces, TurnDelivery, delivery_for};
        for h in Harness::ALL {
            let row = harness_spec(h);
            for shape in [NodeShape::Headless, NodeShape::Interactive] {
                let d = delivery_for(row, shape);
                assert!(
                    !d.note().trim().is_empty(),
                    "{h} {shape:?}: a turn delivery without the measurement behind it"
                );
                let headless = shape == NodeShape::Headless;
                match d {
                    TurnDelivery::TypedTurn { .. } => assert!(
                        headless
                            && matches!(
                                row.surfaces,
                                Surfaces::Headless(_) | Surfaces::JsonlRpc(_)
                            ),
                        "{h} {shape:?}: a typed turn needs a typed control channel"
                    ),
                    TurnDelivery::Continuation { .. } => assert!(
                        headless
                            && row.surfaces == Surfaces::LaunchOnly
                            && row.resume.is_some()
                            && row.argv.contains(&Arg::Resume),
                        "{h} {shape:?}: a continuation is a headless relaunch by resume"
                    ),
                    TurnDelivery::McpChannel { .. } => assert!(
                        !headless && row.push == Push::ClaudeChannel,
                        "{h} {shape:?}: the channel is Claude Code's, and interactive only"
                    ),
                    TurnDelivery::TerminalPaste { submit, .. } => {
                        assert!(
                            !headless && (row.pane.is_some() || row.live_declaration.is_some()),
                            "{h} {shape:?}: a paste needs a terminal marion can type into"
                        );
                        assert!(!submit.is_empty(), "{h}: a paste that never submits");
                    }
                    TurnDelivery::None { .. } => {}
                }
            }
        }
    }

    /// **Every row states the dialogs its TUI shows before the composer**, with the measurement —
    /// an empty list included, whose note says what was searched. A needle is written the way the
    /// pty host reads a screen (one space between words, nothing leading or trailing), or it could
    /// never match; answer keys are never empty; and a row that answers a dialog also names the
    /// dialog marker-free as `Hold`, so a release that moves the selection is held, not answered
    /// with keys measured for another selection. Whether each needle is on its measured screen is
    /// the supervisor's test (`pty::input`), which owns the reader.
    #[test]
    fn every_row_states_its_boot_dialogs_as_the_pty_host_reads_them() {
        use crate::spec::DialogAnswer;
        for h in Harness::ALL {
            let row = harness_spec(h).boot_dialogs;
            assert!(
                !row.note.trim().is_empty(),
                "{h}: boot dialogs without the measurement behind them"
            );
            for d in row.dialogs {
                assert!(!d.note.trim().is_empty(), "{h}: {:?} has no note", d.needle);
                let normal = d.needle.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(
                    !d.needle.is_empty() && d.needle == normal,
                    "{h}: needle {:?} is not written as the host reads a screen",
                    d.needle
                );
                if let DialogAnswer::Keys(keys) = d.answer {
                    assert!(!keys.is_empty(), "{h}: {:?} answers with nothing", d.needle);
                }
            }
            if row
                .dialogs
                .iter()
                .any(|d| matches!(d.answer, DialogAnswer::Keys(_)))
            {
                assert!(
                    row.dialogs
                        .last()
                        .is_some_and(|d| d.answer == DialogAnswer::Hold),
                    "{h}: an answered dialog needs a held, marker-free needle after it"
                );
            }
        }
    }

    /// **A JSONL command channel is selected only with its vocabulary.** `TypedKind::JsonlRpc` is
    /// reachable from a row only through `Surfaces::JsonlRpc(&channel)`; a bare
    /// `Surfaces::Headless(TypedKind::JsonlRpc)` would route a node to a driver with nothing to
    /// write. A row that drives a channel delivers its headless turns on it, states a note, and
    /// takes its prompt as a command rather than argv.
    #[test]
    fn a_row_drives_a_jsonl_channel_only_through_its_stated_vocabulary() {
        use crate::spec::{Arg, Field, NodeShape, Surfaces, TurnDelivery, delivery_for};
        for h in Harness::ALL {
            let row = harness_spec(h);
            assert_ne!(
                row.surfaces,
                Surfaces::Headless(TypedKind::JsonlRpc),
                "{h}: a JSONL channel with no vocabulary"
            );
            let Some(channel) = row.surfaces.channel() else {
                continue;
            };
            assert!(
                !channel.note.trim().is_empty(),
                "{h}: a channel with no measurement"
            );
            assert!(
                matches!(
                    delivery_for(row, NodeShape::Headless),
                    TurnDelivery::TypedTurn { .. }
                ),
                "{h}: a node with a channel takes its later turns on it"
            );
            assert!(
                !row.argv.contains(&Arg::Pos(Field::Prompt)),
                "{h}: the prompt is the channel's first command, never argv"
            );
            assert!(
                channel.prompt.text.is_some() && channel.steer.text.is_some(),
                "{h}"
            );
        }
    }

    /// **A row that answers a boot dialog says where the answer is kept, and the claim holds.**
    /// `CannedHome`: the canned pane carries the row's profile variable at marion's config dir, so
    /// the answer lands there. `OperatorConfig`: the canned pane does not (which is why a test must
    /// relocate), and the relocation moves the very directory the profile carrier names and sets
    /// a variable that carrier clears as an override of its login lookup — two measurements of one
    /// fact that must agree. A row that answers nothing claims nothing.
    ///
    /// Mutation: mark claude `CannedHome`, or codex `OperatorConfig`, or rename the relocation's
    /// `store`. Each fails.
    #[test]
    fn a_row_that_answers_a_boot_dialog_says_where_the_answer_is_kept() {
        use crate::spec::{DialogAnswer, Remembers};
        for h in Harness::ALL {
            let row = harness_spec(h);
            let answers = row
                .boot_dialogs
                .dialogs
                .iter()
                .any(|d| matches!(d.answer, DialogAnswer::Keys(_)));
            let remembers = row.boot_dialogs.remembers;
            if !answers {
                assert_eq!(remembers, Remembers::Nothing, "{h}: answers no dialog");
                continue;
            }
            let pane_env = || {
                let spec = spec_for(h);
                let inv = launch_adapter(h)
                    .unwrap()
                    .compile_pane(&spec, &ctx())
                    .expect("a row that answers a boot dialog has a pane");
                (inv.env, spec.config_dir)
            };
            match remembers {
                Remembers::Nothing => {}
                Remembers::CannedHome => {
                    let carrier = row.profile.expect("a relocated home is a profile variable");
                    let (env, config_dir) = pane_env();
                    assert!(
                        env.iter().any(|(k, v)| k == carrier.env
                            && std::path::Path::new(v).starts_with(&config_dir)),
                        "{h}: the canned pane leaves {} the operator's: {env:?}",
                        carrier.env
                    );
                }
                Remembers::OperatorConfig(r) => {
                    let carrier = row
                        .profile
                        .expect("a relocated config is a profile variable");
                    assert_eq!(r.config, carrier.env, "{h}");
                    assert!(carrier.clear.contains(&r.store), "{h}: {}", r.store);
                    let (env, _) = pane_env();
                    assert!(
                        !env.iter().any(|(k, _)| k == r.config),
                        "{h}: the canned pane relocates {} already; say CannedHome",
                        r.config
                    );
                    for (name, body) in r.seed {
                        let doc: serde_json::Value = serde_json::from_str(body)
                            .unwrap_or_else(|e| panic!("{h}: seed {name} is not JSON: {e}"));
                        assert!(
                            doc.is_object()
                                && !crate::spec::names_a_credential(name)
                                && !crate::spec::holds_a_credential(&doc),
                            "{h}: seed {name} is not a credential-free JSON object"
                        );
                    }
                    for key in r.carry {
                        assert!(
                            !crate::spec::names_a_credential(key),
                            "{h}: carries {key}, which names a credential"
                        );
                    }
                }
            }
        }
    }

    /// **A relocation keeps the operator's login**: `store` takes the operator's own value, then
    /// their `config`'s (a login made under a custom config dir is keyed on it), then empty — the
    /// default entry. Seeds are written 0600 with the operator's `carry` keys from their own copy
    /// (in their `config`, else `$HOME/<home>`), an existing file is left as it is, a carried value
    /// naming a credential is refused, and the operator's file is never written.
    #[test]
    fn a_relocation_points_its_store_at_the_operators_login_and_seeds_once() {
        use std::os::unix::fs::PermissionsExt;
        let r = crate::spec::Relocation {
            config: "CFG",
            store: "STORE",
            home: "",
            seed: &[("seed.json", r#"{"onboarded":true}"#)],
            carry: &["account"],
            note: "test",
        };
        let op = |pairs: Vec<(&'static str, String)>| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        };
        let s = |v: &str| v.to_string();
        assert_eq!(r.operator_store(op(vec![])), "");
        assert_eq!(r.operator_store(op(vec![("CFG", s("/work"))])), "/work");
        assert_eq!(
            r.operator_store(op(vec![("CFG", s("/work")), ("STORE", s("/store"))])),
            "/store"
        );
        assert_eq!(r.operator_store(op(vec![("STORE", s(""))])), "");
        for name in [
            "token",
            "accessToken",
            "refresh_token",
            "apiKey",
            "client-secret",
        ] {
            assert!(crate::spec::names_a_credential(name), "{name}");
        }
        for name in [
            "budgetTokens",
            "token_refresh_buffer_ms",
            "maxTokens",
            "oauthAccount",
        ] {
            assert!(!crate::spec::names_a_credential(name), "{name}");
        }

        let dir = std::env::temp_dir().join(format!("marion-relocation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let theirs = home.join("seed.json");
        let original = r#"{"account":{"email":"a@b"},"projects":{"/x":{}}}"#;
        std::fs::write(&theirs, original).unwrap();
        let home_env = || op(vec![("HOME", home.display().to_string())]);

        let cfg = dir.join("cfg");
        let env = r.apply(&cfg, home_env()).unwrap();
        assert_eq!(
            env,
            [
                ("CFG".to_string(), cfg.display().to_string()),
                ("STORE".to_string(), String::new()),
            ]
        );
        let seed = cfg.join("seed.json");
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&seed).unwrap()).unwrap();
        assert_eq!(
            doc,
            serde_json::json!({"onboarded": true, "account": {"email": "a@b"}}),
            "the carried key only, never the operator's projects"
        );
        let mode = std::fs::metadata(&seed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::write(&seed, "kept").unwrap();
        r.apply(&cfg, home_env()).unwrap();
        assert_eq!(std::fs::read_to_string(&seed).unwrap(), "kept");

        // Their `config` is where their copy lives when they set one; nothing there is fine.
        let elsewhere = dir.join("cfg2");
        r.apply(
            &elsewhere,
            op(vec![("CFG", dir.join("none").display().to_string())]),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("seed.json")).unwrap(),
            r#"{"onboarded":true}"#
        );

        // A test launch carries exactly the relocation, and nothing for a row that needs none.
        use crate::spec::Remembers;
        assert_eq!(
            Remembers::OperatorConfig(r)
                .test_env(&dir.join("cfg4"), home_env())
                .unwrap(),
            r.apply(&dir.join("cfg4"), home_env()).unwrap()
        );
        for none in [Remembers::Nothing, Remembers::CannedHome] {
            assert!(
                none.test_env(&dir.join("cfg5"), home_env())
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            !dir.join("cfg5").exists(),
            "a row that needs no move creates nothing"
        );

        std::fs::write(&theirs, r#"{"account":{"accessToken":"x"}}"#).unwrap();
        let refused = r.apply(&dir.join("cfg3"), home_env()).unwrap_err();
        assert!(
            refused.to_string().contains("names a credential"),
            "{refused}"
        );
        assert!(!dir.join("cfg3/seed.json").exists());
        std::fs::write(&theirs, original).unwrap();
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A claude node that took two turns spent both turns' tokens.** Measured on 2.1.280
    /// (S31 `p0a/out/a`, two stream-json turns in one process): each `result` frame's `usage` is
    /// that turn's alone (10 in / 5 out, twice) while `modelUsage` and `total_cost_usd` run
    /// cumulative — so the row folds `usage` by summing, which a one-turn run reads identically.
    #[test]
    fn a_claude_nodes_usage_sums_the_result_frame_of_every_turn() {
        let turn = |i: u64, o: u64| {
            serde_json::json!({"type": "result", "subtype": "success", "usage": {
                "input_tokens": i, "output_tokens": o,
                "cache_read_input_tokens": 1, "cache_creation_input_tokens": 2}})
        };
        let a = adapter_for(Harness::ClaudeCode).unwrap();
        assert_eq!(
            a.usage(&[turn(10, 5), turn(7, 3)]),
            Some(TokenUsage {
                input: 17,
                output: 8,
                cache_read: 2,
                cache_write: 4,
                reasoning: None,
            })
        );
        assert_eq!(
            a.usage(&[turn(10, 5)]).map(|u| (u.input, u.output)),
            Some((10, 5)),
            "a one-turn run reads as it always did"
        );
    }

    /// **What a message written mid-turn does is row data, refined per ACP agent** (S31): Claude
    /// Code's stream-json folds it, the protocol-generic ACP row queues it, and an ACP binding
    /// takes its refinement row's measurement — opencode and claude-agent-acp fold, codex-acp and
    /// copilot queue — while a command no row names keeps the generic queue.
    #[test]
    fn a_typed_turns_mid_turn_behaviour_is_the_rows_and_an_acp_agent_refines_it() {
        use crate::spec::{MidTurn, NodeShape, TurnDelivery};
        let mid = |a: &dyn HarnessAdapter| match a.turn_delivery(NodeShape::Headless) {
            TurnDelivery::TypedTurn { mid_turn, .. } => mid_turn,
            other => panic!("{:?}: not a typed turn: {other:?}", a.harness()),
        };
        assert_eq!(
            mid(&*adapter_for(Harness::ClaudeCode).unwrap()),
            MidTurn::Fold
        );
        assert_eq!(mid(&AcpAdapter::unbound()), MidTurn::Queue);
        for (id, want) in [
            ("opencode", MidTurn::Fold),
            ("claude-acp", MidTurn::Fold),
            ("codex-acp", MidTurn::Queue),
            ("copilot", MidTurn::Queue),
            // Refused `session/new` vendor-side, so nothing mid-turn was ever measured.
            ("gemini", MidTurn::Queue),
        ] {
            let bound = AcpAdapter::bound(acp::Binding::resolve(id).unwrap());
            assert_eq!(mid(&bound), want, "{id}");
        }
        let generic = AcpAdapter::bound(acp::Binding::resolve("my-agent --acp").unwrap());
        assert_eq!(mid(&generic), MidTurn::Queue);
    }

    /// agy's measured launch (s32): the prompt headed by the working directory, stream-json, the
    /// model, the cwd **and** marion's root as workspaces, and `--mode accept-edits` only where a
    /// write is declared; one document at `<config_dir>/agy-root/.agents/mcp_config.json`.
    #[test]
    fn the_agy_adapter_compiles_the_measured_invocation() {
        let a = AntigravityAdapter;
        let spec = LaunchSpec {
            prompt: "do the task".into(),
            ..agy_spec()
        };
        let inv = a.compile(&spec, &ctx()).unwrap();
        let root = "/state/x/config/agy-root";
        assert_eq!(inv.program, "agy");
        assert_eq!(
            inv.args,
            vec![
                "-p".to_string(),
                format!(
                    "Your working directory is /wt; every relative path below is relative to it. \
                     {root} is marion's configuration directory, not a place for your work.\n\n\
                     do the task"
                ),
                "--output-format".into(),
                "stream-json".into(),
                "--model".into(),
                agent_type::AGY_DEFAULT_MODEL.into(),
                "--add-dir".into(),
                "/wt".into(),
                "--add-dir".into(),
                root.into(),
            ]
        );
        assert_eq!(
            inv.env,
            vec![("AGY_CLI_DISABLE_AUTO_UPDATE".into(), "true".into())]
        );
        let files = a.config_files(&spec, &ctx()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].0,
            PathBuf::from(format!("{root}/.agents/mcp_config.json"))
        );
        assert!(
            !files[0].1.contains("mcp(marion"),
            "marion grants nothing itself"
        );
        assert_eq!(a.compiled_permissions(&spec).unwrap(), vec!["mode:default"]);

        let writer = LaunchSpec {
            tools: vec![agent_type::TOOL_READ.into(), agent_type::TOOL_WRITE.into()],
            ..spec.clone()
        };
        let inv = a.compile(&writer, &ctx()).unwrap();
        assert!(
            inv.args
                .windows(2)
                .any(|w| w == ["--mode", antigravity::ACCEPT_EDITS_MODE]),
            "a write is auto-denied headless without accept-edits: {:?}",
            inv.args
        );
        assert_eq!(
            a.compiled_permissions(&writer).unwrap(),
            vec!["mode:accept-edits"]
        );

        let resumed = LaunchSpec {
            resume: Some("66e0c65e".into()),
            ..spec
        };
        let inv = a.compile(&resumed, &ctx()).unwrap();
        assert_eq!(&inv.args[..2], ["--conversation", "66e0c65e"]);
    }

    /// A resume is checked only where the row measured one naming its own session, and never on a
    /// fresh run: the same stream reads as a refusal only for the agy row asked to resume another id.
    /// agy (s32) and opencode (S31 `p0b/opencode/db1`, `db2`) are the two rows that measured it.
    #[test]
    fn only_a_row_that_measured_an_in_place_resume_checks_one() {
        let fresh = r#"{"event":"init","conversation_id":"new-id","init":{}}"#;
        let agy = AntigravityAdapter;
        assert!(agy.resume_refusal(fresh, Some("old-id")).is_some());
        assert_eq!(agy.resume_refusal(fresh, Some("new-id")), None);
        assert_eq!(agy.resume_refusal(fresh, None), None);
        for h in Harness::ALL {
            let a = launch_adapter(h).unwrap();
            let checks = a
                .spec()
                .stream
                .and_then(|g| g.session.as_ref())
                .is_some_and(|s| s.resumes_in_place);
            assert_eq!(
                checks,
                matches!(h, Harness::Antigravity | Harness::OpenCode),
                "{h}: measured on agy and opencode alone"
            );
        }
    }

    #[test]
    fn agy_refuses_a_canned_or_endpoint_launch_by_name() {
        for auth in [Auth::Canned, Auth::Endpoint] {
            let overlaid = LaunchSpec { auth, ..agy_spec() };
            assert!(
                matches!(
                    AntigravityAdapter.compile(&overlaid, &ctx()),
                    Err(HarnessError::MissingInput { harness: Harness::Antigravity, what })
                        if what.contains("no canned or endpoint provider route")
                ),
                "{auth:?}"
            );
        }
    }

    /// **Every row states how a headless node is let call marion's tools, and the launch carries
    /// exactly that grant.** A headless node has no operator to answer a prompt, and each harness
    /// was measured denying, cancelling or classifier-declining an unapproved marion call at exit 0
    /// — the silent success §6.1 step 8 exists to refuse. The grant is one of a few shapes
    /// ([`spec::Approval`]): a per-tool allow list on argv, a key on marion's own server
    /// declaration, a launch-wide switch whose reach the row states, a protocol answer, or the
    /// operator's own allow list — which marion never writes, so the sweep checks the rule appears
    /// in nothing a launch carries, and `marion doctor` reports whether the operator granted it.
    #[test]
    fn every_row_states_how_its_headless_node_is_approved_to_call_marions_tools() {
        use crate::spec::{Approval, MCP_ALIAS};

        let document_dir = PathBuf::from("/state/agents/019f-root");
        for h in Harness::ALL {
            let approval = harness_spec(h).approval;
            assert!(
                !approval.note().trim().is_empty(),
                "{h}: an approval without the measurement behind it"
            );
            for (name, inv, files) in rendered_launches(h, &document_dir) {
                if !name.ends_with("headless") {
                    continue;
                }
                let in_documents = |needle: &str| files.iter().any(|(_, c)| c.contains(needle));
                let on_argv = |needle: &str| inv.args.iter().any(|a| a.contains(needle));
                let in_env = |needle: &str| inv.env.iter().any(|(_, v)| v.contains(needle));
                match approval {
                    Approval::AllowedToolsArg { flag, .. } => {
                        let at = inv
                            .args
                            .iter()
                            .position(|a| a == flag || a.starts_with(&format!("{flag}=")))
                            .unwrap_or_else(|| panic!("{h} ({name}): no `{flag}`: {:?}", inv.args));
                        let granted = if inv.args[at] == flag {
                            inv.args.get(at + 1)
                        } else {
                            inv.args.get(at)
                        };
                        assert!(
                            granted.is_some_and(|g| g.contains(MCP_ALIAS)),
                            "{h} ({name}): `{flag}` must name marion's server: {:?}",
                            inv.args
                        );
                    }
                    Approval::DeclarationKey { key, .. } => assert!(
                        in_documents(key) || on_argv(key) || in_env(key),
                        "{h} ({name}): marion's declaration must carry `{key}`"
                    ),
                    Approval::CliFlag { flag, .. } => assert!(
                        inv.args.iter().any(|a| a == flag),
                        "{h} ({name}): no `{flag}`: {:?}",
                        inv.args
                    ),
                    Approval::EnvVar { key, value, .. } => assert!(
                        inv.env.iter().any(|(k, v)| k == key && v == value),
                        "{h} ({name}): no `{key}={value}`: {:?}",
                        inv.env
                    ),
                    Approval::SessionMode { category, .. } => {
                        assert_eq!(h, Harness::Acp, "a session mode is a protocol's answer");
                        assert_eq!(
                            category,
                            crate::acp::MODE_CATEGORY,
                            "the select an approval_mode rides is the driver's own"
                        );
                    }
                    Approval::OperatorAllowlist {
                        file,
                        pointer,
                        rule,
                        ..
                    } => {
                        assert!(
                            !file.starts_with('/') && pointer.starts_with('/'),
                            "{h}: the file is under the operator's HOME and the pointer is JSON"
                        );
                        assert!(
                            rule.contains(MCP_ALIAS),
                            "{h}: the rule must name marion's server"
                        );
                        assert!(
                            !in_documents(rule) && !on_argv(rule) && !in_env(rule),
                            "{h} ({name}): marion must never grant the operator's rule itself"
                        );
                    }
                    Approval::None { .. } => {}
                }
            }
        }
        let kinds: Vec<(&str, &str)> = Harness::ALL
            .iter()
            .map(|h| (h.as_str(), harness_spec(*h).approval.kind()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("claude-code", "allowed-tools-arg"),
                ("codex", "declaration-key"),
                ("gemini", "declaration-key"),
                ("opencode", "declaration-key"),
                ("copilot", "allowed-tools-arg"),
                ("goose", "env-var"),
                ("cline", "cli-flag"),
                ("qwen", "cli-flag"),
                ("agy", "operator-allowlist"),
                ("pi", "none"),
                ("acp", "session-mode"),
            ],
            "each row's measured grant, named one at a time so a new row cannot copy a neighbour"
        );
    }

    /// **Every row is a complete, measured declaration** — the whole of what the trait used to
    /// answer in six hand-written impls, stated as data and checked once.
    ///
    /// Each clause below is a guess the table would otherwise invite: a verb outside marion's
    /// vocabulary, a spelling that does not name marion's server, a route left `None` where a
    /// declaration was asked for, a per-agent spelling on a harness that is one program, a note
    /// that names no spike. And the trait's defaults are asserted to read the row, harness by
    /// harness, so that "the adapter says X" and "the row says X" are one claim.
    #[test]
    fn every_row_is_a_complete_measured_declaration() {
        use crate::spec::{Constraint, McpRoute, Spelling, ToolSpelling};
        for h in Harness::ALL {
            let row = harness_spec(h);
            let a = launch_adapter(h).unwrap();
            assert_eq!(a.harness(), h);
            assert_eq!(a.surfaces(), row.surfaces.execution(), "{h}");
            assert_eq!(a.pane_surfaces().is_some(), row.pane.is_some(), "{h}");
            assert!(
                row.note.contains('S') || row.note.contains('s'),
                "{h}: unmeasured row"
            );
            for (verb, native) in row.tool_names {
                assert!(
                    [agent_type::TOOL_READ, agent_type::TOOL_WRITE].contains(verb),
                    "{h}: `{verb}` is not marion's vocabulary"
                );
                assert!(
                    a.tool_names(verb).unwrap().iter().any(|n| n == native),
                    "{h}: `{verb}` does not reach `{native}`"
                );
            }
            assert!(matches!(
                a.tool_names("bash"),
                Err(HarnessError::UnsupportedTool { harness, .. }) if harness == h
            ));
            match row.spelling {
                Spelling::Fixed(spelling) => {
                    assert_ne!(h, Harness::Acp);
                    assert!(spelling.spell("report").contains(crate::spec::MCP_ALIAS));
                    assert_eq!(
                        a.marion_tool_name("report"),
                        spelling.spell("report"),
                        "{h}"
                    );
                }
                Spelling::PerAgent => {
                    assert_eq!(h, Harness::Acp, "only ACP spells per agent");
                    assert_eq!(
                        a.marion_tool_name("report"),
                        ToolSpelling::ServerUnderscoreTool.spell("report"),
                        "the bound opencode agent's spelling"
                    );
                }
            }
            for (auth, route) in [
                (Auth::Canned, row.mcp.canned),
                (Auth::Inherited, row.mcp.live),
            ] {
                assert_ne!(
                    route,
                    McpRoute::None,
                    "{h}: a declaration always has a route"
                );
                let asked = LaunchSpec {
                    auth,
                    ..spec_for(h)
                };
                assert_eq!(a.mcp_route(&asked), route, "{h} {auth:?}");
                let none = LaunchSpec {
                    mcp: McpDeclaration::None,
                    ..asked
                };
                assert_eq!(
                    a.mcp_route(&none),
                    McpRoute::None,
                    "{h}: nothing asked, no route"
                );
            }
            let recorded = a.compiled_permissions(&spec_for(h)).unwrap();
            match row.constraint {
                Constraint::Fixed { prefix, value } => {
                    assert_eq!(recorded, vec![format!("{prefix}{value}")], "{h}")
                }
                Constraint::Mode { prefix, .. } | Constraint::Allowed { prefix } => {
                    assert!(
                        recorded.iter().all(|r| r.starts_with(prefix)),
                        "{h}: {recorded:?}"
                    )
                }
            }
        }
    }

    /// **The resume flag is a row field, measured per harness on the installed binary's `--help`**
    /// (`plan-restart-resume.md` step 5) — so that resume is one field on the launch and not five
    /// branches. This test sets `Fields::resume` directly on the fields the adapter computed, to
    /// pin the rows; `a_harness_whose_row_refuses_resume_is_refused_by_name` goes through
    /// `LaunchSpec::resume` and `compile`.
    ///
    /// claude 2.1.224: `-r, --resume [value]  Resume a conversation by session ID`.
    /// codex 0.147.0: `codex exec resume [SESSION_ID] [PROMPT]`, `--json` still accepted.
    /// opencode 1.17.3: `run -s, --session  session id to continue`.
    /// copilot 1.0.83: `-r, --resume[=value]  Resume from a previous session (optionally specify
    /// existing session ID …)` — an optional value, so `=`-joined.
    #[test]
    fn resume_argv_per_harness() {
        use crate::spec::{Shape, render};
        let resumed = |h: Harness, shape: Shape| -> Vec<String> {
            let a = launch_adapter(h).unwrap();
            let mut f = a.fields(&spec_for(h), &ctx(), shape).unwrap();
            f.resume = Some("SID".into());
            render(harness_spec(h), shape, &f)
                .unwrap_or_else(|e| panic!("{h}: {e:?}"))
                .args
        };
        let window = |args: &[String], n: usize| -> Vec<Vec<String>> {
            args.windows(n).map(|w| w.to_vec()).collect()
        };
        for shape in [Shape::Headless, Shape::Pane] {
            let args = resumed(Harness::ClaudeCode, shape);
            assert!(
                window(&args, 2).contains(&vec!["--resume".into(), "SID".into()]),
                "{args:?}"
            );
        }
        let args = resumed(Harness::Codex, Shape::Headless);
        assert_eq!(
            &args[..5],
            ["exec", "-C", "/wt", "resume", "SID"],
            "the subcommand follows exec and its `-C`, which `exec resume` does not take (s29): {args:?}"
        );
        assert!(args.contains(&"--json".to_string()));
        let args = resumed(Harness::OpenCode, Shape::Headless);
        assert_eq!(&args[..3], ["run", "--session", "SID"], "{args:?}");
        let args = resumed(Harness::Copilot, Shape::Headless);
        assert!(args.contains(&"--resume=SID".to_string()), "{args:?}");
        // And a launch that asks for no resume renders no resume token on any of them.
        for h in Harness::ALL {
            let inv = launch_adapter(h)
                .unwrap()
                .compile(&spec_for(h), &ctx())
                .unwrap();
            assert!(
                !inv.args
                    .iter()
                    .any(|a| a.contains("resume") || a == "--session"),
                "{h}: {:?}",
                inv.args
            );
        }
    }

    /// gemini 0.53.0's `--resume` takes `latest` or an index, not a session id, and ACP resumes
    /// through `session/load` rather than argv; codex's TUI has no measured resume grammar. Each
    /// is a refusal by name, never a fresh session started under a resumed session's id.
    #[test]
    fn a_harness_without_a_measured_resume_flag_refuses_by_name() {
        use crate::spec::{Refusal, Shape, render};
        for (h, shape) in [
            (Harness::Gemini, Shape::Headless),
            (Harness::Acp, Shape::Headless),
            (Harness::Codex, Shape::Pane),
        ] {
            let a = launch_adapter(h).unwrap();
            let mut f = a.fields(&spec_for(h), &ctx(), shape).unwrap();
            f.resume = Some("SID".into());
            assert_eq!(
                render(harness_spec(h), shape, &f),
                Err(Refusal::NoResume),
                "{h}"
            );
            assert!(matches!(
                render_row(harness_spec(h), shape, &f),
                Err(HarnessError::MissingInput { harness, .. }) if harness == h
            ));
        }
    }

    /// **Resume is a field on the launch, and it reaches argv through the row alone**
    /// (`plan-restart-resume.md` step 5). `LaunchSpec::resume` is copied into `Fields::resume` by
    /// the neutral seeding every adapter's `fields` hook builds on, so the same launch that renders
    /// `--resume <id>` on claude renders `exec resume <id>` on codex — and on a row with no
    /// measured resume grammar for the shape asked for, `compile`/`compile_pane` refuse **by
    /// name**, never start a fresh session under the old id.
    #[test]
    fn a_harness_whose_row_refuses_resume_is_refused_by_name() {
        let resuming = |h: Harness| LaunchSpec {
            resume: Some("SID".into()),
            ..spec_for(h)
        };
        for (h, pane) in [(Harness::Gemini, false), (Harness::Codex, true)] {
            let a = launch_adapter(h).unwrap();
            let spec = resuming(h);
            let got = match pane {
                false => a.compile(&spec, &ctx()),
                true => a.compile_pane(&spec, &ctx()),
            };
            match got {
                Err(HarnessError::MissingInput { harness, what }) => {
                    assert_eq!(harness, h);
                    assert!(what.contains("resume"), "{h}: {what}");
                }
                other => panic!("{h} (pane: {pane}) did not refuse by name: {other:?}"),
            }
        }
        // ACP's row has no argv resume either, and is **not** refused: its resume is the protocol's
        // `session/load`, carried on the session declaration rather than the command line
        // (`an_acp_resume_is_a_session_load_with_the_same_declaration`). Argv stays the agent's own.
        let acp_adapter = launch_adapter(Harness::Acp).unwrap();
        let inv = acp_adapter
            .compile(&resuming(Harness::Acp), &ctx())
            .expect("an ACP resume compiles");
        assert!(
            !inv.args.iter().any(|a| a.contains("SID")),
            "{:?}",
            inv.args
        );
        assert_eq!(
            acp_adapter
                .session_declaration(&resuming(Harness::Acp), &ctx())
                .unwrap()
                .unwrap()["method"],
            acp::SESSION_LOAD_METHOD
        );
        // And the rows that carry one render it, through the same field.
        let args = |h: Harness, pane: bool| -> Vec<String> {
            let a = launch_adapter(h).unwrap();
            let spec = resuming(h);
            match pane {
                false => a.compile(&spec, &ctx()),
                true => a.compile_pane(&spec, &ctx()),
            }
            .unwrap_or_else(|e| panic!("{h}: {e}"))
            .args
        };
        for pane in [false, true] {
            let got = args(Harness::ClaudeCode, pane);
            assert!(
                got.windows(2)
                    .any(|w| w == ["--resume".to_string(), "SID".to_string()]),
                "claude (pane: {pane}): {got:?}"
            );
        }
        assert_eq!(
            &args(Harness::Codex, false)[..5],
            ["exec", "-C", "/wt", "resume", "SID"]
        );
        assert_eq!(
            &args(Harness::OpenCode, false)[..3],
            ["run", "--session", "SID"]
        );
        assert!(args(Harness::Copilot, false).contains(&"--resume=SID".to_string()));
    }

    /// **The harness session id is grammar data, read by one engine** (`plan-restart-resume.md`
    /// step 4): each row names the unit that carries the id and the pointer to it, and
    /// [`crate::grammar::session_id`] reads it from a frame. Frames are the measured shapes:
    ///
    /// claude 2.1.220 (`s10/stream-*.jsonl`): `{"type":"system","subtype":"init","session_id"}`.
    /// codex 0.146.0 (`s7/exec-spawn-child.stream.jsonl`): `{"type":"thread.started","thread_id"}`.
    /// gemini 0.53.0 (`s12/README.md`): `{"type":"init","session_id"}`.
    /// opencode 1.17.3 (`s13/README.md`): `sessionID` on **every** frame.
    /// copilot 1.0.83 (`s24/*.stdout.jsonl`): only the terminal `result` frame carries `sessionId`;
    /// no earlier frame names the session, so a copilot run killed before its result has no id
    /// and a resume of it is refused honestly.
    /// ACP reads its stream as code and has no row: nothing to read.
    #[test]
    fn each_harness_row_extracts_its_session_id_from_its_first_frame() {
        use crate::grammar::{Cond, session_id};
        let frames: &[(Harness, &str, &str)] = &[
            (
                Harness::ClaudeCode,
                r#"{"type":"system","subtype":"init","cwd":"/x","session_id":"c-1","tools":[]}"#,
                "c-1",
            ),
            (
                Harness::Codex,
                r#"{"type":"thread.started","thread_id":"t-1"}"#,
                "t-1",
            ),
            (
                Harness::Gemini,
                r#"{"type":"init","timestamp":"<TS>","session_id":"g-1","model":"m"}"#,
                "g-1",
            ),
            (
                Harness::OpenCode,
                r#"{"type":"step_start","timestamp":1,"sessionID":"ses_1","part":{}}"#,
                "ses_1",
            ),
            (
                Harness::Copilot,
                r#"{"type":"result","timestamp":"<TS>","sessionId":"p-1","exitCode":0}"#,
                "p-1",
            ),
        ];
        for (h, frame, want) in frames {
            let g = harness_spec(*h)
                .stream
                .unwrap_or_else(|| panic!("{h} has a stream row"));
            let v: serde_json::Value = serde_json::from_str(frame).unwrap();
            assert_eq!(session_id(g, &v).as_deref(), Some(*want), "{h}");
        }
        // A frame that is not the session-bearing unit yields nothing, on every row: the engine
        // must not read a plausible-looking field off the wrong frame.
        for h in Harness::ALL {
            let Some(g) = harness_spec(h).stream else {
                assert_eq!(h, Harness::Acp, "only ACP reads its stream as code");
                continue;
            };
            let v = serde_json::json!({
                "type": "assistant", "session_id": "no", "thread_id": "no", "sessionId": "no"
            });
            assert_eq!(session_id(g, &v), None, "{h}");
            // Copilot's earlier frames carry no session id, and goose's stream carries none at
            // all (S26); every other row reads its first frame.
            let claims_early = g.session.as_ref().is_some_and(|s| {
                !s.at
                    .frame
                    .iter()
                    .any(|c| matches!(c, Cond::Eq("/type", "result")))
            });
            assert_eq!(
                claims_early,
                !matches!(h, Harness::Copilot | Harness::Goose | Harness::Cline),
                "{h}"
            );
        }
    }
}
