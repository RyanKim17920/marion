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
//! turn one before its MCP server is up; gemini refuses a missing model because `auto` hangs; a
//! canned copilot refuses a missing base URL because the CLI goes looking for a GitHub login. Those
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

use std::path::PathBuf;

use marion_core::harness::Harness;
use marion_core::provider::{KeyHeader, Wire};

use crate::auth::Auth;
use crate::grammar::StreamGrammar;
use crate::invocation::Invocation;
use crate::mcp_bridge::BridgeEnv;
use crate::surfaces::{ExecutionSurfaces, TypedKind};

/// The name marion gives its own MCP server in every declaration, and therefore half of every
/// harness's model-facing spelling of marion's tools. **It must not contain `_`**: gemini exposes
/// MCP tools as `mcp_<server>_<tool>` and its policy engine mis-parses a fully-qualified name with
/// extra underscores, **silently** (S12).
pub const MCP_ALIAS: &str = "marion";

/// One harness, as a row: what its launch looks like, stated as data.
#[derive(Debug)]
pub struct HarnessSpec {
    pub harness: Harness,
    /// §3.4's point in the cross-product this harness runs at.
    pub surfaces: Surfaces,
    /// The binary, or `None` where the launch itself names it — ACP, whose program is the agent's
    /// own and arrives in [`Fields::program`].
    pub program: Option<&'static str>,
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
    /// The `clientInfo.name` this harness sends in MCP `initialize`, where it is measured — so a
    /// bridge the harness started itself (`marion mcp` in an operator's own MCP configuration,
    /// where no `MARION_AGENT_TYPE` names a row) can still find this row's [`Self::push`].
    /// `None` where it was never observed; such a bridge falls back to [`Push::McpLog`].
    pub client_name: Option<&'static str>,
    /// **How a message reaches this node's next turn**, per shape — the one mechanism behind both
    /// a child's end pushed to its parent and a parent's or operator's steer into a child.
    /// Resolved by [`delivery_for`] alone; the sweep `every_row_states_a_turn_delivery_its_
    /// surfaces_can_carry` checks each strategy against the rest of the row.
    pub delivery: Deliveries,
    /// The wires this harness can be pointed at in **endpoint mode**, as recipes, in its order of
    /// preference — only ones this row can actually render, so a provider serving a wire the
    /// harness could speak but the row cannot yet aim it at is refused rather than half-configured.
    /// Endpoint resolution takes the first one the provider serves natively, and the renderer
    /// applies that recipe; empty refuses every endpoint launch of this harness by name.
    pub wires: &'static [WireRecipe],
    /// **Mandatory.** The spike that measured this row, so a reader can tell a transcription from
    /// a guess. The spec sweep refuses an empty one.
    pub note: &'static str,
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
    /// Claude Code's `notifications/claude/channel`, **measured on 2.1.268**: the harness injects
    /// the frame's `content` as a new user turn (`<channel source="marion" k="v">…</channel>`,
    /// `isMeta: true`) and the model reacts within ~2 s — *only* in interactive mode, and *only*
    /// when started with `--dangerously-load-development-channels server:marion`. A bare
    /// `--channels server:marion` is accepted as a flag and refused by the research-preview
    /// allowlist, dropping the event silently. A full-screen "Loading development channels"
    /// dialog appears once at startup, default option 1 = accept. In `-p` mode the event is never
    /// enqueued at all, which is why the headless shape never carries [`Self::argv`]: the flag
    /// is variadic and would only swallow a trailing positional prompt. The server must declare
    /// `capabilities.experimental["claude/channel"]`, and every `meta` key must be
    /// identifier-shaped, since each becomes a tag attribute.
    ClaudeChannel,
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

/// [`Push::ClaudeChannel`]'s launch flag; its value names marion's server as every declaration
/// does (the sweep pins it against [`MCP_ALIAS`], which a `const` string cannot be spliced into).
const CLAUDE_CHANNEL_ARGV: &[&str] = &["--dangerously-load-development-channels", "server:marion"];

impl Push {
    /// The argv that enables this push on an **interactive** launch — or nothing.
    pub const fn argv(self) -> &'static [&'static str] {
        match self {
            Push::ClaudeChannel => CLAUDE_CHANNEL_ARGV,
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
    /// ([`Push::ClaudeChannel`]); the harness folds or queues it itself.
    McpChannel { note: &'static str },
    /// Typed into the node's terminal: `ESC[200~` + text + `ESC[201~` (always bracketed — codex
    /// turns an unbracketed burst's CR into a newline), then `submit` after `submit_delay_ms`,
    /// once `idle` says the TUI is waiting for input.
    TerminalPaste {
        idle: IdleSignal,
        submit: &'static [u8],
        submit_delay_ms: u16,
        note: &'static str,
    },
    /// No measured way. A message for such a node is refused by name, quoting `note`.
    None { note: &'static str },
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

    /// The paste S31 measured on codex, opencode, copilot and claude's TUIs: bracketed, then `\r`
    /// 50 ms later (0 ms submitted on all four; 50 is margin), once the terminal has been quiet
    /// for 1500 ms (busy spinners repaint at ≤ 454 ms, idle output is ~0). One constructor so the
    /// four rows cannot drift apart on values nobody measured separately.
    pub const fn bracketed_paste(note: &'static str) -> TurnDelivery {
        TurnDelivery::TerminalPaste {
            idle: IdleSignal::OutputQuiet { ms: 1500 },
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

/// **The one resolver**: the row's strategy for a node of this shape. No harness is named here —
/// the answer is the row's data, so a new harness is a new row and nothing else.
pub fn delivery_for(row: &HarnessSpec, shape: NodeShape) -> TurnDelivery {
    match shape {
        NodeShape::Headless => row.delivery.headless,
        NodeShape::Interactive => row.delivery.interactive,
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
    /// else — codex's `default_tools_approval_mode = "approve"`, gemini's `trust: true`.
    DeclarationKey {
        key: &'static str,
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
    /// The harness asks nothing headless for an MCP tool: its default is allow.
    None { note: &'static str },
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
            | Approval::None { note } => note,
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
            Approval::None { .. } => "none",
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
    /// Boolean keys, as dotted paths, in the JSON declaration document the row already emits —
    /// gemini's system settings. The document emitter applies them with
    /// [`UpdatePolicy::apply_to_json`], and the sweep reads them back from every emitted document.
    Document {
        keys: &'static [(&'static str, bool)],
        note: &'static str,
    },
    /// The installed binary has no measured switch, or never updates itself. `note` says which,
    /// and what was searched.
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
    /// none under a canned provider, opencode wants `provider/model`, gemini refuses `None`.
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
    /// The provider base URL in this harness's spelling, or `None` where none is overlaid.
    BaseUrl,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
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
    /// Environment the hook derived that no row names — appended after the row's own. ACP's
    /// per-agent canned recipe is the one user.
    pub extra_env: Vec<(String, String)>,
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
            Field::ApiKey => self.api_key.as_ref().map(|k| k.expose().to_string()),
            Field::InlineConfig => self.inline_config.clone(),
            Field::Title => self.title.clone(),
            Field::OutputSchema => self.output_schema.as_ref().and_then(path),
            Field::OutputLastMessage => self.output_last_message.as_ref().and_then(path),
            Field::Resume => self.resume.clone(),
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
/// ([`UpdatePolicy::Pair`]); every other field reads from `f` alone.
fn items(spec: &HarnessSpec, f: &Fields, field: Field) -> Vec<String> {
    let mut items = f.items(field);
    if let (Field::Pairs, Some((k, v))) = (field, spec.updates.pair()) {
        items.insert(0, format!("{k}={v}"));
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
    if f.resume.is_some() && !(spec.resume.is_some() && argv.contains(&Arg::Resume)) {
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
    env.extend(f.extra_env.iter().cloned());
    Ok(Invocation {
        program,
        args,
        env,
        cwd: f.cwd.clone(),
        // What the row carried, including its absence: the model that reached argv is the one the
        // hook placed in `Fields::model`, and nothing else is recorded.
        model: f.model.clone(),
        session_mode: f.session_mode.clone(),
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
}

impl Surfaces {
    pub fn execution(self) -> ExecutionSurfaces {
        match self {
            Surfaces::Headless(kind) => ExecutionSurfaces::headless(kind),
            Surfaces::LaunchOnly => ExecutionSurfaces::launch_only_with_protocol_events(),
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
/// | gemini CLI 0.53.0 | `mcp_marion_report` | S12 |
/// | opencode 1.17.3, `opencode acp` | `marion_report` | S13, S21 |
/// | copilot 1.0.83 | `marion-report` | s24 |
/// | `codex-acp` 1.1.14 | `mcp.marion.report` | S22 |
/// | goose 1.49.0, cline 3.0.61 | `marion__report` | S26, S27 |
/// | agy 1.2.8 | `call_mcp_tool{ServerName: marion, ToolName: report}`, shown as `marion/report` | s32 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSpelling {
    /// `mcp__<server>__<tool>`.
    McpDoubleUnderscore,
    /// `mcp_<server>_<tool>`, single underscores.
    McpSingleUnderscore,
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
            Self::McpSingleUnderscore => format!("mcp_{MCP_ALIAS}_{tool}"),
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
        body: fn(&BridgeEnv) -> String,
    },
    /// A document written to `file` under the node's own directory whose path rides the
    /// environment variable `key` — gemini's `GEMINI_CLI_SYSTEM_SETTINGS_PATH`.
    EnvDocument {
        key: &'static str,
        file: &'static str,
        body: fn(&BridgeEnv) -> String,
    },
    /// The document itself, inline in the environment variable `key`; nothing is written —
    /// opencode's `OPENCODE_CONFIG_CONTENT`.
    EnvInline {
        key: &'static str,
        body: fn(&BridgeEnv) -> String,
    },
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
        body: fn(&BridgeEnv) -> String,
    },
    /// `<flag> <dir>/<root>` on argv, naming a **directory** the harness reads a document out of:
    /// the document is written to `<root>/<file>` under the node's own directory — agy's
    /// `--add-dir <root>`, which loads `<root>/.agents/mcp_config.json` as a workspace's own MCP
    /// declaration (s32). The root is marion's and holds nothing else.
    ArgvRoot {
        flag: &'static str,
        root: &'static str,
        file: &'static str,
        body: fn(&BridgeEnv) -> String,
    },
}

impl LiveDeclaration {
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
    Mode {
        prefix: &'static str,
        default: &'static str,
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
}
