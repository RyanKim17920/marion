//! The fifteen methods' parameters.
//!
//! **Every params struct here declares only parameters the supervisor acts on.** That is the rule
//! §11 item 23 (commit `77557e3`) established for the agent-facing `spawn` schema after five of its
//! eleven parameters turned out to be read nowhere: a caller that asks for something, receives no
//! error, and is told nothing has been given a wrong answer that looks like a right one. The
//! cheapest place to enforce it is here — a parameter that does not exist cannot be silently
//! dropped — and the enforcement is visible in what is *missing* from these structs, so each
//! omission is documented with what asking for it would have meant.
//!
//! Every struct also carries `#[serde(deny_unknown_fields)]`. The crate doc argues the asymmetry
//! with results; the short form is that a typo in a parameter changes what runs.

use marion_core::contract::AgentId;
use marion_core::harness::Harness;
use serde::{Deserialize, Serialize};

use crate::model::{
    ElicitationRequestId, ElicitationResponse, PermissionDecision, PermissionRequestId, ProbeMode,
    QuitDisposition,
};

/// `tree/subscribe` — no parameters, deliberately.
///
/// A filter (`include_terminal`, a depth limit, a subtree root) was the obvious first field and is
/// exactly the parameter this module refuses: the supervisor holds one registry and §5.6's tree
/// pane renders all of it, so a filter would be applied by the client either way and declaring it
/// here would mean marion accepting a narrowing it does not perform. An empty struct rather than
/// `null` params so the shape is stable when the first real parameter arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeSubscribeParams {}

/// `node/get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeGetParams {
    pub agent_id: AgentId,
}

/// `node/attach` — §7.3.3's re-attach, per node.
///
/// One node per call, not a session-wide attach, because §7.3.3's finding is that *"the split is
/// by node, not by session"* and one node's answer (re-subscribe) is not another's (replay). A
/// session-wide parameter would force a single [`crate::AttachMode`] on a tree that legitimately
/// needs all three.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeAttachParams {
    pub agent_id: AgentId,
}

/// `node/detach`.
///
/// §2 and §6.2: this is **subscription bookkeeping over a channel the supervisor never let go of**.
/// It has no force parameter and no disposition, because it does nothing to the node — a detach
/// that could affect a node would be a second, unconfirmed route to §7.3.2's dispositions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDetachParams {
    pub agent_id: AgentId,
}

/// `node/prompt` — a new turn on a node with none in flight (§6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodePromptParams {
    pub agent_id: AgentId,
    pub text: String,
}

/// `node/steer` — mid-flight injection (§6.3). Requires `caps.steer`; a harness without it is
/// answered [`crate::FailureKind::Unsupported`].
///
/// A separate params type from [`NodePromptParams`] despite the identical shape. They are not the
/// same message: §6.3 makes the choice between them a function of the node's state, the two are
/// refused under opposite conditions, and one requires a capability the other does not. Sharing
/// the type would invite sharing the handler, which is where the two rules get merged into one
/// wrong one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSteerParams {
    pub agent_id: AgentId,
    pub text: String,
}

/// `node/cancel` — interrupt the running turn. `caps.interrupt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCancelParams {
    pub agent_id: AgentId,
}

/// `node/kill` — end the node. §6.7 classifies the result as `Cancelled`; §7.3.2's disposition (a)
/// is this operation applied per node, and quitting invents no new terminal state.
///
/// No `signal` and no `force` parameter: §6.7 owns the mechanism (own process group, `killpg`), and
/// a caller-chosen signal is a parameter marion would have to either honour — contradicting the
/// specified path — or ignore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeKillParams {
    pub agent_id: AgentId,
}

/// `node/rename` — sets `Node.name`, the address other agents use with `send` (§2).
///
/// **There is no parameter that could move a bound `allow_peers` grant, and there must never be.**
/// §5.4: a grant binds to an `AgentId` on first successful use and is *never* re-bound; §2 states
/// the consequence directly — *"renaming a node never moves an `allow_peers` grant that has already
/// bound to it"*. An *unbound* grant does resolve its name at first use, so renaming a node into a
/// peer's list before that peer's first call **does** confer the grant; that is late binding's
/// acknowledged cost, bounded by binding happening once, and it is a property of when the peer
/// calls rather than anything this method could take as an argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRenameParams {
    pub agent_id: AgentId,
    pub name: String,
}

/// `permission/reply`.
///
/// Keyed by the request, not by the node: a node can have more than one outstanding ask, and
/// answering "the node's permission" would answer whichever arrived last. §11 item 22 is the gap
/// this method closes — today marion has no route from a permission request to a human and denies
/// by sleeping the bound (§9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionReplyParams {
    pub request_id: PermissionRequestId,
    pub decision: PermissionDecision,
}

/// `elicitation/reply`. §2: a separate method because ACP and Codex both have structured input
/// requests **distinct from** permissions. It shares the permission queue's UI, not its type — an
/// elicitation answered with `Allow` is a category error.
///
/// Not `Eq`: [`ElicitationResponse::Provided`] carries `serde_json::Value`, and JSON numbers do
/// not have a total equality. Comparing two elicitation answers for *exact* equality is not a
/// meaningful operation, so the bound is absent rather than faked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitationReplyParams {
    pub request_id: ElicitationRequestId,
    pub response: ElicitationResponse,
}

/// `policy/set` — **declared by §2 and specified nowhere.**
///
/// This is the uninhabited type, and it is the sharpest application of §11 item 23's rule in this
/// crate. §2 names the method; no section of the design names a single policy key, a value domain,
/// a scope, or what setting one would do. The three available shapes were:
///
/// 1. `serde_json::Value` — accept anything, act on nothing. This is precisely the
///    accept-and-ignore shape `77557e3` removed from `spawn`, except the caller here would have no
///    way at all to discover that their policy was dropped.
/// 2. An invented enum of plausible policies. Worse: it publishes a vocabulary the supervisor
///    would have to either implement to marion's guess or refuse per-variant, and a client written
///    against the guess is a client that must be un-taught.
/// 3. Uninhabited. `policy/set` remains a name the protocol knows — [`crate::Method::PolicySet`]
///    exists, is counted in the fifteen, and round-trips — but no call to it can be constructed in
///    Rust and none can be deserialized from the wire. The refusal arrives as a sentence naming the
///    reason, not as a dropped field.
///
/// (3), because it is the only one under which a client's mistake is visible to the client. When a
/// policy is specified, this becomes an enum with one variant and nothing else changes.
///
/// The hand-written `Deserialize` exists to make the refusal readable: serde's own message for an
/// enum with no variants does not say *why*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnspecifiedPolicy {}

/// The sentence a `policy/set` call is refused with. A constant so the test asserts the same text
/// the wire carries.
pub const POLICY_SET_UNSPECIFIED: &str = "policy/set names no policy this build implements: §2 lists the method and no design section \
     specifies a policy key, so accepting one would be an accept-and-ignore (§11 item 23)";

impl Serialize for UnspecifiedPolicy {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        // Uninhabited: there is no value of this type, so this arm is unreachable by construction
        // rather than by discipline.
        match *self {}
    }
}

impl<'de> Deserialize<'de> for UnspecifiedPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(POLICY_SET_UNSPECIFIED))
    }
}

/// `agent/spawn` — the client creating a **root**.
///
/// Three parameters are absent and each would have been a wrong answer:
///
/// * **`background`** — §5.4 spells it `"background": false // M1: must be false (§9)`, and
///   `77557e3` refuses it by name because a caller asking for a handle instead waits out the
///   child's whole synchronous run. A client-facing spawn does not need it at all: this call
///   returns as soon as the node exists and the client watches through `tree/subscribe`, so there
///   is no blocking mode to opt out of.
/// * **`isolation`** — `AgentType` has no such field and `make_worktree` is called
///   unconditionally, so `shared-cwd` would silently *add* containment the caller did not ask for
///   and `remote` would be served by running on the operator's own machine.
/// * **`verification`** — hardcoded empty at the construction site, so a caller who asked for
///   `cargo test` and one who asked for nothing receive byte-identical contracts, and an empty
///   field reads as "verified, nothing to report" when the truth is "never ran".
///
/// `name` is also absent, but for a different and weaker reason: it is *performable* — §2's
/// `node/rename` sets exactly that field — so declaring it here would duplicate a method rather
/// than fake one. A client that wants a named root calls `node/rename` with the returned id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnParams {
    /// §3.1's `name:` key — the agent type's identity, resolved through §3.1's precedence chain.
    pub agent_type: String,
    pub prompt: String,
}

/// `doctor/run`. §8's two modes, plus an optional single-harness filter — which *is* performed:
/// `--adapter` spawns a real process per harness, so probing one instead of four is a difference
/// the operator can observe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorRunParams {
    pub mode: ProbeMode,
    /// `None` probes every harness in [`Harness::ALL`].
    #[serde(default)]
    pub harness: Option<Harness>,
}

/// `session/quit` — §2's *"the only method on this list that can end the supervisor"*, and M2+.
///
/// The disposition is required and has no default. §7.3.1: a client that closes without calling
/// this *"has chosen nothing"*, and the absence of a call must never decay into a value — see
/// [`QuitDisposition`] and [`crate::ClientGone`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionQuitParams {
    pub disposition: QuitDisposition,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> AgentId {
        AgentId("0199c0ff-ee00-7000-8000-000000000001".into())
    }

    #[test]
    fn params_round_trip() {
        macro_rules! rt {
            ($v:expr) => {{
                let v = $v;
                let s = serde_json::to_string(&v).unwrap();
                let back = serde_json::from_str(&s).unwrap();
                assert_eq!(v, back, "round trip failed for {s}");
            }};
        }
        rt!(TreeSubscribeParams {});
        rt!(NodeGetParams { agent_id: agent() });
        rt!(NodeAttachParams { agent_id: agent() });
        rt!(NodeDetachParams { agent_id: agent() });
        rt!(NodePromptParams {
            agent_id: agent(),
            text: "go".into()
        });
        rt!(NodeSteerParams {
            agent_id: agent(),
            text: "stop that".into()
        });
        rt!(NodeCancelParams { agent_id: agent() });
        rt!(NodeKillParams { agent_id: agent() });
        rt!(NodeRenameParams {
            agent_id: agent(),
            name: "impl".into()
        });
        rt!(PermissionReplyParams {
            request_id: PermissionRequestId("r-1".into()),
            decision: PermissionDecision::Deny {
                reason: "outside writable_scope".into()
            }
        });
        rt!(ElicitationReplyParams {
            request_id: ElicitationRequestId("e-1".into()),
            response: ElicitationResponse::Provided(serde_json::json!({"branch": "main"}))
        });
        rt!(AgentSpawnParams {
            agent_type: "codex-impl".into(),
            prompt: "implement §6.3".into()
        });
        rt!(DoctorRunParams {
            mode: ProbeMode::Adapter,
            harness: Some(Harness::Codex)
        });
        rt!(DoctorRunParams {
            mode: ProbeMode::Capabilities,
            harness: None
        });
        rt!(SessionQuitParams {
            disposition: QuitDisposition::DEFAULT
        });
        rt!(SessionQuitParams {
            disposition: QuitDisposition::KillTree {
                confirmed: vec![agent()]
            }
        });
    }

    #[test]
    fn params_pin_their_wire_shapes() {
        assert_eq!(
            serde_json::to_string(&TreeSubscribeParams {}).unwrap(),
            "{}"
        );
        assert_eq!(
            serde_json::to_string(&NodePromptParams {
                agent_id: AgentId("a".into()),
                text: "go".into()
            })
            .unwrap(),
            r#"{"agent_id":"a","text":"go"}"#
        );
        assert_eq!(
            serde_json::to_string(&DoctorRunParams {
                mode: ProbeMode::Capabilities,
                harness: Some(Harness::ClaudeCode)
            })
            .unwrap(),
            r#"{"mode":"Capabilities","harness":"claude-code"}"#
        );
        assert_eq!(
            serde_json::to_string(&SessionQuitParams {
                disposition: QuitDisposition::DetachAll
            })
            .unwrap(),
            r#"{"disposition":"DetachAll"}"#
        );
    }

    #[test]
    fn an_unknown_parameter_is_rejected_not_ignored() {
        // The §11 item 23 property, at the deserializer. Each of these is a plausible client
        // mistake whose silent acceptance would produce a clean-looking wrong run.
        let cases = [
            (
                r#"{"agent_type":"codex-impl","prompt":"go","background":true}"#,
                "background",
            ),
            (
                r#"{"agent_type":"codex-impl","prompt":"go","verification":["cargo test"]}"#,
                "verification",
            ),
            (
                r#"{"agent_type":"codex-impl","prompt":"go","isolation":"shared-cwd"}"#,
                "isolation",
            ),
        ];
        for (json, field) in cases {
            let e = serde_json::from_str::<AgentSpawnParams>(json).unwrap_err();
            assert!(
                e.to_string().contains(field),
                "rejection must name the field: {e}"
            );
        }
    }

    #[test]
    fn a_misspelled_parameter_is_rejected_rather_than_defaulted() {
        // The failure this protects against is not a malicious field but a typo: `agent_id`
        // spelled `agentid` would otherwise leave `agent_id` missing — caught here — while a typo
        // in an optional field would silently take the default.
        let e = serde_json::from_str::<DoctorRunParams>(r#"{"mode":"Adapter","harnes":"codex"}"#)
            .unwrap_err();
        assert!(e.to_string().contains("harnes"), "{e}");
    }

    #[test]
    fn a_policy_cannot_be_deserialized_and_the_refusal_says_why() {
        let e = serde_json::from_str::<UnspecifiedPolicy>(r#"{"anything":1}"#).unwrap_err();
        assert!(
            e.to_string().contains("policy/set names no policy"),
            "the refusal must be a sentence, got: {e}"
        );
        assert!(e.to_string().contains("§11 item 23"), "{e}");
    }
}
