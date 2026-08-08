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

use crate::caps::Capabilities;
use crate::stream::{
    CallOutcome, ChildExit, MarionCall, StreamOutcome, json_frames, report_commits,
};
use crate::surfaces::{ExecutionSurfaces, TypedKind};

/// The wire protocol version marion speaks. `agent-client-protocol` 2.0.0 is still **wire v1**
/// (§5.2: *"v2 lives behind `unstable_protocol_v2`"*), and both agents S20 probed answered `1`.
pub const PROTOCOL_VERSION: u64 = 1;

/// §5.2's `acp` adapter row as a point in §3.4's cross-product: a typed plane over pipes.
///
/// Not `shared(Acp)`. marion owns no pty for an ACP agent — the agent's output arrives as protocol
/// frames, which is `StructuredUi`, and claiming `NativePty` would mint a
/// [`PtyWitness`](crate::PtyWitness) for a node with no terminal.
pub fn surfaces() -> ExecutionSurfaces {
    ExecutionSurfaces::headless(TypedKind::Acp)
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
pub const MCP_SERVER_NAME: &str = "marion";

/// Why an ACP node with `tools: []` still writes (`Harness::writes_without_a_declaration`).
///
/// ACP has no field anywhere that narrows an agent's own tools — not in `initialize`, not in
/// `session/new`. S21's session had `write`, `edit` and `bash` in scope with marion asking for
/// nothing, and marion's own `clientCapabilities.fs.writeTextFile` hands the agent a further one.
pub const NO_TOOL_AVAILABILITY_SURFACE: &str =
    "acp:no-tool-availability-surface (marion compiles no constraint; the protocol has none)";

/// How one agent spells an MCP server's tool to its model.
///
/// An enum with one variant rather than a `&'static str` format string, because the thing being
/// recorded is a **measurement of an agent**, and there is exactly one agent it has been taken on.
/// A second agent adds a variant here only when somebody has watched it call a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSpelling {
    /// `<server>_<tool>`. Measured on `opencode acp` 1.17.3 (S21): marion declared a server named
    /// `marion` offering `report`, and the model called **`marion_report`** — the name appears in
    /// the `session/update` `tool_call` frame's `title`. The MCP `tools/call` the agent then made
    /// to the server carries the **unprefixed** `report`, which is the wire layer and not a name
    /// any model types; the same two-layer split S13 measured on `opencode run`.
    ServerUnderscoreTool,
}

impl ToolSpelling {
    pub fn spell(self, tool: &str) -> String {
        match self {
            Self::ServerUnderscoreTool => format!("{MCP_SERVER_NAME}_{tool}"),
        }
    }
}

/// One ACP agent marion knows how to name. §5.2's `acp` row is one adapter over many of these.
///
/// **A table of measurements, not of intentions.** `argv` is what S20 launched; `tools` is `None`
/// until somebody has seen that agent call a tool, and [`crate::AcpAdapter::compile`] refuses an
/// agent whose `tools` is `None` rather than guessing one — s14's finding is that claude, gemini
/// and opencode all *silently ignore* an unknown tool name, so a guess buys a run that looks
/// healthy and has no tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    /// What an operator writes to select it (`Extras::acp_agent`).
    pub id: &'static str,
    /// The agent's own argv, verbatim as S20 launched it.
    pub argv: &'static [&'static str],
    /// The measured tool spelling, or `None` where marion has never seen this agent call a tool.
    pub tools: Option<ToolSpelling>,
    /// What is known about running it here — carried so a refusal can quote it.
    pub note: &'static str,
}

/// `opencode acp` — the one agent measured all the way to a tool call (S21).
pub const OPENCODE: Agent = Agent {
    id: "opencode",
    argv: &["opencode", "acp"],
    tools: Some(ToolSpelling::ServerUnderscoreTool),
    note: "S21: initialize, session/new, session/prompt and a real `marion_report` tool call, \
           against opencode 1.17.3",
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
    note: "S20: `initialize` succeeds; `session/new` is refused -32000 (Gemini Code Assist \
           ineligibility). No turn has run, so no tool spelling has been measured",
};

/// Every ACP agent marion can name. Naming one is not having measured it — see [`Agent::tools`].
pub const AGENTS: [Agent; 2] = [OPENCODE, GEMINI];

/// Resolve an operator's `acp_agent` id. `None` is *"marion has never heard of it"*, which the
/// adapter turns into a refusal that lists the ids it does know.
pub fn agent(id: &str) -> Option<Agent> {
    AGENTS.into_iter().find(|a| a.id == id)
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

/// The `session/new` param key [`crate::McpRoute::Session`] verifies. One string, so the compiler
/// keeps the builder and the check on the same key.
pub const MCP_SERVERS_KEY: &str = "mcpServers";

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

/// The `sessionId` out of a `session/new` result, or the agent's own words for why there is none.
pub fn session_id(frame: &str) -> Result<String, AcpError> {
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
    v.get("result")
        .and_then(|r| r.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or(AcpError::NeitherResultNorError)
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
pub fn marion_calls(stdout: &str, prefix: &str) -> Vec<MarionCall> {
    let mut open: Vec<(String, String)> = Vec::new(); // (toolCallId, verb) in call order
    let mut outcome: Vec<(String, CallOutcome)> = Vec::new();
    for frame in json_frames(stdout) {
        let Some(u) = update(&frame) else { continue };
        let Some(id) = call_id(u) else { continue };
        if kind(u) == "tool_call"
            && let Some(verb) = u
                .get("title")
                .and_then(Value::as_str)
                .and_then(|t| t.strip_prefix(prefix))
            && !verb.is_empty()
        {
            open.push((id.to_string(), verb.to_string()));
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

/// §6.1 step 9 for an ACP transcript.
///
/// `report_tool` is this agent's **model-facing** spelling ([`ToolSpelling::spell`]), because the
/// name in a `tool_call` frame's title is the model-facing one — the unprefixed `report` never
/// appears on this wire at all, only on the MCP wire between the agent and marion's bridge.
///
/// The exit code is **not consulted**, for the same reason opencode's reader ignores it: an ACP
/// agent is a long-lived stdio server that marion kills, so its exit status describes marion's
/// shutdown, not the turn. What describes the turn is `stopReason` and the frames.
pub fn parse_stream(stdout: &str, _exit: ChildExit, report_tool: &str) -> StreamOutcome {
    let mut out = StreamOutcome::default();
    // The ids of the calls that were opened *as `report`*. Every other tool call on this session
    // also carries a `rawInput`, and a reader that took `narrative` off whichever object happened
    // to have the key would attribute another tool's arguments to marion's verb.
    let mut report_ids: Vec<String> = Vec::new();
    for frame in json_frames(stdout) {
        // A JSON-RPC error frame at the top level: the agent refused something outright, which is
        // the only shape S20's blocker ever produced.
        if let Some(e) = frame.get("error").and_then(|e| e.get("message"))
            && let Some(m) = e.as_str()
        {
            out.failure.get_or_insert_with(|| m.to_string());
        }
        // §5.2: `session/prompt`'s `stopReason` is the turn's own verdict. `end_turn` and
        // `max_tokens` are endings; `refusal` is a failure the exit code cannot see.
        if let Some(r) = frame.pointer("/result/stopReason").and_then(Value::as_str)
            && r == "refusal"
        {
            out.failure
                .get_or_insert_with(|| "the agent ended the turn with stopReason `refusal`".into());
        }
        let Some(u) = update(&frame) else { continue };
        let Some(id) = call_id(u) else { continue };
        if kind(u) == "tool_call" && u.get("title").and_then(Value::as_str) == Some(report_tool) {
            report_ids.push(id.to_string());
        }
        if report_ids.iter().any(|k| k == id) {
            // `rawInput` is the arguments object, **revised in place** across the call's frames:
            // S21's opening `tool_call` carries `{}` and the two updates carry the real arguments,
            // so the last non-empty one wins rather than the first.
            if let Some(args) = u
                .get("rawInput")
                .filter(|a| a.as_object().is_some_and(|o| !o.is_empty()))
            {
                if let Some(n) = args.get("narrative").and_then(Value::as_str) {
                    out.narrative = Some(n.to_string());
                }
                let commits = report_commits(args);
                if !commits.is_empty() {
                    out.result_commits = commits;
                }
            }
        }
        if u.get("status").and_then(Value::as_str) == Some("failed") {
            let words = text_of(u.get("content"));
            out.failure.get_or_insert(if words.is_empty() {
                format!("the agent marked tool call {id} failed")
            } else {
                words
            });
        }
    }
    out
}

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

    /// The model-facing spelling, as the shipped adapter produces it.
    fn report() -> String {
        ToolSpelling::ServerUnderscoreTool.spell("report")
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
            marion_calls(
                &s21_session(),
                &ToolSpelling::ServerUnderscoreTool.spell("")
            ),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Answered,
            }]
        );
        let out = parse_stream(&s21_session(), ChildExit::default(), &report());
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
        let prefix = ToolSpelling::ServerUnderscoreTool.spell("");
        // Opening frame only: the verb is known, the outcome is not — and `Unknown` is the answer,
        // never `Answered`.
        assert_eq!(
            marion_calls(&opening, &prefix),
            vec![MarionCall {
                verb: "report".into(),
                outcome: CallOutcome::Unknown,
            }]
        );
        // Closing frames only: no verb was ever opened, so there is no call to report.
        assert!(marion_calls(&closing, &prefix).is_empty());
        assert_eq!(
            parse_stream(&closing, ChildExit::default(), &report()).narrative,
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
        let out = parse_stream(&other, ChildExit::default(), &report());
        assert_eq!(out.narrative, None);
        assert!(marion_calls(&other, &ToolSpelling::ServerUnderscoreTool.spell("")).is_empty());
        // And prepending it to the real transcript must not change the real answer.
        let both = format!("{other}\n{}", s21_session());
        assert_eq!(
            parse_stream(&both, ChildExit::default(), &report())
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
        let prefix = ToolSpelling::ServerUnderscoreTool.spell("");
        assert_eq!(
            marion_calls(&frame("completed", ""), &prefix)[0].outcome,
            CallOutcome::Answered
        );
        assert_eq!(
            marion_calls(&frame("in_progress", ""), &prefix)[0].outcome,
            CallOutcome::Unknown,
            "a call that never ended is news, not a success"
        );
        assert_eq!(
            marion_calls(
                &frame(
                    "failed",
                    r#","content":[{"type":"content","content":{"type":"text","text":"depth 3 exceeds max_depth 2"}}]"#
                ),
                &prefix
            )[0]
            .outcome,
            CallOutcome::Refused("depth 3 exceeds max_depth 2".into()),
            "the refusal must carry the agent's own words, or a caller cannot act on it"
        );
        // And a failure reaches the stream outcome, where the exit code cannot see it.
        assert_eq!(
            parse_stream(&frame("failed", ""), ChildExit::default(), &report())
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
        let out = parse_stream(refused, ChildExit::default(), &report());
        assert!(out.failure.is_some_and(|f| f.contains("refusal")));
        // `end_turn` is not a failure, or the assertion above would hold for every transcript.
        assert_eq!(
            parse_stream(
                r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}"#,
                ChildExit::default(),
                &report()
            )
            .failure,
            None
        );
    }

    /// **The registry is a table of measurements.** An agent with no measured spelling must not be
    /// silently served by the one spelling this adapter has — that is the s14 trap. The invariant
    /// is stated over the registry rather than over today's two rows, so a third agent has to
    /// declare which side it is on.
    #[test]
    fn every_launchable_agent_spells_marions_verbs_the_way_this_adapter_says() {
        let mut measured = 0;
        let mut unmeasured = 0;
        for a in AGENTS {
            match a.tools {
                Some(t) => {
                    assert_eq!(
                        t.spell("report"),
                        report(),
                        "`{}` spells marion's verbs differently from what the adapter compiles, so \
                         the adapter needs a per-agent spelling before it can launch it",
                        a.id
                    );
                    measured += 1;
                }
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
        assert!(
            measured > 0 && unmeasured > 0,
            "both sides must be exercised"
        );
        assert_eq!(agent("no-such-agent"), None);
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
        // An empty id is not an id, and neither is an absent one.
        assert!(session_id(r#"{"result":{"sessionId":""}}"#).is_err());
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
