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
//! **What this module is not.** It is the handshake, not a session. `opencode acp` opens a session
//! and `gemini --acp` does not (S20: `session/new` answers a vendor-side ineligibility error), and
//! neither fact is reachable from here — this module never spawns a process. `crate::adapter`
//! deliberately has no ACP `HarnessAdapter`: `parse_stream(stdout, exit)` reads a finished stream
//! and [`McpRoute`](crate::McpRoute) names a *pre-launch* declaration route, while ACP's MCP
//! declaration rides `session/new` **after** launch over a live bidirectional channel. Forcing ACP
//! through that trait would put a fifth `McpRoute` variant in the supervisor's declaration check
//! with nothing behind it to check. The consumer that does exist is `marion doctor`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::caps::Capabilities;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surfaces::{ControlTransport, DisplaySurface};

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/s20");

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
