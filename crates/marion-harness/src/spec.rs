//! The declarative harness spec: **one row of data per harness, one renderer for all of them.**
//!
//! Every harness marion drives is launched the same way — a program, an argv, an environment —
//! and until this module existed each adapter wrote that launch out by hand, so 82% of the adapter
//! seam was per-harness code that differed in *values* and not in *shape*. A sixth harness cost
//! four hundred lines of it. This module turns the shape into a small closed vocabulary ([`Arg`],
//! [`Env`]) and the values into a table ([`HarnessSpec`]), so that a harness is a **row**, and the
//! only code a row needs is the handful of measured decisions no table can hold — a refusal, a
//! model rule, a derived URL — which stay in that adapter's `fields` hook and feed the renderer
//! through [`Fields`].
//!
//! # What is data and what is code, and why the line is where it is
//!
//! *Data*: which flags exist, in what order, joined how, gated on what. Those are facts about a
//! binary's `--help`, transcribed once from a measurement and never computed.
//!
//! *Code*: whether a launch is **allowed** — claude refuses an argv prompt because 2.1.220 takes
//! turn one before its MCP server is up; a canned copilot refuses a missing base URL because the CLI goes looking for a GitHub login. Those
//! are measured *decisions*, each with a fixture behind it, and enum-izing them would turn a
//! sentence of evidence into a variant nobody can check. They live in the adapters.
//!
//! # The risk a table invites
//!
//! A row is easy to write and easy to guess. Two rules keep it honest: `note` is **mandatory** and
//! names the spike that measured the row, and nothing here defaults — an absent value is rendered
//! as absent, never as a plausible stand-in. `adapter::tests::spec_render_matches_the_adapter_that_
//! measured_it` is the invariant: every row renders byte-for-byte what the hand-written adapter it
//! replaced compiled.

use std::path::{Path, PathBuf};
use std::time::Duration;

use marion_core::harness::Harness;
use marion_core::provider::{KeyHeader, Wire};

use crate::auth::Auth;
use crate::grammar::StreamGrammar;
use crate::invocation::Invocation;
use crate::jsonl_channel::JsonlChannel;
use crate::mcp_bridge::{BridgeEnv, NODE_TOKEN_FILE_ENV};
use crate::rpc_channel::RpcChannel;
use crate::surfaces::{ExecutionSurfaces, TypedKind};

/// The name marion gives its own MCP server in every declaration, and therefore half of every
/// harness's model-facing spelling of marion's tools. **It must not contain `_`**: the spellings
/// join server and tool with underscores (`mcp__<server>__<tool>`, `<server>_<tool>`), and a
/// policy engine that splits on them mis-parses a name with extra ones, **silently** (S12 measured
/// that on the since-retired gemini CLI).
pub const MCP_ALIAS: &str = "marion";

/// One harness, as a row: what its launch looks like, stated as data.
#[derive(Debug, Clone, Copy)]
pub struct HarnessSpec {
    /// Documents an overlaying launch (canned, endpoint) writes under its own config dir before
    /// the harness starts, `(path relative to the config dir, body)`: what a harness home the row
    /// relocates there needs so the harness opens as it would on the operator's own (claude's
    /// `.claude.json` past onboarding). Never written under live auth, where the home is theirs.
    pub overlay_documents: &'static [(&'static str, &'static str)],
    /// What the harness's own login reads from the operator's environment — the one exception to
    /// the credentials every launch withholds ([`crate::env_filter`]).
    pub login_env: crate::env_filter::LoginEnv,
    pub harness: Harness,
    /// §3.4's point in the cross-product this harness runs at.
    pub surfaces: Surfaces,
    /// The binary, or `None` where the launch itself names it — ACP, whose program is the agent's
    /// own and arrives in [`Fields::program`].
    pub program: Option<&'static str>,
    /// **The vendor whose models this harness is the own CLI of** (`anthropic` for claude-code), or
    /// `None` for a harness that runs any vendor's model. It is the family a node is judged by
    /// when its model is unnamed or unrecognised, so a default reviewer can come from another
    /// family (`marion_core::review::model_family`).
    pub vendor: Option<&'static str>,
    /// **The versions marion verified this harness against**: the first is the pin its measured
    /// behaviours were taken from, the rest were observed green since, in the order they were
    /// admitted (`scripts/admit-harness.sh` appends; the evidence for each is beside its program
    /// in `marion_testsupport::PINNED_HARNESSES`). The test gate admits exactly these, and doctor
    /// notes an installed version newer than the newest. Empty where the version is not the row's
    /// own — an ACP agent's.
    pub verified: &'static [&'static str],
    /// The headless argv, in order.
    pub argv: &'static [Arg],
    /// The argv of the pane shape, where the harness has one (§3.4's `opaque`). `None` is a
    /// refusal by name at render time, never a fallback to [`Self::argv`].
    pub pane: Option<&'static [Arg]>,
    /// The environment, in order. Rows gated on [`When`] are the whole of live mode: *"live is a
    /// removal"* is a property of the table, not of five hand-written branches.
    pub env: &'static [Env],
    /// How this harness's output stream is read (§6.1 step 9), or `None` where the reader is code
    /// — ACP, whose stream shape is per agent ([`crate::grammar`]'s module docs).
    pub stream: Option<&'static StreamGrammar>,
    /// §3.1's **availability** axis: marion's vocabulary (`marion_core::agent_type::TOOL_READ`,
    /// …) to this harness's own name for it. **A verb not in this list is refused by name**, never
    /// dropped — a launch that quietly loses a tool is §11 item 24's silent failure. Empty is the
    /// honest row for a harness with no availability axis at all (ACP). **A verb may appear more
    /// than once**, where the harness's tool for it depends on the model (copilot's `write`): the
    /// launch then offers every one of them.
    ///
    /// Answering does not imply compiling: on codex and opencode a declared `write` names a tool
    /// the harness already grants unconditionally, and the row's argv carries nothing for it.
    pub tool_names: &'static [(&'static str, &'static str)],
    /// How this harness's model spells one of marion's own tools.
    pub spelling: Spelling,
    /// Which channel the MCP declaration travels on, under each auth mode.
    pub mcp: McpRoutes,
    /// [`Self::mcp`]'s live route **in the detail a launch needs to take it**: the flag or
    /// variable that carries marion's declaration onto a node whose configuration surface is the
    /// operator's own, and the serialisation it carries. `None` where the row has no launch-time
    /// channel — ACP, whose declaration is a `session/new` request after launch. The native
    /// facade's whole injection is derived from this field ([`crate::native::native_adapter`]),
    /// which is what keeps "marion in front of the operator's own harness" from becoming five
    /// hand-written adapters; the spec sweep checks it agrees with `mcp.live` and with what the
    /// managed live launch writes.
    pub live_declaration: Option<LiveDeclaration>,
    /// How the node's capability token reaches the bridge, under each auth mode — inside the
    /// declaration, or withheld from it and carried on the harness's own environment. A
    /// declaration that rides argv must withhold it, because `ps` shows argv to every user on the
    /// machine; [`crate::sweep::validate`] holds every row to it.
    pub token: TokenCarriers,
    /// §6.7's `TaskContract.allowed_tools`: what the audit record says this launch was constrained
    /// by, in this harness's own vocabulary.
    pub constraint: Constraint,
    /// How this harness names a session to resume on its command line, as measured on the
    /// installed binary's `--help` — or `None`, where it has no measured way, and a launch that
    /// asks for one is refused by name rather than started fresh under a resumed session's id.
    /// **Data, so the resume feature has no per-harness branch**: [`Fields::resume`] is seeded from
    /// `LaunchSpec::resume` for every adapter alike, and this row decides how it is spelled.
    pub resume: Option<Resume>,
    /// How a node of this harness is kept from **updating itself under marion**, measured on the
    /// installed binary — an environment variable, a pair on the row's override channel, keys in
    /// the declaration document the row already emits, or the explicit statement that none is
    /// known. Rendered into every shape (headless, pane) and into the native overlay by the one
    /// renderer, so `marion <harness>` sessions carry it too. **Data, so no launch can forget
    /// it**: a binary that replaces itself mid-run (opencode 1.17.3's `Updating to v1.18.29...`;
    /// codex 0.147.0's `Update available` prompt, which an Enter installs; claude's background
    /// updater) changes the program under a running node and interrupts a facade session.
    pub updates: UpdatePolicy,
    /// **What starting this harness costs**, as measured CPU time — the one number every bound
    /// that contains a boot is derived from ([`Boot::budget`]), in the supervisor's readiness
    /// waits and in the tests alike, so no wall clock that includes a boot is a constant typed at
    /// its use.
    pub boot: Boot,
    /// How the bridge tells this harness's model that a **backgrounded child has finished,
    /// without a `wait`** — a notification pushed on the MCP pipe after the handle's own reply.
    /// Rendered by the one renderer into the pane shape and the native prefix only ([`Push`]
    /// says why never headless), so no interactive launch can forget the flag that enables it.
    pub push: Push,
    /// How a **headless** node is let call marion's tools with no operator there to answer a
    /// prompt — the grant, as one of a few measured shapes. Every harness was measured denying,
    /// cancelling or classifier-declining an unapproved marion call at exit 0, so a row that
    /// forgot its grant would launch nodes that report nothing and say nothing. Rendered by the
    /// row's own argv, env and documents where marion compiles it; reported by `marion doctor`
    /// where the operator must ([`Approval::OperatorAllowlist`]).
    pub approval: Approval,
    /// How a node of this harness is made **read-only** — a reviewer's launch. One of a few
    /// measured shapes ([`ReadOnly`]); rendered by [`render`] only when the launch asks for it, and
    /// held by the sweep `every_row_states_how_a_read_only_node_is_kept_from_writing`.
    pub read_only: ReadOnly,
    /// The `clientInfo.name` this harness sends in MCP `initialize`, where it is measured — so a
    /// bridge the harness started itself (`marion mcp` in an operator's own MCP configuration,
    /// where no `MARION_AGENT_TYPE` names a row) can still find this row's [`Self::push`].
    /// `None` where it was never observed; such a bridge falls back to [`Push::McpLog`].
    pub client_name: Option<&'static str>,
    /// **Whole stderr lines this harness prints on every run**, whatever happened — not news, so
    /// never what a parent reads as the child's last word ([`Self::quiet_stderr`]). Empty where
    /// none was seen.
    pub stderr_boilerplate: &'static [&'static str],
    /// **How a message reaches this node's next turn**, per shape — the one mechanism behind both
    /// a child's end pushed to its parent and a parent's or operator's steer into a child.
    /// Resolved by [`delivery_for`] alone; [`crate::sweep::validate`] checks each strategy against
    /// the rest of the row.
    pub delivery: Deliveries,
    /// **How a running turn is ended early**, per shape — what a cancel writes before its grace,
    /// and what a node past its wall clock gets before the kill. Resolved by [`abort_for`] alone;
    /// [`crate::sweep::validate`] checks each verb against the rest of the row.
    pub abort: Aborts,
    /// **The dialogs this harness's TUI can put up before its composer exists** — measured first
    /// screens in a fresh directory. A paste typed into one answers it: claude 2.1.283's folder
    /// trust defaults to `No, exit`, so a paste + CR quits the session. The paste driver holds
    /// while one is on screen, and answers only a dialog the row marks answerable, only in a
    /// workspace marion created ([`DialogAnswer`]).
    pub boot_dialogs: BootDialogs,
    /// The wires this harness can be pointed at in **endpoint mode**, as recipes, in its order of
    /// preference — only ones this row can actually render, so a provider serving a wire the
    /// harness could speak but the row cannot yet aim it at is refused rather than half-configured.
    /// Endpoint resolution takes the first one the provider serves natively, and the renderer
    /// applies that recipe; empty refuses every endpoint launch of this harness by name.
    pub wires: &'static [WireRecipe],
    /// How this harness is pointed at a profile directory the operator logged into — or `None`,
    /// with the reason in `profile::tests`' sweep. Applied by [`render`] under live auth only.
    pub profile: Option<crate::profile::ProfileCarrier>,
    /// **Mandatory.** The spike that measured this row, so a reader can tell a transcription from
    /// a guess. The spec sweep refuses an empty one.
    pub note: &'static str,
    /// **The launch inputs this row cannot run without**, per auth mode, in the order they are
    /// checked — each refused by name with its own measured reason before the row's hook runs
    /// ([`crate::adapter::requirements`]). Empty where the row needs nothing a launch could omit.
    pub requires: &'static [Requirement],
    /// How §3.1's two axes are compiled from a launch ([`crate::HarnessAdapter::axes`]).
    pub axes: AxesRule,
    /// **Documents the harness reads beside its declaration**, written under the node's own
    /// directory before launch, per auth mode ([`ConfigFile`]) — qwen's settings with its memory
    /// side turn switched off. Empty where the row writes none but its declaration.
    pub files: &'static [ConfigFile],
    /// How the launch's model reaches the row's [`Field::Model`].
    pub model: ModelForm,
    /// Whether the first turn waits on the bridge's readiness marker (§6.1 step 8).
    pub readiness: Readiness,
    /// What the harness's software claims before any surface clips it
    /// ([`crate::caps::advertised`]): every `true` names its measurement in the row.
    pub advertised: Advertised,
    /// **Can a node on this harness change a file with an empty `tools:` list?**
    ///
    /// §6.6's occupancy rule is *"at most one node with **write tools** per cwd"*, and reading that
    /// off `marion_core::agent_type::AgentType::tools` alone gets the answer backwards on half the
    /// harnesses. §3.1's `tools:` is a **grant list** — what marion must positively enable that the
    /// harness would not do on its own — and two of the four need no such grant. That asymmetry is
    /// already stated in `marion_core::agent_type::TOOL_READ`'s table, which says outright that
    /// answering `read` with codex's shell *"would let a reader of `tools: [read]` believe a codex
    /// node was read-only when it is not"*. The same trap, one field over.
    ///
    /// What each adapter actually compiles, which is where these two answers come from:
    ///
    /// | harness | with `tools: []` | source |
    /// |---|---|---|
    /// | codex | **writes** — `sandbox_mode = "workspace-write"` on every node marion configures, and `codex exec` has no per-tool knob at all | `codex::SANDBOX_MODE` |
    /// | opencode | **writes** — marion compiles no constraint whatsoever | `opencode::NO_COMPILED_TOOL_CONSTRAINT` |
    /// | claude-code | withheld — `--tools ""` unless `write` is declared | `ClaudeCodeAdapter::permission_axis` |
    /// | copilot | withheld — `--available-tools` names only marion's verbs unless `write` is declared, and an ungranted `create` is `denied` (measured, `tests/fixtures/s24/`) | `CopilotAdapter::permission_axis` |
    /// | acp | **writes** — twice over; see below | `acp::NO_TOOL_AVAILABILITY_SURFACE` |
    ///
    /// **The `acp` arm is decided, not inherited.** Two independent reasons, either sufficient:
    ///
    /// 1. *ACP has no tool-availability surface at all.* There is no field in `initialize` or
    ///    `session/new` that narrows an agent's own tools, so marion compiles no constraint — the
    ///    opencode row's situation, one protocol up. S21 measured it: an `opencode acp` session
    ///    marion opened had `write`, `edit` and `bash` in scope with marion having asked for
    ///    nothing. Worse, marion's own `initialize` **hands the agent a write channel**
    ///    (`clientCapabilities.fs.writeTextFile: true`, [`crate::acp::initialize_request`]), so an ACP node with `tools: []` can change a
    ///    file *through marion*.
    /// 2. *marion does not know which agent this is until after the process exists.* §5.2's `acp`
    ///    row is one adapter over many agents and the identity arrives in the handshake, so any
    ///    `false` here would be a claim about an agent nobody had named yet.
    ///
    /// Both land on the same answer as the erring-towards-`true` rule below, which is the only
    /// reason a fifth arm is safe to add at all.
    ///
    /// **Erring towards `true` is the only safe direction here** and is what the two `true` arms
    /// are. A false `true` costs a caller a refusal they can lift with one documented parameter
    /// (`allow_concurrent_writes`); a false `false` lets two agents write one tree, which §6.6
    /// calls worse than two humans doing it — each harness keeps its own checkpoint state, so a
    /// restore in one silently reverts the other's work — and the caller finds out afterwards, if
    /// at all.
    ///
    /// Stated by every row, so a new harness cannot inherit an answer nobody measured for it; the
    /// adapter sweep
    /// `the_harnesses_that_write_without_a_grant_are_the_ones_that_compile_no_constraint` pins it
    /// against what each row compiles.
    pub writes_without_grant: bool,
    /// **How this harness contains the node it runs** ([`crate::containment`]): with an OS sandbox
    /// of its own, or not at all. A caller may spawn only a type at least as contained as itself,
    /// and a child's verification runs inside its row's sandbox where there is one. Stated by every
    /// row, so a new harness cannot inherit a sandbox nobody measured.
    pub containment: crate::containment::ContainmentRule,
    /// **How marion's own OS sandbox meets this harness** ([`crate::os_sandbox`]): wrapped,
    /// replacing a sandbox of its own, or not applied and why. Stated by every row, so a new
    /// harness is never run under a profile nobody measured it under.
    pub os_sandbox: crate::os_sandbox::OsSandboxRule,
    /// **The launch flags that put this harness in a read-only mode** ([`crate::authority`]): a
    /// session the operator started that way is a read-only node that delegates nothing that
    /// writes. Empty where no read-only mode is measured.
    pub read_only_modes: &'static [crate::authority::ReadOnlyMode],
}

/// **The capabilities a row's binary claims**, keyed by version: what every version measured
/// has, and what arrived from a version on. Nothing is claimed before the first measured version,
/// and an unreadable version gets only `always`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Advertised {
    pub always: crate::caps::Capabilities,
    pub from_version: &'static [(crate::caps::Capabilities, (u64, u64, u64))],
}

impl Advertised {
    pub const NONE: Advertised = Advertised {
        always: crate::caps::Capabilities::NONE,
        from_version: &[],
    };

    /// The claim at `version`.
    pub fn at(self, version: &str) -> crate::caps::Capabilities {
        self.from_version
            .iter()
            .filter(|(_, since)| crate::caps::at_least(version, *since))
            .fold(self.always, |caps, (more, _)| caps.join(*more))
    }
}

/// **Whether a node's first turn is withheld until its bridge has answered `tools/list`.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The prompt rides argv, or the protocol itself gates the turn; no marker is required.
    Ungated,
    /// The prompt is a frame written after the bridge touches its marker. A headless launch with
    /// an argv prompt is refused with `prompt`, and a declaration with no marker to touch with
    /// `marker` — both are a first turn taken with no tools, which nothing else reports.
    Marker {
        prompt: &'static str,
        marker: &'static str,
    },
}

/// **How a row spells the launch's model**, before its hook sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelForm {
    /// The model the launch names, verbatim.
    AsGiven,
    /// None under [`Auth::Canned`], whose endpoint serves one model whatever is asked; verbatim
    /// otherwise.
    OmitUnderCanned,
    /// The row's hook spells it, because no form above says what it measured; the row says why.
    Hook,
}

impl ModelForm {
    pub fn apply(self, auth: Auth, model: Option<String>) -> Option<String> {
        match (self, auth) {
            (ModelForm::OmitUnderCanned, Auth::Canned) => None,
            _ => model,
        }
    }
}

/// **How a row compiles §3.1's availability and permission axes**, from the declared tools mapped
/// into its own spelling and marion's own verbs ([`crate::LaunchSpec::allowed_tools`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxesRule {
    /// Availability is the declared tools; permission is marion's verbs plus the same tools.
    Split,
    /// A coarse mode instead of lists: `mode` exactly when a declared tool is one of `when_any`,
    /// none otherwise. The permission list is the declared tools among `by_name`: those no mode
    /// short of the widest approves, granted by name instead.
    Mode {
        mode: &'static str,
        when_any: &'static [&'static str],
        by_name: &'static [&'static str],
    },
    /// One list on both axes: marion's verbs plus the declared tools. `refuse_empty` refuses an
    /// empty list by name where the harness reads an empty list as no constraint at all.
    OneList { refuse_empty: Option<&'static str> },
    /// The row's hook computes them, because no rule above says what it measured; the row says why.
    Hook,
}

/// **One input a row cannot launch without**, under the auth modes it names. Row data rather
/// than a branch in the row's hook, so every row's refusals are stated the same way and swept by
/// one test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirement {
    pub modes: Modes,
    pub need: Need,
    /// The refusal, in the row's words: what the harness does without the input, as measured.
    pub why: &'static str,
}

/// The auth modes a [`Requirement`] binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modes {
    All,
    /// Canned and endpoint: the modes that overlay a provider ([`Auth::overlays`]).
    Overlay,
    Canned,
    Endpoint,
    /// Live: the operator's own login.
    Inherited,
}

impl Modes {
    pub const fn covers(self, auth: Auth) -> bool {
        match self {
            Modes::All => true,
            Modes::Overlay => auth.overlays(),
            Modes::Canned => matches!(auth, Auth::Canned),
            Modes::Endpoint => matches!(auth, Auth::Endpoint),
            Modes::Inherited => matches!(auth, Auth::Inherited),
        }
    }
}

/// What a [`Requirement`] needs of the launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    BaseUrl,
    Model,
    ApiKey,
    /// Any model but this one — the row's canned default, which names marion's test endpoint's
    /// plumbing and nothing a live login serves. No model at all satisfies it.
    ModelOtherThan(&'static str),
    /// Nothing satisfies it: the row has no measured recipe for the mode.
    NoRecipe,
}

/// **How one harness speaks one wire in endpoint mode**: the wire, and the environment that
/// selects it on top of the row's own overlay rows — empty where the overlay already speaks it,
/// a switch such as copilot's `COPILOT_PROVIDER_WIRE_API` where the harness speaks several. A
/// variable the recipe names replaces the overlay row's value of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireRecipe {
    pub wire: Wire,
    pub env: &'static [(&'static str, &'static str)],
    /// The key headers this recipe can present ([`KeyHeader`]), each with the variables that make
    /// the harness present the key there — applied over the row's own, by name. A provider whose
    /// header is not listed is refused by name rather than sent the key in another header.
    pub keys: &'static [KeyRecipe],
    /// **Mandatory**: how the recipe was established, as for a row's own `note`.
    pub note: &'static str,
}

/// **How one recipe presents the key in one header**: the rows that do it, over the row's own
/// (empty where the row's overlay already does), and how that was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyRecipe {
    pub header: KeyHeader,
    pub env: &'static [Env],
    pub note: &'static str,
}

/// The one [`KeyRecipe`] most recipes carry: a Bearer key, presented by the row's own overlay.
pub const BEARER_BY_OVERLAY: KeyRecipe = KeyRecipe {
    header: KeyHeader::Bearer,
    env: &[],
    note: "the row's own overlay sends the key as `Authorization: Bearer`",
};

/// **How a backgrounded child's end reaches the parent model without a `wait`** — the
/// notification the bridge pushes on its stdio pipe, and the launch flag that makes the harness
/// deliver it.
///
/// One measured variant, one unmeasured fallback, one explicit absence. The sweep
/// `every_row_states_its_push_strategy_and_renders_its_argv_only_into_interactive_shapes` pins
/// where each row's [`Self::argv`] lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// A harness's own **channel notification**, measured on its row ([`ChannelPush`]): the frame
    /// the harness injects as a new user turn, the capability the server must declare for it to be
    /// delivered, and the launch flag an interactive launch needs. Never on a headless launch.
    Channel(&'static ChannelPush),
    /// MCP's own `notifications/message` (logging), under `capabilities.logging`. **Unmeasured**
    /// on every row that carries it: it is what the protocol lets any server send, and what a
    /// client may show, log or drop. No launch flag.
    McpLog,
    /// Nothing is pushed, or nothing pushed was **measured delivered**. ACP: the declaration is a
    /// `session/new` request after launch, so there is no stdio pipe of marion's to push on. agy
    /// 1.2.8: a `notifications/message` pushed after a call reached the pipe and the harness
    /// surfaced it nowhere, headless or in its TUI (s32).
    None,
}

/// **A harness's channel notification, as its row measured it** — [`Push::Channel`]'s data. The
/// frame is `{"method": method, "params": {"content": …, "meta": …}}`; the harness reads each
/// `meta` key as an attribute, so the keys are identifier-shaped.
#[derive(Debug, PartialEq, Eq)]
pub struct ChannelPush {
    /// The notification's method.
    pub method: &'static str,
    /// The key the server declares under `capabilities.experimental`; a harness drops the
    /// notification from a server that did not.
    pub capability: &'static str,
    /// The flag that makes an **interactive** launch deliver it.
    pub argv: &'static [&'static str],
}

impl Push {
    /// The argv that enables this push on an **interactive** launch — or nothing.
    pub const fn argv(self) -> &'static [&'static str] {
        match self {
            Push::Channel(c) => c.argv,
            Push::McpLog | Push::None => &[],
        }
    }
}

/// A row's [`TurnDelivery`] for each of the two shapes a node can run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deliveries {
    /// The node marion drives over pipes: a typed control channel, or a launch-only relaunch.
    pub headless: TurnDelivery,
    /// The node a person can watch: a pane marion hosts, or a native `marion <harness>` session.
    pub interactive: TurnDelivery,
}

/// Which of a node's two shapes a delivery is for. Not [`Shape`]: that names an argv, and an
/// interactive node is a pane **or** a native session, which render differently and take a
/// message the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeShape {
    Headless,
    Interactive,
}

/// **How marion hands a message to a node's next turn** — measured per harness and per shape
/// (S31, `tests/fixtures/s31-turn-delivery/`).
///
/// **Marion queues on its own side and delivers at a turn boundary, except where a row measured
/// the harness folding a mid-turn write safely.** S31 measured the typed surfaces under a write
/// made while a turn is in flight: Claude Code's stream-json and two ACP agents (opencode,
/// claude-agent-acp) fold it into the running turn — the model reads it at its next tool round,
/// with no completion of its own — while codex-acp never answers the first prompt and copilot's
/// ACP supersedes it with an empty `end_turn`. [`MidTurn`] carries that difference on the typed
/// row; every other strategy is turn-boundary only.
///
/// Every variant carries a `note` naming the measurement, `None` included — an absence says what
/// was searched, as [`UpdatePolicy::None`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnDelivery {
    /// A new user turn on the node's typed control channel: a stream-json `user` frame, an ACP
    /// `session/prompt`. Between turns it is always the next turn; `mid_turn` says what marion
    /// does with a message that arrives while a turn is in flight.
    TypedTurn {
        mid_turn: MidTurn,
        note: &'static str,
    },
    /// The next turn is a **relaunch of the same session**: the row's [`HarnessSpec::resume`]
    /// grammar with the message as the prompt, after the previous process exited.
    Continuation { note: &'static str },
    /// Claude Code's `notifications/claude/channel`, pushed on the MCP pipe marion's bridge holds
    /// ([`Push::Channel`]); the harness folds or queues it itself.
    McpChannel { note: &'static str },
    /// Typed into the node's terminal: `ESC[200~` + text + `ESC[201~` (always bracketed — codex
    /// turns an unbracketed burst's CR into a newline), then `submit` after `submit_delay_ms`,
    /// once `idle` says the TUI is waiting for input — and, before the first paste, once `boot`
    /// says its composer submits at all.
    TerminalPaste {
        idle: IdleSignal,
        boot: BootSignal,
        submit: &'static [u8],
        submit_delay_ms: u16,
        note: &'static str,
    },
    /// No measured way. A message for such a node is refused by name, quoting `note`.
    None { note: &'static str },
}

impl HarnessSpec {
    /// `stderr` without the lines [`Self::stderr_boilerplate`] names, trimmed. A line is dropped
    /// only when it is one of them whole, so an error that merely quotes one is kept.
    pub fn quiet_stderr(&self, stderr: &str) -> String {
        stderr
            .lines()
            .filter(|l| !self.stderr_boilerplate.contains(&l.trim()))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }
}

impl TurnDelivery {
    /// The measurement behind this strategy, or behind its absence.
    pub const fn note(self) -> &'static str {
        match self {
            TurnDelivery::TypedTurn { note, .. }
            | TurnDelivery::Continuation { note }
            | TurnDelivery::McpChannel { note }
            | TurnDelivery::TerminalPaste { note, .. }
            | TurnDelivery::None { note } => note,
        }
    }

    /// **When a message queued for a node taking turns this way reaches its model**: the clause a
    /// steer's acknowledgement ends with ("reaches codex-impl 8ea3 …"). Read off the strategy
    /// alone, so a row that changes strategy changes what marion promises, and no harness is named.
    pub const fn arrival(self) -> &'static str {
        match self {
            TurnDelivery::TypedTurn {
                mid_turn: MidTurn::Fold,
                ..
            } => "at its next tool round, or as its next turn if none is running",
            TurnDelivery::TypedTurn {
                mid_turn: MidTurn::Queue,
                ..
            } => "when its current turn ends, as its next turn",
            TurnDelivery::Continuation { .. } => {
                "when its current run ends: it takes messages only between runs, each as a \
                 relaunch of its session"
            }
            TurnDelivery::McpChannel { .. } => {
                "at its next tool round or turn, pushed on its MCP channel"
            }
            TurnDelivery::TerminalPaste { .. } => {
                "once its terminal goes quiet, typed in as its next turn"
            }
            TurnDelivery::None { .. } => "never: its harness has no measured way to take a turn",
        }
    }

    /// The paste S31 measured on codex, opencode, copilot and claude's TUIs: bracketed, then `\r`
    /// 50 ms later (0 ms submitted on all four; 50 is margin), once the terminal has been quiet
    /// for 1500 ms (busy spinners repaint at ≤ 454 ms, idle output is ~0). One constructor so the
    /// four rows cannot drift apart on values nobody measured separately; `boot` is the one value
    /// each row states for itself.
    pub const fn bracketed_paste(boot: BootSignal, note: &'static str) -> TurnDelivery {
        TurnDelivery::TerminalPaste {
            idle: IdleSignal::OutputQuiet { ms: 1500 },
            boot,
            submit: b"\r",
            submit_delay_ms: 50,
            note,
        }
    }
}

/// **What marion does with a message for a typed-turn node whose turn is still running** — the one
/// place S31 found the harnesses disagree (`tests/fixtures/s31-turn-delivery/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidTurn {
    /// Write it now: the harness folds it into the running turn at its next tool-result boundary
    /// (the model reads it in its next request of the same turn), or queues it as the next turn
    /// when the in-flight request is the turn's last. Measured on Claude Code's stream-json
    /// (`p0a/b3`, `p0a/b`, `p0a/b2`) and on the opencode and claude-agent-acp ACP agents
    /// (`p0a/acp-*-fold`). A fold emits no completion of its own, so marion's `MessageDelivered`
    /// is the acknowledgement and the node's answer is still its last completion.
    Fold,
    /// Hold it until the turn ends, then send it as the next turn: a write while a turn runs
    /// loses a response on this agent (codex-acp never answers the first prompt; copilot's ACP
    /// supersedes it with an empty `end_turn`), or it was never measured.
    Queue,
}

/// How marion tells an interactive node is waiting for input. One measured variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleSignal {
    /// No output for `ms` milliseconds.
    OutputQuiet { ms: u32 },
}

/// A row's [`BootDialog`]s, with the measurement behind them — `dialogs` empty and `note` saying
/// what was searched where none was seen, or why the first screen was not measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootDialogs {
    pub dialogs: &'static [BootDialog],
    /// Where the TUI keeps an answer marion gives with a dialog's [`DialogAnswer::Keys`].
    pub remembers: Remembers,
    pub note: &'static str,
}

/// **Where a TUI keeps the answer to one of its boot dialogs** — folder trust, above all, which a
/// harness writes down keyed by the directory's path (in a git worktree, the main checkout's).
/// marion answers a dialog only where the answer cannot land in the operator's own config
/// ([`Self::marion_may_answer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remembers {
    /// Nowhere past the session, or marion answers none of the row's dialogs.
    Nothing,
    /// Under the harness home an overlaying launch (canned or endpoint) relocates onto marion's
    /// config dir (codex's `CODEX_HOME`, claude's `CLAUDE_CONFIG_DIR`): the answer lands in
    /// marion's scratch and goes with it. Under live auth that home is the operator's own.
    CannedHome,
}

impl Remembers {
    /// **Whether marion may answer the row's boot dialog with its keys, under `auth`**: only where
    /// the answer cannot land in the operator's own config — nowhere past the session, or a home
    /// the overlay moved onto marion's directory (canned and endpoint alike). Under live auth that
    /// home is the operator's. Measured 2026-09-27 on claude 2.1.283 and codex 0.155.1: in a git
    /// worktree both key folder trust on the **main repository**, so an answer marion gave there
    /// would trust the operator's repository for good. Where they already trust it no dialog
    /// shows at all; where they do not, it is theirs.
    pub fn marion_may_answer(&self, auth: Auth) -> bool {
        match self {
            Remembers::Nothing => true,
            Remembers::CannedHome => auth.overlays(),
        }
    }
}

/// **One dialog a TUI can show before its composer**, recognised by `needle`: text on the screen
/// as the pty host reads it — visible characters, with every run of spaces, line breaks and
/// cursor moves read as one space (claude draws `No, exit` as `No,` `ESC[8G` `exit`).
///
/// Where the TUI marks its selection, the needle includes the marker and the selected option
/// (`❯ No, exit`): the answer's keys are only right for the selection that was measured, so a
/// release that moves the default no longer matches as answerable. Such a release still shows
/// the dialog, so a row lists a second, marker-free needle with [`DialogAnswer::Hold`] beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootDialog {
    pub needle: &'static str,
    /// **What the operator does about it where marion does not answer** — one action, with
    /// `{repo}` for the repository the node runs in. It is the held node's attention item and, past
    /// the grace, the reason it ends ([`BootDialog::action_for`]).
    pub action: &'static str,
    pub answer: DialogAnswer,
    pub note: &'static str,
}

impl BootDialog {
    /// [`Self::action`] for `repo`.
    pub fn action_for(&self, repo: &str) -> String {
        self.action.replace("{repo}", repo)
    }
}

/// What marion may do about a [`BootDialog`] on a node's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAnswer {
    /// Safe to answer **in a workspace marion created** — folder trust for a worktree marion made
    /// is marion's own decision — with these keys, written once, as measured. Anywhere else (an
    /// operator's own directory) the dialog is the operator's, and the paste waits as for
    /// [`DialogAnswer::Hold`].
    Keys(&'static [u8]),
    /// Never answered by marion: a login or a consent screen, or a dialog whose answer was not
    /// measured. The paste waits for someone else to dismiss it, and is dropped by name past the
    /// grace.
    Hold,
}

/// **What marks a TUI whose composer submits**, before marion's first paste into it: the boot
/// moment [`IdleSignal`]'s quiet is counted from. A paste typed before it is lost — not refused,
/// not queued — so the mark has to be the harness's own sign that its real input loop is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootSignal {
    /// The first text drawn under bracketed paste: the first screen *is* the composer. Copilot
    /// 1.0.83 turns bracketed paste on and then draws nothing for seconds, discarding a paste in
    /// that window, so bracketed paste alone is not the mark.
    FirstDraw,
    /// The first window title (OSC 0 or 2) set under bracketed paste. For a TUI that draws a
    /// **provisional composer** while its startup continues — one that takes a paste and drops
    /// Enter — and sets its title only once the real one is up, so no quiet stretch inside the
    /// provisional phase can pass for boot. The operator's own config can switch titles off
    /// (codex: `tui.terminal_title = []`), so its absence is not proof the TUI is still booting.
    WindowTitle,
}

impl BootSignal {
    /// **Whether the operator's own configuration can switch this mark off.** Such a mark that
    /// has not come by the paste grace says nothing about boot — a provisional composer lasts
    /// seconds — so a message is delivered on a drawn screen then, rather than lost to a
    /// preference. A mark nothing can switch off, missing that long, means the TUI never booted.
    pub const fn operator_can_switch_off(self) -> bool {
        match self {
            BootSignal::FirstDraw => false,
            BootSignal::WindowTitle => true,
        }
    }

    /// The mark in words, for a journal note or a refusal: "it never {this}".
    pub const fn describe(self) -> &'static str {
        match self {
            BootSignal::FirstDraw => "drew a screen",
            BootSignal::WindowTitle => "set its window title",
        }
    }
}

/// **The one resolver**: the row's strategy for a node of this shape. No harness is named here —
/// the answer is the row's data, so a new harness is a new row and nothing else.
pub fn delivery_for(row: &HarnessSpec, shape: NodeShape) -> TurnDelivery {
    match shape {
        NodeShape::Headless => row.delivery.headless,
        NodeShape::Interactive => row.delivery.interactive,
    }
}

/// A row's [`AbortVerb`] for each of the two shapes a node can run in — how a running turn is ended
/// early, so a cancelled node stops with its work on disk rather than being killed mid-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aborts {
    /// The node marion drives over pipes.
    pub headless: AbortVerb,
    /// The node a person can watch: a pane marion hosts, or a native `marion <harness>` session.
    pub interactive: AbortVerb,
}

/// The longest grace any row may give its abort before marion kills the node anyway. A cancel of a
/// tree waits at most one grace per depth level, so an unbounded row would make `marion cancel`
/// unbounded too.
pub const MAX_CANCEL_GRACE_MS: u32 = 30_000;

/// **How marion asks a running node to stop** — measured per harness and per shape, like
/// [`TurnDelivery`]. Whatever the verb, a node still running when its grace ends is killed: the
/// verb decides only whether the harness gets the chance to close its turn first.
///
/// Every variant carries a `note` naming the measurement, `None` included — an absence says what
/// was searched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortVerb {
    /// The typed channel's own interrupt: pi's JSONL `abort`, ACP's `session/cancel`, codex
    /// app-server's `turn/interrupt`. The frame is the dialect's; the row states only that it was
    /// measured and how long the harness takes to close the turn.
    Channel { grace_ms: u32, note: &'static str },
    /// Keystrokes into the node's terminal, `gap_ms` apart — Esc, then Ctrl-C, for a TUI whose
    /// composer interrupts the running turn on them.
    Keys {
        keys: &'static [&'static [u8]],
        gap_ms: u16,
        grace_ms: u32,
        note: &'static str,
    },
    /// Nothing measured ends a turn early on this shape: a cancel kills the node at once.
    None { note: &'static str },
}

impl AbortVerb {
    /// [`Self::kind`] of [`AbortVerb::None`], for a reader of the journal's `verb` string.
    pub const NONE_KIND: &'static str = "none";

    /// The measurement behind the row's verb.
    pub const fn note(self) -> &'static str {
        match self {
            AbortVerb::Channel { note, .. }
            | AbortVerb::Keys { note, .. }
            | AbortVerb::None { note } => note,
        }
    }

    /// The verb's stable name, for `marion doctor`, the journal and the sweep that pins each row's.
    pub const fn kind(self) -> &'static str {
        match self {
            AbortVerb::Channel { .. } => "channel",
            AbortVerb::Keys { .. } => "keys",
            AbortVerb::None { .. } => Self::NONE_KIND,
        }
    }

    /// How long marion waits after the verb before it kills the node; zero for [`Self::None`].
    pub const fn grace_ms(self) -> u32 {
        match self {
            AbortVerb::Channel { grace_ms, .. } | AbortVerb::Keys { grace_ms, .. } => grace_ms,
            AbortVerb::None { .. } => 0,
        }
    }
}

/// The abort-verb resolver, beside [`delivery_for`]: the row's verb for a node of this shape.
pub fn abort_for(row: &HarnessSpec, shape: NodeShape) -> AbortVerb {
    match shape {
        NodeShape::Headless => row.abort.headless,
        NodeShape::Interactive => row.abort.interactive,
    }
}

/// **The shortest wall clock marion gives any harness to boot** — the 30 s the readiness waits
/// held before boot cost was row data, kept as the floor so a row whose measured boot is small
/// waits exactly as long as before.
pub const BOOT_FLOOR: Duration = Duration::from_secs(30);

/// **How many times over its cores a machine may be committed while a boot still fits its
/// budget.** A boot is CPU-bound — a starved harness sits runnable, not blocked — so its wall time
/// grows with the load: ~3 CPU-seconds of opencode took ~45 s at a load average of 150 on 12
/// cores. 25 covers the worst ambient load measured on the development machine (274 on 12 cores,
/// 2026-09-28) with a little to spare.
pub const TOLERATED_OVERSUBSCRIPTION: u32 = 25;

/// **What booting a harness costs**: CPU time, user plus system, from `exec` until it sends its
/// first model request — over every process it re-execs into (qwen relaunches itself under a
/// second `node`; the relaunch is part of the boot).
///
/// CPU time and not wall time because only CPU time is a property of the harness: the wall time
/// of the same boot moves with whatever else the machine is doing, and a wall clock copied from a
/// quiet machine is the bound that fails on a busy one. [`Self::budget`] turns it back into the
/// wall clock a wait needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boot {
    /// The most CPU a boot was measured to take, and how it was measured.
    Measured { cpu: Duration, note: &'static str },
    /// Never measured here — no canned route reaches the harness, or the row is a protocol rather
    /// than one program. The budget is [`BOOT_FLOOR`]; `note` says why there is no number.
    Unmeasured { note: &'static str },
}

impl Boot {
    /// The wall clock a boot is given: its CPU cost on a machine committed
    /// [`TOLERATED_OVERSUBSCRIPTION`] times over, never less than [`BOOT_FLOOR`].
    pub fn budget(self) -> Duration {
        match self {
            Boot::Measured { cpu, .. } => (cpu * TOLERATED_OVERSUBSCRIPTION).max(BOOT_FLOOR),
            Boot::Unmeasured { .. } => BOOT_FLOOR,
        }
    }

    /// Where the row's figure came from.
    pub fn note(self) -> &'static str {
        match self {
            Boot::Measured { note, .. } | Boot::Unmeasured { note } => note,
        }
    }
}

/// **How a headless node is let call marion's tools** — the grant a launch needs where no operator
/// is there to answer a prompt, as one of a small closed set of measured shapes.
///
/// A statement about the launch, not a second copy of it: where marion compiles the grant, the
/// row's own [`Arg`]s, [`Env`]s and declaration documents render it and the sweep
/// `every_row_states_how_its_headless_node_is_approved_to_call_marions_tools` holds them to this
/// value; where the operator must grant it, marion writes nothing and `marion doctor` reports
/// whether the grant is there. Every variant carries the `note` naming the measurement, and the
/// launch-wide switches carry a `scope` saying how much **more** than marion's tools they approve,
/// because that is the reach an operator is agreeing to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// A per-tool allow list on argv naming marion's tools — claude's `--allowedTools`,
    /// copilot's `--allow-tool=marion(report)`. The narrowest shape: nothing else is approved.
    AllowedToolsArg {
        flag: &'static str,
        note: &'static str,
    },
    /// A key on marion's **own** server declaration approves that server's tools and nothing
    /// else — codex's `default_tools_approval_mode = "approve"`.
    DeclarationKey {
        key: &'static str,
        /// The operator config the key exists to override, merged at the top of the declaration
        /// document — a JSON object into a JSON document, `key = value` lines into a TOML one — where the harness's own default already runs marion's tool
        /// unasked, stripping the key alone proves nothing, and this is what makes the grant
        /// load-bearing (conformance's P-approval). `None` where the harness refuses the tool
        /// by default without the key.
        contest: Option<&'static str>,
        note: &'static str,
    },
    /// A launch-wide flag on argv — qwen's `--yolo`, cline's `--auto-approve`.
    CliFlag {
        flag: &'static str,
        scope: &'static str,
        note: &'static str,
    },
    /// A launch-wide environment variable — goose's `GOOSE_MODE=auto`.
    EnvVar {
        key: &'static str,
        value: &'static str,
        scope: &'static str,
        note: &'static str,
    },
    /// A protocol's own permission surface, answered by marion's client — ACP's
    /// `session/request_permission`, which the ACP driver answers itself with the agent's own
    /// allow option. An operator who wants the agent to stop asking names one of its modes as the
    /// agent type's `approval_mode`, which the driver sets on the session's `category` select
    /// (`acp::MODE_CATEGORY`) through the same `acp::SelectChannel` a model rides, refusing a
    /// session that does not offer it. The modes are the agent's own strings, read off its
    /// `session/new` answer; the row states only the select's category, never a second list.
    SessionMode {
        category: &'static str,
        note: &'static str,
    },
    /// Only the **operator's own settings** can grant it, and marion never edits them: `rule`
    /// in the JSON array at `pointer` of `file`, a path under the operator's `HOME`. No flag,
    /// environment variable or marion-owned document was measured approving marion's tools
    /// without also approving everything else, so the grant is the operator's to add, once, and
    /// `marion doctor` names the line when it is missing.
    OperatorAllowlist {
        file: &'static str,
        pointer: &'static str,
        rule: &'static str,
        note: &'static str,
    },
    /// **Every tool the node is offered runs without asking**: the harness's headless default
    /// (pi), or a launch-wide mode the row's env states so an operator's config cannot change it
    /// (goose's `GOOSE_MODE=auto`) — no grant of marion's is load-bearing. Declared, not a
    /// mechanism: conformance's P-approval is unsupported for it, and `marion doctor` warns that
    /// only the node's sandbox and containment bound it.
    ApproveAll { note: &'static str },
}

impl Approval {
    /// The measurement behind the row's grant.
    pub const fn note(self) -> &'static str {
        match self {
            Approval::AllowedToolsArg { note, .. }
            | Approval::DeclarationKey { note, .. }
            | Approval::CliFlag { note, .. }
            | Approval::EnvVar { note, .. }
            | Approval::SessionMode { note, .. }
            | Approval::OperatorAllowlist { note, .. }
            | Approval::ApproveAll { note } => note,
        }
    }

    /// The shape's stable name, for `marion doctor` and for the sweep that pins each row's.
    pub const fn kind(self) -> &'static str {
        match self {
            Approval::AllowedToolsArg { .. } => "allowed-tools-arg",
            Approval::DeclarationKey { .. } => "declaration-key",
            Approval::CliFlag { .. } => "cli-flag",
            Approval::EnvVar { .. } => "env-var",
            Approval::SessionMode { .. } => "session-mode",
            Approval::OperatorAllowlist { .. } => "operator-allowlist",
            Approval::ApproveAll { .. } => "approve-all",
        }
    }
}

/// **How a node of this harness is made read-only** — a reviewer's launch, which may read the
/// change it judges and must not alter it — as one of a small closed set of measured shapes.
///
/// Every read-only launch also drops `write` from its declared tools and runs in its own worktree
/// under an **empty** writable scope, so a change that gets through anyway is recorded as a scope
/// violation on the contract whatever the row says ([`Self::ScopeOnly`] is that and nothing more).
/// The variants are what each harness adds on top, measured on the installed binaries against
/// marion's canned provider with the write scripted anyway (`tests/fixtures/s38-read-only/`), so a
/// row is held to its harness *refusing* the call, not merely to the model not being offered it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnly {
    /// The availability axis is the whole switch: a launch declared without `write` offers no tool
    /// that changes a file, and a scripted call to one is refused by the harness itself — claude's
    /// `--tools`, qwen's `--core-tools`, pi's `--tools`.
    /// `verified` is whether that refusal was measured against a scripted write; a row that only
    /// believes it (agy, which has no canned route) does not count as blocking writes.
    ToolsAxis { verified: bool, note: &'static str },
    /// A pair on the row's own override channel ([`Field::Pairs`]), rendered after the launch's
    /// own pairs so it wins over them — codex's `sandbox_mode="read-only"`.
    Pair {
        key: &'static str,
        value: &'static str,
        note: &'static str,
    },
    /// A launch-wide environment variable — opencode's `OPENCODE_PERMISSION`.
    EnvVar {
        key: &'static str,
        value: &'static str,
        note: &'static str,
    },
    /// No switch was measured: the worktree and its empty writable scope are the whole of it, so a
    /// write is recorded after the fact rather than refused.
    ScopeOnly { note: &'static str },
}

impl ReadOnly {
    /// The measurement behind the row's switch.
    pub const fn note(self) -> &'static str {
        match self {
            ReadOnly::ToolsAxis { note, .. }
            | ReadOnly::Pair { note, .. }
            | ReadOnly::EnvVar { note, .. }
            | ReadOnly::ScopeOnly { note } => note,
        }
    }

    /// **Whether a write is refused, not merely recorded**: a measured switch. `false` for
    /// [`Self::ScopeOnly`] and for an unverified tools axis, where the empty writable scope is the
    /// only guard and a write lands in the reviewer's worktree before it is recorded. A reviewer is
    /// never chosen by default from a row that answers `false`, and one chosen explicitly says so.
    pub const fn blocks_writes(self) -> bool {
        match self {
            ReadOnly::ToolsAxis { verified, .. } => verified,
            ReadOnly::Pair { .. } | ReadOnly::EnvVar { .. } => true,
            ReadOnly::ScopeOnly { .. } => false,
        }
    }

    /// The shape's stable name, for the contract's record and the sweep that pins each row's.
    pub const fn kind(self) -> &'static str {
        match self {
            ReadOnly::ToolsAxis { .. } => "tools-axis",
            ReadOnly::Pair { .. } => "pair",
            ReadOnly::EnvVar { .. } => "env-var",
            ReadOnly::ScopeOnly { .. } => "scope-only",
        }
    }
}

/// The measured switch that keeps a harness from updating itself, or the honest absence of one.
///
/// Every variant carries a `note` naming where in the installed binary (help text, its strings, a
/// runtime type check) the switch was found — or, for [`Self::None`], what was searched — because
/// an update switch is exactly the kind of row a reader would otherwise guess from another
/// harness's. The sweep `every_row_states_its_update_policy_and_renders_it_into_every_launch_shape`
/// refuses an empty note and checks the switch reaches every launch the row can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePolicy {
    /// An environment variable: appended to every shape's env after the row's own [`Env`] rows
    /// (which must not repeat it) and to the native injection's overlay.
    Env {
        key: &'static str,
        value: &'static str,
        note: &'static str,
    },
    /// A `key=value` pair on the row's override channel — rendered **first** among
    /// [`Field::Pairs`] wherever the row's argv places that field (codex's `-c`), and first in a
    /// native [`LiveDeclaration::ArgvPairs`] prefix. A row with this policy and no `Pairs` slot
    /// in a shape's argv would drop it silently; the sweep catches that.
    Pair {
        key: &'static str,
        value: &'static str,
        note: &'static str,
    },
    /// Boolean keys, as dotted paths, in the JSON declaration document the row already emits. The
    /// document emitter applies them with
    /// [`UpdatePolicy::apply_to_json`], and the sweep reads them back from every emitted document.
    Document {
        keys: &'static [(&'static str, bool)],
        note: &'static str,
    },
    /// The installed binary **never updates itself**, so there is nothing to switch off: an update
    /// is an explicit subcommand the operator runs (goose's `goose update`). `note` says what was
    /// searched and not found. Running the binary bare is safe, so a `--version` probe may.
    Never { note: &'static str },
    /// No switch is known for the program the row runs — none was measured, or the row has no
    /// single program to measure (ACP, whose binary is the bound agent's). `note` says which, and
    /// what was searched. Distinct from [`Self::Never`] because the two answer the safety
    /// question oppositely: a binary that may update itself and has no switch is one marion must
    /// not run just to read a version.
    None { note: &'static str },
}

impl UpdatePolicy {
    /// The environment this policy contributes to a launch — one variable, or nothing.
    pub fn env(self) -> Option<(String, String)> {
        match self {
            UpdatePolicy::Env { key, value, .. } => Some((key.to_string(), value.to_string())),
            _ => None,
        }
    }

    /// The pair this policy contributes to the row's override channel — one, or nothing.
    pub fn pair(self) -> Option<(String, String)> {
        match self {
            UpdatePolicy::Pair { key, value, .. } => Some((key.to_string(), value.to_string())),
            _ => None,
        }
    }

    /// Write a [`Self::Document`] policy's keys into a JSON object, creating the intermediate
    /// objects a dotted path names. A no-op for every other variant.
    pub fn apply_to_json(self, doc: &mut serde_json::Value) {
        let UpdatePolicy::Document { keys, .. } = self else {
            return;
        };
        for (path, value) in keys {
            let mut node = &mut *doc;
            let mut parts = path.split('.').peekable();
            while let Some(part) = parts.next() {
                if !node.is_object() {
                    *node = serde_json::Value::Object(Default::default());
                }
                let object = node.as_object_mut().expect("just made an object");
                node = object
                    .entry(part)
                    .or_insert_with(|| serde_json::Value::Object(Default::default()));
                if parts.peek().is_none() {
                    *node = serde_json::Value::Bool(*value);
                }
            }
        }
    }
}

/// The neutral values a row's [`Arg`]s and [`Env`]s read from.
///
/// Named rather than a map so a row cannot reference a value nobody computes: every variant here
/// is a field of [`Fields`], and the renderer resolves it in exactly one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Cwd,
    ConfigDir,
    /// The model in this harness's own spelling, **after** the adapter's rule — codex compiles
    /// none under a canned provider, opencode wants `provider/model`, a canned copilot refuses `None`.
    Model,
    /// The compiled prompt. Carries a value even when empty; see [`Arg::PosIfNonEmpty`].
    Prompt,
    /// §3.1's **availability** axis in this harness's spelling ([`Axes::tools`]).
    Tools,
    /// §3.1's **permission** axis in this harness's spelling ([`Axes::allowed`]).
    Allowed,
    /// A coarse mode where the harness has one instead of a list ([`Axes::mode`]).
    Mode,
    /// How argv names the MCP declaration document — a path, or copilot's `@path`.
    McpConfig,
    /// The provider base URL in marion's canonical `…/v1` form, or `None` where none is overlaid.
    BaseUrl,
    /// [`Self::BaseUrl`] without its trailing `/v1` ([`base_url_root`]): the form a harness whose
    /// SDK appends its own versioned path wants — Claude Code adds `/v1/messages`, the google-genai
    /// SDK `/v1beta/...` — so the `/v1` marion stores is not doubled.
    BaseUrlRoot,
    ApiKey,
    /// Repeatable `key=value` overrides (codex `-c`).
    Pairs,
    /// An inline configuration document carried by an env var (opencode).
    InlineConfig,
    /// A session title (opencode `--title`).
    Title,
    OutputSchema,
    OutputLastMessage,
    /// The argv tail an ACP agent's own table supplies, verbatim.
    AgentArgs,
    /// The session to resume, where a launch asks for one. Rendered by [`Arg::Resume`] alone.
    Resume,
    /// The profile directory this launch selected, exactly as stored ([`Fields::profile_dir`]).
    ProfileDir,
}

/// One argv element, or the rule for several.
///
/// Closed on purpose: these ten shapes cover five harnesses' `--help` output, and a sixth that
/// needs a ninth should add it here — visibly — rather than write a branch in an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    /// A literal token.
    Lit(&'static str),
    /// `<flag> <value>` when the field carries a value; nothing otherwise.
    Flag(&'static str, Field),
    /// `<flag>=<value>` when the field carries a value; nothing otherwise.
    FlagEq(&'static str, Field),
    /// `<flag> <key>=<value>` when the field carries a value, the value a TOML string; nothing
    /// otherwise. codex's `-c model="…"`, on the command that has no `-m` (`codex app-server`).
    Pair(&'static str, &'static str, Field),
    /// `<flag> <item>` once per item of a list field.
    Each(&'static str, Field),
    /// `<flag>=<item>` once per item of a list field.
    EachEq(&'static str, Field),
    /// `<flag> <a,b,…>` — the list comma-joined, emitted **even when empty**. Claude Code's
    /// `--tools ""` is the documented "no built-in tools" spelling, and omitting the flag would be
    /// a different launch.
    Joined(&'static str, Field),
    /// The bare value, when the field carries one.
    Pos(Field),
    /// The bare value, only when it is non-empty. The two TUI shapes: an empty prompt is a pane
    /// opened at its composer, not a positional `""`.
    PosIfNonEmpty(Field),
    /// Every item of a list field, bare.
    Items(Field),
    /// The row's [`HarnessSpec::resume`] tokens around [`Field::Resume`], where a launch asks for
    /// one; nothing where it does not. A shape whose argv has no `Resume` slot, or a row whose
    /// `resume` is `None`, **refuses** a launch that asks for one.
    Resume,
    /// `<flag> <config_dir>/<child>` **under [`When::Overlay`] only** — an isolation flag, the argv
    /// counterpart of an [`Env`] row whose `val` is [`Val::Under`] and whose `when` is `Overlay`.
    /// cline's `--config`/`--data-dir`, which s27 measured as load-bearing *beside* the relocation
    /// variables (flags alone leak `~/.cline/data/db/sessions.db`; variables alone fork a hub
    /// daemon), and which a live node must not carry: pointing them at marion's directory is what
    /// hides the operator's own configuration.
    Isolation(&'static str, &'static str),
    /// A literal token **under [`When::Overlay`] only** — a switch that keeps the operator's own
    /// configuration out of the node. A live node must not carry one: its premise is the
    /// operator's harness as they configured it, and that configuration is where a credential can
    /// live (claude's `--setting-sources ""` drops a settings `apiKeyHelper` or `env` block;
    /// opencode's `--pure` drops an auth plugin).
    CannedLit(&'static str),
}

/// How a harness names a session to resume on argv, as `<harness> --help` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// `<flag> <id>` — claude `--resume <session-id>`, opencode `run --session <id>`.
    Flag(&'static str),
    /// `<flag>=<id>` — copilot `--resume[=value]`, whose value is optional and so must be joined.
    FlagEq(&'static str),
    /// `<word> <id>` as a subcommand after the row's own — codex `exec resume [SESSION_ID]`.
    Subcommand(&'static str),
}

/// Where an environment value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Val {
    Lit(&'static str),
    /// The field's value; the variable is **omitted** when the field carries none. Present or
    /// absent, never empty — `mcp_bridge::BASE_URL_ENV`'s rule, made structural.
    Field(Field),
    /// A path under the node's config dir: `""` is the dir itself, `"config"` a child.
    Under(&'static str),
}

/// When an [`Env`] row applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Always,
    /// Only where marion overlays the provider ([`Auth::overlays`]: canned or endpoint) — the
    /// isolation and the provider overlay, which is what live mode removes (§6.4).
    Overlay,
    /// Only under [`Auth::Endpoint`] — what a real third-party endpoint needs that marion's own
    /// canned one does not.
    Endpoint,
    /// Only under [`Auth::Canned`] — where marion owns the whole environment and the provider is
    /// its own local one, so nothing the node needs is on the network.
    Canned,
    /// Only when the field carries a value. Claude Code blanks `ANTHROPIC_API_KEY` **beside** a
    /// token, and only beside one.
    Present(Field),
}

/// One environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env {
    pub key: &'static str,
    pub val: Val,
    pub when: When,
}

/// Which of the row's two argv shapes to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Headless,
    Pane,
}

/// §3.1's two axes and the coarse mode, in **this harness's** spelling.
///
/// Separate from [`Fields`] because the audit record (`compiled_permissions`) needs them without
/// a [`crate::SpawnCtx`], and the adapter's `axes` hook is where an unmappable tool is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Axes {
    pub tools: Vec<String>,
    pub allowed: Vec<String>,
    pub mode: Option<String>,
}

/// Everything a row reads. Built by the adapter's `fields` hook — seeded neutrally from the
/// launch, then adjusted by whatever that harness has measured.
///
/// `Debug` is hand-written: see the impl below.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Fields {
    /// Variables the operator passes through to this agent type's nodes past the inherit filter
    /// (`env_passthrough`).
    pub env_passthrough: Vec<String>,
    pub cwd: PathBuf,
    pub config_dir: PathBuf,
    pub auth: Auth,
    /// The wire an endpoint launch speaks — `LaunchSpec::wire`, seeded neutrally — whose
    /// [`WireRecipe`] the renderer applies. `None` on canned and live launches.
    pub wire: Option<Wire>,
    /// The header the endpoint's provider reads its key from, whose [`KeyRecipe`] the renderer
    /// applies; `None` is the default, [`KeyHeader::Bearer`].
    pub key_header: Option<KeyHeader>,
    /// The program, where the row's is `None`.
    pub program: Option<String>,
    pub prompt: String,
    pub model: Option<String>,
    /// The ACP session mode the driver sets ([`Invocation::session_mode`]); never on argv.
    pub session_mode: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<marion_core::secret::Secret>,
    pub axes: Axes,
    pub mcp_config: Option<String>,
    pub pairs: Vec<(String, String)>,
    pub inline_config: Option<String>,
    pub title: Option<String>,
    pub output_schema: Option<PathBuf>,
    pub output_last_message: Option<PathBuf>,
    pub agent_args: Vec<String>,
    /// The session this launch resumes, if any — `LaunchSpec::resume`, copied by the neutral
    /// seeding so the feature is a field and not five branches.
    pub resume: Option<String>,
    /// The profile directory this launch selected, if any — carried onto the row's
    /// [`HarnessSpec::profile`] variable by [`render`].
    pub profile_dir: Option<PathBuf>,
    /// Environment the hook derived that no row names — appended after the row's own. ACP's
    /// per-agent canned recipe is the one user.
    pub extra_env: Vec<(String, String)>,
    /// A read-only launch: the row's [`HarnessSpec::read_only`] switch is rendered.
    pub read_only: bool,
}

/// **The strings a declaration or a credential can travel in print their shape, never their
/// value**: the key through `Secret`, a config pair or variable by its name, an inline document or
/// a declaration by its length, an ACP agent's argv by its count. A `Fields` is what a failing
/// render or a refusal has in hand, and each of those once held the node token or a document
/// with a key inside it. Destructured, so a field added to the struct is a compile error here
/// until someone decides how it prints.
impl std::fmt::Debug for Fields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Fields {
            cwd,
            config_dir,
            auth,
            wire,
            key_header,
            program,
            prompt,
            model,
            session_mode,
            base_url,
            api_key,
            axes,
            mcp_config,
            pairs,
            inline_config,
            title,
            output_schema,
            output_last_message,
            agent_args,
            resume,
            profile_dir,
            extra_env,
            read_only,
            env_passthrough,
        } = self;
        struct Names<'a>(&'a [(String, String)]);
        impl std::fmt::Debug for Names<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_list()
                    .entries(self.0.iter().map(|(k, _)| format!("{k}=***")))
                    .finish()
            }
        }
        let bytes = |s: &Option<String>| s.as_ref().map(|s| format!("<{} bytes>", s.len()));
        f.debug_struct("Fields")
            .field("env_passthrough", env_passthrough)
            .field("cwd", cwd)
            .field("config_dir", config_dir)
            .field("auth", auth)
            .field("wire", wire)
            .field("key_header", key_header)
            .field("program", program)
            .field("prompt", prompt)
            .field("model", model)
            .field("session_mode", session_mode)
            .field("base_url", base_url)
            .field("api_key", api_key)
            .field("axes", axes)
            .field("mcp_config", &bytes(mcp_config))
            .field("pairs", &Names(pairs))
            .field("inline_config", &bytes(inline_config))
            .field("title", title)
            .field("output_schema", output_schema)
            .field("output_last_message", output_last_message)
            .field("agent_args", &format!("<{} args>", agent_args.len()))
            .field("resume", resume)
            .field("profile_dir", profile_dir)
            .field("extra_env", &Names(extra_env))
            .field("read_only", read_only)
            .finish()
    }
}

/// A base URL without its trailing `/v1` (and without trailing slashes either side of it) —
/// [`Field::BaseUrlRoot`]'s value. A URL with no `/v1` is returned as it is, minus trailing slashes.
pub fn base_url_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

impl Fields {
    /// The scalar a field carries, or the list it carries comma-joined — `None` where it carries
    /// nothing, which for a list means empty.
    fn value(&self, field: Field) -> Option<String> {
        let path = |p: &PathBuf| Some(p.to_string_lossy().into_owned());
        match field {
            Field::Cwd => path(&self.cwd),
            Field::ConfigDir => path(&self.config_dir),
            Field::Model => self.model.clone(),
            Field::Prompt => Some(self.prompt.clone()),
            Field::Mode => self.axes.mode.clone(),
            Field::McpConfig => self.mcp_config.clone(),
            Field::BaseUrl => self.base_url.clone(),
            Field::BaseUrlRoot => self.base_url.as_deref().map(base_url_root),
            Field::ApiKey => self.api_key.as_ref().map(|k| k.expose().to_string()),
            Field::InlineConfig => self.inline_config.clone(),
            Field::Title => self.title.clone(),
            Field::OutputSchema => self.output_schema.as_ref().and_then(path),
            Field::OutputLastMessage => self.output_last_message.as_ref().and_then(path),
            Field::Resume => self.resume.clone(),
            Field::ProfileDir => self.profile_dir.as_ref().and_then(path),
            Field::Tools | Field::Allowed | Field::Pairs | Field::AgentArgs => {
                let items = self.items(field);
                (!items.is_empty()).then(|| items.join(","))
            }
        }
    }

    /// The items of a list field; a scalar is a list of at most one.
    fn items(&self, field: Field) -> Vec<String> {
        match field {
            Field::Tools => self.axes.tools.clone(),
            Field::Allowed => self.axes.allowed.clone(),
            Field::Pairs => self.pairs.iter().map(|(k, v)| format!("{k}={v}")).collect(),
            Field::AgentArgs => self.agent_args.clone(),
            scalar => self.value(scalar).into_iter().collect(),
        }
    }
}

/// The environment `rows` describe, read from `f`, in row order.
///
/// Public because one harness's isolation is another's recipe: an ACP agent that is the opencode
/// binary one subcommand over is pointed at a canned provider by exactly opencode's rows.
pub fn render_env(rows: &[Env], f: &Fields) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for row in rows {
        if !env_applies(row.when, f) {
            continue;
        }
        if let Some(v) = env_value(*row, f) {
            env.push((row.key.to_string(), v));
        }
    }
    env
}

/// Whether an [`Env`] row's gate lets it reach this launch. One arm per [`When`].
fn env_applies(when: When, f: &Fields) -> bool {
    match when {
        When::Always => true,
        When::Overlay => f.auth.overlays(),
        When::Endpoint => f.auth == Auth::Endpoint,
        When::Canned => f.auth == Auth::Canned,
        When::Present(field) => f.value(field).is_some(),
    }
}

/// The value an [`Env`] row carries, or `None` where it carries none and the variable is
/// therefore **omitted** rather than set empty. One arm per [`Val`].
fn env_value(row: Env, f: &Fields) -> Option<String> {
    match row.val {
        Val::Lit(s) => Some(s.to_string()),
        Val::Field(field) => f.value(field),
        Val::Under(rel) => Some(env_under(rel, f)),
    }
}

/// [`Val::Under`]: a path under the node's config dir — `""` is the dir itself.
fn env_under(rel: &str, f: &Fields) -> String {
    let path = if rel.is_empty() {
        f.config_dir.clone()
    } else {
        f.config_dir.join(rel)
    };
    path.to_string_lossy().into_owned()
}

/// Why a row could not render a launch. Each is the adapter's to name the harness in, because a
/// pane request answered with the headless launch, or a resume answered with a fresh session, is
/// the silent downgrade the refusal exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The row has no pane shape.
    NoPaneShape,
    /// The row names no program and the launch supplied none.
    NoProgram,
    /// The launch asks to resume a session and this shape of this harness has no measured way to
    /// name one.
    NoResume,
    /// An endpoint launch names a wire this row has no [`WireRecipe`] for — or names none.
    NoWireRecipe,
    /// The endpoint's provider reads its key from a header this wire's recipe cannot present.
    NoKeyRecipe(KeyHeader),
}

/// The items a list [`Field`] contributes to a shape's argv.
///
/// The row's update policy rides its override channel ahead of the launch's own pairs
/// ([`UpdatePolicy::Pair`]), and a read-only launch's switch after them, so it wins
/// ([`ReadOnly::Pair`]); every other field reads from `f` alone.
fn items(spec: &HarnessSpec, f: &Fields, field: Field) -> Vec<String> {
    let mut items = f.items(field);
    if let (Field::Pairs, Some((k, v))) = (field, spec.updates.pair()) {
        items.insert(0, format!("{k}={v}"));
    }
    if let (Field::Pairs, true, ReadOnly::Pair { key, value, .. }) =
        (field, f.read_only, spec.read_only)
    {
        items.push(format!("{key}={value}"));
    }
    items
}

/// [`Arg::Flag`]: `<flag> <value>` when the field carries a value; nothing otherwise.
fn arg_flag(flag: &str, field: Field, f: &Fields) -> Vec<String> {
    f.value(field)
        .map(|v| vec![flag.to_string(), v])
        .unwrap_or_default()
}

/// [`Arg::FlagEq`]: `<flag>=<value>` when the field carries a value; nothing otherwise.
fn arg_flag_eq(flag: &str, field: Field, f: &Fields) -> Vec<String> {
    f.value(field)
        .map(|v| vec![format!("{flag}={v}")])
        .unwrap_or_default()
}

/// [`Arg::Each`]: `<flag> <item>` once per item of a list field.
fn arg_each(flag: &str, field: Field, spec: &HarnessSpec, f: &Fields) -> Vec<String> {
    items(spec, f, field)
        .into_iter()
        .flat_map(|item| [flag.to_string(), item])
        .collect()
}

/// [`Arg::EachEq`]: `<flag>=<item>` once per item of a list field.
fn arg_each_eq(flag: &str, field: Field, spec: &HarnessSpec, f: &Fields) -> Vec<String> {
    items(spec, f, field)
        .into_iter()
        .map(|item| format!("{flag}={item}"))
        .collect()
}

/// [`Arg::Isolation`]: the isolation flag, where marion overlays the provider alone.
fn arg_isolation(flag: &str, child: &str, f: &Fields) -> Vec<String> {
    if !f.auth.overlays() {
        return Vec::new();
    }
    let path = f.config_dir.join(child).to_string_lossy().into_owned();
    vec![flag.to_string(), path]
}

/// [`Arg::Resume`]: the row's [`HarnessSpec::resume`] tokens around the session the launch asks
/// to resume; nothing where either is absent. The *refusal* for a launch that asks a shape with
/// no `Resume` slot is [`render`]'s, because only it can see the whole argv.
fn arg_resume(spec: &HarnessSpec, f: &Fields) -> Vec<String> {
    let (Some(id), Some(how)) = (f.value(Field::Resume), spec.resume) else {
        return Vec::new();
    };
    match how {
        Resume::Flag(flag) | Resume::Subcommand(flag) => vec![flag.to_string(), id],
        Resume::FlagEq(flag) => vec![format!("{flag}={id}")],
    }
}

/// The tokens one [`Arg`] contributes to a shape's argv, in order — empty where the row's own
/// rule says this launch carries nothing for it. One arm per variant, each a single rule.
fn render_arg(arg: Arg, spec: &HarnessSpec, f: &Fields) -> Vec<String> {
    match arg {
        Arg::Lit(s) => vec![s.to_string()],
        Arg::Flag(flag, field) => arg_flag(flag, field, f),
        Arg::FlagEq(flag, field) => arg_flag_eq(flag, field, f),
        Arg::Pair(flag, key, field) => f
            .value(field)
            .map(|v| {
                vec![
                    flag.to_string(),
                    format!("{key}={}", serde_json::Value::String(v)),
                ]
            })
            .unwrap_or_default(),
        Arg::Each(flag, field) => arg_each(flag, field, spec, f),
        Arg::EachEq(flag, field) => arg_each_eq(flag, field, spec, f),
        Arg::Joined(flag, field) => vec![flag.to_string(), items(spec, f, field).join(",")],
        Arg::Pos(field) => f.value(field).into_iter().collect(),
        Arg::PosIfNonEmpty(field) => f
            .value(field)
            .filter(|v| !v.is_empty())
            .into_iter()
            .collect(),
        Arg::Items(field) => items(spec, f, field),
        Arg::Isolation(flag, child) => arg_isolation(flag, child, f),
        Arg::CannedLit(s) => match f.auth.overlays() {
            true => vec![s.to_string()],
            false => Vec::new(),
        },
        Arg::Resume => arg_resume(spec, f),
    }
}

/// The row's recipe for `wire`, where it has one.
pub fn recipe_for(spec: &HarnessSpec, wire: Option<Wire>) -> Option<&'static WireRecipe> {
    let wire = wire?;
    spec.wires.iter().find(|r| r.wire == wire)
}

/// A recipe's variables over the overlay's: a name both carry takes the recipe's value, in place.
fn apply_recipe(env: &mut Vec<(String, String)>, recipe: &WireRecipe) {
    for (k, v) in recipe.env {
        match env.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = v.to_string(),
            None => env.push((k.to_string(), v.to_string())),
        }
    }
}

/// argv + env for one shape of one row, or the [`Refusal`] the row states.
pub fn render(spec: &HarnessSpec, shape: Shape, f: &Fields) -> Result<Invocation, Refusal> {
    let argv = match shape {
        Shape::Headless => spec.argv,
        Shape::Pane => spec.pane.ok_or(Refusal::NoPaneShape)?,
    };
    let program = match spec.program {
        Some(p) => p.to_string(),
        None => f.program.clone().ok_or(Refusal::NoProgram)?,
    };
    // A thread channel resumes over the protocol, so its argv carries no resume and needs none.
    let resumes_on_argv = spec.resume.is_some() && argv.contains(&Arg::Resume);
    let resumes_on_channel = shape == Shape::Headless && spec.surfaces.rpc().is_some();
    if f.resume.is_some() && !resumes_on_argv && !resumes_on_channel {
        return Err(Refusal::NoResume);
    }
    // The push flag leads the pane argv, so the row's own first flag closes it: it is variadic
    // (claude 2.1.268), and appended it would swallow the positional prompt the pane seeds.
    let mut args: Vec<String> = match shape {
        Shape::Pane => spec.push.argv().iter().map(|s| s.to_string()).collect(),
        Shape::Headless => Vec::new(),
    };
    for arg in argv {
        args.extend(render_arg(*arg, spec, f));
    }
    let mut env = render_env(spec.env, f);
    if f.auth == Auth::Endpoint {
        let recipe = recipe_for(spec, f.wire).ok_or(Refusal::NoWireRecipe)?;
        apply_recipe(&mut env, recipe);
        let header = f.key_header.unwrap_or_default();
        let key = recipe
            .keys
            .iter()
            .find(|k| k.header == header)
            .ok_or(Refusal::NoKeyRecipe(header))?;
        for (k, v) in render_env(key.env, f) {
            match env.iter_mut().find(|(n, _)| *n == k) {
                Some(slot) => slot.1 = v,
                None => env.push((k, v)),
            }
        }
    }
    env.extend(spec.updates.env());
    if let (true, ReadOnly::EnvVar { key, value, .. }) = (f.read_only, spec.read_only) {
        env.push((key.to_string(), value.to_string()));
    }
    env.extend(f.extra_env.iter().cloned());
    let env_remove = crate::profile::apply(spec.profile.as_ref(), f, &mut env);
    Ok(Invocation {
        sandbox: None,
        program,
        args,
        env,
        env_remove,
        cwd: f.cwd.clone(),
        // What the row carried, including its absence: the model that reached argv is the one the
        // hook placed in `Fields::model`, and nothing else is recorded.
        model: f.model.clone(),
        session_mode: f.session_mode.clone(),
        inherit: Some(crate::env_filter::InheritFilter {
            login: spec.login_env,
            auth: f.auth,
            passthrough: f.env_passthrough.clone(),
        }),
    })
}

/// §3.4's point in the cross-product, as a row can state it. The pane shape is always `opaque`
/// where a row has one, so it is not a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surfaces {
    /// Typed control over pipes, `StructuredUi`, `ProtocolEvents`.
    Headless(TypedKind),
    /// The prompt rides argv, nothing is displayed, the JSONL is read: `codex exec`'s shape, which
    /// is not one of the four presets.
    LaunchOnly,
    /// Headless over a JSONL command channel ([`TypedKind::JsonlRpc`]), **with its vocabulary**:
    /// the channel is part of the choice, so a row cannot select the surface without stating
    /// what to write on it. A row without one stays [`Self::LaunchOnly`].
    JsonlRpc(&'static JsonlChannel),
    /// Headless over an id-correlated JSON-RPC thread server ([`TypedKind::AppServer`]), **with its
    /// vocabulary**, for the same reason as [`Self::JsonlRpc`]: codex's `app-server` (S36).
    AppServer(&'static RpcChannel),
}

impl Surfaces {
    pub fn execution(self) -> ExecutionSurfaces {
        match self {
            Surfaces::Headless(kind) => ExecutionSurfaces::headless(kind),
            Surfaces::LaunchOnly => ExecutionSurfaces::launch_only_with_protocol_events(),
            Surfaces::JsonlRpc(_) => ExecutionSurfaces::headless(TypedKind::JsonlRpc),
            Surfaces::AppServer(_) => ExecutionSurfaces::headless(TypedKind::AppServer),
        }
    }

    /// The row's JSONL command channel, where it drives one.
    pub fn channel(self) -> Option<&'static JsonlChannel> {
        match self {
            Surfaces::JsonlRpc(c) => Some(c),
            Surfaces::Headless(_) | Surfaces::LaunchOnly | Surfaces::AppServer(_) => None,
        }
    }

    /// The row's JSON-RPC thread channel, where it drives one. A resume on such a row is the
    /// channel's own request ([`RpcChannel::resume`]), never an argv element.
    pub fn rpc(self) -> Option<&'static RpcChannel> {
        match self {
            Surfaces::AppServer(c) => Some(c),
            Surfaces::Headless(_) | Surfaces::LaunchOnly | Surfaces::JsonlRpc(_) => None,
        }
    }
}

/// How a model spells one of marion's tools — a **measurement of a harness**, never a guess.
///
/// An enum rather than a format string because the thing recorded is what a model was watched
/// typing, and s14's finding is that an unknown tool name is *silently ignored*: a spelling
/// generalised from one harness to another buys a turn that ends having called nothing.
///
/// | who | what the model typed | where |
/// |---|---|---|
/// | claude-code 2.1.220, codex 0.146.0 (as a JavaScript identifier), `claude-agent-acp` 0.66.0 | `mcp__marion__report` | S1, S6, S22 |
/// | opencode 1.17.3, `opencode acp` | `marion_report` | S13, S21 |
/// | copilot 1.0.83 | `marion-report` | s24 |
/// | `codex-acp` 1.1.14 | `mcp.marion.report` | S22 |
/// | goose 1.49.0, cline 3.0.61 | `marion__report` | S26, S27 |
/// | agy 1.2.8 | `call_mcp_tool{ServerName: marion, ToolName: report}`, shown as `marion/report` | s32 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSpelling {
    /// `mcp__<server>__<tool>`.
    McpDoubleUnderscore,
    /// `<server>_<tool>`.
    ServerUnderscoreTool,
    /// `<server>-<tool>`.
    ServerHyphenTool,
    /// `mcp.<server>.<tool>`.
    McpDotted,
    /// `<server>__<tool>` — the `mcp__` form without its prefix.
    ServerDoubleUnderscoreTool,
    /// `<server>/<tool>` — agy's own name for an MCP tool, in its TUI and its permission rules
    /// (`mcp(marion/report)`). The model calls it through the generic `call_mcp_tool` with the
    /// two halves as separate arguments, so a stream is read by server and verb, not this string.
    ServerSlashTool,
}

impl ToolSpelling {
    pub fn spell(self, tool: &str) -> String {
        match self {
            Self::McpDoubleUnderscore => format!("mcp__{MCP_ALIAS}__{tool}"),
            Self::ServerUnderscoreTool => format!("{MCP_ALIAS}_{tool}"),
            Self::ServerHyphenTool => format!("{MCP_ALIAS}-{tool}"),
            Self::McpDotted => format!("mcp.{MCP_ALIAS}.{tool}"),
            Self::ServerDoubleUnderscoreTool => format!("{MCP_ALIAS}__{tool}"),
            Self::ServerSlashTool => format!("{MCP_ALIAS}/{tool}"),
        }
    }
}

/// Whose spelling a row carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spelling {
    /// One measured spelling for the whole harness.
    Fixed(ToolSpelling),
    /// The spelling is the **agent's**, not the protocol's, and the adapter bound to an agent
    /// answers for it — ACP, where one row serves agents that spell the same tool three ways.
    /// Nothing here defaults: an adapter that never binds an agent has no spelling and refuses.
    PerAgent,
}

/// **A declaration's body**: the `mcpServers` map most harnesses read, described as data
/// ([`McpServers`]), or — where a harness's declaration is not that map — the row's own serialiser,
/// which the row names and says why. Data where it can be, so a row read from a file can carry it.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    McpServers(McpServers),
    /// One command line with the bridge's identity in front ([`CommandLine`]) — goose's
    /// `--with-extension` token.
    CommandLine(CommandLine),
    Code(fn(&BridgeEnv) -> String),
}

impl Body {
    /// The body for the node `b` serves.
    pub fn render(self, b: &BridgeEnv) -> String {
        match self {
            Body::McpServers(m) => m.render(b),
            Body::CommandLine(c) => c.render(b),
            Body::Code(f) => f(b),
        }
    }

    /// [`Self::render`], or the part of `b` the body's grammar cannot spell ([`CommandLine::
    /// unspellable`]) — the refusal a launch owes rather than a declaration read back wrong.
    pub fn spell(self, b: &BridgeEnv) -> Result<String, String> {
        match self {
            Body::CommandLine(c) => match c.unspellable(b) {
                Some(token) => Err(token),
                None => Ok(c.render(b)),
            },
            other => Ok(other.render(b)),
        }
    }
}

/// **The bridge as one command line**: `<head><K=V …> <command> <args…>`, space-separated — the
/// bridge's identity pairs, its program, its arguments. `head` joins the first word directly
/// (`marion:MARION_REPO=…`). Where the harness splits the line on whitespace, a word with
/// whitespace inside cannot be spelled and is refused ([`Self::unspellable`]) rather than declared
/// as two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandLine {
    pub head: &'static str,
    pub split_on_whitespace: bool,
}

impl CommandLine {
    pub fn render(self, b: &BridgeEnv) -> String {
        let mut words: Vec<String> = b
            .pairs()
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        words.push(b.bridge.to_string_lossy().into_owned());
        words.extend(b.args.iter().cloned());
        format!("{}{}", self.head, words.join(" "))
    }

    /// The first word whitespace would split in two — the bridge program, one of its args, or a
    /// pair — or `None` where the whole line is spellable, or the harness does not split it.
    pub fn unspellable(self, b: &BridgeEnv) -> Option<String> {
        if !self.split_on_whitespace {
            return None;
        }
        let has_space = |s: &str| s.chars().any(char::is_whitespace);
        let program = b.bridge.to_string_lossy();
        if has_space(&program) {
            return Some(program.into_owned());
        }
        if let Some(a) = b.args.iter().find(|a| has_space(a)) {
            return Some(a.clone());
        }
        b.pairs()
            .into_iter()
            .find(|(k, v)| has_space(k) || has_space(v))
            .map(|(k, v)| format!("{k}={v}"))
    }
}

/// **`{"mcpServers": {"marion": {command, args, env}}}`, in one harness's dialect**: the stdio
/// entry marion's bridge is declared by, with the few ways harnesses measured it differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpServers {
    /// `"type": "stdio"` stated on the entry.
    pub typed: bool,
    /// The key the entry sits under inside marion's server, where the harness nests it (cline's
    /// `transport`).
    pub nested: Option<&'static str>,
    /// The entry's `tools` list, where the harness gates tools per server (copilot's `["*"]`);
    /// empty states none.
    pub tools: &'static [&'static str],
    /// Pretty-printed, as a file the harness reads; compact where it rides one argv token.
    pub pretty: bool,
}

impl McpServers {
    /// The whole document as JSON.
    pub fn json(self, b: &BridgeEnv) -> serde_json::Value {
        serde_json::json!({ MCP_SERVERS_KEY: { MCP_ALIAS: self.entry(b) } })
    }

    /// marion's server entry alone, without the `mcpServers` map around it.
    pub fn entry(self, b: &BridgeEnv) -> serde_json::Value {
        let mut entry = serde_json::json!({
            "command": b.bridge.to_string_lossy(),
            "args": b.args,
            "env": b.env_json(),
        });
        if self.typed {
            entry["type"] = serde_json::json!("stdio");
        }
        if let Some(key) = self.nested {
            entry = serde_json::json!({ key: entry });
        }
        if !self.tools.is_empty() {
            entry["tools"] = serde_json::json!(self.tools);
        }
        entry
    }

    pub fn render(self, b: &BridgeEnv) -> String {
        let v = self.json(b);
        if self.pretty {
            serde_json::to_string_pretty(&v).expect("a Value always serialises")
        } else {
            v.to_string()
        }
    }
}

/// The top-level key of an [`McpServers`] document.
pub const MCP_SERVERS_KEY: &str = "mcpServers";

/// **One document a row writes under the node's directory**, as data: a JSON object, and marion's
/// server added under [`MCP_SERVERS_KEY`] where a bridge is declared and the document carries the
/// declaration. Rendered structurally — the base is parsed and the entry inserted — never by
/// splicing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigFile {
    /// Where, relative to the node's config dir; never absolute, never through `..`.
    pub path: &'static str,
    /// The auth modes it is written under.
    pub modes: Modes,
    /// The document without marion's server: a JSON object, as text.
    pub base: &'static str,
    /// marion's server entry in the harness's dialect, where this document is the declaration.
    pub servers: Option<McpServers>,
    pub pretty: bool,
}

impl ConfigFile {
    /// The document, with `bridge`'s entry where one is declared. `None` where the base is not a
    /// JSON object — a row the validator refuses ([`crate::sweep::validate`]).
    pub fn render(self, bridge: Option<&BridgeEnv>) -> Option<String> {
        let mut doc: serde_json::Value = serde_json::from_str(self.base).ok()?;
        doc.as_object()?;
        if let (Some(servers), Some(b)) = (self.servers, bridge) {
            doc[MCP_SERVERS_KEY] = serde_json::json!({ MCP_ALIAS: servers.entry(b) });
        }
        Some(if self.pretty {
            serde_json::to_string_pretty(&doc).expect("a Value always serialises")
        } else {
            doc.to_string()
        })
    }
}

/// How marion's MCP declaration is carried onto a node that keeps the operator's own
/// configuration — [`McpRoutes::live`], spelled out.
///
/// [`McpRoute`] answers *"was the route taken?"* and so names only the kind of channel; this
/// answers *"how is it taken?"* — the exact flag or variable, the file name where one is written,
/// and the body — so that a launch that must inject **only** the declaration (the native facade,
/// where every other flag belongs to the operator) can be rendered from the row alone. The body is
/// a function of [`BridgeEnv`] rather than a string because the declaration names the node it
/// serves; it is the same function the adapter's `config_files`/`fields` hook calls, so the two
/// cannot drift, and the sweep proves it.
#[derive(Debug, Clone, Copy)]
pub enum LiveDeclaration {
    /// `<flag> <prefix><path>` on argv, naming a document written to `file` under the node's own
    /// directory — claude's `--mcp-config <path>`, copilot's `--additional-mcp-config @<path>`.
    ArgvDocument {
        flag: &'static str,
        file: &'static str,
        prefix: &'static str,
        body: Body,
        /// Argv names the document even on a launch that declares no bridge (claude's).
        always: bool,
    },
    /// A document written to `file` under the node's own directory whose path rides the
    /// environment variable `key` — cline's `CLINE_MCP_SETTINGS_PATH`.
    EnvDocument {
        key: &'static str,
        file: &'static str,
        body: Body,
    },
    /// The document itself, inline in the environment variable `key`; nothing is written —
    /// opencode's `OPENCODE_CONFIG_CONTENT`.
    EnvInline { key: &'static str, body: Body },
    /// `<flag> <k>=<v>` once per pair on argv; nothing is written — codex's `-c`. `key` is the
    /// config key every pair sits under, the needle [`McpRoute::Argv`] checks argv for.
    ArgvPairs {
        flag: &'static str,
        key: &'static str,
        pairs: fn(&BridgeEnv) -> Vec<(String, String)>,
    },
    /// `<flag> <body>` once on argv, the whole declaration in one token; nothing is written —
    /// goose's `--with-extension "marion:<command> <args>"`. `key` is the text the token must
    /// carry, the needle [`McpRoute::Argv`] checks argv for (`marion:`, the extension's name).
    ArgvInline {
        flag: &'static str,
        key: &'static str,
        body: Body,
    },
    /// `<flag> <dir>/<root>` on argv, naming a **directory** the harness reads a document out of:
    /// the document is written to `<root>/<file>` under the node's own directory — agy's
    /// `--add-dir <root>`, which loads `<root>/.agents/mcp_config.json` as a workspace's own MCP
    /// declaration (s32). The root is marion's and holds nothing else.
    ArgvRoot {
        flag: &'static str,
        root: &'static str,
        file: &'static str,
        body: Body,
    },
}

impl LiveDeclaration {
    /// The document this channel writes — its path under the node's config dir and its body —
    /// or `None` for a channel that writes nothing.
    pub fn document(self) -> Option<(PathBuf, Body)> {
        match self {
            LiveDeclaration::ArgvDocument { file, body, .. }
            | LiveDeclaration::EnvDocument { file, body, .. } => Some((PathBuf::from(file), body)),
            LiveDeclaration::ArgvRoot {
                root, file, body, ..
            } => Some((PathBuf::from(root).join(file), body)),
            LiveDeclaration::EnvInline { .. }
            | LiveDeclaration::ArgvPairs { .. }
            | LiveDeclaration::ArgvInline { .. } => None,
        }
    }

    /// How argv names the declaration ([`Field::McpConfig`]) for a node whose config dir is
    /// `config_dir`, where the channel names it on argv at all: the document's path behind its
    /// prefix, or the root directory. `declared` is whether the launch asked for a bridge.
    pub fn argv_name(self, config_dir: &std::path::Path, declared: bool) -> Option<String> {
        match self {
            LiveDeclaration::ArgvDocument {
                file,
                prefix,
                always,
                ..
            } => (declared || always)
                .then(|| format!("{prefix}{}", config_dir.join(file).to_string_lossy())),
            LiveDeclaration::ArgvRoot { root, .. } => {
                declared.then(|| config_dir.join(root).to_string_lossy().into_owned())
            }
            _ => None,
        }
    }

    /// The [`McpRoute`] this channel is an instance of — what `mcp.live` must say of a row that
    /// carries it.
    pub fn route(self) -> McpRoute {
        match self {
            LiveDeclaration::ArgvDocument { .. }
            | LiveDeclaration::EnvDocument { .. }
            | LiveDeclaration::ArgvRoot { .. } => McpRoute::Document,
            LiveDeclaration::EnvInline { key, .. } => McpRoute::Environment(key),
            LiveDeclaration::ArgvPairs { key, .. } | LiveDeclaration::ArgvInline { key, .. } => {
                McpRoute::Argv(key)
            }
        }
    }
}

/// **How** the MCP declaration reaches the node, under each auth mode.
///
/// Two fields rather than one because two harnesses route differently once marion stops owning
/// the config surface: a live codex node's `[mcp_servers.marion]` cannot go in `~/.codex/config.toml`
/// (§6.4) and rides `-c` flags; a live opencode node's cannot go under a relocated `$XDG_CONFIG_HOME`
/// and rides `OPENCODE_CONFIG_CONTENT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpRoutes {
    pub canned: McpRoute,
    pub live: McpRoute,
}

/// [`HarnessSpec::token`]: the node token's carrier under each auth mode, beside
/// [`McpRoutes`] because the carrier a route can take depends on the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCarriers {
    pub canned: TokenCarrier,
    pub live: TokenCarrier,
}

impl TokenCarriers {
    /// Both modes carry the token inside the declaration.
    pub const DECLARATION: TokenCarriers = TokenCarriers::both(TokenCarrier::Declaration);

    /// One carrier for both modes, where the route does not change with the auth mode.
    pub const fn both(carrier: TokenCarrier) -> TokenCarriers {
        TokenCarriers {
            canned: carrier,
            live: carrier,
        }
    }

    /// The carrier for a launch under `auth` — endpoint rides the canned route, as
    /// `HarnessAdapter::mcp_route` has it.
    pub fn for_auth(self, auth: Auth) -> TokenCarrier {
        match auth {
            Auth::Canned | Auth::Endpoint => self.canned,
            Auth::Inherited => self.live,
        }
    }
}

/// How a node's capability token (`MARION_NODE_TOKEN`) travels from marion to the bridge the
/// harness starts — measured per row, never assumed.
///
/// The declaration names the node; the token proves it. Where the declaration is a private
/// document, an environment variable or marion's own ACP pipe, the token can sit inside it. Where
/// the declaration rides **argv** it cannot: argv is readable through `ps` by every user on the
/// machine. Then the token is withheld from the declaration and written to a 0600 file in the
/// node's own directory; the harness's environment carries only the file's path
/// ([`NODE_TOKEN_FILE_ENV`]), and the row states how the harness passes that on to the MCP server
/// it starts. The token itself is never in the harness's environment, which every shell command
/// its model runs inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCarrier {
    /// Inside the declaration, beside the node's identity.
    Declaration,
    /// Withheld from the declaration; its file's path is set on the harness's environment, which
    /// its MCP launcher hands a stdio server whole. `note` names the measurement.
    InheritedEnv { note: &'static str },
    /// Withheld from the declaration; its file's path is set on the harness's environment, and the
    /// launcher hands a server only an allowlist of the parent's variables, so the declaration
    /// names the path's variable in the harness's own pass-through list ([`Self::forwarded`]).
    /// `note` names the measurement.
    ForwardedEnv { note: &'static str },
    /// Withheld from a declaration that itself rides the harness's environment (so a model's shell
    /// would inherit it): the declaration names the token's 0600 file instead, and the harness's
    /// own environment carries nothing more. `note` names why.
    DeclaredFile { note: &'static str },
}

impl TokenCarrier {
    /// Whether the declaration leaves the token out.
    pub fn withholds(self) -> bool {
        !matches!(self, TokenCarrier::Declaration)
    }

    /// The bridge as this carrier's declaration states it: `b` itself, or `b` without its token —
    /// naming the token's file (`token_file`) instead where this carrier is
    /// [`Self::DeclaredFile`] and a token was minted.
    pub fn declared(self, b: &BridgeEnv, token_file: &Path) -> BridgeEnv {
        let mut declared = b.clone();
        if self.withholds() {
            declared.node_token = None;
        }
        if matches!(self, TokenCarrier::DeclaredFile { .. }) && b.node_token.is_some() {
            declared.node_token_file = Some(token_file.to_path_buf());
        }
        declared
    }

    /// Whether the harness's own environment names the token's file (the two `…Env` carriers),
    /// rather than the declaration or nothing.
    pub fn names_file_in_env(self) -> bool {
        matches!(
            self,
            TokenCarrier::InheritedEnv { .. } | TokenCarrier::ForwardedEnv { .. }
        )
    }

    /// What the harness's own environment carries for its bridge: the path of the token's file
    /// (`token_file`), where this carrier withholds the token from the declaration and one was
    /// minted. Present or absent, never empty; never the token.
    pub fn process_env(self, b: &BridgeEnv, token_file: &Path) -> Vec<(String, String)> {
        match (self.names_file_in_env(), &b.node_token) {
            (true, Some(_)) => vec![(
                NODE_TOKEN_FILE_ENV.to_string(),
                token_file.to_string_lossy().into_owned(),
            )],
            _ => Vec::new(),
        }
    }

    /// The token's file, `(token_file, token)`, where this carrier withholds the token from the
    /// declaration and one was minted: the caller writes it 0600 with the launch's other
    /// documents, before the harness starts.
    pub fn document(self, b: &BridgeEnv, token_file: &Path) -> Option<(PathBuf, String)> {
        match (self.withholds(), &b.node_token) {
            (true, Some(t)) => Some((token_file.to_path_buf(), t.expose().to_string())),
            _ => None,
        }
    }

    /// The variables a declaration must name for its harness to pass them on — the token file's,
    /// under [`Self::ForwardedEnv`]; none otherwise. Named whether or not a token was minted:
    /// passing on an unset variable passes nothing (measured on codex 0.145.0 through 0.155.1).
    pub fn forwarded(self) -> &'static [&'static str] {
        match self {
            TokenCarrier::ForwardedEnv { .. } => &[NODE_TOKEN_FILE_ENV],
            TokenCarrier::Declaration
            | TokenCarrier::InheritedEnv { .. }
            | TokenCarrier::DeclaredFile { .. } => &[],
        }
    }
}

/// The channel a launch's MCP declaration travels on — stated by the row, never inferred.
///
/// The distinction exists because "no configuration file" and "no bridge" are different facts that
/// look identical downstream. A live opencode node legitimately writes no file at all: its
/// declaration rides `OPENCODE_CONFIG_CONTENT`. Before this enum the supervisor read an empty
/// `config_files` as a refusal, and the obvious "fix" — accept an empty vec — would have turned
/// that refusal into a **hole**: any adapter that forgot its declaration entirely would launch a
/// node with no bridge, take a turn with no marion tools, and exit 0 having called nothing (§6.1
/// step 8's failure class). So the row says which route is taken and the supervisor checks *that*
/// route was actually taken ([`McpRoute::verify`](crate::McpRoute::verify)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpRoute {
    /// The first document `config_files` emits carries it.
    Document,
    /// This env var of the compiled [`Invocation`] carries it inline, and no file is written.
    Environment(&'static str),
    /// The compiled [`Invocation`]'s **argv** carries it inline, and no file is written — the
    /// payload is the config key that must appear in it. Neither a document nor an env var: a live
    /// codex node's `-c <dotted.key>=<toml>` flags.
    Argv(&'static str),
    /// The adapter's **post-launch** `session/new` request carries it, and the payload is the
    /// param key that must hold it (`mcpServers`). ACP, and only ACP: `session/new`'s block is
    /// compiled before anything is sent, so it is checkable at exactly the moment argv is (S21).
    Session(&'static str),
    /// No declaration was asked for — §9's fallback branch. **Not** the same as a row that was
    /// asked for one and produced none, which is a refusal.
    None,
}

/// What §6.7's `allowed_tools` records — *"the compiled, harness-native constraint, or the
/// harness's coarsest equivalent where it has no per-tool allowlist at all"* (§3.1). **Never
/// marion's own vocabulary**: echoing it there would make the field claim a constraint that never
/// existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    /// The permission axis itself, each entry prefixed — the literal contents of the list the
    /// harness checks a call against. `prefix` is load-bearing where the harness's grant kinds
    /// collide with marion's verbs (copilot's `write`).
    Allowed { prefix: &'static str },
    /// A coarse mode: `prefix` followed by the axes' mode, or by `default` where the launch relaxed
    /// nothing — recorded in **both** states, because a node that ran under the default mode ran
    /// under a real constraint.
    ///
    /// `allowed` is where a mode row that also grants single tools past the mode records them:
    /// each entry of the axes' allow list, under that prefix. `None`
    /// on a row whose adapter never grants one, and the sweep holds the two together.
    Mode {
        prefix: &'static str,
        default: &'static str,
        allowed: Option<&'static str>,
    },
    /// One fixed value for every launch: the sandbox mode codex always compiles, or the honest
    /// record that marion compiled no constraint at all. Not `[]`, which would read as "no tool
    /// was allowed" about a node that could run `bash`.
    Fixed {
        prefix: &'static str,
        value: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`Field::BaseUrlRoot`]'s value drops one trailing `/v1` and the slashes around it, and
    /// leaves a URL without one as it is.
    #[test]
    fn a_base_url_root_loses_the_v1_an_sdk_appends_for_itself() {
        assert_eq!(base_url_root("https://x.example/v1/"), "https://x.example");
        assert_eq!(base_url_root("https://x.example"), "https://x.example");
        assert_eq!(
            base_url_root("http://127.0.0.1:8099/v1"),
            "http://127.0.0.1:8099"
        );
    }

    /// **What a steer's acknowledgement promises is the row's own strategy.** A live run was told
    /// its message "reaches codex at its next tool round or turn" while its `exec` row took one
    /// only between runs, and it arrived a whole generation later. Only a folding typed row may
    /// promise a tool round; a relaunch row says the message waits for the run to end.
    #[test]
    fn a_delivery_promises_only_the_arrival_its_strategy_measured() {
        let fold = TurnDelivery::TypedTurn {
            mid_turn: MidTurn::Fold,
            note: "",
        };
        let queue = TurnDelivery::TypedTurn {
            mid_turn: MidTurn::Queue,
            note: "",
        };
        let relaunch = TurnDelivery::Continuation { note: "" };
        assert!(fold.arrival().contains("next tool round"));
        for d in [
            queue,
            relaunch,
            TurnDelivery::bracketed_paste(BootSignal::FirstDraw, ""),
        ] {
            assert!(
                !d.arrival().contains("tool round"),
                "{d:?}: {}",
                d.arrival()
            );
        }
        assert!(queue.arrival().starts_with("when its current turn ends"));
        assert!(
            relaunch.arrival().starts_with("when its current run ends")
                && relaunch.arrival().contains("only between runs"),
            "{}",
            relaunch.arrival()
        );
        // Every row's headless and interactive strategy says something, and none names a harness.
        for h in marion_core::Harness::ALL {
            for shape in [NodeShape::Headless, NodeShape::Interactive] {
                let words = delivery_for(crate::adapter::harness_spec(h), shape).arrival();
                assert!(!words.is_empty());
                for name in ["codex", "claude", "opencode", "gemini", "pi", "copilot"] {
                    assert!(!words.contains(name), "{words}");
                }
            }
        }
    }

    /// **A row's boilerplate stderr is not news**: codex prints "Reading additional input from
    /// stdin..." on every `exec`, and a live run's parent read it as the child's last word. Only
    /// a whole line the row names is dropped; anything around it, and any other harness's
    /// identical line, is kept.
    #[test]
    fn a_rows_boilerplate_stderr_lines_are_dropped_and_nothing_else() {
        let codex = crate::adapter::harness_spec(Harness::Codex);
        assert_eq!(
            codex.quiet_stderr("Reading additional input from stdin...\nreal trouble\n"),
            "real trouble"
        );
        assert_eq!(
            codex.quiet_stderr("Reading additional input from stdin...\n"),
            ""
        );
        assert_eq!(
            codex.quiet_stderr("x Reading additional input from stdin..."),
            "x Reading additional input from stdin..."
        );
        let claude = crate::adapter::harness_spec(Harness::ClaudeCode);
        assert_eq!(
            claude.quiet_stderr("Reading additional input from stdin..."),
            "Reading additional input from stdin..."
        );
    }

    /// A row reads the key through [`Field::ApiKey`] to place it where its harness looks; a
    /// `{:?}` of the fields it reads from must not show it.
    #[test]
    fn the_endpoint_key_is_readable_by_a_row_but_never_a_debug_print() {
        let f = Fields {
            api_key: Some("sk-SENTINEL-fields-93e0".into()),
            ..Fields::default()
        };
        let printed = format!("{f:?} {f:#?}");
        assert!(!printed.contains("SENTINEL"), "{printed}");
        assert_eq!(
            f.value(Field::ApiKey).as_deref(),
            Some("sk-SENTINEL-fields-93e0")
        );
    }

    /// **Every string a declaration or a credential has travelled in prints without its value**:
    /// config pairs and variables by name, documents by length, an agent's argv by count. Each of
    /// these once held the node token.
    #[test]
    fn no_field_that_can_carry_the_node_token_prints_it() {
        const T: &str = "tok-SENTINEL-fields-1f4a";
        let f = Fields {
            mcp_config: Some(format!("{{\"MARION_NODE_TOKEN\":\"{T}\"}}")),
            pairs: vec![("mcp_servers.marion.env.MARION_NODE_TOKEN".into(), T.into())],
            inline_config: Some(format!("{{\"MARION_NODE_TOKEN\":\"{T}\"}}")),
            agent_args: vec!["--additional-mcp-config".into(), T.into()],
            extra_env: vec![("MARION_NODE_TOKEN".into(), T.into())],
            ..Fields::default()
        };
        let printed = format!("{f:?} {f:#?}");
        assert!(!printed.contains("SENTINEL"), "{printed}");
        for shown in [
            "mcp_servers.marion.env.MARION_NODE_TOKEN=***",
            "MARION_NODE_TOKEN=***",
            "<2 args>",
        ] {
            assert!(printed.contains(shown), "{shown} missing from {printed}");
        }
    }
}
