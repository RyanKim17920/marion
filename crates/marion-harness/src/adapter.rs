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
use marion_core::contract::AgentId;
use marion_core::harness::Harness;

use crate::claude_code::{self, HeadlessSpec, McpEnv, compile_headless};
use crate::codex::{self, ExecSpec, compile_exec};
use crate::gemini;
use crate::invocation::Invocation;
use crate::opencode;
use crate::stream::{ChildExit, MarionCall, StreamOutcome};
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

/// **How** marion's MCP declaration reaches this node — stated by the adapter, never inferred.
///
/// The distinction exists because "no configuration file" and "no bridge" are different facts that
/// look identical downstream. A live opencode node legitimately writes no file at all: its
/// declaration rides `OPENCODE_CONFIG_CONTENT`, because a file under an isolated `$XDG_CONFIG_HOME`
/// would isolate away the very login it is meant to use. Before this enum the supervisor read an
/// empty `config_files` as the refusal [`crate::RootError::NoMcpDeclaration`], and the obvious
/// "fix" — accept an empty vec — would have turned that refusal into a **hole**: any adapter that
/// forgot its declaration entirely would launch a node with no bridge, take a turn with no marion
/// tools, and exit 0 having called nothing (§6.1 step 8's failure class, §12's silent-failure
/// family). So the adapter says which route it took and the supervisor checks *that* route was
/// actually taken; an adapter that declares nothing still fails, loudly and by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpRoute {
    /// The first document [`HarnessAdapter::config_files`] emits carries it.
    Document,
    /// This env var of the compiled [`Invocation`] carries it inline, and no file is written.
    Environment(&'static str),
    /// The compiled [`Invocation`]'s **argv** carries it inline, and no file is written — the
    /// payload is the config key that must appear in it.
    ///
    /// A third variant rather than a reuse of either neighbour, because a live codex node is
    /// neither: `codex exec` resolves `mcp_servers` out of `$CODEX_HOME/config.toml`, and under
    /// [`Auth::Inherited`] that file is the **operator's own** — §6.4's central MUST forbids marion
    /// writing it. So the declaration is re-routed onto repeatable `-c <dotted.key>=<toml>` flags,
    /// which are neither a document nor an env var. Returning [`McpRoute::None`] instead would let
    /// the supervisor's verification pass on a node that has no bridge at all, which is §6.1 step
    /// 8's failure class; spelling it `Environment` would have marion check a variable nobody set.
    Argv(&'static str),
    /// No declaration was asked for — [`McpDeclaration::None`], §9's fallback branch. **Not** the
    /// same as an adapter that was asked for one and produced none, which is the refusal above.
    None,
}

/// Where the node's provider credential comes from, and therefore which endpoint it talks to.
///
/// An enum rather than a bool because an adapter reads *intent*, not a flag: the two modes differ in
/// what marion is entitled to overlay, and a `bool` at the call site would say `true` without saying
/// true of what. §6.4's central MUST is unchanged under either — marion never mutates the user's
/// real harness config — so what varies is only what marion *adds*, never what it edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Auth {
    /// marion mints the credential and points the node at its own canned endpoint. Every M1 path
    /// takes this, and it is the default so that adding the axis changed no existing behaviour.
    #[default]
    Canned,
    /// The node authenticates with the login the operator already has, inherited from marion's own
    /// environment.
    ///
    /// Nothing is *seeded*: no `crates/` code calls `env_clear`, so a child inherits marion's
    /// environment wholesale and marion only ever layers on top. Live mode is therefore the
    /// **absence** of three overlays rather than the presence of a credential store — marion pushes
    /// no key, overrides no base URL, and leaves the harness's own resolution alone.
    Inherited,
}

impl Auth {
    /// The spelling that rides a bridge declaration's `env` block (`MARION_AUTH`).
    ///
    /// A string rather than a bool for the same reason the type is an enum: an MCP `env` block is
    /// `Record<string,string>` on every harness that has one, and `"true"` would say *true of what*.
    pub fn as_wire(self) -> &'static str {
        match self {
            Auth::Canned => "canned",
            Auth::Inherited => "inherited",
        }
    }

    /// The inverse, for the bridge reading the declaration marion wrote.
    ///
    /// **An unrecognised value is `None`, and the caller must not treat that as `Inherited`.** The
    /// two failure directions are not symmetric: guessing canned costs a run against an endpoint
    /// that is not there, while guessing live points the operator's real credential somewhere marion
    /// did not choose. Absence — a declaration written before this key existed — is the caller's to
    /// resolve, and every such declaration meant [`Auth::Canned`].
    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim() {
            "canned" => Some(Auth::Canned),
            "inherited" => Some(Auth::Inherited),
            _ => None,
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
    pub api_key: Option<String>,
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
    /// ([`crate::claude_code::NODE_TOKEN_ENV`]).
    ///
    /// It rides `SpawnCtx` rather than `LaunchSpec` for the reason [`Self::agent_type`] does: this
    /// is *what marion knows about the node*, not what was asked for. Nobody asks for a token.
    ///
    /// `None` where no owner minted one — every spawn path but the supervisor's own `agent/spawn`,
    /// until steps 5 and 6 land. A node with no token declares no key at all rather than an empty
    /// one, so its bridge states no capability rather than a worthless one.
    pub node_token: Option<String>,
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
    /// shape is a different node, not the same node with an extra fd.
    fn pane_surfaces(&self) -> Option<ExecutionSurfaces> {
        None
    }

    /// argv + env for [`Self::pane_surfaces`]'s shape. Called **only** where that is `Some`.
    ///
    /// Defaulted to the refusal rather than to [`Self::compile`], so a fifth harness that declares
    /// a pane surface and forgets this one gets a named error instead of a TUI request answered
    /// with a headless launch.
    fn compile_pane(&self, spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        let _ = (spec, ctx);
        Err(HarnessError::NoPaneSurface(self.harness()))
    }

    /// The configuration files this harness needs, as `(absolute path, contents)`. The caller
    /// writes them; the adapter decides what and where, because "what and where" is the part that
    /// differs per harness. Paths are always under `spec.config_dir`.
    fn config_files(
        &self,
        spec: &LaunchSpec,
        ctx: &SpawnCtx,
    ) -> Result<Vec<(PathBuf, String)>, HarnessError>;

    /// Which channel this launch's MCP declaration travels on — see [`McpRoute`].
    ///
    /// Required rather than defaulted on purpose: a default of [`McpRoute::Document`] would be
    /// inherited silently by a fifth harness whose declaration is not a document, which is the
    /// same class of mistake as the fallback [`adapter_for`] refuses to make.
    fn mcp_route(&self, spec: &LaunchSpec) -> McpRoute;

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
    fn tool_name(&self, tool: &str) -> Result<String, HarnessError>;

    /// Every tool [`LaunchSpec::tools`] declares, in this harness's own spelling, or the first
    /// refusal.
    ///
    /// Provided rather than written four times: the *mapping* is per-harness ([`Self::tool_name`])
    /// and refusing on the first unmappable name is not. Every `compile` must call it — including
    /// the two harnesses that do nothing with the result — because the refusal is the part that is
    /// owed to a declaration on all four.
    fn native_tools(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        spec.tools.iter().map(|t| self.tool_name(t)).collect()
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
    fn compiled_permissions(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError>;

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
    fn marion_calls(&self, stdout: &str) -> Vec<MarionCall>;

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

    /// What `--allowedTools` carries: §3.1's *"the same list, plus marion's own `mcp__marion__*`"*.
    ///
    /// One derivation, called by `compile` and by `compiled_permissions`, so §6.7's audit record
    /// and the flag it describes cannot disagree. Two expressions of this would be two chances for
    /// the contract to name a permission the node was never granted — the class of defect
    /// `32ec905` fixed for `harness` and this method exists to keep out of `allowed_tools`.
    fn permission_axis(
        adapter: &ClaudeCodeAdapter,
        spec: &LaunchSpec,
    ) -> Result<Vec<String>, HarnessError> {
        let mut allowed = spec.allowed_tools.clone();
        allowed.extend(adapter.native_tools(spec)?);
        Ok(allowed)
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
            // **Not `unwrap_or_default()`.** That wrote `MARION_BASE_URL: ""` into a live root's
            // declaration, the bridge read it back as `Ok("")`, and the child it spawned was
            // compiled canned against an endpoint spelled as the empty string — a live root whose
            // child was neither live nor working, with nothing anywhere reporting it.
            base_url: spec.base_url.clone(),
            auth: spec.auth,
            agent_id: ctx.agent_id.clone(),
            agent_type: ctx.agent_type.clone(),
            depth: ctx.depth,
            node_token: ctx.node_token.clone(),
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
        // **Live is pure removal, and this harness is the case where that is literally true.**
        // `CLAUDE_CONFIG_DIR` is already never set (isolating it breaks OAuth — the Keychain entry
        // is keyed to the real config dir), `--strict-mcp-config --mcp-config` already keeps the
        // MCP declaration fileless inside marion's own agent dir, and `--setting-sources ""`
        // already excludes the user's settings, plugins and hooks. So the only thing standing
        // between a logged-in `claude` and marion is the three env vars marion overlays, and
        // dropping them is the whole of live mode: nothing is seeded, because nothing was cleared.
        let (base_url, api_key) = match spec.auth {
            Auth::Canned => (
                spec.base_url
                    .as_deref()
                    .map(claude_code::anthropic_base_url),
                spec.api_key.clone(),
            ),
            // `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN` and `ANTHROPIC_API_KEY` all fall out of
            // `compile_headless` together: it emits each only when given the value behind it, so
            // withholding both here is exactly "do not overlay", with no second branch to drift.
            Auth::Inherited => (None, None),
        };
        // §3.1's two axes, compiled from **one** declaration, which is why they cannot disagree:
        // availability is the mapped list, permission is *"the same list, plus marion's own
        // `mcp__marion__*`"*. This harness is the one of four where both axes are marion's to set
        // and where opening only the first is a measured dead end (§11 item 24).
        Ok(compile_headless(&HeadlessSpec {
            cwd: spec.cwd.clone(),
            model: spec.model.clone(),
            tools: self.native_tools(spec)?,
            allowed_tools: Self::permission_axis(self, spec)?,
            mcp_config: Self::mcp_config_path(spec),
            base_url,
            api_key,
        }))
    }

    /// §3.4's `opaque`: a pty and nothing else. **Not `interactive`** — that preset also claims
    /// `TranscriptRecords`, and marion reads no transcript this harness writes; claiming an
    /// observation source nothing consumes would put a false row in §3.4's derivation table.
    fn pane_surfaces(&self) -> Option<ExecutionSurfaces> {
        Some(ExecutionSurfaces::opaque())
    }

    /// The TUI, with the same isolation and the same two axes the headless shape gets. See
    /// [`claude_code::compile_pane`] for why the prompt may ride argv here and may not there.
    fn compile_pane(&self, spec: &LaunchSpec, _ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        let (base_url, api_key) = match spec.auth {
            Auth::Canned => (
                spec.base_url
                    .as_deref()
                    .map(claude_code::anthropic_base_url),
                spec.api_key.clone(),
            ),
            Auth::Inherited => (None, None),
        };
        Ok(claude_code::compile_pane(
            &HeadlessSpec {
                cwd: spec.cwd.clone(),
                model: spec.model.clone(),
                tools: self.native_tools(spec)?,
                allowed_tools: Self::permission_axis(self, spec)?,
                mcp_config: Self::mcp_config_path(spec),
                base_url,
                api_key,
            },
            &spec.prompt,
        ))
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

    /// `--mcp-config` names a document, in both auth modes: live mode drops three env vars and
    /// changes nothing about where the declaration lives.
    fn mcp_route(&self, spec: &LaunchSpec) -> McpRoute {
        match spec.mcp {
            McpDeclaration::Marion => McpRoute::Document,
            McpDeclaration::None => McpRoute::None,
        }
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

    /// `write` → **`Write`**, measured on 2.1.222: `--tools "Write"` puts a tool of that name, with
    /// schema `{file_path, content}`, into the request body's tool list (§11 item 24). Its own
    /// description asks for an absolute path; the same measurement drove it at a **relative** one
    /// deliberately, and the file landed in the node's worktree — this harness resolves against the
    /// process cwd, so `Invocation.cwd` places it and nothing further is owed here.
    ///
    /// `read` → **`Read`**, measured on 2.1.222 (`tests/fixtures/s14/README.md`): `--tools Read`
    /// puts `Read` in the request body's tool list, and the run marion compiles today — `--tools ""`
    /// — has no `Read` at all. **This is the one harness of four where the grant buys something**;
    /// on gemini and opencode a read tool is already declared by default.
    ///
    /// The negative half of that measurement is why the mapping exists rather than a pass-through:
    /// `--tools read`, marion's own word unmapped, yields `body.tools []`, **exit 0, empty stderr**,
    /// and a `system/init` frame that agrees — indistinguishable from a healthy run and from the
    /// bogus `--tools NotATool`. That is §12's accept-and-ignore shape with marion on the producing
    /// end.
    ///
    /// `Edit` is *not* mapped, and its absence is deliberate rather than pending: item 24 records
    /// that `Edit` and `Bash` were never tried, and this codebase does not name a grant it has not
    /// watched arrive. s14 declared `Bash` once, only to settle the comma separator for the
    /// multi-name case, and makes no other claim about it.
    fn tool_name(&self, tool: &str) -> Result<String, HarnessError> {
        match tool {
            agent_type::TOOL_READ => Ok("Read".into()),
            agent_type::TOOL_WRITE => Ok("Write".into()),
            _ => Err(HarnessError::UnsupportedTool {
                harness: Harness::ClaudeCode,
                tool: tool.to_string(),
            }),
        }
    }

    /// **The one harness of four with a real per-tool allowlist**, so this is §3.1's first branch
    /// rather than its "coarsest equivalent" fallback: the record is the literal contents of
    /// `--allowedTools`, which is the flag the CLI checks a call against.
    fn compiled_permissions(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        Self::permission_axis(self, spec)
    }

    /// The prefix is derived from this adapter's own `marion_tool_name`, so the reader and the
    /// compiler of the name can never disagree about the spelling.
    fn marion_calls(&self, stdout: &str) -> Vec<MarionCall> {
        claude_code::marion_calls(stdout, &self.marion_tool_name(""))
    }
}

/// Codex 0.146.0, `exec` surface (§9). marion's M1 child.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

impl CodexAdapter {
    fn config_path(spec: &LaunchSpec) -> PathBuf {
        spec.config_dir.join("config.toml")
    }

    /// The bridge declaration in the neutral form both of codex's routes serialise — the generated
    /// `config.toml` under [`Auth::Canned`], the `-c` overrides under [`Auth::Inherited`]. Built
    /// once so the two cannot carry different `env` blocks.
    fn bridge_env(spec: &LaunchSpec, ctx: &SpawnCtx) -> Option<codex::BridgeEnv> {
        (spec.mcp == McpDeclaration::Marion).then(|| codex::BridgeEnv {
            bridge: ctx.bridge.clone(),
            args: ctx.bridge_args.clone(),
            repo: ctx.repo.clone(),
            state: ctx.state_dir.clone(),
            // Omitted rather than blanked under `Inherited` — see `ClaudeCodeAdapter::mcp_env`.
            base_url: spec.base_url.clone(),
            auth: spec.auth,
            agent_id: ctx.agent_id.clone(),
            agent_type: ctx.agent_type.clone(),
            depth: ctx.depth,
            node_token: ctx.node_token.clone(),
            ready_file: ctx.ready_file.clone(),
        })
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

    /// **Under `Inherited` the declaration is compiled into argv, not written to a file** — and
    /// `CODEX_HOME` is dropped, which is what makes the login visible in the first place. The two go
    /// together: unsetting the variable points codex at the operator's `~/.codex/auth.json` *and* at
    /// the operator's `~/.codex/config.toml`, and marion is forbidden to write the second (§6.4), so
    /// the `-c` overlay is the only channel left.
    fn compile(&self, spec: &LaunchSpec, ctx: &SpawnCtx) -> Result<Invocation, HarnessError> {
        // Called for the **refusal**, which is owed on all four harnesses, and for nothing else:
        // see `Self::tool_name` for why a codex declaration compiles no flag. Discarding the names
        // is the honest outcome, not a forgotten `?`.
        let _already_granted = self.native_tools(spec)?;
        let config_overrides = match spec.auth {
            Auth::Canned => Vec::new(),
            Auth::Inherited => Self::bridge_env(spec, ctx)
                .as_ref()
                .map(codex::live_config_overrides)
                .unwrap_or_default(),
        };
        Ok(compile_exec(&ExecSpec {
            cwd: spec.cwd.clone(),
            codex_home: match spec.auth {
                Auth::Canned => Some(spec.config_dir.clone()),
                Auth::Inherited => None,
            },
            // Canned compiles none, so every existing contract still records `None` and the canned
            // argv is byte-identical to what it was before `-m` was known to exist here.
            model: match spec.auth {
                Auth::Canned => None,
                Auth::Inherited => spec.model.clone(),
            },
            prompt: spec.prompt.clone(),
            output_schema: spec.extra.output_schema.clone(),
            output_last_message: spec.extra.output_last_message.clone(),
            config_overrides,
        }))
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
        // **Closed.** This used to be a `TODO(phase-3)` beside a deliberate `let _ = &ctx.agent_id`:
        // `config_toml` took only `(bridge, bridge_args, base_url)` and emitted no per-server `env`,
        // so a codex node's bridge had neither marion's paths nor the node's identity. A codex child
        // survived it (its one call is `report`, which reads nothing); a codex **root** did not —
        // `spawn` answered `marion: MARION_REPO is not set`, and `TaskContract.requester` would have
        // read `"unattributed-root"`. codex's TOML has always accepted `env` inside
        // `[mcp_servers.<name>]`, so nothing was blocking it but this call.
        let bridge = Self::bridge_env(spec, ctx).unwrap_or_else(|| codex::BridgeEnv {
            bridge: ctx.bridge.clone(),
            args: ctx.bridge_args.clone(),
            repo: ctx.repo.clone(),
            state: ctx.state_dir.clone(),
            base_url: spec.base_url.clone(),
            auth: spec.auth,
            agent_id: ctx.agent_id.clone(),
            agent_type: ctx.agent_type.clone(),
            depth: ctx.depth,
            node_token: ctx.node_token.clone(),
            ready_file: ctx.ready_file.clone(),
        });
        Ok(vec![(
            Self::config_path(spec),
            codex::config_toml(&bridge, base_url),
        )])
    }

    /// The second adapter whose route depends on the auth mode, and for the same reason as
    /// opencode's: the file marion would write is the operator's own once the isolation is dropped.
    /// A canned node's `[mcp_servers.marion]` lives in the generated `config.toml`; a live node's
    /// rides `-c mcp_servers.marion.…` on its own command line, which is neither a document nor an
    /// env var — hence [`McpRoute::Argv`].
    fn mcp_route(&self, spec: &LaunchSpec) -> McpRoute {
        match (spec.mcp, spec.auth) {
            (McpDeclaration::None, _) => McpRoute::None,
            (McpDeclaration::Marion, Auth::Canned) => McpRoute::Document,
            (McpDeclaration::Marion, Auth::Inherited) => McpRoute::Argv(codex::MCP_SERVER_KEY),
        }
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

    /// `write` → **`sandbox:workspace-write`**, which is §3.1's *"the harness's coarsest
    /// equivalent where it has no per-tool allowlist at all"* — that section names this exact
    /// string for this exact harness.
    ///
    /// **This harness's availability axis is a sandbox mode, and marion already opens it.**
    /// `codex exec` exposes no `--tools` and no permission list, only
    /// `--sandbox <read-only|workspace-write|danger-full-access>`; `codex::config_toml` compiles
    /// `sandbox_mode = "workspace-write"` on every node and always has, which is why a codex child
    /// is the one this matrix has always been able to drive to a write. So a declaration here is
    /// **satisfied rather than newly granted**, and `compile` emits nothing for it.
    ///
    /// The judgement call, stated: this could instead have made `workspace-write` *conditional* on
    /// the declaration, which reads tidier and would silently demote every codex node marion spawns
    /// today to `read-only` — a behaviour change on the harness that was never broken, taken to
    /// close a gap on two others. The opt-in rule cuts the other way here.
    ///
    /// **`read` has no arm, and its absence is the decision rather than an omission.**
    /// `tests/fixtures/s14/README.md` measured this harness's whole declaration — `apply_patch,
    /// create_goal, exec_command, get_goal, update_goal, update_plan, view_image, write_stdin`,
    /// **identical under `--sandbox read-only` and `--sandbox workspace-write`** — and there is no
    /// read tool in it. Reading a file on codex is `exec_command`, i.e. the shell.
    ///
    /// The tempting move is to answer `read` the way `write` is answered above, *satisfied rather
    /// than newly granted*. It does not transfer, for a reason the two cases do not share: `write`
    /// names a **measured correspondence** — `apply_patch`, gated by a sandbox mode marion actually
    /// compiles — whereas `read` would name the shell, which also writes, execs and reaches the
    /// network. A reader of `tools: [read]` would take a codex node for read-only when it is
    /// nothing of the kind. So the declaration is refused by name, the launch aborts, and the
    /// operator is told which verb and which harness. See `marion_core::agent_type::TOOL_READ` for
    /// why refusal beats recording the absence in the compiled spec.
    fn tool_name(&self, tool: &str) -> Result<String, HarnessError> {
        match tool {
            agent_type::TOOL_WRITE => Ok(format!("sandbox:{}", codex::SANDBOX_MODE)),
            _ => Err(HarnessError::UnsupportedTool {
                harness: Harness::Codex,
                tool: tool.to_string(),
            }),
        }
    }

    /// **§3.1's worked example, verbatim, and the reason the sentence exists.** That section names
    /// this harness as the "coarsest equivalent" case and this string as its record: `codex exec`
    /// exposes only `--sandbox` and `--add-dir`, so `sandbox:workspace-write` is the whole of the
    /// constraint a codex child ran under.
    ///
    /// **It replaces a hardcoded `["apply_patch", "shell"]`** that `build_contract` wrote for every
    /// child of every harness. Those are marion-side tool *names*, not a list codex ever checked a
    /// call against — exactly what §3.1 forbids in as many words: *"echoing marion's own vocabulary
    /// there would make the field claim a constraint that never existed."*
    ///
    /// Constant, and correctly so: `codex::config_toml` compiles that one sandbox mode on every
    /// node, so there is nothing about this launch that could vary it. A declaration changes
    /// nothing here for the reason [`Self::tool_name`] gives — it is satisfied, not compiled — and
    /// the record says the same thing whether or not one arrived, because the constraint did not
    /// move. `native_tools` still runs, so an unmappable name is refused here as it is in `compile`.
    fn compiled_permissions(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        self.native_tools(spec)?;
        Ok(vec![format!("sandbox:{}", codex::SANDBOX_MODE)])
    }

    /// **No prefix.** codex's stream names the server and the tool as two fields, so the flat
    /// identifier above never appears in it — see [`codex::marion_calls`].
    fn marion_calls(&self, stdout: &str) -> Vec<MarionCall> {
        codex::marion_calls(stdout)
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

    /// The approval mode this launch runs under — the whole of gemini's tool constraint.
    ///
    /// One derivation, shared by `compile` and `compiled_permissions`, for the reason
    /// `ClaudeCodeAdapter::permission_axis` gives: a second copy could put a mode in the audit
    /// record that the argv never carried.
    fn approval_mode(
        adapter: &GeminiAdapter,
        spec: &LaunchSpec,
    ) -> Result<&'static str, HarnessError> {
        let native = adapter.native_tools(spec)?;
        Ok(if native.iter().any(|t| gemini::is_edit_tool(t)) {
            gemini::AUTO_EDIT_APPROVAL_MODE
        } else {
            gemini::DEFAULT_APPROVAL_MODE
        })
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
        // §3.1's availability axis, in the only form this harness has one: a mode, not a list. The
        // marion → gemini mapping is `Self::tool_name`'s, and which *gemini* names need the mode is
        // `gemini::is_edit_tool`'s, so neither half is restated here.
        let auto_edit = Self::approval_mode(self, spec)? == gemini::AUTO_EDIT_APPROVAL_MODE;
        Ok(gemini::compile_prompt(&gemini::PromptSpec {
            cwd: spec.cwd.clone(),
            model,
            auto_edit,
            prompt: spec.prompt.clone(),
            cli_home: spec.config_dir.clone(),
            settings: Self::settings_path(spec),
            base_url: spec.base_url.clone(),
            api_key: spec.api_key.clone(),
            auth: spec.auth,
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
            // Omitted rather than blanked under `Inherited` — see `ClaudeCodeAdapter::mcp_env`.
            base_url: spec.base_url.clone(),
            auth: spec.auth,
            agent_id: ctx.agent_id.clone(),
            agent_type: ctx.agent_type.clone(),
            depth: ctx.depth,
            node_token: ctx.node_token.clone(),
            ready_file: ctx.ready_file.clone(),
        });
        // The one key whose right value is not marion's to choose. Under `Canned` marion supplies
        // `GEMINI_API_KEY` and so selects `gemini-api-key`; under `Inherited` it supplies no
        // credential at all, and this document is the *system settings* layer, which outranks the
        // operator's own — so a hardcoded selection here would override a real `oauth-personal`
        // profile with a type that has no credential behind it and fail with S12's code 41.
        let json = match spec.auth {
            Auth::Canned => gemini::settings_json(bridge.as_ref()),
            Auth::Inherited => {
                gemini::settings_json_with_auth(bridge.as_ref(), &gemini::live_auth_type())
            }
        };
        Ok(vec![(
            Self::settings_path(spec),
            serde_json::to_string_pretty(&json).expect("a Value always serialises"),
        )])
    }

    /// A document in both modes. `GEMINI_CLI_SYSTEM_SETTINGS_PATH` is resolved *independently* of
    /// `GEMINI_CLI_HOME` (S12's precedence table: two layers, two variables), so live mode drops the
    /// sandbox home and the injection route survives untouched. There is no `--settings` flag and no
    /// inline analogue, so this file is the only channel gemini has.
    fn mcp_route(&self, spec: &LaunchSpec) -> McpRoute {
        match spec.mcp {
            McpDeclaration::Marion => McpRoute::Document,
            McpDeclaration::None => McpRoute::None,
        }
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

    /// `write` → **`write_file`**, measured on 0.53.0: under
    /// [`gemini::AUTO_EDIT_APPROVAL_MODE`] it appears in `functionDeclarations` with schema
    /// `{file_path, content}`, and under the default mode it appears nowhere but the prose of the
    /// system instruction (§11 item 24). The same measurement drove it at a **relative** path and
    /// the file landed in the node's worktree, so `Invocation.cwd` places it.
    ///
    /// `replace` is gemini's *other* edit tool and the same mode restores it, but marion's
    /// vocabulary has no verb that means it today, so nothing maps there. It is still named by
    /// [`gemini::is_edit_tool`], which answers about gemini's names rather than marion's.
    ///
    /// `read` → **`read_file`**, measured on 0.53.0 (`tests/fixtures/s14/README.md`): it is one of
    /// the eight `functionDeclarations` present under the **default** approval mode, so the grant is
    /// a **no-op** and `compile` emits nothing for it — `is_edit_tool("read_file")` is false, which
    /// is what keeps a reading node out of `auto_edit` and its write tools. Answered anyway, for
    /// [`HarnessAdapter::tool_name`]'s stated reason: answering is not the same as compiling, and
    /// making the grant conditional here would narrow what this harness has always been able to do.
    ///
    /// s14 also measured that `--allowed-tools` neither gates nor validates on 0.53.0 —
    /// `--allowed-tools read_file` and `--allowed-tools NotATool` produce byte-identical
    /// declarations — so there is no flag here for marion to compile even if it wanted one.
    fn tool_name(&self, tool: &str) -> Result<String, HarnessError> {
        match tool {
            agent_type::TOOL_READ => Ok("read_file".into()),
            agent_type::TOOL_WRITE => Ok("write_file".into()),
            _ => Err(HarnessError::UnsupportedTool {
                harness: Harness::Gemini,
                tool: tool.to_string(),
            }),
        }
    }

    /// **The approval mode, because on this harness the mode *is* the constraint.** 0.53.0 has no
    /// `--tools` flag and no per-tool permission list; what decides whether a gemini child can
    /// change a file is which of `default` / `auto_edit` / `yolo` it runs under, and under the
    /// first the mutating tools are withheld from `functionDeclarations` entirely.
    ///
    /// `approval-mode:` prefixed, on the shape §3.1 gives codex (`sandbox:workspace-write`): the
    /// axis and its value, so a reader can tell a *mode* from a *tool name* at a glance and never
    /// mistake this for a per-tool allowlist gemini does not have.
    ///
    /// Recorded in **both** states, not only the relaxed one — see [`gemini::DEFAULT_APPROVAL_MODE`].
    fn compiled_permissions(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        Ok(vec![format!(
            "approval-mode:{}",
            Self::approval_mode(self, spec)?
        )])
    }

    fn marion_calls(&self, stdout: &str) -> Vec<MarionCall> {
        gemini::marion_calls(stdout, &self.marion_tool_name(""))
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
        // **The default is marion's own plumbing, and under `--live` that plumbing does not
        // exist.** `marion/default` names the provider block `config_json` *generates*, pointed at
        // marion's canned endpoint. A live node writes no provider block at all — deliberately, so
        // as not to shadow the operator's real one — so `-m marion/default` would resolve nothing
        // and S13 measured that as `Error: {"name":"UnknownError",…}` with exit 1. Refused by name
        // instead: the operator has to say which of *their* providers a live node should use.
        if spec.auth == Auth::Inherited && m == marion_core::agent_type::OPENCODE_DEFAULT_MODEL {
            return Err(HarnessError::MissingInput {
                harness: Harness::OpenCode,
                what: "the built-in default model `marion/default` names the provider block marion \
                       generates for its own canned endpoint, and a --live node writes none — so \
                       it resolves to no provider at all. Name a real provider/model from the \
                       operator's own opencode config instead (marion run --live -m …)",
            });
        }
        opencode::ModelRef::parse(m).ok_or(HarnessError::MissingInput {
            harness: Harness::OpenCode,
            what: "the model must be in `provider/model` form, which is the only spelling `-m` \
                   accepts and the one the generated provider block has to repeat",
        })
    }

    /// The bridge declaration, in the neutral form both routes serialise.
    fn bridge_env(spec: &LaunchSpec, ctx: &SpawnCtx) -> Option<opencode::BridgeEnv> {
        (spec.mcp == McpDeclaration::Marion).then(|| opencode::BridgeEnv {
            bridge: ctx.bridge.clone(),
            args: ctx.bridge_args.clone(),
            repo: ctx.repo.clone(),
            state: ctx.state_dir.clone(),
            base_url: spec.base_url.clone(),
            auth: spec.auth,
            agent_id: ctx.agent_id.clone(),
            agent_type: ctx.agent_type.clone(),
            depth: ctx.depth,
            node_token: ctx.node_token.clone(),
            ready_file: ctx.ready_file.clone(),
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
        // For the **refusal** only — see `Self::tool_name`. Discarding the names is the honest
        // outcome on this harness, not a forgotten `?`.
        let _already_granted = self.native_tools(spec)?;
        // **Under `Inherited` the declaration is compiled into the env, not written to a file.**
        // S13: auth resolves through `$XDG_DATA_HOME` and config through `$XDG_CONFIG_HOME` — two
        // variables — so marion cannot relocate the config without also having to relocate, and
        // therefore hide, the login. `OPENCODE_CONFIG_CONTENT` is last in the merge order and
        // merges *over* the operator's own config, which is exactly the wrong property for
        // isolation and exactly the right one here.
        let config_content = match spec.auth {
            Auth::Canned => None,
            Auth::Inherited => Self::bridge_env(spec, ctx).map(|b| {
                serde_json::to_string(&opencode::live_config_json(Some(&b)))
                    .expect("a Value always serialises")
            }),
        };
        Ok(opencode::compile_run(&opencode::RunSpec {
            cwd: spec.cwd.clone(),
            sandbox: spec.config_dir.clone(),
            model: Self::model_ref(spec)?,
            // Any stable string suppresses the title-generation call; the node's own id makes the
            // session identifiable in `opencode session list` without leaking the prompt.
            title: format!("marion-{}", ctx.agent_id.0),
            prompt: spec.prompt.clone(),
            auth: spec.auth,
            config_content,
        }))
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
        let bridge = Self::bridge_env(spec, ctx);
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

    /// The one adapter whose route depends on the auth mode: a file under an isolated
    /// `$XDG_CONFIG_HOME` when marion owns the config surface, and inline
    /// `OPENCODE_CONFIG_CONTENT` when the operator does.
    fn mcp_route(&self, spec: &LaunchSpec) -> McpRoute {
        match (spec.mcp, spec.auth) {
            (McpDeclaration::None, _) => McpRoute::None,
            (McpDeclaration::Marion, Auth::Canned) => McpRoute::Document,
            (McpDeclaration::Marion, Auth::Inherited) => {
                McpRoute::Environment(opencode::CONFIG_CONTENT_ENV)
            }
        }
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

    /// `write` → **`write`**, which is the name opencode already declares. Measured off the request
    /// log of a child spawned through `spawn`: an opencode node's tool list carries `write`, `edit`
    /// and `bash` alongside marion's MCP tools, with no flag from marion asking for any of them.
    ///
    /// So, as on codex, a declaration here is **satisfied rather than newly granted** and `compile`
    /// emits nothing for it. The judgement call is the same one and lands the same way: opencode's
    /// config *does* have a per-tool block that could disable these, and using the declaration to
    /// drive it would silently narrow every opencode node marion spawns today. Opening a route on
    /// two harnesses is not a licence to close one on a third.
    ///
    /// A spelling collision, not a shared vocabulary: marion's `write` and opencode's `write` are
    /// the same six letters by coincidence, and the mapping is written out rather than defaulted
    /// so that a future marion verb cannot pass through unmapped.
    ///
    /// `read` → **`read`**, the same collision and the same no-op: s14 measured opencode 1.17.3's
    /// default tool list as `bash, edit, glob, grep, read, skill, task, todowrite, webfetch, write`,
    /// so the tool is there before marion says anything. `OPENCODE_PERMISSION` *does* gate — s14
    /// measured `{"read":"deny"}` taking the schema from 10 tools to 9 — which is precisely why
    /// marion compiles nothing into it: driving that block off the declaration would silently
    /// narrow every opencode node marion spawns today.
    fn tool_name(&self, tool: &str) -> Result<String, HarnessError> {
        match tool {
            agent_type::TOOL_READ => Ok("read".into()),
            agent_type::TOOL_WRITE => Ok("write".into()),
            _ => Err(HarnessError::UnsupportedTool {
                harness: Harness::OpenCode,
                tool: tool.to_string(),
            }),
        }
    }

    /// **The one harness where the honest record is that marion compiled nothing** — see
    /// [`opencode::NO_COMPILED_TOOL_CONSTRAINT`], which carries the measurement and the argument
    /// against both an empty list and an invented native spelling.
    ///
    /// `native_tools` still runs, so an unmappable name is refused here as it is in `compile`.
    fn compiled_permissions(&self, spec: &LaunchSpec) -> Result<Vec<String>, HarnessError> {
        self.native_tools(spec)?;
        Ok(vec![opencode::NO_COMPILED_TOOL_CONSTRAINT.into()])
    }

    fn marion_calls(&self, stdout: &str) -> Vec<MarionCall> {
        opencode::marion_calls(stdout, &self.marion_tool_name(""))
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
    use crate::claude_code::{AGENT_TYPE_ENV, DEPTH_ENV};
    use crate::codex::config_toml;
    use crate::stream::CallOutcome;
    use crate::surfaces::{ControlTransport, DisplaySurface};

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
            tools: vec![],
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
            codex_home: Some("/state/x/config".into()),
            model: None,
            prompt: "do the task".into(),
            output_schema: None,
            output_last_message: None,
            config_overrides: Vec::new(),
        });
        assert_eq!(via_adapter, via_free_function);
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
                    "--model",
                    "haiku",
                ],
            ),
            (
                "codex",
                vec![
                    "exec",
                    "--json",
                    "--skip-git-repo-check",
                    "-C",
                    "/wt",
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
        ];
        // The env block, keys and values, in order. Captured from the same pre-axis run: the axis
        // must be provably invisible in every channel a harness is configured through, not only
        // the one it happens to compile into on claude.
        let expected_env: Vec<(&str, Vec<(&str, &str)>)> = vec![
            (
                "claude",
                vec![("ANTHROPIC_BASE_URL", "http://127.0.0.1:8099")],
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
                    ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
                    ("OPENCODE_DISABLE_SHARE", "1"),
                    ("OPENCODE_DB", ":memory:"),
                    ("PWD", "/wt"),
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
                "codex" => 1275,
                "gemini" => 718,
                "opencode" => 962,
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
            ClaudeCodeAdapter.tool_name(agent_type::TOOL_READ).unwrap(),
            "Read",
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
            GeminiAdapter.tool_name(agent_type::TOOL_READ).unwrap(),
            "read_file"
        );
        assert!(
            !gemini::is_edit_tool("read_file"),
            "read is not an edit tool, or declaring it would compile auto_edit and grant writes"
        );
        assert_eq!(
            OpenCodeAdapter.tool_name(agent_type::TOOL_READ).unwrap(),
            "read",
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
            let adapter = adapter_for(t.harness).unwrap();
            for tool in &t.tools {
                adapter.tool_name(tool).unwrap_or_else(|e| {
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
            GeminiAdapter.tool_name(agent_type::TOOL_WRITE).unwrap(),
            "write_file"
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
            CodexAdapter.tool_name(agent_type::TOOL_WRITE).unwrap(),
            format!("sandbox:{}", codex::SANDBOX_MODE),
            "§3.1 names this exact string for this exact harness"
        );
        assert_eq!(
            OpenCodeAdapter.tool_name(agent_type::TOOL_WRITE).unwrap(),
            "write"
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
        let expected = serde_json::to_string_pretty(&claude_code::mcp_config_json(&McpEnv {
            bridge: "/bin/marion-supervisor".into(),
            repo: "/repo".into(),
            state: "/state".into(),
            // The `/v1` form, which is what the bridge hands a Codex grandchild.
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-root".into()),
            agent_type: "claude".into(),
            depth: 0,
            node_token: None,
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
            &codex::BridgeEnv {
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
                tools: vec![],
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
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-root".into()),
            agent_type: "claude".into(),
            depth: 0,
            node_token: None,
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
            },
            Some(&opencode::BridgeEnv {
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
        }
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
    #[test]
    fn no_config_file_any_adapter_emits_ever_escapes_marions_own_agent_dir() {
        for auth in [Auth::Canned, Auth::Inherited] {
            let mut checked = 0;
            let mut bound: Vec<Harness> = Vec::new();
            for h in Harness::ALL {
                let spec = LaunchSpec {
                    auth,
                    // What `--live` implies: marion names no endpoint and mints no credential.
                    base_url: match auth {
                        Auth::Canned => spec_for(h).base_url,
                        Auth::Inherited => None,
                    },
                    api_key: match auth {
                        Auth::Canned => spec_for(h).api_key,
                        Auth::Inherited => None,
                    },
                    // The canned default names marion's own generated provider block, which a live
                    // opencode node deliberately does not write (see the refusal it earns).
                    model: match (h, auth) {
                        (Harness::OpenCode, Auth::Inherited) => {
                            Some("anthropic/claude-sonnet-4-5".into())
                        }
                        _ => spec_for(h).model,
                    },
                    ..spec_for(h)
                };
                let adapter = adapter_for(h).unwrap();
                let Ok(files) = adapter.config_files(&spec, &ctx()) else {
                    continue;
                };
                // Whichever route this adapter took, it took *a* route, and the route it named is
                // the one it actually used.
                match adapter.mcp_route(&spec) {
                    McpRoute::Document => assert!(
                        !files.is_empty(),
                        "{h} under {auth:?}: names a document route and wrote none"
                    ),
                    McpRoute::Environment(k) => {
                        assert!(
                            files.is_empty(),
                            "{h} under {auth:?}: an env route that also writes files has two \
                             declarations and no single authority"
                        );
                        let inv = adapter
                            .compile(&spec, &ctx())
                            .unwrap_or_else(|e| panic!("{h} under {auth:?}: {e}"));
                        let (_, v) =
                            inv.env.iter().find(|(n, _)| n == k).unwrap_or_else(|| {
                                panic!("{h} under {auth:?}: ${k} was never set")
                            });
                        assert!(
                            v.contains("\"mcp\"") && v.contains(opencode::MCP_ALIAS),
                            "{h} under {auth:?}: ${k} carries no marion declaration: {v}"
                        );
                    }
                    McpRoute::Argv(key) => {
                        assert!(
                            files.is_empty(),
                            "{h} under {auth:?}: an argv route that also writes files has two \
                             declarations and no single authority"
                        );
                        let inv = adapter
                            .compile(&spec, &ctx())
                            .unwrap_or_else(|e| panic!("{h} under {auth:?}: {e}"));
                        assert!(
                            inv.args.iter().any(|a| a.contains(key)),
                            "{h} under {auth:?}: argv carries no marion declaration: {:?}",
                            inv.args
                        );
                    }
                    McpRoute::None => {
                        panic!("{h} under {auth:?}: asked for marion's bridge and routed nowhere")
                    }
                }
                bound.push(h);
                for (path, _) in &files {
                    assert!(
                        path.starts_with(&spec.config_dir),
                        "{h} under {auth:?} would write {} outside its agent dir {} — §6.4: \
                         marion never mutates the user's real harness config",
                        path.display(),
                        spec.config_dir.display()
                    );
                    assert!(
                        path.is_absolute(),
                        "{h} under {auth:?}: {} is relative, so where it lands depends on the \
                         writer's cwd",
                        path.display()
                    );
                }
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
        assert!(
            inv.env.is_empty(),
            "the three are the whole overlay, so a live node's env additions are empty: {:?}",
            inv.env
        );
        // And the isolation that was never an overlay is untouched.
        assert!(
            !inv.env.iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"),
            "isolating it breaks OAuth, under --live most of all"
        );
    }

    /// The other half: nothing *else* changes. A live node still gets the fileless MCP declaration
    /// and still excludes the user's settings — those are what keep §6.4's MUST true when the
    /// credential is real, so a "live means don't isolate anything" reading would be the exact
    /// mistake this pins shut.
    #[test]
    fn a_live_claude_node_keeps_the_same_argv_as_a_canned_one() {
        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..claude_spec()
        };
        let canned = claude_spec();
        assert_eq!(
            ClaudeCodeAdapter.compile(&live, &ctx()).unwrap().args,
            ClaudeCodeAdapter.compile(&canned, &ctx()).unwrap().args,
            "live differs from canned in env only; argv is the measured 2.1.220 launch either way"
        );
        let inv = ClaudeCodeAdapter.compile(&live, &ctx()).unwrap();
        assert!(inv.args.iter().any(|a| a == "--strict-mcp-config"));
        let i = inv.args.iter().position(|a| a == "--mcp-config").unwrap();
        assert_eq!(
            PathBuf::from(&inv.args[i + 1]),
            ClaudeCodeAdapter.config_files(&live, &ctx()).unwrap()[0].0,
            "the declaration a live node reads is still the one marion wrote in its agent dir"
        );
        let i = inv
            .args
            .iter()
            .position(|a| a == "--setting-sources")
            .unwrap();
        assert_eq!(
            inv.args[i + 1],
            "",
            "the user's settings, plugins and hooks stay out under --live too"
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
            let a = adapter_for(h).unwrap();
            assert_eq!(
                a.compile(&explicit, &ctx()).ok(),
                a.compile(&spec_for(h), &ctx()).ok(),
                "{h}: naming the default must not change the compile"
            );
            assert_eq!(
                a.config_files(&explicit, &ctx()).ok(),
                a.config_files(&spec_for(h), &ctx()).ok(),
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
                serde_json::to_string_pretty(&gemini::settings_json(Some(&gemini::BridgeEnv {
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
                    },
                    Some(&opencode::BridgeEnv {
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
            let files = adapter_for(h)
                .unwrap()
                .config_files(&spec_for(h), &ctx)
                .unwrap_or_else(|e| panic!("{h}: {e}"));
            let doc = files
                .first()
                .map(|(_, c)| c.clone())
                .unwrap_or_else(|| panic!("{h}: emitted no configuration document"));
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
            let files = adapter_for(h)
                .unwrap()
                .config_files(&spec_for(h), &ctx)
                .unwrap_or_else(|e| panic!("{h}: {e}"));
            let doc = files
                .first()
                .map(|(_, c)| c.clone())
                .unwrap_or_else(|| panic!("{h}: emitted no configuration document"));
            assert!(
                doc.contains(claude_code::NODE_TOKEN_ENV),
                "{h}: {} is not in its bridge env:\n{doc}",
                claude_code::NODE_TOKEN_ENV
            );
            assert!(
                doc.contains("\"MARION-TOKEN-VALUE-4e1b\""),
                "{h}: the token VALUE must be carried, not just its key:\n{doc}"
            );
        }
    }

    /// **Present or absent, never empty** — [`claude_code::BASE_URL_ENV`]'s rule, applied to the one
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
            let doc = adapter_for(h)
                .unwrap()
                .config_files(&spec_for(h), &ctx)
                .unwrap_or_else(|e| panic!("{h}: {e}"))[0]
                .1
                .clone();
            assert!(
                !doc.contains(claude_code::NODE_TOKEN_ENV),
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

    /// The auth selection a live node declares is the operator's own, read from their settings —
    /// and never the canned `gemini-api-key`, which under `--live` has no key behind it and fails
    /// with S12's code 41.
    #[test]
    fn a_live_gemini_node_declares_an_auth_type_it_could_actually_authenticate_with() {
        let live = LaunchSpec {
            auth: Auth::Inherited,
            base_url: None,
            api_key: None,
            ..gemini_spec()
        };
        let files = GeminiAdapter.config_files(&live, &ctx()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&files[0].1).unwrap();
        assert_eq!(
            v["security"]["auth"]["selectedType"],
            serde_json::json!(gemini::live_auth_type()),
            "the operator's real selection, or the oauth-personal fallback — not marion's"
        );
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
                "OPENCODE_DISABLE_AUTOUPDATE",
                "OPENCODE_DISABLE_SHARE",
                "OPENCODE_DB",
                // Placement, not isolation: opencode resolves its project directory from the
                // environment and re-enters `$PWD`, so a node given only `cwd` works in the
                // directory marion was launched from. See
                // `opencode::tests::the_node_is_placed_in_its_own_cwd_by_pwd_too_not_only_by_chdir`.
                "PWD",
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

    /// **Live mode is removal here too, and this is the list of what is removed** — asserted by
    /// name, because the failure defended against is one of the two creeping back. `CODEX_HOME`
    /// pointed anywhere but the operator's home hides the very `auth.json` the node exists to use,
    /// and `MARION_DUMMY_KEY` is a minted placeholder standing beside a real credential.
    #[test]
    fn a_live_codex_node_carries_neither_codex_home_nor_the_minted_key() {
        let inv = CodexAdapter.compile(&codex_live_spec(), &ctx()).unwrap();
        for k in ["CODEX_HOME", "MARION_DUMMY_KEY"] {
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
        assert!(
            !inv.args.iter().any(|a| a == "-c" || a == "-m"),
            "{:?}",
            inv.args
        );
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

    /// **`marion/default` names marion's own generated plumbing, and a live node generates none.**
    /// It would resolve to no provider at all — S13 measured that as
    /// `Error: {"name":"UnknownError",…}`, exit 1 — so it is refused by name at compile time
    /// instead, with the refusal saying what to pass instead.
    #[test]
    fn the_canned_default_model_is_refused_under_live_rather_than_resolving_to_no_provider() {
        let live = LaunchSpec {
            model: Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL.into()),
            ..opencode_live_spec()
        };
        let e = OpenCodeAdapter.compile(&live, &ctx()).unwrap_err();
        assert!(
            matches!(
                &e,
                HarnessError::MissingInput {
                    harness: Harness::OpenCode,
                    ..
                }
            ),
            "{e}"
        );
        assert!(
            e.to_string().contains("marion/default"),
            "the refusal must name the problem: {e}"
        );
        // And it is a *live* refusal only — canned is what that default exists for.
        assert!(
            OpenCodeAdapter
                .compile(
                    &LaunchSpec {
                        model: Some(marion_core::agent_type::OPENCODE_DEFAULT_MODEL.into()),
                        ..opencode_spec()
                    },
                    &ctx()
                )
                .is_ok()
        );
        // A real provider/model is accepted live.
        assert!(
            OpenCodeAdapter
                .compile(&opencode_live_spec(), &ctx())
                .is_ok()
        );
    }

    /// **The route is stated, never inferred — and every adapter states one.** This is what keeps
    /// `RootError::NoMcpDeclaration` from becoming a hole: "declares by another route" and "declared
    /// nothing at all" are different answers here, and only the first is legal.
    #[test]
    fn every_adapter_states_the_route_its_mcp_declaration_travels_on() {
        for h in Harness::ALL {
            let a = adapter_for(h).unwrap();
            let spec = spec_for(h);
            match a.mcp_route(&spec) {
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
            let a = adapter_for(h).unwrap();
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
            .filter(|h| adapter_for(*h).unwrap().pane_surfaces().is_some())
            .collect();
        assert!(
            !panes.is_empty(),
            "no harness has a pane shape, so `marion attach` has nothing to reach"
        );
        for h in panes {
            let s = adapter_for(h).unwrap().pane_surfaces().unwrap();
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

    /// A harness with no pane shape refuses by name rather than compiling the headless one.
    ///
    /// Mutation: default `compile_pane` to `self.compile(spec, ctx)`. This fails.
    #[test]
    fn a_harness_with_no_pane_shape_refuses_rather_than_launching_the_headless_one() {
        for h in Harness::ALL {
            let a = adapter_for(h).unwrap();
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
            let adapter = adapter_for(h).unwrap();
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
        let cases: [(Harness, &str, &str, MarionCall); 8] = [
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
        ];
        for (h, label, stream, want) in cases {
            assert_eq!(
                adapter_for(h).unwrap().marion_calls(stream),
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
        let unanswered: [(Harness, &str); 3] = [
            (
                Harness::ClaudeCode,
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__marion__spawn","input":{}}]}}"#,
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
            let got = adapter_for(h).unwrap().marion_calls(stream);
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
            .map(|h| adapter_for(h).unwrap())
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
        let cases: [(Harness, Evidence, bool); 4] = [
            // A per-tool allowlist with `write` not in it: the mutating tool is simply absent.
            (Harness::ClaudeCode, Withholds("Write"), false),
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
        ];
        for (harness, evidence, writes) in cases {
            let adapter =
                adapter_for(harness).unwrap_or_else(|e| panic!("{harness} has an adapter: {e}"));
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
}
