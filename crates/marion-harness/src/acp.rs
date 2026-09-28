//! The client half of ACP's `initialize` handshake — §3.3's **stage two**, for the one surface
//! where marion has a handshake and no static per-agent table.
//!
//! > §3.3: *"**Refined** at session open where a handshake exists (ACP `initialize`, app-server
//! > capability reads), narrowing — never widening — the static set."*
//!
//! # Why there is no per-agent static table here
//!
//! §3.3 keys the static table `(harness, harness_version, surfaces)`. For every other harness
//! marion knows the version before it launches anything, so the table can be looked up. For ACP it
//! cannot: *the agent supplies its own identity in the handshake*, and §5.2's `acp` row is **one**
//! adapter serving many agents. So the ACP stage one is the surface ceiling and nothing else, and
//! every per-agent fact arrives in stage two. [`AgentHandshake::caps`] is both stages in the order
//! §3.3 gives them.
//!
//! That is not a weaker model, it is the same model with the version arriving later — which is
//! precisely the shape spike S20 measured: two agents behind one adapter, differing on exactly two
//! of §3.3's ten fields.
//!
//! # What is measured, and against what
//!
//! `tests/fixtures/s20/{gemini,opencode}-initialize.json` are **verbatim** `initialize` responses
//! captured 2026-08-08 from `gemini --acp` 0.53.0 and `opencode acp` 1.17.3 on this machine. The
//! tests below parse those bytes; nothing here is written against a hand-made frame, because a
//! hand-made frame tests marion's idea of ACP rather than ACP.
//!
//! **What this module is not.** It is the wire, not a process. Nothing here spawns anything; the
//! frames below are values, and `marion_supervisor::doctor` is what puts them on a pipe.
//!
//! # The fifth `McpRoute`, and what is behind it
//!
//! `89b822d` refused to write an ACP `HarnessAdapter`, on the argument that
//! [`McpRoute`](crate::McpRoute) names a *pre-launch* declaration route while ACP's rides
//! `session/new` **after** launch — so a fifth variant would sit in the supervisor's declaration
//! check *"with nothing behind it to check"*. That last clause was the load-bearing one and **S21
//! measured it false**: `session/new`'s `mcpServers` is a declaration marion compiles before it
//! sends anything, so it can be checked exactly as argv is. The measurement is the whole
//! transcript in `tests/fixtures/s21/opencode-acp-mcp.jsonl` — a real `opencode acp` started
//! marion's MCP server, called `initialize`, called `tools/list`, and then called
//! `tools/call {"name":"report"}`. [`McpRoute::Session`](crate::McpRoute::Session) checks the
//! declaration marion compiled; it is a fourth channel, not a fourth spelling of "trust me".

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use marion_core::harness::Harness;

use crate::adapter::SESSION_NEW_ID;
use crate::adapter::{
    HarnessAdapter, HarnessError, LaunchSpec, McpDeclaration, Row, SpawnCtx, declared_bridge,
};
use crate::auth::Auth;
use crate::caps::Capabilities;
use crate::grammar::{
    ActivityRule, Cond, Reasoning, SessionId, TextUnit, ToolUnit, UsageFold, UsageRule, Where,
};
use crate::spec;
use crate::spec::{
    Advertised, Approval, Arg, AxesRule, BootDialogs, Constraint, Deliveries, Field, HarnessSpec,
    McpRoute, McpRoutes, MidTurn, ModelForm, Push, ReadOnly, Readiness, Remembers, Spelling,
    Surfaces, TokenCarriers, TurnDelivery, UpdatePolicy,
};
use crate::stream::{
    CallOutcome, ChildExit, MarionCall, StreamOutcome, json_frames, report_commits,
};
use crate::surfaces::{ExecutionSurfaces, TypedKind};
use crate::{copilot, grammar, opencode};

/// The ACP row: **one row, many agents.** It names no program and no flag of its own, because
/// there is nothing per-protocol to compile — the agent's argv ([`Agent::argv`]) is spliced in
/// whole, the prompt rides `session/prompt`, the model is chosen inside the session, and the bridge
/// is declared in `session/new`. The env is per-agent too ([`CannedRecipe`]) and arrives beside the
/// argv rather than as rows here.
pub const SPEC: HarnessSpec = HarnessSpec {
    harness: Harness::Acp,
    // `Typed(Acp)` control: a bidirectional JSON-RPC session marion can address a turn on.
    // `StructuredUi`, not `NativePty`: marion owns no terminal for an ACP agent, and `NativePty`
    // would mint a `PtyWitness` for a process that has none (§11 item 1).
    surfaces: Surfaces::Headless(TypedKind::Acp),
    program: None,
    argv: &[Arg::Items(Field::AgentArgs)],
    pane: None,
    env: &[],
    stream: None,
    // **Every marion verb is refused, by name.** ACP has no availability axis at all: nothing in
    // `initialize` or `session/new` names, grants or withholds a tool of the agent's own, and the
    // agent's tool list is a fact about a vendor this row is deliberately blind to.
    tool_names: &[],
    spelling: Spelling::PerAgent,
    // The one route that is a pipe rather than a file or an environ: `session/new`'s `mcpServers`.
    mcp: McpRoutes {
        canned: McpRoute::Session(MCP_SERVERS_KEY),
        live: McpRoute::Session(MCP_SERVERS_KEY),
    },
    // No launch-time channel: the declaration is sent after launch, so there is nothing a native
    // facade could inject, and no native adapter for this row.
    live_declaration: None,
    // Marion's own pipe: the `session/new` request carries the token beside the node's identity.
    token: TokenCarriers::DECLARATION,
    constraint: Constraint::Fixed {
        prefix: "",
        value: NO_TOOL_AVAILABILITY_SURFACE,
    },
    // ACP resumes through `session/load` where the agent advertises `loadSession` — a protocol
    // request, not argv — so the row names no flag and an argv resume is refused.
    resume: None,
    // The program is the agent's own, chosen at bind time, so no one switch fits the row: an
    // opencode agent's would be opencode's and a codex-acp agent's codex-acp's. The row states no
    // policy; the opencode agent's canned recipe reuses opencode's row, switch included
    // (`AcpAdapter::fields`), and every other agent runs with whatever its own binary does.
    updates: UpdatePolicy::None {
        note: "no single program: the switch is the bound agent's; only the opencode agent's \
               canned recipe carries one (opencode's row)",
    },
    // No stdio pipe of marion's to push on: the declaration is a `session/new` request.
    push: Push::None,
    // The protocol's own permission surface: the ACP driver answers `session/request_permission`
    // itself, selecting an offered allow option (S21); an agent type's `approval_mode` is set on
    // the session's mode select before the prompt (2026-09-22, all four measured agents).
    approval: Approval::SessionMode {
        category: MODE_CATEGORY,
        note: "S21: marion's client answers session/request_permission with the agent's own \
               allow option; 2026-09-22: an agent type's approval_mode is set on the session's \
               `mode` select (claude-agent-acp, codex-acp, copilot, opencode)",
    },
    // No protocol-wide switch: rejecting `session/request_permission` is not read-only on an agent
    // that never asks (s38: opencode acp's default allows, and the file landed). A mode such as
    // opencode's `plan` does hold, but it is one agent's string, so it belongs to an agent type's
    // `approval_mode`, never to this row.
    read_only: ReadOnly::ScopeOnly {
        note: "s38 on opencode acp 1.18.32: a `reject_once` answer holds only where the agent \
               asks; with its default `allow` no request came and the write landed. \
               `session/set_config_option mode=plan` refused it, agent-specifically",
    },
    client_name: None,
    delivery: Deliveries {
        // Protocol-generic: any agent takes a second `session/prompt` after the first resolved.
        headless: TurnDelivery::TypedTurn {
            // The protocol-generic answer. An agent measured to fold says so on its refinement
            // row ([`Agent::mid_turn`]), and `AcpAdapter::turn_delivery` layers it on.
            mid_turn: MidTurn::Queue,
            note: "S31 p0a/acp-*: a second session/prompt after the first resolved is a new turn \
                   on every agent probed; one written while a prompt is in flight is folded \
                   (opencode, claude-agent-acp), orphans the first (codex-acp) or supersedes it \
                   (copilot), so an agent no row measured to fold gets its next prompt only at \
                   the turn boundary",
        },
        interactive: TurnDelivery::None {
            note: "ACP has no interactive shape: no pane row and no native lane",
        },
    },
    // None: ACP has no provider channel in its handshake, and the per-agent canned recipes are not endpoint recipes yet. An endpoint launch of an ACP type is refused by name.
    boot_dialogs: BootDialogs {
        dialogs: &[],
        remembers: Remembers::Nothing,
        note: "ACP has no interactive shape",
    },
    wires: &[],
    // No carrier: the protocol row serves many agents, each with its own credential store.
    profile: None,
    note: "S20 (initialize on gemini --acp and opencode acp), S21 (a full opencode acp session \
           with a real marion_report call), S22 (the claude-agent-acp and codex-acp shims to \
           end_turn), S28 (copilot --acp to a real marion-report call; qwen, goose and gemini \
           refused session/new vendor-side). The argv of every refinement row is the one those \
           spikes launched; a generic `acp:<command>` row is the operator's own",
    requires: &[],
    axes: AxesRule::Split,
    // The model rides the session, set by the driver over the protocol, and under the opencode
    // canned recipe it is that config's own ([`AcpAdapter`]'s hook).
    model: ModelForm::Hook,
    readiness: Readiness::Ungated,
    // **The one row where `advertised` describes a protocol rather than a program**, because
    // §5.2's `acp` adapter serves many agents and the version here is not even readable until
    // one of them has answered `initialize`. So `version` is deliberately unused: it keys the
    // *agent*, and the agent is stage two's business.
    //
    // Five of the ten, and the five splits are three different arguments:
    //
    // * `token_deltas`, `usage` — **measured, S21.** A real `opencode acp` turn streamed
    //   `agent_message_chunk` frames one token at a time (`"The"`, `" user"`, `" wants"`) and
    //   emitted a `usage_update` with `used`/`size`/`cost`, and its `session/prompt` response
    //   carried a `usage` object. Both are in `tests/fixtures/s21/`.
    // * `fork`, `resume`, `view` — **the protocol has them and the handshake decides.** ACP v1
    //   defines `session/load` and advertises `sessionCapabilities`, and §3.3 makes ACP the
    //   worked example of a static set *refined* at session open. A `false` here would be
    //   final: [`Capabilities::meet`] cannot widen, so `opencode acp`'s advertised `fork` could
    //   never be published and M5's *"differing capabilities"* would have nothing to differ on.
    //   That is the one place this table states a protocol's shape rather than a measurement,
    //   and it is stated only because the very next stage narrows it per agent.
    // * `steer`, `interrupt`, `permissions`, `elicitation`, `set_model` — **not measured.** ACP
    //   defines all five (`session/prompt` mid-turn, `session/cancel`,
    //   `session/request_permission`, `session/request_input`, the `model` `configOption` S21
    //   saw in a `session/new` result), and marion has driven none of them against a live
    //   agent. §3.3's *degrade visibly*: a `false` here is "marion has not measured it", and
    //   the honest cost is an understated tool rather than a greyed-in action that then fails.
    advertised: Advertised {
        always: Capabilities {
            fork: true,
            resume: true,
            view: true,
            token_deltas: true,
            usage: true,
            ..Capabilities::NONE
        },
        from_version: &[],
    },
};

/// The wire protocol version marion speaks. `agent-client-protocol` 2.0.0 is still **wire v1**
/// (§5.2: *"v2 lives behind `unstable_protocol_v2`"*), and both agents S20 probed answered `1`.
pub const PROTOCOL_VERSION: u64 = 1;

/// §5.2's `acp` adapter row as a point in §3.4's cross-product: a typed plane over pipes.
///
/// Not `shared(Acp)`. marion owns no pty for an ACP agent — the agent's output arrives as protocol
/// frames, which is `StructuredUi`, and claiming `NativePty` would mint a
/// [`PtyWitness`](crate::PtyWitness) for a node with no terminal.
pub fn surfaces() -> ExecutionSurfaces {
    SPEC.surfaces.execution()
}

/// The `initialize` request marion sends, as a JSON-RPC frame ready for a newline-delimited pipe.
///
/// The `clientCapabilities` block is what S20's probe sent and both agents accepted. It is stated
/// here rather than in the probe so the shipped client and the measurement cannot drift.
pub fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": true, "writeTextFile": true},
                "terminal": true,
            },
        },
    })
}

/// Why a frame was not a usable handshake. Every variant names the agent's own words where it has
/// any, because §8's whole point about `--adapter` is that a probe reports *why*.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AcpError {
    #[error("the agent's first frame is not JSON: {0}")]
    NotJson(String),
    #[error("the agent answered `initialize` with a JSON-RPC error: {message} (code {code})")]
    Refused { code: i64, message: String },
    #[error("the agent's `initialize` frame carries neither `result` nor `error`")]
    NeitherResultNorError,
    #[error(
        "the agent speaks ACP wire v{got}; marion speaks v{PROTOCOL_VERSION} (§5.2: v2 is still \
         behind `unstable_protocol_v2`)"
    )]
    ProtocolVersion { got: u64 },
    #[error(
        "the agent's `initialize` result names no `agentInfo.name`, so it has no identity to key on"
    )]
    Anonymous,
}

/// What an agent said in its `initialize` result — the fields §3.3 keys on, plus the ones it
/// deliberately does **not**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHandshake {
    /// `agentInfo.name`. The first third of §3.3's key: for ACP, *this* is the harness identity.
    pub name: String,
    /// `agentInfo.version`. §3.3's middle third, supplied by the agent rather than looked up.
    pub version: Option<String>,
    /// `agentCapabilities.loadSession`. ACP v1: `session/load` MUST replay history (§5.2), which
    /// is §3.3's `view` and nothing else.
    pub load_session: bool,
    /// `agentCapabilities.sessionCapabilities`' keys — `{close, fork, list, resume}` on
    /// `opencode acp`, **absent entirely** on `gemini --acp`. Two of these are §3.3 fields; the
    /// other two are not, and are carried rather than mapped.
    pub session_capabilities: BTreeSet<String>,
    /// `agentCapabilities.promptCapabilities`' keys — `{image, audio, embeddedContext}` on gemini,
    /// `{image, embeddedContext}` on opencode.
    ///
    /// **Recorded, and deliberately mapped to no [`Capabilities`] field.** §3.3 lists ten fields
    /// and prompt content types are not among them, so `audio` gets carried here and claimed
    /// nowhere. The alternative — an eleventh capability — is the second-source-of-truth bug §3.3
    /// names, and `a_content_type_difference_is_recorded_and_claimed_nowhere` is the assertion
    /// that keeps the two apart.
    pub prompt_content_types: BTreeSet<String>,
    /// `authMethods[].id`. Carried because it is the evidence behind a refusal an operator has to
    /// act on: S20's `gemini --acp` blocker is *"migrate to Antigravity"*, and the route out of it
    /// is one of the ids this agent listed (`gemini-api-key`, `vertex-ai`, `gateway`) — a choice
    /// §6.4 forbids marion from making for the operator, and therefore one it must be able to
    /// *name*.
    pub auth_methods: Vec<String>,
    /// `agentCapabilities.mcpCapabilities`' keys whose value is `true` — the MCP transports this
    /// agent takes **beyond stdio**, which ACP requires of every agent (`http`, `sse`). Read, not
    /// tabled: the protocol advertises it, so no refinement row restates it. An advertised `false`
    /// (S33: `goose acp` sends `sse: false`) is not a transport, and an absent object is none.
    /// Carried like [`Self::prompt_content_types`] and mapped to no [`Capabilities`] field.
    pub mcp_transports: BTreeSet<String>,
}

impl AgentHandshake {
    /// Parse one JSON-RPC response frame — the whole line, as the agent wrote it.
    pub fn parse(frame: &str) -> Result<Self, AcpError> {
        let v: Value = serde_json::from_str(frame).map_err(|e| AcpError::NotJson(e.to_string()))?;
        if let Some(err) = v.get("error") {
            return Err(AcpError::Refused {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("<no message>")
                    .to_string(),
            });
        }
        let result = v.get("result").ok_or(AcpError::NeitherResultNorError)?;

        let got = result
            .get("protocolVersion")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if got != PROTOCOL_VERSION {
            return Err(AcpError::ProtocolVersion { got });
        }

        let info = result.get("agentInfo");
        let name = info
            .and_then(|i| i.get("name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or(AcpError::Anonymous)?
            .to_string();

        let agent = result.get("agentCapabilities");
        Ok(Self {
            name,
            version: info
                .and_then(|i| i.get("version"))
                .and_then(Value::as_str)
                .map(str::to_string),
            load_session: agent
                .and_then(|a| a.get("loadSession"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            session_capabilities: object_keys(agent, "sessionCapabilities"),
            prompt_content_types: object_keys(agent, "promptCapabilities"),
            auth_methods: result
                .get("authMethods")
                .and_then(Value::as_array)
                .map(|ms| {
                    ms.iter()
                        .filter_map(|m| m.get("id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            mcp_transports: agent
                .and_then(|a| a.get("mcpCapabilities"))
                .and_then(Value::as_object)
                .map(|o| {
                    o.iter()
                        .filter(|(_, on)| on.as_bool() == Some(true))
                        .map(|(k, _)| k.clone())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// §3.3's key, spelled for an agent that supplies its own identity.
    pub fn key(&self) -> String {
        match &self.version {
            Some(v) => format!("{} {}", self.name, v),
            None => self.name.clone(),
        }
    }

    /// **Stage two.** What this handshake narrows `base` to.
    ///
    /// Only the fields the handshake *speaks to* are narrowed; the rest meet with `true` and so
    /// come through untouched. That is the difference between refining a set and replacing it, and
    /// it is why an agent that advertises nothing (S20's `gemini --acp` advertises no
    /// `sessionCapabilities` at all) loses `fork` and `resume` and keeps whatever else the base
    /// held — rather than losing everything, which would read as "this agent can do nothing".
    ///
    /// The map is deliberately three fields wide. `close` and `list` are real
    /// `sessionCapabilities` keys with no §3.3 field to land on, and inventing one for them would
    /// be the same mistake as inventing `audio`.
    #[must_use]
    pub fn refine(&self, base: Capabilities) -> Capabilities {
        base.meet(Capabilities {
            fork: self.session_capabilities.contains("fork"),
            resume: self.session_capabilities.contains("resume"),
            view: self.load_session,
            ..Capabilities::ALL
        })
    }

    /// Both stages, in §3.3's order: the surface ceiling, narrowed by this agent's handshake.
    ///
    /// This is what `marion doctor` publishes for an ACP agent, and the reason M5's *"reporting
    /// their differing capabilities"* has something to report.
    pub fn caps(&self, s: &ExecutionSurfaces) -> Capabilities {
        self.refine(Capabilities::ceiling(s))
    }
}

/// The key set of `parent.field`, or empty when the object is absent. An **absent**
/// `sessionCapabilities` and an empty one are the same answer — no session capability is
/// advertised — and S20 measured the absent form, so collapsing them here is the measurement and
/// not a convenience.
fn object_keys(parent: Option<&Value>, field: &str) -> BTreeSet<String> {
    parent
        .and_then(|p| p.get(field))
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// The name marion gives its own MCP server in `session/new`. **Model-facing**, not internal: S21
/// measured `opencode acp` presenting the server's `report` tool to the model as `marion_report`,
/// so this string is half of what a compiled prompt has to say.
pub const MCP_SERVER_NAME: &str = crate::spec::MCP_ALIAS;

/// Why an ACP node with `tools: []` still writes (`Harness::writes_without_a_declaration`).
///
/// ACP has no field anywhere that narrows an agent's own tools — not in `initialize`, not in
/// `session/new`. S21's session had `write`, `edit` and `bash` in scope with marion asking for
/// nothing, and marion's own `clientCapabilities.fs.writeTextFile` hands the agent a further one.
/// What [`crate::AcpAdapter::marion_tool_name`] answers on the **unbound** adapter — the one built
/// from the harness name alone, with no agent and so no reading. A bound adapter with no measured
/// spelling answers [`GENERIC_SPELLING`] instead; this sentinel is for the adapter that cannot be
/// launched at all.
///
/// **Not a tool name in any of the four measured spellings, and deliberately not one in any
/// plausible fifth**: it carries a colon, which no MCP tool name may. So it matches nothing in any
/// transcript and would be a permission entry naming a tool that cannot exist — which is why no
/// launch is allowed to reach it, and every route to a launch refuses first, by name.
pub const UNBOUND_TOOL_NAME: &str = "acp:no-agent-bound:";

pub const NO_TOOL_AVAILABILITY_SURFACE: &str =
    "acp:no-tool-availability-surface (marion compiles no constraint; the protocol has none)";

pub use crate::spec::ToolSpelling;

/// The three spellings ACP agents were measured to use, and where the verb's **arguments** sit
/// for each — which is not always the `rawInput` itself.
impl ToolSpelling {
    /// The verb's **arguments**, given a frame's `rawInput` — which is not always the arguments.
    ///
    /// S22 measured `codex-acp` reporting a marion call as an `execute` kind whose `rawInput` is
    /// *structured*: `{"server":"marion","tool":"report","arguments":{"narrative":"…"}}`. A reader
    /// written against S21's flat shape finds the call and reads no arguments out of it — which is
    /// a run that reported nothing, recorded as a run whose report was empty.
    ///
    /// Keyed on the spelling rather than sniffed for an `arguments` key, because the nesting is a
    /// **fact about an agent** measured alongside its spelling, and a shape sniff would also fire
    /// on a marion verb that one day takes an argument called `arguments`.
    pub fn arguments(self, raw_input: &Value) -> Option<&Value> {
        match self {
            Self::McpDotted => raw_input.get("arguments"),
            _ => Some(raw_input),
        }
        .filter(|a| a.as_object().is_some_and(|o| !o.is_empty()))
    }
}

/// How marion's verbs are recognised in one agent's transcript: **the baseline, or a measured
/// refinement of it.**
///
/// Four agents have been watched calling `report`, and they spelled it four ways — `marion_report`
/// (S21), `mcp__marion__report` and `mcp.marion.report` (S22), `marion-report` (S28). What every one
/// of them has in common is the pair the name is built from: marion's server alias and the verb,
/// joined by *some* separator, sometimes under an `mcp` prefix. [`Reading::Generic`] recognises
/// that pair and nothing narrower, which is what lets an agent marion has never named — the
/// `acp:<command>` path — be read at all. [`Reading::Measured`] is the refinement: the one exact
/// name a row was watched to use, so a transcript is never read in a neighbour's spelling and the
/// pinned captures stay pinned.
///
/// The generic reader is deliberately **not** a prefix match on `marion`: S22's `codex-acp` opens
/// every session with a `tool_call` titled `mcp__marion__startup` — a startup diagnostic for a verb
/// marion does not have — and a reader that took "starts with marion's alias" as "one of marion's
/// verbs" would report it as marion's. The generic reader names *that* call `startup`, which is what
/// the agent named it, and [`parse_stream`] reads only `report` off the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The exact model-facing name this agent was watched to use (an [`Agent::tools`] row).
    Measured(ToolSpelling),
    /// The `<server> <verb>` pair, read off whatever the agent wrote. The baseline every ACP agent
    /// gets until somebody has measured it.
    Generic,
}

impl From<ToolSpelling> for Reading {
    fn from(s: ToolSpelling) -> Self {
        Self::Measured(s)
    }
}

impl Reading {
    /// The marion verb an opening `tool_call` update names, or `None` where the call is not one of
    /// marion's in this reading.
    ///
    /// Two shapes carry the verb on the measured agents and the generic reader takes both:
    /// the `title` (every agent), and — where an agent structures the call — `rawInput`'s own
    /// `server`/`tool` pair (S22's `codex-acp`: `{"server":"marion","tool":"report",…}`), which is
    /// the *strongest* evidence a frame can carry because it names the pair outright rather than
    /// spelling it.
    pub fn verb(self, update: &Value) -> Option<String> {
        match self {
            Self::Measured(s) => update
                .get("title")
                .and_then(Value::as_str)
                .and_then(|t| t.strip_prefix(s.spell("").as_str()))
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            Self::Generic => update
                .get("rawInput")
                .and_then(structured_pair)
                .or_else(|| {
                    update
                        .get("title")
                        .and_then(Value::as_str)
                        .and_then(spelled_pair)
                })
                .map(str::to_string),
        }
    }

    /// The verb's **arguments** out of a `rawInput`.
    ///
    /// Measured: the row's own answer ([`ToolSpelling::arguments`]). Generic: `arguments` when the
    /// input is the structured `{server, tool, arguments}` triple **naming marion's server**, else
    /// the input itself. The generic branch keys on the whole triple rather than on an `arguments`
    /// key alone, for the reason [`ToolSpelling::arguments`] gives: a marion verb that one day takes
    /// an argument called `arguments` would not also carry a `server` and a `tool`.
    pub fn arguments(self, raw_input: &Value) -> Option<&Value> {
        match self {
            Self::Measured(s) => s.arguments(raw_input),
            Self::Generic => structured_pair(raw_input)
                .and_then(|_| raw_input.get("arguments"))
                .or(Some(raw_input))
                .filter(|a| a.as_object().is_some_and(|o| !o.is_empty())),
        }
    }

    /// The name a model-facing tool carries in this reading — the exact spelling where one was
    /// measured, and the baseline [`GENERIC_SPELLING`] where none was.
    pub fn spell(self, tool: &str) -> String {
        match self {
            Self::Measured(s) => s.spell(tool),
            Self::Generic => GENERIC_SPELLING.spell(tool),
        }
    }
}

/// What the generic reading answers when asked to *spell* a marion verb rather than read one.
///
/// On the ACP row the answer reaches no model: ACP has no tool-availability surface, so
/// `compiled_permissions` records [`NO_TOOL_AVAILABILITY_SURFACE`] and never this string, and the
/// prompt rides `session/prompt` verbatim. It is the plainest of the four measured shapes and the
/// first one measured (S21), and it is a **default**, not a claim about any agent: the reader does
/// not depend on it, which is the whole point of [`Reading::Generic`].
pub const GENERIC_SPELLING: ToolSpelling = ToolSpelling::ServerUnderscoreTool;

/// The verb out of a structured `rawInput` — `{"server": <alias>, "tool": <verb>, …}` — where the
/// server is marion's. `None` for every other object, including one naming another server.
fn structured_pair(raw_input: &Value) -> Option<&str> {
    let server = raw_input.get("server")?.as_str()?;
    if server != MCP_SERVER_NAME {
        return None;
    }
    raw_input.get("tool")?.as_str().filter(|t| !t.is_empty())
}

/// The verb out of a spelled tool name — `<alias><sep><verb>` or `mcp<sep><alias><sep><verb>`, for
/// any run of non-alphanumeric characters as the separator. Exactly two tokens after the optional
/// `mcp`, so `marion_report_extra` or a bare `marion` is not a verb of marion's.
fn spelled_pair(title: &str) -> Option<&str> {
    let mut tokens = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty());
    let mut first = tokens.next()?;
    if first == "mcp" {
        first = tokens.next()?;
    }
    if first != MCP_SERVER_NAME {
        return None;
    }
    let verb = tokens.next()?;
    tokens.next().is_none().then_some(verb)
}

/// One ACP agent marion has **measured** — a refinement over the generic path, never a
/// prerequisite for it. §5.2's `acp` row is one adapter over many of these and over every agent
/// that is not one of these.
///
/// **A table of measurements, not of intentions.** `argv` is what a spike launched; `tools` is
/// `None` until somebody has seen that agent call a tool, and an agent with none is read with
/// [`Reading::Generic`] rather than with a neighbour's spelling. What a row adds over the baseline
/// is exactly what was measured: the pinned spelling, a canned recipe, a quirk in how the bridge
/// reaches it, and the note an operator reads when the agent cannot run here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    /// What an operator writes to select it (`Extras::acp_agent`), and the first word of an
    /// `acp:<command>` selector that should bind this row instead of the generic path.
    pub id: &'static str,
    /// The agent's own argv, verbatim as the spike launched it.
    pub argv: &'static [&'static str],
    /// The measured tool spelling, or `None` where marion has never seen this agent call a tool.
    pub tools: Option<ToolSpelling>,
    /// How this agent is pointed at a provider **marion** chose, or `None` where marion has never
    /// made one do it. See [`CannedRecipe`].
    pub canned: Option<CannedRecipe>,
    /// How marion's bridge reaches this agent. [`Declaration::Session`] on every row but one.
    pub declaration: Declaration,
    /// What this agent was measured to do with a `session/prompt` sent while one is in flight,
    /// overriding the row's [`MidTurn::Queue`]; `None` where it was not measured.
    pub mid_turn: Option<MidTurn>,
    /// How this agent's `session/prompt` usage counts the model's reasoning — `thoughtTokens`
    /// **beside** `outputTokens` or **within** it — refining [`USAGE`], which says nothing about
    /// reasoning. `None` where no split was measured. One of [`USAGES`]' rules, by the sweep test.
    pub reasoning: Option<Reasoning>,
    /// The `agentInfo.name` this agent answered `initialize` with — its identity **on the wire**,
    /// which is how a binary behind any command line is recognised as this row
    /// ([`identity_note`]). Held to the row's S33 capture by the sweep test.
    pub agent_info: &'static str,
    /// The user-level command that installs it, quoted where the binary is missing.
    pub install: &'static str,
    /// How far `session/new` got on the machine that measured it (S33), held to the capture.
    pub reach: Reach,
    /// What is known about running it here — carried so a refusal can quote it.
    pub note: &'static str,
}

/// How far an agent's `session/new` got when it was measured — without an account marion made,
/// a login marion ran, or any `authenticate` request.
///
/// **A record, not a gate.** The driver never reads it: what a live session offers is read off
/// the live answer ([`session_select`]), and a refusal arrives in the agent's own words. The row
/// carries it so the doctor and the docs can say which agents opened a session and which stop at
/// an account wall, and the sweep test keeps it true against the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// `session/new` answered with a session.
    Opened {
        /// Only once the agent was pointed at a provider through its own env (a dummy key, a
        /// loopback base URL) — on an operator's machine, their own provider configuration.
        provider: bool,
        /// The answer advertised a [`MODEL_CATEGORY`] select.
        model: bool,
        /// The answer advertised a [`MODE_CATEGORY`] select (a config option or `modes`).
        mode: bool,
    },
    /// `session/new` was refused — an account wall, or a vendor-side refusal.
    Refused,
}

/// The channel marion's bridge is declared on for one agent.
///
/// The protocol has exactly one: `session/new`'s `mcpServers` (S21 watched `opencode acp` start the
/// declared server; the two Registry shims did the same in S22). [`Declaration::Argv`] exists for
/// the agent that was measured to *ignore* that channel while honouring one of its own — a quirk,
/// carried on a refinement row because the generic path can only ever use what the protocol gives
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Declaration {
    /// The protocol's own: the `mcpServers` block of `session/new` (and of `session/load`).
    Session,
    /// The agent's own argv flag, taking one JSON document in the shape of copilot's
    /// `~/.copilot/mcp-config.json` ([`crate::copilot::mcp_config_json`]) — and the session block
    /// is then left **empty**, so that a version which one day honours the protocol channel does
    /// not start a second bridge. Argv is readable through `ps`, so the document never carries the
    /// node token: `token` states how the agent passes it on instead, and must withhold it.
    Argv {
        flag: &'static str,
        token: crate::spec::TokenCarrier,
    },
}

/// The flag copilot takes its per-session MCP document on. S28 measured it, and measured that the
/// protocol-standard channel does nothing on 1.0.83.
pub const COPILOT_MCP_FLAG: &str = "--additional-mcp-config";

/// How one ACP agent is pointed at [`crate::Auth::Canned`]'s endpoint.
///
/// **This is per agent because there is nothing per protocol.** ACP's `initialize` and
/// `session/new` name no provider, no base URL and no credential — a fact `AcpAdapter::compile`
/// used to turn into a blanket refusal of `Auth::Canned` on every agent. That refusal read the
/// right fact and drew the wrong conclusion: the *protocol* has no such channel, and the *agent*
/// behind it may have one that marion already knows how to compile. An agent with no measured
/// recipe is still refused, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CannedRecipe {
    /// opencode's own config document under `$XDG_CONFIG_HOME`, plus the sandbox relocations —
    /// exactly what [`crate::opencode::config_json`] and [`crate::opencode::isolation_env`] already
    /// compile for `opencode run`, because `opencode acp` is the same binary reading the same
    /// files.
    ///
    /// **Derived, not yet measured on `opencode acp` itself.** What *is* measured is S13: the same
    /// binary, under the same document at the same path, running against marion's canned provider
    /// at $0.00 on the `opencode run` subcommand. The step this recipe takes on top is that `acp`
    /// reads the same document — which is a claim about opencode's config loading being
    /// subcommand-independent, and it is the reason this variant names a document rather than an
    /// argv: there is no argv. Until an ACP-subcommand capture exists, treat the row as one
    /// inference deep.
    ///
    /// The document's `model` key is what carries the model, since ACP chooses the model *inside*
    /// the session (S21's `session/new` result carries a `configOptions` `model` select) and marion
    /// has measured no argv that sets it.
    OpencodeConfigDocument,
}

/// `opencode acp` — the one agent measured all the way to a tool call (S21).
pub const OPENCODE: Agent = Agent {
    id: "opencode",
    argv: &["opencode", "acp"],
    tools: Some(ToolSpelling::ServerUnderscoreTool),
    canned: Some(CannedRecipe::OpencodeConfigDocument),
    declaration: Declaration::Session,
    // S31 `p0a/acp-opencode-fold` (opencode 1.18.32): the second prompt is folded into the running
    // loop and both responses arrive when it drains.
    mid_turn: Some(MidTurn::Fold),
    // s36 (1.18.32): the prompt response's `thoughtTokens` 7 sits beside `outputTokens` 43 of the
    // provider's 50 completion tokens, and `totalTokens` 1050 counts both.
    reasoning: Some(Reasoning::Beside(THOUGHT_TOKENS)),
    agent_info: "OpenCode",
    install: "brew install opencode",
    reach: Reach::Opened {
        provider: false,
        model: true,
        mode: true,
    },
    note: "S21: initialize, session/new, session/prompt and a real `marion_report` tool call, \
           against opencode 1.17.3. Its canned recipe is inferred from S13 over the same binary, \
           not measured on the `acp` subcommand",
};

/// `gemini --acp` — completes `initialize` and is refused `session/new` **vendor-side**.
///
/// It is in the table because a doctor row saying *why* an installed agent cannot run is worth
/// more than an absent row, and its `tools` is `None` because no turn has ever happened: S20's
/// `session/new` answers `-32000`, *"This client is no longer supported for Gemini Code Assist for
/// individuals"*, reproduced with no marion involved.
pub const GEMINI: Agent = Agent {
    id: "gemini",
    argv: &["gemini", "--acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    // No turn has ever run, so nothing mid-turn was measured.
    mid_turn: None,
    reasoning: None,
    agent_info: "gemini-cli",
    install: "npm i -g @google/gemini-cli",
    reach: Reach::Refused,
    note: "S20: `initialize` succeeds; `session/new` is refused -32000 (Gemini Code Assist \
           ineligibility). No turn has run, so no tool spelling has been measured",
};

/// `@agentclientprotocol/claude-agent-acp` — an ACP Registry **shim**, not a vendor's own server.
///
/// S22 ran it to a terminal `end_turn` against the operator's already-established `claude` login,
/// with no new credential supplied, and watched it call marion's declared MCP tool. `argv` is the
/// command S22 launched, **version-pinned**, because an unpinned `npx -y` resolves to whatever the
/// registry holds today and this row's `tools` is a measurement of 0.66.0 and of nothing else.
pub const CLAUDE_ACP: Agent = Agent {
    id: "claude-acp",
    argv: &["npx", "-y", "@agentclientprotocol/claude-agent-acp@0.66.0"],
    tools: Some(ToolSpelling::McpDoubleUnderscore),
    canned: None,
    declaration: Declaration::Session,
    // S31 `p0a/acp-claude-acp-fold`, measured on 0.81.0 rather than this row's pinned 0.66.0: the
    // second prompt is folded or queued into the running loop, both responses at the drain.
    mid_turn: Some(MidTurn::Fold),
    reasoning: None,
    agent_info: "@agentclientprotocol/claude-agent-acp",
    install: "npm i -g @agentclientprotocol/claude-agent-acp",
    reach: Reach::Opened {
        provider: false,
        model: true,
        mode: true,
    },
    note: "S22: initialize, session/new, session/prompt to `end_turn` and a real \
           `mcp__marion__report` call, wrapping the operator's own claude-code 2.1.220. S33: \
           0.66.0 still opens a session; 0.81.2 (the registry's current) starts the declared \
           bridge at session/new",
};

/// `@agentclientprotocol/codex-acp` — the second ACP Registry shim, over the local `codex`.
///
/// `argv` names the installed executable rather than `npx -y @agentclientprotocol/codex-acp`, and
/// that is a measurement too: S22's first probe of this agent returned **zero frames** and looked
/// like a dead agent, because `npx -y` was still downloading `@openai/codex` when the 30 s
/// `initialize` budget expired. `codex-acp` is the bin npm installs, and run from
/// `node_modules/.bin` the same version handshakes in under a second. A cold download is not a
/// property of the agent, and compiling one into a launch would make every first run look like a
/// hang.
pub const CODEX_ACP: Agent = Agent {
    id: "codex-acp",
    argv: &["codex-acp"],
    tools: Some(ToolSpelling::McpDotted),
    canned: None,
    declaration: Declaration::Session,
    // S31 `p0a/acp-codex-acp` (1.13.0): the second prompt is steered into the turn and the first
    // is never answered, so a message waits for the turn boundary.
    mid_turn: Some(MidTurn::Queue),
    // s22 (1.1.14): `outputTokens` is codex's own `output_tokens`, which already holds its
    // `reasoningOutputTokens` (the response's `_meta.quota` carries both, and `thoughtTokens` is
    // the latter).
    reasoning: Some(Reasoning::Within(THOUGHT_TOKENS)),
    agent_info: "@agentclientprotocol/codex-acp",
    install: "npm i -g @agentclientprotocol/codex-acp",
    reach: Reach::Opened {
        provider: false,
        model: true,
        mode: true,
    },
    note: "S22: initialize, session/new, session/prompt to `end_turn` and a real \
           `mcp.marion.report` call, wrapping the operator's own codex 0.147.0. S33: 1.13.1 \
           opens a session under the same login and starts the declared bridge at session/new. \
           Install it rather than relying on `npx -y`",
};

/// `copilot --acp` — GitHub Copilot CLI's own ACP server, measured to a real `report` call (S28).
///
/// Its spelling is the fourth: `marion-report`, `<server>-<tool>`. Its quirk is the one that makes
/// the row worth having: on 1.0.83 the `mcpServers` block of `session/new` is accepted and
/// **ignored** — the declared server is never started (S28: two sessions, zero frames reached it,
/// and the model answered that the tool *"is not available in this session"*). The bridge reaches
/// copilot only through its own argv channel, `--additional-mcp-config <json>`, which is the
/// document marion's copilot adapter already compiles for `copilot -p`; the row's `note` says so
/// because a generic `acp:copilot --acp` launch would open a session, take the turn, and report
/// nothing.
pub const COPILOT: Agent = Agent {
    id: "copilot",
    argv: &["copilot", "--acp"],
    tools: Some(ToolSpelling::ServerHyphenTool),
    canned: None,
    declaration: Declaration::Argv {
        flag: COPILOT_MCP_FLAG,
        token: crate::spec::TokenCarrier::InheritedEnv {
            note: "copilot 1.0.83 `--acp` (2026-09-27, stub MCP server declared by \
                   `--additional-mcp-config` with no `env`): the server's environment is \
                   copilot's own, `MARION_NODE_TOKEN` included",
        },
    },
    // S31 `p0a/acp-copilot` (1.0.87): the second prompt supersedes the first, which returns an
    // empty `end_turn` with stale usage, so a message waits for the turn boundary.
    mid_turn: Some(MidTurn::Queue),
    reasoning: None,
    agent_info: "Copilot",
    install: "npm i -g @github/copilot",
    reach: Reach::Opened {
        provider: false,
        model: false,
        mode: true,
    },
    note: "S28: initialize, session/new, session/prompt to `end_turn` and a real `marion-report` \
           call against copilot 1.0.83 — but only with the bridge declared through \
           `--additional-mcp-config`; the `session/new` `mcpServers` declaration is ignored by \
           this version, so a generic launch of it reaches no bridge (S33 reconfirmed on 1.0.83 \
           with COPILOT_AUTO_UPDATE=false: the declared server never started)",
};

/// `kilo acp` — Kilo Code's CLI, an opencode fork, and the one S33 agent that opened a session
/// with no account and no configuration at all.
pub const KILO: Agent = Agent {
    id: "kilo",
    argv: &["kilo", "acp"],
    tools: None,
    // opencode's config document is likely its shape too, and that is exactly an unmeasured claim.
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "Kilo",
    install: "npm i -g @kilocode/cli",
    reach: Reach::Opened {
        provider: false,
        model: true,
        mode: true,
    },
    note: "S33: kilo 7.8.1 opens a session with no account and starts the stdio bridge declared \
           in session/new; a prompt needs a Kilo login or a provider in its own config, so no \
           turn has run",
};

/// `qwen --acp` — refused by S28 with no provider, opened by S33 with one.
pub const QWEN: Agent = Agent {
    id: "qwen",
    argv: &["qwen", "--acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "qwen-code",
    install: "npm i -g @qwen-code/qwen-code",
    reach: Reach::Opened {
        provider: true,
        model: true,
        mode: true,
    },
    note: "S33: qwen-code 0.23.0 refuses session/new until a provider is configured (S28), and \
           with OPENAI_API_KEY/OPENAI_BASE_URL/OPENAI_MODEL set it opens and runs a turn to \
           end_turn against a local endpoint. It starts the declared bridge but defers MCP tools \
           behind its own tool_search, so the model is not offered marion's verbs up front",
};

/// `goose acp` — Block's goose, opened on the provider its own config names.
pub const GOOSE: Agent = Agent {
    id: "goose",
    argv: &["goose", "acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "goose",
    install: "brew install block-goose-cli",
    reach: Reach::Opened {
        provider: true,
        model: true,
        mode: true,
    },
    note: "S33: goose 1.52.0 refuses session/new with a bare -32603 until a provider is \
           configured (GOOSE_PROVIDER, or its own config), then opens and runs a turn to \
           end_turn against a local endpoint. The declared bridge was not started within 8 s of \
           session/new",
};

/// `fast-agent-acp` — fast-agent's ACP entrypoint, measured to a real call on marion's bridge.
pub const FAST_AGENT: Agent = Agent {
    id: "fast-agent",
    argv: &["fast-agent-acp", "-x"],
    // S33 watched a real `tools/call` reach the bridge, titled `marion/report` — which the generic
    // reading already reads, so no spelling of its own is recorded.
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "fast-agent-acp",
    install: "uv tool install fast-agent-acp",
    reach: Reach::Opened {
        provider: true,
        model: false,
        mode: true,
    },
    note: "S33: fast-agent-acp 0.10.37 exits at initialize with no model configured \
           (FAST_AGENT_MODEL or its own config). Configured, it opens (modes only, no config \
           options), asks session/request_permission for marion's tool, and a real call reached \
           the bridge — offered to the model as `marion__report`, titled `marion/report`",
};

/// `vibe-acp` — Mistral Vibe's ACP entrypoint.
pub const VIBE: Agent = Agent {
    id: "vibe",
    argv: &["vibe-acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "@mistralai/mistral-vibe",
    install: "uv tool install mistral-vibe",
    reach: Reach::Opened {
        provider: true,
        model: true,
        mode: true,
    },
    note: "S33: mistral-vibe 2.25.8 refuses session/new without MISTRAL_API_KEY, opens with one \
           and starts the declared bridge; a prompt goes to Mistral's own API, so no turn has run",
};

/// `vtcode acp` — VT Code, whose ACP server is off until the operator turns it on.
pub const VTCODE: Agent = Agent {
    id: "vtcode",
    argv: &["vtcode", "acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "vtcode",
    install: "brew install vtcode",
    reach: Reach::Opened {
        provider: false,
        model: true,
        mode: false,
    },
    note: "S33: vtcode 0.169.0 exits \"Agent Client Protocol integration is disabled\" unless \
           VT_ACP_ENABLED=1 or `[acp]` in its vtcode.toml. Enabled, it opens with no key; its \
           agent select carries no `mode` category, so an approval_mode finds no select. The \
           declared bridge was not started at session/new, and a prompt needs a provider key",
};

/// `auggie --acp` — Augment's CLI, stopped at its account wall.
pub const AUGGIE: Agent = Agent {
    id: "auggie",
    argv: &["auggie", "--acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "auggie",
    install: "npm i -g @augmentcode/auggie",
    reach: Reach::Refused,
    note: "S33: auggie 0.36.0 offers no authMethods and refuses session/new until the operator \
           runs `auggie login` in a terminal (an Augment account)",
};

/// `qodercli --acp` — Qoder's CLI, stopped at its account wall.
pub const QODER: Agent = Agent {
    id: "qoder",
    argv: &["qodercli", "--acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "qoder-cli",
    install: "npm i -g @qoder-ai/qodercli",
    reach: Reach::Refused,
    note: "S33: qoder-cli 1.1.64 refuses session/new until the operator runs `qodercli login` \
           (a Qoder account)",
};

/// `cline --acp` — the Cline CLI's ACP mode, which wants the protocol's `authenticate` first.
pub const CLINE: Agent = Agent {
    id: "cline",
    argv: &["cline", "--acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "cline",
    install: "npm i -g cline",
    reach: Reach::Refused,
    note: "S33: cline 3.0.61 refuses session/new with \"Call authenticate before starting a \
           session\" — an `authenticate` request choosing one of its sign-in methods, a choice \
           marion leaves to the operator",
};

/// `pi-acp` — the community shim over the `pi` coding agent, refused by a version skew.
pub const PI_ACP: Agent = Agent {
    id: "pi-acp",
    argv: &["pi-acp"],
    tools: None,
    canned: None,
    declaration: Declaration::Session,
    mid_turn: None,
    reasoning: None,
    agent_info: "pi-acp",
    install: "npm i -g pi-acp",
    reach: Reach::Refused,
    note: "S33: pi-acp 0.0.34 refuses session/new -32603 because it calls \
           `get_available_thinking_levels`, which the installed pi 0.80.2 does not have — a skew \
           between the shim and pi, not an account wall",
};

/// Every ACP agent marion has a refinement row for. Naming one is not having measured it — see
/// [`Agent::tools`] — and not being named is not being refused: see [`Binding`].
///
/// **Not every probed agent is a row.** Factory's `droid` answers `session/new` without a key by
/// *starting a device pairing* and printing its code (S33), and the doctor's `--adapter` mode opens
/// a session on every row — so a row would make a routine probe start a login flow. It stays
/// reachable as the operator's own `acp:droid exec --output-format acp-daemon`.
pub const AGENTS: [Agent; 15] = [
    OPENCODE, GEMINI, CLAUDE_ACP, CODEX_ACP, COPILOT, KILO, QWEN, GOOSE, FAST_AGENT, VIBE, VTCODE,
    AUGGIE, QODER, CLINE, PI_ACP,
];

/// The refinement row for an id, or `None` where marion has none — which is **not** a refusal;
/// [`Binding::resolve`] falls back to the generic path.
pub fn agent(id: &str) -> Option<Agent> {
    AGENTS.into_iter().find(|a| a.id == id)
}

/// **What an ACP launch is bound to**: the agent's argv, and whatever refinement marion has for it.
///
/// This is the layering the `acp` row is built on. An operator's selector — an agent type's
/// `acp_agent`, whether from the `acp-opencode` built-in or an `acp:<command>` type — is resolved
/// **once**, here, and in one order: a word that is a row's [`Agent::id`] binds that row and its
/// measured argv; anything else is a command line, split on whitespace, bound to no row. Both are
/// launchable. The difference is what marion *knows* about the agent, which is exactly what a
/// refinement is.
///
/// Whitespace-split and nothing cleverer, on purpose: a selector is typed by the operator into an
/// agent-type string, and a shell-quoting grammar here would be a second shell with its own bugs.
/// An argument that needs a space in it needs a wrapper script, and the refusal below says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    selector: String,
    argv: Vec<String>,
    refinement: Option<Agent>,
}

impl Binding {
    /// Resolve a selector. The only refusal is the one the protocol cannot get past: no program.
    pub fn resolve(selector: &str) -> Result<Self, BindError> {
        let selector = selector.trim();
        if let Some(agent) = agent(selector) {
            return Ok(Self::refined(agent));
        }
        let argv: Vec<String> = selector.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return Err(BindError::NoProgram);
        }
        Ok(Self {
            selector: selector.to_string(),
            argv,
            refinement: None,
        })
    }

    /// A binding straight from a refinement row — what `marion doctor` builds when it enumerates
    /// [`AGENTS`].
    pub fn refined(agent: Agent) -> Self {
        Self {
            selector: agent.id.to_string(),
            argv: agent.argv.iter().map(|s| s.to_string()).collect(),
            refinement: Some(agent),
        }
    }

    /// What the operator wrote, trimmed.
    pub fn selector(&self) -> &str {
        &self.selector
    }

    /// The program and its arguments. Never empty.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The row this binding refines, where there is one.
    pub fn refinement(&self) -> Option<&Agent> {
        self.refinement.as_ref()
    }

    /// How this agent's transcript is read: its measured spelling, or the baseline.
    pub fn reading(&self) -> Reading {
        self.refinement
            .and_then(|a| a.tools)
            .map(Reading::Measured)
            .unwrap_or(Reading::Generic)
    }

    /// The canned recipe, where a row has measured one. `None` on the generic path — the protocol
    /// names no provider, and that is the one thing the baseline cannot supply.
    pub fn canned(&self) -> Option<CannedRecipe> {
        self.refinement.and_then(|a| a.canned)
    }

    /// What this agent was measured to do with a prompt sent mid-turn, where a row measured it.
    /// `None` is "the row's generic answer applies", never "unsafe".
    pub fn mid_turn(&self) -> Option<MidTurn> {
        self.refinement.and_then(|a| a.mid_turn)
    }

    /// The usage rule this agent is read under: [`USAGE`], refined by the row's measured
    /// [`Agent::reasoning`] split. A split no rule in [`USAGES`] states — which the sweep test
    /// forbids — is read under the protocol's own rule rather than guessed at.
    pub fn usage(&self) -> &'static UsageRule {
        let reasoning = self.refinement.and_then(|a| a.reasoning);
        USAGES
            .iter()
            .copied()
            .find(|rule| rule.reasoning == reasoning)
            .unwrap_or(&USAGE)
    }

    /// The channel the bridge is declared on: the protocol's, unless a row measured otherwise.
    pub fn declaration(&self) -> Declaration {
        self.refinement
            .map(|a| a.declaration)
            .unwrap_or(Declaration::Session)
    }

    /// One line for a doctor row or a refusal: what is bound, and how much is known about it.
    pub fn describe(&self) -> String {
        match &self.refinement {
            Some(a) => format!("`{}` — {}", a.id, a.note),
            None => format!(
                "`{}` — no refinement row: identity from `initialize`, bridge via `session/new`, \
                 tool names read off its own frames",
                self.argv.join(" ")
            ),
        }
    }
}

/// Why a selector could not be bound. One variant, because the generic path accepts everything
/// else: a selector is either a row's id or a command, and a command is anything with a program.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error(
        "the ACP agent selector names no program. Name a refinement row ({}) or a command \
         (`acp:<program> [args…]`, split on whitespace — an argument that needs a space needs a \
         wrapper script)",
        AGENTS.iter().map(|a| a.id).collect::<Vec<_>>().join(", ")
    )]
    NoProgram,
}

/// What an agent's `initialize` identity says about its binding, where it says anything: the
/// **runtime** half of recognising an agent, because a command line names a program and only the
/// handshake names the agent behind it.
///
/// * A generic binding (`acp:<command>`) whose agent answers with a row's [`Agent::agent_info`]:
///   name the row, so the operator can select it by id and get what was measured for it.
/// * A refined binding whose agent answers under another name: the row's argv now reaches a
///   different agent, and its measurements are stale.
///
/// `None` otherwise — an agent no row knows is simply the generic path, and a row whose binary
/// still answers as measured has nothing to report.
pub fn identity_note(binding: &Binding, handshake: &AgentHandshake) -> Option<String> {
    match binding.refinement() {
        Some(row) if row.agent_info != handshake.name => Some(format!(
            "identity drift: row `{}` was measured answering `initialize` as `{}`, and this \
             binary answers as `{}` — the row's refinements may not apply",
            row.id, row.agent_info, handshake.name
        )),
        Some(_) => None,
        None => AGENTS
            .iter()
            .find(|a| a.agent_info == handshake.name)
            .map(|row| {
                format!(
                    "identity: answers `initialize` as `{}`, the agent of refinement row `{}` — \
                     select it by that id for what was measured on it",
                    handshake.name, row.id
                )
            }),
    }
}

/// One stdio MCP server, in `session/new`'s own shape. S21 sent exactly this and the agent
/// launched the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerDecl {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl McpServerDecl {
    fn to_value(&self) -> Value {
        json!({
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "env": self.env.iter()
                .map(|(n, v)| json!({"name": n, "value": v}))
                .collect::<Vec<_>>(),
        })
    }
}

/// `session/new` — **the launch that matters**, because it is where marion's bridge is declared.
///
/// `mcpServers` is always present, empty where there is no declaration, because S21 sent the key
/// on every probe and an absent key is a shape nothing here has measured.
pub fn session_new_request(id: u64, cwd: &Path, mcp: &[McpServerDecl]) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/new",
        "params": {
            "cwd": cwd,
            "mcpServers": mcp.iter().map(McpServerDecl::to_value).collect::<Vec<_>>(),
        },
    })
}

/// `session/load` — **the resume that is a protocol request rather than a flag.**
///
/// ACP v1 says an agent advertising `agentCapabilities.loadSession` MUST replay the session's
/// history as `session/update` notifications before answering, so the loaded session is the old
/// one continued and not a fresh one under the old id. The request carries the same `cwd` and the
/// same `mcpServers` block as `session/new` — the bridge is declared again because the agent's MCP
/// processes did not survive the agent — so [`crate::McpRoute::Session`] verifies a resume exactly
/// as it verifies a fresh launch. The `sessionId` is the one the agent handed back from its own
/// `session/new`; nothing here mints one.
///
/// An agent that does not advertise `loadSession` is refused this by name (the driver checks the
/// handshake before sending it), because the protocol has no other resume: `session/resume` exists
/// only behind ACP v2's unstable flag, and both `sessionCapabilities.resume` and `fork` are
/// carried on [`AgentHandshake`] without being driven.
pub fn session_load_request(id: u64, session_id: &str, cwd: &Path, mcp: &[McpServerDecl]) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/load",
        "params": {
            "sessionId": session_id,
            "cwd": cwd,
            MCP_SERVERS_KEY: mcp.iter().map(McpServerDecl::to_value).collect::<Vec<_>>(),
        },
    })
}

/// The methods a session-opening request can carry, read back off the request marion compiled —
/// so a driver correlates and continues on what the adapter actually built.
pub const SESSION_NEW_METHOD: &str = "session/new";
pub const SESSION_LOAD_METHOD: &str = "session/load";

/// The `session/new` param key [`crate::McpRoute::Session`] verifies. One string, so the compiler
/// keeps the builder and the check on the same key.
pub const MCP_SERVERS_KEY: &str = "mcpServers";

/// Where an ACP session states the tokens it spent: the `session/prompt` **response**'s
/// `result.usage`, one per turn, so a session's spend is their sum. The shape is the protocol's
/// own and every measured agent emits it — `opencode acp` (`s21`, `s23`), `claude-agent-acp` and
/// `codex-acp` (`s22`) — with `totalTokens` equal to the four counters' sum, so `inputTokens`
/// excludes the cache. The `usage_update` session notification is context-window occupancy
/// (`used`/`size`), not spend, and is deliberately not a unit here.
pub const USAGE: UsageRule = UsageRule {
    at: Where {
        frame: &[Cond::Has("/result/stopReason"), Cond::Has("/result/usage")],
        each: None,
        unit: &[],
    },
    input: "/result/usage/inputTokens",
    output: "/result/usage/outputTokens",
    cache_read: Some("/result/usage/cachedReadTokens"),
    cache_write: Some("/result/usage/cachedWriteTokens"),
    reasoning: None,
    input_includes_cache: false,
    fold: UsageFold::Sum,
    in_flight: None,
};

/// Where the protocol's usage object counts reasoning tokens, on the agents that report them.
pub const THOUGHT_TOKENS: &str = "/result/usage/thoughtTokens";

/// [`USAGE`] for an agent whose `thoughtTokens` sit **beside** `outputTokens` ([`OPENCODE`]).
pub const USAGE_THOUGHTS_BESIDE: UsageRule = UsageRule {
    reasoning: Some(Reasoning::Beside(THOUGHT_TOKENS)),
    ..USAGE
};

/// [`USAGE`] for an agent whose `outputTokens` already holds its `thoughtTokens` ([`CODEX_ACP`]).
pub const USAGE_THOUGHTS_WITHIN: UsageRule = UsageRule {
    reasoning: Some(Reasoning::Within(THOUGHT_TOKENS)),
    ..USAGE
};

/// Every usage rule an ACP agent is read under: the protocol's own, and its two refinements.
pub static USAGES: [&UsageRule; 3] = [&USAGE, &USAGE_THOUGHTS_BESIDE, &USAGE_THOUGHTS_WITHIN];

/// Where an ACP session is named: the `session/new` **response**'s `result.sessionId` — the id
/// `session/load` takes back (S21 captured it on `opencode acp`, `ses_…`). A `session/load`
/// answer carries none, since the id was the request's, so a resumed node is never journaled a
/// second, different session. Protocol-wide, like [`USAGE`]: no agent refines where its id sits.
pub const SESSION: SessionId = SessionId {
    at: Where {
        frame: &[Cond::Has("/result/sessionId")],
        each: None,
        unit: &[],
    },
    path: "/result/sessionId",
    // A resume is a `session/load` of the journaled id, whose answer names no session at all, so
    // there is no first session unit to check against it.
    resumes_in_place: false,
    by_title: None,
};

/// What an ACP node has been doing, read off the protocol's own `session/update` notifications:
/// a `tool_call`, and the `tool_call_update`s after it, name the tool in `title` and its input in
/// `rawInput` under one `toolCallId`; `agent_message_chunk` streams the model's words one delta at
/// a time. opencode's first `tool_call` is `pending` with `rawInput: {}` and the `in_progress`
/// update carries the input (s21), where codex-acp's first sighting already does (s22) — so both
/// frame kinds are units, and the reader keeps a call's last non-empty arguments.
pub const ACTIVITY: ActivityRule = ActivityRule {
    calls: &[
        ToolUnit {
            at: Where {
                frame: &[Cond::Eq("/params/update/sessionUpdate", "tool_call")],
                each: None,
                unit: &[],
            },
            name: "/params/update/title",
            args: "/params/update/rawInput",
            id: Some("/params/update/toolCallId"),
            shape: crate::grammar::CallShape::Tool,
        },
        ToolUnit {
            at: Where {
                frame: &[Cond::Eq("/params/update/sessionUpdate", "tool_call_update")],
                each: None,
                unit: &[],
            },
            name: "/params/update/title",
            args: "/params/update/rawInput",
            id: Some("/params/update/toolCallId"),
            shape: crate::grammar::CallShape::Tool,
        },
    ],
    text: &[TextUnit {
        at: Where {
            frame: &[Cond::Eq(
                "/params/update/sessionUpdate",
                "agent_message_chunk",
            )],
            each: None,
            unit: &[],
        },
        path: "/params/update/content/text",
        joins: true,
    }],
};

/// `session/prompt`. One text block: §8's micro-contract asserts a *response shape*.
pub fn prompt_request(id: u64, session_id: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": text}]},
    })
}

/// `session/cancel` — a **notification**, which is ACP's own shape for it and the reason §8's
/// interrupt step is expressible here at all. It carries no id and is answered by the pending
/// `session/prompt` returning `stopReason: "cancelled"`.
pub fn cancel_notification(session_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": session_id},
    })
}

/// ACP's request for changing one of a session's advertised `configOptions`. The model and the
/// mode are both chosen **inside** the session on every agent marion has measured (S21, S22, S28,
/// and the 2026-09-22 probe of all four), and this is the protocol's own channel for choosing them
/// — no argv, no per-agent config document.
pub const SET_CONFIG_OPTION_METHOD: &str = "session/set_config_option";

/// ACP's older, mode-only request: `session/set_mode {sessionId, modeId}` against the session's
/// `modes.availableModes`. Used only where an agent advertises `modes` and no `mode` config option;
/// every agent measured on 2026-09-22 advertises both and answers both.
pub const SET_MODE_METHOD: &str = "session/set_mode";

/// The `configOptions` category of the session's model select.
pub const MODEL_CATEGORY: &str = "model";
/// The `configOptions` category of the session's mode select — where an agent's approval behaviour
/// lives (claude-agent-acp's `acceptEdits`/`bypassPermissions`, codex-acp's `agent-full-access`,
/// copilot's `#autopilot`, opencode's `build`/`plan`).
pub const MODE_CATEGORY: &str = "mode";

/// How a contract's `allowed_tools` names the session mode an ACP launch set: `session-mode:<id>`,
/// beside [`NO_TOOL_AVAILABILITY_SURFACE`].
pub const SESSION_MODE_PREFIX: &str = "session-mode:";

/// How a [`SessionSelect`] is changed.
///
/// A small shared type on purpose: a row-level approval strategy that chooses a session mode can
/// name this channel rather than a second spelling of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectChannel {
    /// `session/set_config_option` on the config option with this `id` — the agent's own string,
    /// read rather than assumed.
    ConfigOption { config_id: String },
    /// `session/set_mode`, for an agent that advertises `modes` and no `mode` config option.
    SetMode,
}

/// One session-level select an agent advertised in its `session/new` (or `session/load`) answer:
/// how it is changed, what it is set to, and every value it offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSelect {
    pub channel: SelectChannel,
    pub current: Option<String>,
    /// Every offered value, flattened out of ACP's option groups where the agent groups them.
    pub values: Vec<String>,
}

/// The advertised select of `category`, keyed on ACP's own category rather than on any agent's id
/// for it; for [`MODE_CATEGORY`], the `modes` object where no config option carries it. `None`
/// where the answer offers none (copilot's ACP session has no model select, S28) or is not an
/// answer. Also reads a `config_option_update` notification's `configOptions`.
pub fn session_select(frame: &str, category: &str) -> Option<SessionSelect> {
    let v: Value = serde_json::from_str(frame).ok()?;
    let options = v
        .pointer("/result/configOptions")
        .or_else(|| v.pointer("/params/update/configOptions"))
        .and_then(Value::as_array);
    if let Some(opt) = options.and_then(|os| {
        os.iter()
            .find(|o| o.get("category").and_then(Value::as_str) == Some(category))
    }) {
        let mut values = Vec::new();
        for o in opt.get("options").and_then(Value::as_array)? {
            match o.get("options").and_then(Value::as_array) {
                Some(group) => values.extend(
                    group
                        .iter()
                        .filter_map(|g| g.get("value").and_then(Value::as_str))
                        .map(str::to_string),
                ),
                None => values.extend(o.get("value").and_then(Value::as_str).map(str::to_string)),
            }
        }
        return Some(SessionSelect {
            channel: SelectChannel::ConfigOption {
                config_id: opt.get("id").and_then(Value::as_str)?.to_string(),
            },
            current: opt
                .get("currentValue")
                .and_then(Value::as_str)
                .map(str::to_string),
            values,
        });
    }
    if category != MODE_CATEGORY {
        return None;
    }
    let modes = v.pointer("/result/modes")?;
    Some(SessionSelect {
        channel: SelectChannel::SetMode,
        current: modes
            .get("currentModeId")
            .and_then(Value::as_str)
            .map(str::to_string),
        values: modes
            .get("availableModes")?
            .as_array()?
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
    })
}

/// The request that sets `select` to `value` in `session_id`, on the select's own channel.
pub fn select_request(id: u64, session_id: &str, select: &SessionSelect, value: &str) -> Value {
    let (method, params) = match &select.channel {
        SelectChannel::ConfigOption { config_id } => (
            SET_CONFIG_OPTION_METHOD,
            json!({"sessionId": session_id, "configId": config_id, "value": value}),
        ),
        SelectChannel::SetMode => (
            SET_MODE_METHOD,
            json!({"sessionId": session_id, "modeId": value}),
        ),
    };
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// The select's value after a [`select_request`], off the `configOptions` the answer carries —
/// `None` where the answer does not restate it (a `session/set_mode` answer is `{}`) — or the
/// agent's refusal in its own words.
pub fn selected_value(frame: &str, select: &SessionSelect) -> Result<Option<String>, AcpError> {
    let v = session_answer(frame)?;
    let result = v.get("result").ok_or(AcpError::NeitherResultNorError)?;
    let SelectChannel::ConfigOption { config_id } = &select.channel else {
        return Ok(None);
    };
    Ok(result
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|os| {
            os.iter()
                .find(|o| o.get("id").and_then(Value::as_str) == Some(config_id))
        })
        .and_then(|o| o.get("currentValue").and_then(Value::as_str))
        .map(str::to_string))
}

/// The `sessionId` out of a `session/new` result, or the agent's own words for why there is none.
pub fn session_id(frame: &str) -> Result<String, AcpError> {
    let v = session_answer(frame)?;
    v.get("result")
        .and_then(|r| r.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or(AcpError::NeitherResultNorError)
}

/// Whether a `session/load` answer opened the session. Its result carries **no** `sessionId` — the
/// id was the request's — so the only questions are "is it a response" and "did the agent refuse",
/// and the refusal comes back in the agent's own words (an unknown id, an ineligible login).
pub fn session_loaded(frame: &str) -> Result<(), AcpError> {
    let v = session_answer(frame)?;
    v.get("result")
        .map(|_| ())
        .ok_or(AcpError::NeitherResultNorError)
}

/// One JSON-RPC response frame, with a JSON-RPC `error` already turned into [`AcpError::Refused`].
fn session_answer(frame: &str) -> Result<Value, AcpError> {
    let v: Value = serde_json::from_str(frame).map_err(|e| AcpError::NotJson(e.to_string()))?;
    if let Some(err) = v.get("error") {
        return Err(AcpError::Refused {
            code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("<no message>")
                .to_string(),
        });
    }
    Ok(v)
}

/// A `session/update` payload, where the frame is one.
fn update(frame: &Value) -> Option<&Value> {
    (frame.get("method").and_then(Value::as_str) == Some("session/update"))
        .then(|| frame.get("params")?.get("update"))
        .flatten()
}

fn kind(u: &Value) -> &str {
    u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("")
}

fn call_id(u: &Value) -> Option<&str> {
    u.get("toolCallId").and_then(Value::as_str)
}

/// Every call to one of marion's verbs an ACP transcript shows, **paired by `toolCallId`**.
///
/// The pairing is not a stylistic choice. S21's own capture is the counter-example to reading the
/// terminal frame alone: the `tool_call_update` that carries `status: "completed"` carries
/// **`"title": ""`**, so the verb exists only on the opening `tool_call` frame and the outcome only
/// on the closing one. A reader that took either frame in isolation would report either a call with
/// no result or a result with no verb.
pub fn marion_calls(stdout: &str, reading: impl Into<Reading>) -> Vec<MarionCall> {
    let reading = reading.into();
    let mut open: Vec<(String, String)> = Vec::new(); // (toolCallId, verb) in call order
    let mut outcome: Vec<(String, CallOutcome)> = Vec::new();
    for frame in json_frames(stdout) {
        let Some(u) = update(&frame) else { continue };
        let Some(id) = call_id(u) else { continue };
        if kind(u) == "tool_call"
            && let Some(verb) = reading.verb(u)
        {
            open.push((id.to_string(), verb));
        }
        if let Some(o) = terminal_outcome(u) {
            outcome.retain(|(k, _)| k != id);
            outcome.push((id.to_string(), o));
        }
    }
    open.into_iter()
        .map(|(id, verb)| MarionCall {
            verb,
            outcome: outcome
                .iter()
                .find(|(k, _)| *k == id)
                .map(|(_, o)| o.clone())
                .unwrap_or(CallOutcome::Unknown),
        })
        .collect()
}

/// ACP's three terminal `status` values, and nothing else. `pending` and `in_progress` are *not*
/// outcomes — a stream that stops there is [`CallOutcome::Unknown`], which is news.
fn terminal_outcome(u: &Value) -> Option<CallOutcome> {
    match u.get("status").and_then(Value::as_str)? {
        "completed" => Some(CallOutcome::Answered),
        "failed" => Some(CallOutcome::Refused(text_of(u.get("content")))),
        _ => None,
    }
}

/// The text inside a `content` array of ACP content blocks, joined. Empty where there is none.
fn text_of(content: Option<&Value>) -> String {
    content
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.pointer("/content/text").or_else(|| b.get("text")))
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// §6.1 step 9 for an ACP transcript, read in **this agent's** [`Reading`].
///
/// The name matched is the model-facing one, because that is what a `tool_call` frame's title
/// carries — the unprefixed `report` never appears on this wire at all, only on the MCP wire between
/// the agent and marion's bridge. The whole [`Reading`] is taken rather than a compiled string,
/// because the *arguments* are in a different place on one of the measured agents
/// ([`Reading::arguments`]) and a reader handed only a name cannot know which.
///
/// The exit code is **not consulted**, for the same reason opencode's reader ignores it: an ACP
/// agent is a long-lived stdio server that marion kills, so its exit status describes marion's
/// shutdown, not the turn. What describes the turn is `stopReason` and the frames.
pub fn parse_stream(stdout: &str, _exit: ChildExit, reading: impl Into<Reading>) -> StreamOutcome {
    let reading = reading.into();
    let mut out = StreamOutcome::default();
    // The ids of the calls that were opened *as `report`*. Every other tool call on this session
    // also carries a `rawInput`, and a reader that took `narrative` off whichever object happened
    // to have the key would attribute another tool's arguments to marion's verb.
    let mut report_ids: Vec<String> = Vec::new();
    for frame in json_frames(stdout) {
        if let Some(m) = frame_failure(&frame) {
            out.failure.get_or_insert(m);
        }
        read_update(&frame, reading, &mut report_ids, &mut out);
    }
    out
}

/// What a top-level frame claims about the turn going wrong, if anything.
///
/// A JSON-RPC `error` frame is the agent refusing something outright, which is the only shape
/// S20's blocker ever produced. §5.2's `stopReason` is the turn's own verdict: `end_turn` and
/// `max_tokens` are endings, `refusal` is a failure the exit code cannot see. The outright refusal
/// is the more specific claim, so it is the one reported where a frame somehow carries both.
fn frame_failure(frame: &Value) -> Option<String> {
    let rpc = frame
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string);
    rpc.or_else(|| {
        let stop = frame.pointer("/result/stopReason").and_then(Value::as_str);
        (stop == Some("refusal"))
            .then(|| "the agent ended the turn with stopReason `refusal`".to_string())
    })
}

/// What one frame's `session/update` says about **marion's own** calls, folded into `out`.
///
/// **Every read here is confined to marion's own calls, and both halves of that are measured.**
/// The arguments half is S21's: `rawInput` is revised in place on every tool call the session
/// makes, so a reader taking `narrative` off whichever object had the key would file another
/// tool's arguments as marion's report.
///
/// The failure half is S22's, and it is the sharper one. `codex-acp`'s **first** frame, before the
/// session is in use, is a `tool_call` titled `mcp__marion__startup` with `status: "failed"` — a
/// startup diagnostic, wearing the *claude* shim's prefix on the *codex* shim, for a verb marion
/// does not have. A blanket "any failed tool call fails the turn" reads that as marion's verb being
/// refused and reports a refusal for a turn that went on to end `end_turn` having called `report`
/// successfully. A tool of the agent's own failing is the agent's business; what this function
/// reports is what became of marion's.
fn read_update(
    frame: &Value,
    reading: Reading,
    report_ids: &mut Vec<String>,
    out: &mut StreamOutcome,
) {
    let Some(u) = update(frame) else { return };
    let Some(id) = call_id(u) else { return };
    if kind(u) == "tool_call" && reading.verb(u).as_deref() == Some("report") {
        report_ids.push(id.to_string());
    }
    if !report_ids.iter().any(|k| k == id) {
        return;
    }
    read_report_args(u, reading, out);
    if u.get("status").and_then(Value::as_str) == Some("failed") {
        out.failure.get_or_insert_with(|| failed_words(u, id));
    }
}

/// The narrative and commits marion's `report` carried, read in **this agent's** [`Reading`].
fn read_report_args(u: &Value, reading: Reading, out: &mut StreamOutcome) {
    let Some(args) = u.get("rawInput").and_then(|raw| reading.arguments(raw)) else {
        return;
    };
    if let Some(n) = args.get("narrative").and_then(Value::as_str) {
        out.narrative = Some(n.to_string());
    }
    let commits = report_commits(args);
    if !commits.is_empty() {
        out.result_commits = commits;
    }
}

/// Why a failed call to marion failed, in the agent's own words — or, where it gave none, the
/// call it marked failed, so the failure is never recorded as an empty string.
fn failed_words(u: &Value, id: &str) -> String {
    let words = text_of(u.get("content"));
    if words.is_empty() {
        format!("the agent marked tool call {id} failed")
    } else {
        words
    }
}

/// §5.2's `acp` row: **one adapter, many agents** (§9's M5).
///
/// # What the surfaces are, and why
///
/// [`surfaces`] — `Typed(Acp)` control, `StructuredUi` display, `ProtocolEvents`
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
/// this adapter publishes a capability; see [`SPEC`]'s `advertised` for the five that are
/// claimed and the five that are not.
/// # Baseline and refinement, and why the struct has a field
///
/// **Any agent that speaks ACP over stdio runs through this adapter with no row naming it.** The
/// protocol supplies what a per-harness row supplies elsewhere: identity arrives in `initialize`,
/// the bridge is declared in `session/new`'s `mcpServers`, and the agent's spelling of marion's
/// verbs is read off its own `tool_call` frames ([`Reading::Generic`] — four agents have been
/// watched spelling `report` four ways, and every one of them spelled the same `<server> <verb>`
/// pair). An [`Agent`] row is a **refinement** over that path: a pinned spelling, a canned
/// recipe, a measured quirk. [`Binding`] is the layering made a value, and it is what this
/// adapter is bound to.
///
/// The binding rides the adapter rather than the spec because [`HarnessAdapter::marion_tool_name`]
/// and the stream readers take no [`LaunchSpec`], and their answers are per agent. [`Self::unbound`]
/// is the protocol-level adapter — what [`crate::adapter::adapter_for`] hands back from a [`Harness`] alone, enough
/// for the surface questions (`surfaces`, `mcp_route`) that have no per-agent answer — and it
/// **cannot be launched**: every method that would put marion's verbs in front of a model refuses it
/// by name. [`crate::adapter::adapter_for_type`] is the seam that binds one.
#[derive(Debug, Clone)]
pub struct AcpAdapter {
    /// `None` on the protocol-level adapter; see the type's doc comment.
    pub(crate) binding: Option<Binding>,
}

impl AcpAdapter {
    /// The adapter for the ACP **protocol**, bound to no agent. Unlaunchable by construction.
    pub fn unbound() -> Self {
        Self { binding: None }
    }

    /// The adapter for one refinement row.
    pub fn for_agent(agent: Agent) -> Self {
        Self::bound(Binding::refined(agent))
    }

    /// The adapter for one resolved binding — a row or a generic command.
    pub fn bound(binding: Binding) -> Self {
        Self {
            binding: Some(binding),
        }
    }

    /// Resolve an operator's selector — a row's id or a command line — into a bound adapter. The
    /// one refusal is a selector with no program in it; see [`Binding::resolve`].
    pub fn resolve(selector: &str) -> Result<Self, HarnessError> {
        Binding::resolve(selector)
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
    fn binding(&self, spec: &LaunchSpec) -> Result<&Binding, HarnessError> {
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
            Binding::resolve(selector).map_err(|e| HarnessError::AcpAgent(e.to_string()))?;
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
    fn reading(&self) -> Option<Reading> {
        self.binding.as_ref().map(Binding::reading)
    }
}

impl HarnessAdapter for AcpAdapter {
    fn harness(&self) -> Harness {
        Harness::Acp
    }

    /// argv is the agent's own, verbatim as S20 launched it, and **nothing else is compiled into
    /// it** — [`SPEC`]'s one row splices [`spec::Field::AgentArgs`] and names no program of
    /// its own, because the program is the agent's.
    ///
    /// * *No prompt.* It rides `session/prompt` after the handshake, so `spec.prompt` reaches argv
    ///   on no ACP agent — the claude-code situation, one protocol over.
    /// * *No model on argv.* S21's `session/new` result carries a `configOptions` `model`
    ///   **select**: the model is chosen inside the session, and marion has measured no argv that
    ///   sets it. The compiled [`crate::invocation::Invocation::model`] is the requested one because the ACP driver
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
        let mut f = self.launch_fields(spec, ctx)?;
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
        if let (Declaration::Argv { flag, .. }, McpDeclaration::Marion) =
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
            (_, Some(CannedRecipe::OpencodeConfigDocument)) => {
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
        let mut out = vec![NO_TOOL_AVAILABILITY_SURFACE.to_string()];
        out.extend(
            spec.extra
                .approval_mode
                .as_ref()
                .map(|m| format!("{}{m}", SESSION_MODE_PREFIX)),
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
    /// under [`CannedRecipe::OpencodeConfigDocument`] carries a **provider**, and no `mcp`
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
            Some(CannedRecipe::OpencodeConfigDocument) => {
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
    /// The env block is [`crate::mcp_bridge::BridgeEnv::pairs`] — the same derivation every other declaration
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
        // `Reading::Generic` reads the `<server> <verb>` pair in any spelling.
        //
        // The bridge's contract, verbatim — the same pairs every document-shaped declaration
        // carries — because the process on the other end is the same `marion-supervisor mcp`.
        let servers = match (spec.mcp, self.binding(spec)?.declaration()) {
            (McpDeclaration::None, _) => Vec::new(),
            // Declared on argv by `fields`; an entry here too would be the second bridge the
            // `Declaration::Argv` doc names.
            (McpDeclaration::Marion, Declaration::Argv { .. }) => Vec::new(),
            (McpDeclaration::Marion, Declaration::Session) => vec![McpServerDecl {
                name: MCP_SERVER_NAME.into(),
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
            Some(session) => Ok(Some(session_load_request(
                SESSION_NEW_ID,
                session,
                &spec.cwd,
                &servers,
            ))),
            None if servers.is_empty() => Ok(None),
            None => Ok(Some(session_new_request(
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
        match self.binding(spec).map(Binding::declaration) {
            Ok(Declaration::Argv { token, .. }) => token,
            Ok(Declaration::Session) | Err(_) => self.spec().token.for_auth(spec.auth),
        }
    }

    /// The row's route — `session/new`'s block — except on a binding whose refinement measured the
    /// bridge reaching the agent on argv, where it is that flag, verified against the compiled argv
    /// exactly as codex's `-c` overrides are. The unbound adapter answers for the protocol.
    fn mcp_route(&self, spec: &LaunchSpec) -> McpRoute {
        let declaration = self
            .binding
            .as_ref()
            .map(Binding::declaration)
            .unwrap_or(Declaration::Session);
        match (spec.mcp, declaration) {
            (McpDeclaration::None, _) => McpRoute::None,
            (McpDeclaration::Marion, Declaration::Argv { flag, .. }) => McpRoute::Argv(flag),
            (McpDeclaration::Marion, Declaration::Session) => McpRoute::Session(MCP_SERVERS_KEY),
        }
    }

    fn parse_stream(&self, stdout: &str, exit: ChildExit) -> StreamOutcome {
        match self.reading() {
            Some(r) => parse_stream(stdout, exit, r),
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
    /// | any other agent | [`GENERIC_SPELLING`] |
    ///
    /// On this row the answer reaches no model: ACP has no availability axis
    /// ([`NO_TOOL_AVAILABILITY_SURFACE`]) and the prompt rides `session/prompt`, so the string
    /// is a record and never a compiled constraint. The transcript is read with the whole
    /// [`Reading`], not with this string, which is what makes a generic agent readable.
    ///
    /// The unbound adapter gets [`UNBOUND_TOOL_NAME`] — a string that is not a tool name in
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
                    .and_then(Binding::mid_turn)
                    .unwrap_or(mid_turn),
                note,
            },
            other => other,
        }
    }

    fn marion_tool_name(&self, tool: &str) -> String {
        match self.reading() {
            Some(r) => r.spell(tool),
            None => format!("{}{tool}", UNBOUND_TOOL_NAME),
        }
    }

    fn marion_calls(&self, stdout: &str) -> Vec<MarionCall> {
        self.reading()
            .map(|r| marion_calls(stdout, r))
            .unwrap_or_default()
    }

    /// The protocol's own `session/prompt` response shape ([`USAGE`]), bound or not: spend is
    /// a protocol fact. How a bound agent's counters split its reasoning is the one refinement
    /// ([`Binding::usage`]).
    fn usage_rule(&self) -> Option<&'static grammar::UsageRule> {
        Some(self.binding.as_ref().map_or(&USAGE, Binding::usage))
    }

    /// The protocol's own `session/new` answer ([`SESSION`]), whatever the agent: where a
    /// session is named is a protocol fact, as spend is.
    fn session_id(&self, frame: &serde_json::Value) -> Option<String> {
        grammar::session_in(&SESSION, frame)
    }

    /// The protocol's `session/update` frames ([`ACTIVITY`]), whatever the agent: every
    /// agent names its calls and streams its words through the same updates.
    fn activity(&self) -> Option<&'static grammar::ActivityRule> {
        Some(&ACTIVITY)
    }
}

/// This row's entry in [`crate::adapter::ROWS`]: bound to the agent a launch names, or — from a
/// harness name alone — the unbound protocol adapter.
pub const ROW: Row = Row {
    spec: &SPEC,
    adapter: |agent| match agent {
        Some(selector) => Ok(Box::new(AcpAdapter::resolve(selector)?)),
        None => Ok(Box::new(AcpAdapter::unbound())),
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surfaces::{ControlTransport, DisplaySurface};

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/s20");
    const S21: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/s21");

    /// S21's transcript: **the agent's own stdout, verbatim**, from a live `opencode acp` that
    /// marion drove through `initialize`, `session/new` and one `session/prompt`.
    fn s21_session() -> String {
        let p = format!("{S21}/opencode-acp-session.jsonl");
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    /// The agent whose transcript S21 captured, and whose spelling the tests below read it in.
    const OC: ToolSpelling = ToolSpelling::ServerUnderscoreTool;

    const S22: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/s22");

    /// One shim's own stdout, verbatim, from a live run against the operator's own vendor CLI.
    fn s22(agent: &str) -> String {
        let p = format!("{S22}/{agent}-session.jsonl");
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    /// The `session/new` answer out of a transcript: the frame whose `result` opened the session.
    fn session_new_answer(transcript: &str) -> String {
        transcript
            .lines()
            .find(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| v.pointer("/result/sessionId").cloned())
                    .is_some()
            })
            .expect("a session/new answer")
            .to_string()
    }

    /// **The model select is found by ACP's own `category`, on every agent that advertised one.**
    /// opencode names it `model` (S21), and so do the two ACP Registry shims (S22); copilot's ACP
    /// session (S28) offers no model select at all, which is `None` rather than a guess.
    #[test]
    fn the_model_select_is_read_off_the_session_answer_by_its_category() {
        let oc = session_select(&session_new_answer(&s21_session()), MODEL_CATEGORY)
            .expect("S21 offers one");
        assert_eq!(oc.channel, config("model"));
        assert_eq!(oc.current.as_deref(), Some("opencode/big-pickle"));
        assert!(oc.values.iter().any(|v| v == "opencode/big-pickle"));
        assert_eq!(oc.values.len(), 66, "every option, flat");
        for agent in ["claude-agent-acp", "codex-acp"] {
            let o = session_select(&session_new_answer(&s22(agent)), MODEL_CATEGORY).expect(agent);
            assert_eq!(o.channel, config("model"), "{agent}");
            assert!(
                o.current.as_ref().is_some_and(|c| o.values.contains(c)),
                "{agent}"
            );
        }
        let copilot = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/s28/copilot-acp-session.jsonl"
        ))
        .unwrap();
        assert_eq!(
            session_select(&session_new_answer(&copilot), MODEL_CATEGORY),
            None
        );
    }

    /// ACP lets a select's options come in groups; the values are the leaves either way.
    #[test]
    fn grouped_model_options_flatten_to_their_values() {
        let frame =
            json!({"jsonrpc": "2.0", "id": 1, "result": {"sessionId": "s", "configOptions": [
                {"id": "m", "category": "model", "type": "select", "currentValue": "b/two",
                 "options": [
                    {"group": "a", "name": "A", "options": [{"value": "a/one", "name": "one"}]},
                    {"group": "b", "name": "B", "options": [{"value": "b/two", "name": "two"}]}
                 ]}
            ]}})
            .to_string();
        let o = session_select(&frame, MODEL_CATEGORY).unwrap();
        assert_eq!(o.channel, config("m"));
        assert_eq!(o.values, vec!["a/one", "b/two"]);
        assert_eq!(o.current.as_deref(), Some("b/two"));
    }

    fn config(id: &str) -> SelectChannel {
        SelectChannel::ConfigOption {
            config_id: id.into(),
        }
    }

    /// The request is ACP's `session/set_config_option`, measured on `opencode acp` 1.18.32: a
    /// value it offers comes back as the option's new `currentValue`, and one it does not is a
    /// `-32602` in its own words.
    #[test]
    fn a_model_is_set_through_the_protocols_config_option_request() {
        let select = SessionSelect {
            channel: config("model"),
            current: None,
            values: vec![],
        };
        let r = select_request(4, "ses_1", &select, "opencode/nemotron-3-ultra-free");
        assert_eq!(r["method"], SET_CONFIG_OPTION_METHOD);
        assert_eq!(r["id"], 4);
        assert_eq!(
            r["params"],
            json!({"sessionId": "ses_1", "configId": "model", "value": "opencode/nemotron-3-ultra-free"})
        );
        let took = json!({"jsonrpc": "2.0", "id": 4, "result": {"configOptions": [
            {"id": "model", "category": "model", "type": "select",
             "currentValue": "opencode/nemotron-3-ultra-free", "options": []}
        ]}})
        .to_string();
        assert_eq!(
            selected_value(&took, &select).unwrap().as_deref(),
            Some("opencode/nemotron-3-ultra-free")
        );
        let refused = r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"Invalid params: model not found: nope/x"}}"#;
        assert!(matches!(
            selected_value(refused, &select),
            Err(AcpError::Refused { code: -32602, .. })
        ));
    }

    /// **The mode select, on all four measured agents, by the same category** — the approval
    /// behaviour each one lets a client choose. Measured 2026-09-22 (opencode 1.18.32,
    /// claude-agent-acp 0.66.0, codex-acp 1.13.0, copilot 1.0.83 `--acp`): every one advertises a
    /// `mode` config option **and** the `modes` object, and answers both `session/set_config_option`
    /// and `session/set_mode`. The committed captures carry the same selects.
    #[test]
    fn the_mode_select_is_read_by_its_category_on_every_measured_agent() {
        let copilot = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/s28/copilot-acp-session.jsonl"
        ))
        .unwrap();
        for (agent, transcript, current, auto) in [
            ("opencode", s21_session(), "build", "build"),
            (
                "claude-agent-acp",
                s22("claude-agent-acp"),
                "default",
                "acceptEdits",
            ),
            ("codex-acp", s22("codex-acp"), "agent", "agent-full-access"),
            (
                "copilot",
                copilot,
                "https://agentclientprotocol.com/protocol/session-modes#agent",
                "https://agentclientprotocol.com/protocol/session-modes#autopilot",
            ),
        ] {
            let m = session_select(&session_new_answer(&transcript), MODE_CATEGORY).expect(agent);
            assert_eq!(m.channel, config("mode"), "{agent}");
            assert_eq!(m.current.as_deref(), Some(current), "{agent}");
            assert!(
                m.values.iter().any(|v| v == auto),
                "{agent}: {:?}",
                m.values
            );
        }
    }

    /// An agent with `modes` and no `mode` config option still has its mode set, over
    /// `session/set_mode`, whose answer restates nothing.
    #[test]
    fn a_mode_with_no_config_option_falls_back_to_set_mode() {
        let frame = json!({"jsonrpc": "2.0", "id": 1, "result": {"sessionId": "s", "modes": {
            "currentModeId": "ask", "availableModes": [{"id": "ask", "name": "Ask"}, {"id": "yolo", "name": "Yolo"}]
        }}})
        .to_string();
        let m = session_select(&frame, MODE_CATEGORY).unwrap();
        assert_eq!(m.channel, SelectChannel::SetMode);
        assert_eq!(m.values, vec!["ask", "yolo"]);
        let r = select_request(3, "s", &m, "yolo");
        assert_eq!(r["method"], SET_MODE_METHOD);
        assert_eq!(r["params"], json!({"sessionId": "s", "modeId": "yolo"}));
        assert_eq!(
            selected_value(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#, &m).unwrap(),
            None
        );
        assert_eq!(
            session_select(&frame, MODEL_CATEGORY),
            None,
            "only the mode falls back"
        );
    }

    /// The model-facing spelling, as the shipped adapter produces it.
    fn report() -> String {
        OC.spell("report")
    }

    /// **The measured mapping, pinned to the capture it was measured in.**
    ///
    /// s14's finding is that claude, gemini and opencode all *silently ignore* an unknown tool
    /// name, so a wrong spelling here buys a run that looks healthy and calls nothing. This
    /// asserts the string marion compiles is the string the model actually typed — read out of a
    /// verbatim transcript rather than restated.
    #[test]
    fn the_tool_name_marion_compiles_is_the_one_the_model_typed() {
        assert_eq!(report(), "marion_report");
        let titles: Vec<String> = json_frames(&s21_session())
            .iter()
            .filter_map(|f| {
                let u = update(f)?;
                (kind(u) == "tool_call").then(|| u.get("title")?.as_str().map(str::to_string))?
            })
            .collect();
        assert_eq!(
            titles,
            vec![report()],
            "the live agent opened exactly one tool call and this is what it called it"
        );
    }

    /// **There is something behind the fifth `McpRoute`.** The MCP side of the same run: the agent
    /// started the server marion declared in `session/new`, handshook with it, listed its tools,
    /// and called one. Without this the `Session` route would be an assertion about marion's own
    /// JSON and nothing else.
    #[test]
    fn the_session_new_declaration_actually_starts_marions_mcp_server() {
        let p = format!("{S21}/opencode-acp-mcp.jsonl");
        let log = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        let inbound: Vec<Value> = log
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|f| f["dir"] == "in")
            .map(|f| f["frame"].clone())
            .collect();
        let methods: Vec<&str> = inbound
            .iter()
            .filter_map(|f| f.get("method")?.as_str())
            .collect();
        assert!(
            methods.starts_with(&["initialize", "notifications/initialized", "tools/list"]),
            "the agent must have spoken MCP to the server marion declared: {methods:?}"
        );
        // And the wire name is the **unprefixed** verb, which is the other half of the two-layer
        // split: `marion_report` is what the model types, `report` is what crosses MCP.
        let called = inbound
            .iter()
            .find(|f| f.get("method").and_then(Value::as_str) == Some("tools/call"))
            .expect("the agent called a tool on marion's server");
        assert_eq!(
            called.pointer("/params/name").and_then(Value::as_str),
            Some("report")
        );
        assert_ne!(
            called.pointer("/params/name").and_then(Value::as_str),
            Some(report().as_str()),
            "compiling the model-facing spelling onto the MCP wire would be §3.1's mistake"
        );
    }

    /// §6.1 step 8's post-hoc assertion over a real transcript: the verb, and what came of it.
    #[test]
    fn a_real_transcript_yields_the_verb_and_its_outcome() {
        assert_eq!(
            marion_calls(&s21_session(), OC),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered,
            }]
        );
        let out = parse_stream(&s21_session(), ChildExit::default(), OC);
        assert_eq!(out.narrative.as_deref(), Some("hello from acp"));
        assert_eq!(out.failure, None, "the turn ended `end_turn`");
    }

    /// **The pairing, killed one half at a time.** S21's own frames make this necessary: the
    /// opening `tool_call` carries the title and an empty `rawInput`, and the closing update
    /// carries the arguments and `"title": ""`. A reader of either frame alone finds a verb with
    /// no outcome or an outcome with no verb, and both read as "the node never called marion".
    #[test]
    fn neither_half_of_a_call_can_be_read_without_the_other() {
        let all = s21_session();
        let opening: String = all
            .lines()
            .filter(|l| !l.contains("tool_call_update"))
            .collect::<Vec<_>>()
            .join("\n");
        let closing: String = all
            .lines()
            .filter(|l| !l.contains(r#""sessionUpdate":"tool_call""#))
            .collect::<Vec<_>>()
            .join("\n");
        // Opening frame only: the verb is known, the outcome is not — and `Unknown` is the answer,
        // never `Answered`.
        assert_eq!(
            marion_calls(&opening, OC),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Unknown,
            }]
        );
        // Closing frames only: no verb was ever opened, so there is no call to report.
        assert!(marion_calls(&closing, OC).is_empty());
        assert_eq!(
            parse_stream(&closing, ChildExit::default(), OC).narrative,
            None,
            "arguments with no opening frame belong to no verb marion can name"
        );
    }

    /// **A narrative is attributed to the call it came from.** ACP revises `rawInput` in place on
    /// every tool call, marion's and the agent's own alike, so a reader that took the first
    /// `narrative` key it saw would file another tool's arguments as marion's report.
    #[test]
    fn another_tools_arguments_are_not_read_as_marions_report() {
        let other = format!(
            "{}\n{}",
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"other","title":"write","status":"pending","rawInput":{}}}}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"other","status":"completed","rawInput":{"narrative":"not marion's"}}}}"#
        );
        let out = parse_stream(&other, ChildExit::default(), OC);
        assert_eq!(out.narrative, None);
        assert!(marion_calls(&other, OC).is_empty());
        // And prepending it to the real transcript must not change the real answer.
        let both = format!("{other}\n{}", s21_session());
        assert_eq!(
            parse_stream(&both, ChildExit::default(), OC)
                .narrative
                .as_deref(),
            Some("hello from acp")
        );
    }

    /// The three outcomes, one at a time. `failed` carries the agent's own words; a call left in
    /// `in_progress` is `Unknown` and not a success by omission.
    #[test]
    fn each_terminal_status_maps_to_its_own_outcome() {
        let frame = |status: &str, extra: &str| {
            format!(
                "{}\n{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"s\",\"update\":{{\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"c1\",\"status\":\"{status}\"{extra}}}}}}}",
                format_args!(
                    "{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"sessionId\":\"s\",\"update\":{{\"sessionUpdate\":\"tool_call\",\"toolCallId\":\"c1\",\"title\":\"{}\",\"status\":\"pending\"}}}}}}",
                    report()
                )
            )
        };
        assert_eq!(
            marion_calls(&frame("completed", ""), OC)[0].outcome,
            CallOutcome::Answered
        );
        assert_eq!(
            marion_calls(&frame("in_progress", ""), OC)[0].outcome,
            CallOutcome::Unknown,
            "a call that never ended is news, not a success"
        );
        assert_eq!(
            marion_calls(
                &frame(
                    "failed",
                    r#","content":[{"type":"content","content":{"type":"text","text":"depth 3 exceeds max_depth 2"}}]"#
                ),
                OC
            )[0]
            .outcome,
            CallOutcome::Refused("depth 3 exceeds max_depth 2".into()),
            "the refusal must carry the agent's own words, or a caller cannot act on it"
        );
        // And a failure reaches the stream outcome, where the exit code cannot see it.
        assert_eq!(
            parse_stream(&frame("failed", ""), ChildExit::default(), OC)
                .failure
                .as_deref(),
            Some("the agent marked tool call c1 failed")
        );
    }

    /// **`stopReason: "refusal"` is a failed turn the exit code cannot see** — the ACP instance of
    /// the S12 shape (gemini: exit 0 with a JSON error body). S21's own capture ends `end_turn`,
    /// so this pins the other branch.
    #[test]
    fn a_refused_turn_is_a_failure_however_the_process_exits() {
        let refused = r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"refusal"}}"#;
        let out = parse_stream(refused, ChildExit::default(), OC);
        assert!(out.failure.is_some_and(|f| f.contains("refusal")));
        // `end_turn` is not a failure, or the assertion above would hold for every transcript.
        assert_eq!(
            parse_stream(
                r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}"#,
                ChildExit::default(),
                OC
            )
            .failure,
            None
        );
    }

    /// **The registry is a table of measurements, and no two measured agents share a spelling.**
    ///
    /// This assertion used to say the opposite — that every launchable agent spells marion's verbs
    /// the *one* way the adapter compiles — and it was right to, because there was one measurement.
    /// S22 took two more and got two more answers. What survives is the invariant underneath: a
    /// spelling is a fact about an agent, so an agent with none is refused rather than served by a
    /// neighbour's, and two agents that genuinely differ must not be recorded as agreeing.
    #[test]
    fn every_measured_agent_has_its_own_spelling_and_the_unmeasured_have_none() {
        let mut spellings: Vec<(&str, String)> = Vec::new();
        let mut unmeasured = 0;
        for a in AGENTS {
            match a.tools {
                Some(t) => spellings.push((a.id, t.spell("report"))),
                None => unmeasured += 1,
            }
            assert!(!a.argv.is_empty(), "`{}` names no program", a.id);
            assert_eq!(
                agent(a.id),
                Some(a),
                "`{}` must resolve by its own id",
                a.id
            );
        }
        // Four agents, four names, none of them guessable from another (S21, S22, S28).
        assert_eq!(
            spellings,
            vec![
                ("opencode", "marion_report".to_string()),
                ("claude-acp", "mcp__marion__report".to_string()),
                ("codex-acp", "mcp.marion.report".to_string()),
                ("copilot", "marion-report".to_string()),
            ]
        );
        let mut distinct: Vec<&String> = spellings.iter().map(|(_, s)| s).collect();
        distinct.sort();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            spellings.len(),
            "two agents recorded as spelling a verb the same way is a claim, not a default"
        );
        assert!(unmeasured > 0, "the refusing side must be exercised too");
        assert_eq!(agent("no-such-agent"), None);
    }

    /// **The third measurement, read out of the two verbatim shim transcripts.**
    ///
    /// s14's finding is that an unknown tool name is silently ignored, so the cost of a wrong
    /// spelling is a turn that ends `end_turn` having called nothing. This asserts the string
    /// marion compiles for each shim is the string that shim's model actually typed — read out of
    /// S22's captures rather than restated — and, in the same breath, that each agent's spelling
    /// finds *nothing* in the other's transcript, which is what "silently ignored" would look like
    /// in production.
    #[test]
    fn each_shim_is_read_in_its_own_spelling_and_in_no_others() {
        for (session, spelling, id) in [
            (
                s22("claude-agent-acp"),
                ToolSpelling::McpDoubleUnderscore,
                "claude-acp",
            ),
            (s22("codex-acp"), ToolSpelling::McpDotted, "codex-acp"),
        ] {
            let calls = marion_calls(&session, spelling);
            assert_eq!(
                calls,
                vec![MarionCall {
                    verb: "report".into(),
                    outcome: CallOutcome::Answered,
                }],
                "{id}: one marion call, answered"
            );
            let out = parse_stream(&session, ChildExit::default(), spelling);
            assert_eq!(
                out.narrative.as_deref(),
                Some("hello from acp"),
                "{id}: the narrative the model actually passed"
            );
            assert_eq!(out.failure, None, "{id}: the turn ended `end_turn`");

            // **No other spelling finds this agent's report.** That is the s14 failure mode made
            // visible: a healthy transcript, and a reader that reports no report at all.
            //
            // Stated over `report` rather than over "any call", because on one of these two
            // captures the weaker claim is simply false: `codex-acp`'s transcript contains a
            // startup diagnostic titled in the *claude* shim's spelling, so a reader in that
            // spelling does find something here — a verb marion does not have, marked failed. That
            // is the trap, and it is pinned in
            // `a_startup_diagnostic_in_another_agents_spelling_is_not_a_refused_turn` rather than
            // asserted away here.
            for other in AGENTS.iter().filter_map(|a| a.tools) {
                if other == spelling {
                    continue;
                }
                assert!(
                    !marion_calls(&session, other)
                        .iter()
                        .any(|c| c.verb == "report"),
                    "{id}: `{:?}` found this agent's `report` in a spelling it does not use",
                    other
                );
                assert_eq!(
                    parse_stream(&session, ChildExit::default(), other).narrative,
                    None,
                    "{id}: `{other:?}` read a narrative out of another agent's transcript"
                );
            }
        }
    }

    /// **`codex-acp` does not flatten the call, and a reader of the flat shape gets nothing.**
    ///
    /// Its `rawInput` is `{"server","tool","arguments"}`, so the narrative is one level deeper than
    /// the other two agents put it. This is the trap stated as its own row rather than only as part
    /// of the transcript read above: strip the nesting handling and the call is still *found* — the
    /// title matches — and it reports an empty report.
    #[test]
    fn the_codex_shims_arguments_are_nested_and_are_read_where_they_are() {
        let raw =
            json!({"server":"marion","tool":"report","arguments":{"narrative":"hello from acp"}});
        assert_eq!(
            ToolSpelling::McpDotted.arguments(&raw),
            Some(&raw["arguments"]),
            "one level deeper than S21's shape"
        );
        // Read as a flat object — which is what the other two agents are — the narrative is absent
        // rather than wrong, and the call is still found. That combination is the whole hazard.
        assert_eq!(
            ToolSpelling::McpDoubleUnderscore
                .arguments(&raw)
                .and_then(|a| a.get("narrative")),
            None
        );
        assert_eq!(
            ToolSpelling::ServerUnderscoreTool.arguments(&raw),
            Some(&raw)
        );
        // And an empty or absent `arguments` is not arguments: the opening frame of a call carries
        // `{}`, and `rawInput` is revised in place, so the last non-empty one has to win.
        assert_eq!(
            ToolSpelling::McpDotted.arguments(&json!({"server":"marion","arguments":{}})),
            None
        );
        assert_eq!(ToolSpelling::McpDotted.arguments(&json!({})), None);
        assert_eq!(
            ToolSpelling::ServerUnderscoreTool.arguments(&json!({})),
            None
        );
    }

    /// **The phantom `mcp__marion__startup` frame, which is neither marion's nor a real failure.**
    ///
    /// S22: `codex-acp`'s *first* frame, before the session is in use, is a `tool_call` with
    /// `status: "failed"`, titled with the **claude** shim's `mcp__<server>__<tool>` spelling, on
    /// the **codex** shim, naming a verb (`startup`) marion does not have. It matched marion's
    /// server name and nothing else about it was real; the startup then succeeded and the same
    /// session went on to call `report`.
    ///
    /// Two readers would have been fooled and both are pinned here: one matching by the
    /// `mcp__marion__` prefix, and one treating *any* failed tool call as a failed turn.
    #[test]
    fn a_startup_diagnostic_in_another_agents_spelling_is_not_a_refused_turn() {
        let session = s22("codex-acp");
        assert!(
            session.contains("mcp__marion__startup"),
            "the premise: the frame is in the capture"
        );
        // The turn is read as what it was — one answered call, no failure — in this agent's own
        // spelling.
        assert_eq!(
            parse_stream(&session, ChildExit::default(), ToolSpelling::McpDotted).failure,
            None,
            "a startup diagnostic for a verb marion does not have is not marion's verb failing"
        );
        // And the trap is real: read with the *claude* shim's prefix — the one the frame is
        // actually titled in — a prefix-matching reader picks it up as a refused marion verb.
        let wrong = marion_calls(&session, ToolSpelling::McpDoubleUnderscore);
        assert_eq!(
            wrong,
            vec![MarionCall {
                verb: "startup".into(),
                outcome: CallOutcome::Refused(
                    "[codex-acp forwarded startup error] MCP server `marion` startup was cancelled."
                        .into()
                ),
            }],
            "the hazard must be present, or the row above guards nothing"
        );
    }

    const S28: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/s28");

    /// Every verbatim transcript in which a real agent called marion's `report`, with the row that
    /// was measured on it — the corpus the generic reader has to cover to be a baseline at all.
    fn measured_report_transcripts() -> Vec<(&'static str, ToolSpelling, String)> {
        let s28 = format!("{S28}/copilot-acp-session.jsonl");
        vec![
            (
                "opencode",
                ToolSpelling::ServerUnderscoreTool,
                s21_session(),
            ),
            (
                "claude-acp",
                ToolSpelling::McpDoubleUnderscore,
                s22("claude-agent-acp"),
            ),
            ("codex-acp", ToolSpelling::McpDotted, s22("codex-acp")),
            (
                "copilot",
                ToolSpelling::ServerHyphenTool,
                std::fs::read_to_string(&s28).unwrap_or_else(|e| panic!("{s28}: {e}")),
            ),
        ]
    }

    /// **The baseline reads every measured transcript, and reads it as its own row does.**
    ///
    /// Four agents spelled `report` four ways, and [`Reading::Generic`] has to find all four —
    /// verb, outcome and the narrative the model actually passed — with no row telling it which
    /// it is looking at. That is the whole claim behind "any ACP agent works": an agent marion has
    /// never named is read this way, so this is the assertion that the way is wide enough. And the
    /// refinement must agree with it on the captures it was pinned to, or a row would be changing
    /// what marion records rather than sharpening it.
    #[test]
    fn the_generic_reading_finds_every_measured_agents_report() {
        for (id, measured, session) in measured_report_transcripts() {
            let generic = marion_calls(&session, Reading::Generic);
            assert!(
                generic.iter().any(|c| {
                    *c == MarionCall {
                        verb: "report".into(),
                        outcome: CallOutcome::Answered,
                    }
                }),
                "{id}: the generic reading must find the answered report: {generic:?}"
            );
            let out = parse_stream(&session, ChildExit::default(), Reading::Generic);
            assert_eq!(
                out.narrative.as_deref(),
                Some("hello from acp"),
                "{id}: the narrative the model actually passed"
            );
            assert_eq!(out.failure, None, "{id}: the turn ended `end_turn`");
            // Refinement and baseline agree on what became of marion's own verb.
            let refined = parse_stream(&session, ChildExit::default(), measured);
            assert_eq!(refined.narrative, out.narrative, "{id}");
            assert_eq!(refined.failure, out.failure, "{id}");
        }
    }

    /// **S28's quirk, read off the capture.** The same copilot that called `marion-report` when the
    /// bridge came in on argv opened a session over a `session/new` that declared the very same
    /// server — and never started it. The transcript of that session is in the corpus so the
    /// refinement row's `note` is a measurement and not a memory: no marion call, no failure, a
    /// turn that ended `end_turn` having reported nothing — the exact shape a generic
    /// `acp:copilot --acp` launch would produce, and the reason the row exists.
    #[test]
    fn copilots_session_new_declaration_is_measured_ignored() {
        let p = format!("{S28}/copilot-acp-session-new-mcp-ignored.jsonl");
        let ignored = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        // The model's words arrive one token per `agent_message_chunk`, so they are joined before
        // being read.
        let said: String = json_frames(&ignored)
            .iter()
            .filter_map(|f| {
                let u = update(f)?;
                (kind(u) == "agent_message_chunk").then(|| u.pointer("/content/text")?.as_str())?
            })
            .collect();
        assert!(
            said.contains("is not available in this session"),
            "the premise: the model said the tool was not there: {said:?}"
        );
        for reading in [Reading::Generic, Reading::Measured(COPILOT.tools.unwrap())] {
            assert!(marion_calls(&ignored, reading).is_empty(), "{reading:?}");
            let out = parse_stream(&ignored, ChildExit::default(), reading);
            assert_eq!(out.narrative, None, "{reading:?}");
            assert_eq!(
                out.failure, None,
                "{reading:?}: `end_turn`, and nothing else to say"
            );
        }
        // And the MCP wire of the run that *did* reach the bridge shows the argv-declared server
        // handshaking and taking the unprefixed `report`, as every other agent's did.
        let p = format!("{S28}/copilot-acp-mcp.jsonl");
        let mcp = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        let methods: Vec<String> = mcp
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|f| f.get("method")?.as_str().map(str::to_string))
            .collect();
        assert!(
            methods.contains(&"tools/list".to_string())
                && methods.contains(&"tools/call".to_string()),
            "{methods:?}"
        );
    }

    /// **The generic reader is a pair match, not a prefix match**, and the difference is S22's
    /// phantom `mcp__marion__startup`. Every shape a title or a structured input can take is
    /// enumerated here, so a fifth spelling lands on one side or the other by construction.
    #[test]
    fn the_generic_reading_recognises_the_pair_in_any_spelling_and_nothing_else() {
        let call = |title: &str, raw: Value| {
            json!({
                "sessionUpdate": "tool_call", "toolCallId": "c", "title": title,
                "status": "pending", "rawInput": raw
            })
        };
        for title in [
            "marion_report",
            "mcp__marion__report",
            "mcp.marion.report",
            "marion-report",
            "marion/report",
            "mcp_marion_report",
            "marion: report",
        ] {
            assert_eq!(
                Reading::Generic.verb(&call(title, json!({}))).as_deref(),
                Some("report"),
                "{title}"
            );
        }
        for title in [
            "report",
            "marion",
            "marion_report_extra",
            "other_report",
            "Using skill: skill01",
            "ToolSearch",
            "write",
            "",
        ] {
            assert_eq!(
                Reading::Generic.verb(&call(title, json!({}))),
                None,
                "{title}"
            );
        }
        // The structured shape names the pair outright and wins over the title.
        let structured =
            json!({"server": "marion", "tool": "report", "arguments": {"narrative": "n"}});
        assert_eq!(
            Reading::Generic
                .verb(&call("exec", structured.clone()))
                .as_deref(),
            Some("report")
        );
        assert_eq!(
            Reading::Generic.arguments(&structured),
            Some(&structured["arguments"])
        );
        // Another server's structured call is not marion's, whatever its title says.
        let other = json!({"server": "github", "tool": "report", "arguments": {"narrative": "n"}});
        assert_eq!(
            Reading::Generic.verb(&call("github_report", other.clone())),
            None
        );
        // A flat input is the arguments; an empty one is none.
        let flat = json!({"narrative": "n"});
        assert_eq!(Reading::Generic.arguments(&flat), Some(&flat));
        assert_eq!(Reading::Generic.arguments(&json!({})), None);
        // The phantom: found under its own verb, so `parse_stream` — which reads only `report` —
        // does not take its failure for marion's.
        let session = s22("codex-acp");
        assert!(
            marion_calls(&session, Reading::Generic)
                .iter()
                .any(|c| c.verb == "startup" && matches!(c.outcome, CallOutcome::Refused(_))),
            "the hazard must be present, or the row above guards nothing"
        );
        assert_eq!(
            parse_stream(&session, ChildExit::default(), Reading::Generic).failure,
            None
        );
    }

    /// **A selector is a row's id or a command, and only an empty one is refused.** The rows keep
    /// their measured argv and reading; everything else gets the program the operator named and
    /// the generic reading.
    #[test]
    fn a_binding_is_a_refinement_row_by_id_or_a_generic_command() {
        for a in AGENTS {
            let b = Binding::resolve(a.id).unwrap();
            assert_eq!(b.refinement(), Some(&a), "`{}`", a.id);
            assert_eq!(b.argv(), a.argv, "`{}`", a.id);
            assert_eq!(
                b.reading(),
                a.tools.map(Reading::Measured).unwrap_or(Reading::Generic),
                "`{}`",
                a.id
            );
            assert_eq!(b, Binding::refined(a));
        }
        let generic = Binding::resolve("  copilot --acp --allow-all-tools ").unwrap();
        assert_eq!(
            generic.refinement(),
            None,
            "a command is not a row, even one whose first word is"
        );
        assert_eq!(generic.argv(), ["copilot", "--acp", "--allow-all-tools"]);
        assert_eq!(generic.reading(), Reading::Generic);
        assert_eq!(generic.canned(), None);
        assert_eq!(
            generic.declaration(),
            Declaration::Session,
            "the protocol's channel is the only one the baseline has — which is why `copilot --acp` \
             as a command, unlike the `copilot` row, reaches no bridge on 1.0.83"
        );
        assert_eq!(
            Binding::resolve("copilot").unwrap().declaration(),
            COPILOT.declaration
        );
        assert!(matches!(
            COPILOT.declaration,
            Declaration::Argv {
                flag: COPILOT_MCP_FLAG,
                ..
            }
        ));
        assert_eq!(generic.selector(), "copilot --acp --allow-all-tools");
        assert!(generic.describe().contains("no refinement row"));
        for empty in ["", "   ", "\t"] {
            assert_eq!(
                Binding::resolve(empty),
                Err(BindError::NoProgram),
                "{empty:?}"
            );
        }
        assert!(
            BindError::NoProgram.to_string().contains("opencode"),
            "the refusal lists the rows"
        );
    }

    /// The three request shapes, against the ones a live agent answered. Values, not prose: S21
    /// sent exactly these and `opencode acp` opened a session and took a turn.
    #[test]
    fn the_session_requests_are_the_ones_a_live_agent_answered() {
        let n = session_new_request(
            1,
            Path::new("/wt"),
            &[McpServerDecl {
                name: MCP_SERVER_NAME.into(),
                command: "/bin/marion-supervisor".into(),
                args: vec!["mcp".into()],
                env: vec![("MARION_DEPTH".into(), "0".into())],
            }],
        );
        assert_eq!(n["method"], "session/new");
        assert_eq!(n["params"]["cwd"], "/wt");
        let server = &n["params"][MCP_SERVERS_KEY][0];
        assert_eq!(server["name"], MCP_SERVER_NAME);
        assert_eq!(server["command"], "/bin/marion-supervisor");
        assert_eq!(server["args"][0], "mcp");
        // The env block is an **array of `{name, value}`**, not an object — ACP's own shape, and
        // the one S21 sent. An object here is silently ignored rather than rejected.
        assert_eq!(
            server["env"][0],
            json!({"name": "MARION_DEPTH", "value": "0"})
        );

        let p = prompt_request(2, "ses_1", "hi");
        assert_eq!(p["method"], "session/prompt");
        assert_eq!(p["params"]["sessionId"], "ses_1");
        assert_eq!(
            p["params"]["prompt"][0],
            json!({"type": "text", "text": "hi"})
        );

        // `session/cancel` is a **notification**: an id would make it a request the agent must
        // answer, and it answers by ending the pending prompt instead.
        let c = cancel_notification("ses_1");
        assert_eq!(c["method"], "session/cancel");
        assert!(c.get("id").is_none());

        for f in [&n, &p, &c] {
            assert!(!serde_json::to_string(f).unwrap().contains('\n'));
        }
    }

    /// **A resume is `session/load` with the same declaration**, and its answer carries no id.
    #[test]
    fn a_resume_is_a_session_load_carrying_the_same_bridge_declaration() {
        let decl = McpServerDecl {
            name: MCP_SERVER_NAME.into(),
            command: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            env: vec![],
        };
        let load =
            session_load_request(1, "ses_prev", Path::new("/wt"), std::slice::from_ref(&decl));
        let new = session_new_request(1, Path::new("/wt"), std::slice::from_ref(&decl));
        assert_eq!(load["method"], SESSION_LOAD_METHOD);
        assert_eq!(new["method"], SESSION_NEW_METHOD);
        assert_eq!(load["params"]["sessionId"], "ses_prev");
        assert_eq!(load["params"]["cwd"], new["params"]["cwd"]);
        assert_eq!(
            load["params"][MCP_SERVERS_KEY], new["params"][MCP_SERVERS_KEY],
            "the bridge is declared again on a resume, in the same shape"
        );
        assert!(!serde_json::to_string(&load).unwrap().contains('\n'));

        // The answer: `null`, `{}` and an object are all "loaded"; an error is the refusal in the
        // agent's words; a frame with neither is neither.
        for ok in [
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        ] {
            assert_eq!(session_loaded(ok), Ok(()), "{ok}");
        }
        assert!(matches!(
            session_loaded(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unknown session"}}"#),
            Err(AcpError::Refused { code: -32602, message }) if message == "unknown session"
        ));
        assert_eq!(
            session_loaded(r#"{"jsonrpc":"2.0","id":1}"#),
            Err(AcpError::NeitherResultNorError)
        );
    }

    /// The `sessionId` read, and S20's refusal in the same shape a live probe sees it.
    #[test]
    fn a_session_is_either_opened_or_refused_in_the_agents_own_words() {
        assert_eq!(
            session_id(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"ses_01"}}"#),
            Ok("ses_01".into())
        );
        let refused = session_id(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"This client is no longer supported for Gemini Code Assist for individuals."}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(&refused, AcpError::Refused { code: -32000, message } if message.contains("Gemini Code Assist")),
            "got {refused:?}"
        );
        // An empty or blank id is not an id, and neither is an absent one. A padded id is the
        // agent's own bytes and is kept as sent: marion echoes it back, it never rewrites it.
        assert!(session_id(r#"{"result":{"sessionId":""}}"#).is_err());
        assert!(session_id(r#"{"result":{"sessionId":" \t "}}"#).is_err());
        assert_eq!(
            session_id(r#"{"result":{"sessionId":" ses_01 "}}"#),
            Ok(" ses_01 ".into())
        );
        assert!(session_id(r#"{"result":{}}"#).is_err());
    }

    fn fixture(name: &str) -> String {
        let p = format!("{FIXTURES}/{name}");
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"))
    }

    fn opencode() -> AgentHandshake {
        AgentHandshake::parse(&fixture("opencode-initialize.json")).expect("S20 captured this")
    }

    fn gemini() -> AgentHandshake {
        AgentHandshake::parse(&fixture("gemini-initialize.json")).expect("S20 captured this")
    }

    /// The frames are verbatim, so this is the parser measured against two real agents rather than
    /// against marion's idea of one.
    #[test]
    fn both_agents_s20_probed_parse_into_the_identity_section_3_3_keys_on() {
        assert_eq!(opencode().key(), "OpenCode 1.17.3");
        assert_eq!(gemini().key(), "gemini-cli 0.53.0");
        assert!(opencode().load_session && gemini().load_session);
    }

    /// **S20's finding, as an assertion.** Two agents behind one adapter, differing on exactly two
    /// of §3.3's ten fields.
    #[test]
    fn the_two_agents_differ_on_fork_and_resume_and_on_nothing_else() {
        let s = surfaces();
        let o = opencode().caps(&s);
        let g = gemini().caps(&s);
        assert_ne!(
            o, g,
            "M5 needs *differing* capabilities, not two identical rows"
        );

        assert!(
            o.fork && o.resume,
            "opencode advertises sessionCapabilities {{close, fork, list, resume}}"
        );
        assert!(
            !g.fork && !g.resume,
            "gemini advertises no `sessionCapabilities` object at all"
        );
        // The difference is those two fields and no others. Naming the pair rather than comparing
        // two sets: a swap between the rows would leave the sets equal.
        let differing: Vec<&str> = Capabilities::FIELDS
            .iter()
            .copied()
            .filter(|f| o.granted().contains(f) != g.granted().contains(f))
            .collect();
        assert_eq!(differing, vec!["fork", "resume"]);
    }

    /// **Neither fixture can see a `fork`/`resume` swap.** `opencode acp` advertises both and
    /// `gemini --acp` advertises neither, so exchanging the two arms of [`AgentHandshake::refine`]
    /// leaves every assertion above passing. The map is therefore pinned one key at a time, on
    /// frames derived from the real one by removing a single key — the only thing here that is not
    /// a verbatim capture, and it is deliberately a test of *marion's map*, not of ACP.
    #[test]
    fn each_session_capability_maps_to_its_own_field_and_not_its_neighbours() {
        let s = surfaces();
        for (keep, want_fork, want_resume) in [
            ("fork", true, false),
            ("resume", false, true),
            // Real keys with no §3.3 field to land on. If either grew one, this fails.
            ("close", false, false),
            ("list", false, false),
        ] {
            let mut h = opencode();
            h.session_capabilities = BTreeSet::from([keep.to_string()]);
            let c = h.caps(&s);
            assert_eq!(c.fork, want_fork, "`{keep}` alone → fork");
            assert_eq!(c.resume, want_resume, "`{keep}` alone → resume");
        }
    }

    /// `loadSession` is the only source of `view`, and both captures answer `true` — so without
    /// this the field could be hardcoded. ACP v1: `session/load` MUST replay history (§5.2).
    #[test]
    fn load_session_is_the_only_source_of_view() {
        let s = surfaces();
        assert!(
            opencode().caps(&s).view,
            "both agents advertise loadSession"
        );
        let mut denied = opencode();
        denied.load_session = false;
        assert!(!denied.caps(&s).view);

        // An **absent** advertisement is not a claim. Both captures carry `loadSession: true`, so
        // without this row the field could default to `true` and no fixture would notice —
        // §3.3's degrade-visibly rule read the wrong way round. `gemini --acp` advertising no
        // `sessionCapabilities` object at all is the same shape one level down.
        let silent = AgentHandshake::parse(
            r#"{"result":{"protocolVersion":1,"agentInfo":{"name":"quiet"},"agentCapabilities":{}}}"#,
        )
        .unwrap();
        assert!(
            !silent.load_session,
            "an agent that advertises no `loadSession` has not advertised one"
        );
        assert!(!silent.caps(&s).view);
        // And it moves nothing else.
        assert_eq!(
            denied.caps(&s),
            Capabilities {
                view: false,
                ..opencode().caps(&s)
            }
        );
    }

    /// S20's own caution, kept honest. `audio` is a **real measured difference** between the two
    /// agents and it is not a capability; if it ever becomes one, this fails.
    #[test]
    fn a_content_type_difference_is_recorded_and_claimed_nowhere() {
        let (o, g) = (opencode(), gemini());
        assert!(
            g.prompt_content_types.contains("audio") && !o.prompt_content_types.contains("audio"),
            "the difference must actually be present, or this test guards nothing: {:?} vs {:?}",
            g.prompt_content_types,
            o.prompt_content_types
        );
        assert_ne!(g.prompt_content_types, o.prompt_content_types, "recorded");

        // And it reaches no capability field: strip the two agents' *session* difference and the
        // capability rows become identical, which they could not if `audio` were mapped anywhere.
        let s = surfaces();
        let mut g_as_if = g.clone();
        g_as_if.session_capabilities = o.session_capabilities.clone();
        assert_eq!(
            g_as_if.caps(&s),
            o.caps(&s),
            "prompt content types must contribute nothing to §3.3's ten fields"
        );
    }

    /// S33's captures: the agent's own `initialize` answer for every agent probed on 2026-09-27.
    const S33: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/s33-acp-agents"
    );

    /// One S33 capture's frame by JSON-RPC id: `0` is `initialize`, `1` is `session/new`.
    fn s33_frame(file: &str, id: u64) -> String {
        let p = format!("{S33}/{file}.jsonl");
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"));
        text.lines()
            .find(|l| {
                serde_json::from_str::<Value>(l)
                    .is_ok_and(|v| v.get("id").and_then(Value::as_u64) == Some(id))
            })
            .unwrap_or_else(|| panic!("{p} carries no frame with id {id}"))
            .to_string()
    }

    /// **Every refinement row is a measurement, and this is the sweep that holds it to one.**
    ///
    /// Each row in [`AGENTS`] has an S33 capture named for its id, and every per-agent fact the row
    /// states that the wire can witness is read back off that capture rather than trusted: the
    /// identity it answers `initialize` with ([`Agent::agent_info`]), whether `session/new` opened
    /// a session or was refused ([`Agent::reach`]), and — for an opened session — whether the
    /// answer advertised a `model` and a `mode` select, read with the very [`session_select`] the
    /// driver uses. A row added without a capture, or a capture that drifts from its row, fails
    /// here by the row's name.
    #[test]
    fn every_refinement_row_matches_its_s33_capture() {
        for a in AGENTS {
            let id = a.id;
            assert!(!a.agent_info.is_empty(), "`{id}` states no agentInfo");
            assert!(!a.install.is_empty(), "`{id}` states no install command");
            assert!(!a.note.is_empty(), "`{id}` states no note");
            let hs = AgentHandshake::parse(&s33_frame(id, 0))
                .unwrap_or_else(|e| panic!("`{id}`'s capture is no handshake: {e}"));
            assert_eq!(hs.name, a.agent_info, "`{id}` answers as another agent");
            let answer = s33_frame(id, 1);
            match a.reach {
                Reach::Opened { model, mode, .. } => {
                    assert!(
                        session_id(&answer).is_ok(),
                        "`{id}` is recorded as opening a session: {answer}"
                    );
                    assert_eq!(
                        session_select(&answer, MODEL_CATEGORY).is_some(),
                        model,
                        "`{id}`'s model select"
                    );
                    assert_eq!(
                        session_select(&answer, MODE_CATEGORY).is_some(),
                        mode,
                        "`{id}`'s mode select"
                    );
                }
                Reach::Refused => assert!(
                    matches!(session_id(&answer), Err(AcpError::Refused { .. })),
                    "`{id}` is recorded as refused: {answer}"
                ),
            }
        }
    }

    /// **An agent's identity is what it says on the wire, and the doctor holds a row to it.**
    ///
    /// Two ways a command line and a row can disagree, both only visible after `initialize`: an
    /// operator's `acp:<command>` that turns out to be an agent marion has a row for (then say
    /// which, so they can select it by id and get its refinements), and a row's own argv that now
    /// launches something answering under another name (then say the row is stale). An agent no
    /// row knows, and a row whose binary still answers as measured, need no note.
    #[test]
    fn the_handshake_identity_names_a_matching_row_or_a_drifted_one() {
        let kilo = AgentHandshake::parse(&s33_frame("kilo", 0)).unwrap();
        let generic = Binding::resolve("/opt/bin/kilo acp").unwrap();
        let note = identity_note(&generic, &kilo).expect("a generic binding answering as a row");
        assert!(note.contains("`kilo`") && note.contains("Kilo"), "{note}");

        assert_eq!(identity_note(&Binding::refined(KILO), &kilo), None);

        let drifted = identity_note(&Binding::refined(VIBE), &kilo).expect("a stale row");
        assert!(
            drifted.contains("`vibe`")
                && drifted.contains("@mistralai/mistral-vibe")
                && drifted.contains("Kilo"),
            "{drifted}"
        );

        let stranger = AgentHandshake::parse(
            r#"{"result":{"protocolVersion":1,"agentInfo":{"name":"nobody-knows"}}}"#,
        )
        .unwrap();
        assert_eq!(identity_note(&generic, &stranger), None);
    }

    /// **Every agent that opened a session is a built-in type, named for its row, and no other.**
    ///
    /// An operator reaches an agent through an agent *type*, so a row that opened a session and
    /// has no type is reachable only by typing its command. The rule is mechanical so neither side
    /// can drift: row `<id>` with [`Reach::Opened`] is built-in `acp-<id>`, bound to that row by
    /// id; a refused row has no type (it could not run a node); and every `acp-` built-in names a
    /// row.
    #[test]
    fn every_opened_row_is_a_builtin_type_and_every_acp_builtin_is_a_row() {
        use marion_core::agent_type::{builtin, builtin_names};
        for a in AGENTS {
            let name = format!("acp-{}", a.id);
            match (a.reach, builtin(&name)) {
                (Reach::Opened { .. }, Some(t)) => {
                    assert_eq!(t.harness, Harness::Acp, "{name}");
                    assert_eq!(t.acp_agent.as_deref(), Some(a.id), "{name}");
                    assert!(
                        builtin_names().contains(&name.as_str()),
                        "{name} is not listed"
                    );
                }
                (Reach::Opened { .. }, None) => {
                    panic!("`{}` opened a session and has no {name}", a.id)
                }
                (Reach::Refused, Some(_)) => panic!("`{}` was refused and still has {name}", a.id),
                (Reach::Refused, None) => {}
            }
        }
        for name in builtin_names().iter().filter(|n| n.starts_with("acp-")) {
            let id = &name["acp-".len()..];
            assert!(agent(id).is_some(), "{name} names no refinement row");
        }
    }

    /// **Each row launches the argv its capture was taken with, and states nothing unmeasured.**
    ///
    /// The argv is the one S33 probed (and S20–S28 before it); a refinement a row does not have
    /// — a tool spelling nobody watched a model type, a mid-turn answer nobody measured, a canned
    /// recipe nobody ran — is `None` and leaves the generic path's answer in force, never a
    /// neighbour's. The table is written out whole so a new row is a line here as well as a line
    /// in [`AGENTS`].
    #[test]
    fn every_row_launches_its_probed_argv_and_claims_only_what_was_measured() {
        type Row = (
            &'static str,
            &'static [&'static str],
            Option<ToolSpelling>,
            Option<MidTurn>,
        );
        let expected: &[Row] = &[
            (
                "opencode",
                &["opencode", "acp"],
                Some(ToolSpelling::ServerUnderscoreTool),
                Some(MidTurn::Fold),
            ),
            ("gemini", &["gemini", "--acp"], None, None),
            (
                "claude-acp",
                &["npx", "-y", "@agentclientprotocol/claude-agent-acp@0.66.0"],
                Some(ToolSpelling::McpDoubleUnderscore),
                Some(MidTurn::Fold),
            ),
            (
                "codex-acp",
                &["codex-acp"],
                Some(ToolSpelling::McpDotted),
                Some(MidTurn::Queue),
            ),
            (
                "copilot",
                &["copilot", "--acp"],
                Some(ToolSpelling::ServerHyphenTool),
                Some(MidTurn::Queue),
            ),
            ("kilo", &["kilo", "acp"], None, None),
            ("qwen", &["qwen", "--acp"], None, None),
            ("goose", &["goose", "acp"], None, None),
            ("fast-agent", &["fast-agent-acp", "-x"], None, None),
            ("vibe", &["vibe-acp"], None, None),
            ("vtcode", &["vtcode", "acp"], None, None),
            ("auggie", &["auggie", "--acp"], None, None),
            ("qoder", &["qodercli", "--acp"], None, None),
            ("cline", &["cline", "--acp"], None, None),
            ("pi-acp", &["pi-acp"], None, None),
        ];
        let actual: Vec<Row> = AGENTS
            .iter()
            .map(|a| (a.id, a.argv, a.tools, a.mid_turn))
            .collect();
        assert_eq!(actual, expected);
        // Only opencode has a measured canned recipe, and only copilot a measured quirk in how the
        // bridge reaches it; every other row takes the protocol's own channel.
        for a in AGENTS {
            assert_eq!(a.canned.is_some(), a.id == "opencode", "`{}`", a.id);
            assert_eq!(
                a.declaration != Declaration::Session,
                a.id == "copilot",
                "`{}`",
                a.id
            );
        }
        // The generic path's witness stays outside the table (`tests/fixtures/acp/README.md`).
        assert!(AGENTS.iter().all(|a| a.agent_info != "fake-acp-agent"));
    }

    /// **Which MCP transports an agent takes is advertised, so it is read, never tabled.** ACP's
    /// `agentCapabilities.mcpCapabilities` names the transports beyond stdio (which every agent
    /// must take) as booleans, and S33 measured all three shapes: both on (`kilo`), one on and one
    /// explicitly off (`goose`: `http: true, sse: false`), and the object absent (`auggie`).
    #[test]
    fn the_mcp_transports_an_agent_advertises_are_read_off_its_handshake() {
        let read = |file: &str| {
            AgentHandshake::parse(&s33_frame(file, 0))
                .unwrap()
                .mcp_transports
                .into_iter()
                .collect::<Vec<_>>()
        };
        assert_eq!(read("kilo"), ["http", "sse"]);
        assert_eq!(
            read("goose"),
            ["http"],
            "an advertised `false` is not a transport"
        );
        assert!(
            read("auggie").is_empty(),
            "no object, no transport beyond stdio"
        );
    }

    /// §3.3 forbids stage two from widening. A meet cannot, and this is the property stated over
    /// every base a caller could hand it, including one it has no business enlarging.
    #[test]
    fn refinement_narrows_and_never_widens() {
        for h in [opencode(), gemini()] {
            for base in [
                Capabilities::ALL,
                Capabilities::NONE,
                Capabilities::ceiling(&surfaces()),
                Capabilities::ceiling(&ExecutionSurfaces::opaque()),
                Capabilities {
                    fork: true,
                    resume: true,
                    ..Capabilities::NONE
                },
            ] {
                let got = h.refine(base);
                assert!(
                    got.is_at_or_below(&base),
                    "{} widened {:?} to {:?}",
                    h.key(),
                    base.granted(),
                    got.granted()
                );
            }
            // Not vacuous: from an all-true base, gemini's handshake must actually remove two.
            assert!(
                gemini().refine(Capabilities::ALL) != Capabilities::ALL,
                "an agent advertising no session capabilities must narrow something"
            );
        }
    }

    /// The handshake speaks to three of the ten. The other seven must pass through, or `refine`
    /// would be a replacement wearing a meet's name — an agent would come back able to do nothing.
    #[test]
    fn the_seven_fields_the_handshake_is_silent_about_pass_through_untouched() {
        let silent = [
            "steer",
            "interrupt",
            "permissions",
            "elicitation",
            "set_model",
            "token_deltas",
            "usage",
        ];
        for h in [opencode(), gemini()] {
            let got = h.refine(Capabilities::ALL);
            for f in silent {
                assert!(
                    got.granted().contains(&f),
                    "{} narrowed `{f}`, which its `initialize` result says nothing about",
                    h.key()
                );
            }
        }
    }

    /// §3.3's ceiling still applies to an ACP node: a handshake cannot lift it. `opencode acp`
    /// advertises `fork` and `resume`, and on a surface with no channel it gets neither.
    #[test]
    fn a_handshake_cannot_lift_the_surfaces_ceiling() {
        let o = opencode();
        assert!(
            o.caps(&surfaces()).fork,
            "on the ACP surface it is published"
        );
        let launch_only = ExecutionSurfaces::launch_only_with_protocol_events();
        let clipped = o.caps(&launch_only);
        assert!(
            !clipped.fork && !clipped.resume && !clipped.steer,
            "§3.3: caps may only sit at or below the surface, whatever the agent advertises"
        );
    }

    /// The ACP surface is a typed plane over pipes. §11 item 1's rule is that a headless node must
    /// never reach the pty launcher, and `NativePty` here would mint it a witness.
    #[test]
    fn the_acp_surface_is_typed_over_pipes_and_mints_no_pty_witness() {
        let s = surfaces();
        assert_eq!(s.control, ControlTransport::Typed(TypedKind::Acp));
        assert_eq!(s.display, DisplaySurface::StructuredUi);
        assert!(s.has_typed_control_plane());
        assert!(s.display_plane().is_none());
    }

    /// S20's blocker is a JSON-RPC error frame, and the parser must report it as the agent's own
    /// words rather than as an absence. This is the exact `session/new` refusal S20 recorded,
    /// which is the frame shape an `initialize` refusal would also take.
    #[test]
    fn a_refusal_is_reported_with_the_agents_own_words_not_as_a_missing_result() {
        let frame = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"This client is no longer supported for Gemini Code Assist for individuals. To continue using Gemini, please migrate to the Antigravity suite of products: https://antigravity.google"}}"#;
        let e = AgentHandshake::parse(frame).unwrap_err();
        assert!(
            matches!(&e, AcpError::Refused { code: -32000, message } if message.contains("Antigravity")),
            "got {e:?}"
        );
        assert!(
            e.to_string().contains("Antigravity"),
            "an operator reading the refusal must see the vendor's sentence: {e}"
        );
    }

    /// The auth ids marion must be able to *name* and must never *choose* (§6.4). S20's blocker is
    /// unblocked by one of these, so a probe that dropped them would report an impasse with no exit.
    #[test]
    fn the_auth_methods_that_would_unblock_an_agent_are_carried() {
        assert_eq!(
            gemini().auth_methods,
            vec!["oauth-personal", "gemini-api-key", "vertex-ai", "gateway"]
        );
        assert_eq!(opencode().auth_methods, vec!["opencode-login"]);
    }

    /// Every non-handshake shape refused by name, so a probe can tell "not an ACP agent" from "an
    /// ACP agent that said no" — S20's negative control (`codex --acp` exits 2 writing nothing)
    /// depends on exactly that distinction.
    #[test]
    fn each_way_a_frame_can_fail_to_be_a_handshake_is_a_distinct_named_error() {
        assert!(matches!(
            AgentHandshake::parse("").unwrap_err(),
            AcpError::NotJson(_)
        ));
        assert!(matches!(
            AgentHandshake::parse("error: unexpected argument '--acp' found").unwrap_err(),
            AcpError::NotJson(_)
        ));
        assert_eq!(
            AgentHandshake::parse(r#"{"jsonrpc":"2.0","id":0}"#).unwrap_err(),
            AcpError::NeitherResultNorError
        );
        assert_eq!(
            AgentHandshake::parse(r#"{"result":{"protocolVersion":2,"agentInfo":{"name":"x"}}}"#)
                .unwrap_err(),
            AcpError::ProtocolVersion { got: 2 }
        );
        assert_eq!(
            AgentHandshake::parse(r#"{"result":{"protocolVersion":1}}"#).unwrap_err(),
            AcpError::Anonymous
        );
    }

    /// The request marion sends is the request S20 measured both agents answering.
    #[test]
    fn the_initialize_request_is_the_one_both_agents_answered() {
        let r = initialize_request(0);
        assert_eq!(r["method"], "initialize");
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 0);
        assert_eq!(r["params"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["params"]["clientCapabilities"]["terminal"], true);
        assert_eq!(
            r["params"]["clientCapabilities"]["fs"]["readTextFile"],
            true
        );
        // One line, no embedded newline: the transport is newline-delimited.
        assert!(!serde_json::to_string(&r).unwrap().contains('\n'));
    }
}
