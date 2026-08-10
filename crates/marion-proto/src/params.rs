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

use std::path::PathBuf;

use marion_core::contract::{AgentId, Isolation};
use marion_core::harness::Harness;
use serde::{Deserialize, Serialize};

use crate::model::{
    ElicitationRequestId, ElicitationResponse, PermissionDecision, PermissionRequestId, ProbeMode,
    QuitDisposition,
};
use crate::native::NativeLaunchContext;

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

/// **Who is asking, when the caller is a node rather than a client** — §5.4's *"per-node capability
/// token bound to its `AgentId`"*, as a parameter.
///
/// Two fields, and the interesting part is the three that are **not** here.
///
/// * **No `agent_type`, no `depth`.** §6.1 step 2's gates read the caller's type and depth, and the
///   supervisor already knows both: it wrote the caller's `SpawnIntent` and can read it back out of
///   its own registry. A caller that *states* them is a caller that can lie about them, and a
///   forged `depth: 0` defeats `max_depth` outright. So this states only identity, and the
///   supervisor derives everything it gates on.
/// * **No `live_children`.** Same argument, one step further: the count is a property of the tree,
///   which only the supervisor can see.
///
/// [`Self::node_token`] is what makes [`Self::agent_id`] a claim marion can check rather than a
/// string on the wire. Without it `agent/spawn` on the socket would let any process that can
/// `connect(2)` assert any identity, and §6.1 step 2's gates would be advisory — a check the caller
/// chooses whether to fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnCaller {
    /// The **caller's** own `AgentId` — the node whose `spawn` this is, which becomes the child's
    /// `parent_id` (§7.5) and stamps `TaskContract.requester` (§9).
    pub agent_id: AgentId,
    /// The secret the supervisor minted for that node at spawn and wrote into its MCP declaration's
    /// `env` block, beside `MARION_AGENT_ID`.
    ///
    /// **No `#[serde(default)]`.** An omitted token must be a deserialization failure rather than
    /// the empty string, or the credential becomes one every process on the machine already has.
    pub node_token: String,
}

/// `agent/spawn` — a client creating a **root**, or a node spawning a **child**.
///
/// The two are one method and one params type, distinguished by [`Self::caller`]: `None` is the
/// client, `Some` is the node. There is deliberately no sixteenth method for the child case. §2
/// enumerates fifteen, [`crate::Method::ALL`] is pinned at fifteen by test, and the distinction is
/// an `Option` — a second method would have duplicated every field below to carry one extra one.
///
/// Everything from here down is the **root** argument, and it is unchanged because it stays true
/// for `caller: None`. Three parameters are absent and each would have been a wrong answer:
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
///
/// **The four fields below `caller` are the ones a child spawn cannot be expressed without**, and
/// they were absent while this method could only make a root. `77557e3` removed parameters marion
/// read nowhere; these are read everywhere — `acceptance_criteria` and `writable_scope` are §9's
/// contract terms and §5.4's write ceiling, `timeout_secs` is §3.1's wall clock, and `model` is
/// what `spawn` compiles into the child's invocation. Each is optional and each *absent* value is a
/// resolution the supervisor performs rather than a default this struct invents: an empty
/// `writable_scope` is `run_spawn`'s `**` (the agent type's ceiling still applies), an absent
/// `timeout_secs` is the bound the supervisor resolves, and an absent `model` is §3.1's own `model`
/// key. Writing any of those numbers here would make this struct a second source of truth for a
/// value §3.1 already owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnParams {
    /// §3.1's `name:` key — the agent type's identity, resolved through §3.1's precedence chain.
    pub agent_type: String,
    pub prompt: String,
    /// Byte-exact launch inputs captured by a native facade. Absent for legacy callers and all
    /// existing generic spawn paths; omission is the backward-compatible wire spelling. The
    /// dormant payload is boxed so the enclosing protocol call does not reserve its full size for
    /// every legacy spawn; serde keeps the JSON shape unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_launch: Option<Box<NativeLaunchContext>>,
    /// `None` is a client creating a root; `Some` is a node spawning a child. See [`SpawnCaller`].
    #[serde(default)]
    pub caller: Option<SpawnCaller>,
    /// **The working tree the spawn is of** — required with `caller: None`, forbidden with
    /// `caller: Some(_)`, and the supervisor refuses both mismatches by name.
    ///
    /// The pairing is not a style choice, it is the two halves of one fact about *who knows*:
    ///
    /// * With **no caller**, nobody but the client does. §2 keys a supervisor on `git rev-parse
    ///   --git-common-dir`, so one supervisor serves a repository and every linked worktree of it
    ///   over one socket and one journal; `<state>/<project-hash>` names the project and cannot be
    ///   inverted into a working tree, and there is no *right* default among the trees it covers.
    ///   Defaulting to the project root would branch a feature worktree's children off the main
    ///   tree's HEAD — a real branch off real commits, silently.
    /// * With a **caller**, the supervisor already knows, because it recorded the tree when it
    ///   claimed that node. A caller that states it is a caller that can lie about it, and this
    ///   type keeps `agent_type` and `depth` off [`SpawnCaller`] for exactly that reason: every
    ///   gated fact is derived from what the supervisor minted, never asserted by the frame.
    ///
    /// A single `Option` and not a second method, for the reason the doc above already gives: §2
    /// enumerates fifteen methods and [`crate::Method::ALL`] is pinned at fifteen by test.
    #[serde(default)]
    pub repo: Option<PathBuf>,
    /// §9's contract terms. Empty is *"none stated"*, which is what a root has.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// §5.4's write ceiling for the child, as globs. Empty is not "write nothing": `run_spawn`
    /// reads it as `**`, still clipped by the agent type's own `scope_ceiling`.
    #[serde(default)]
    pub writable_scope: Vec<String>,
    /// §3.1's wall clock, in seconds. **`Option`, not a number with a default**: the supervisor
    /// clamps and resolves the bound, and a literal here would be a second spelling of it that
    /// could drift from the one the node actually ran under and its contract records.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// The model in marion's request vocabulary. `None` falls back to the agent type's own `model`
    /// key (§3.1), which is what makes a spawn launchable without the caller having to know which
    /// harness needs a model and in what spelling.
    #[serde(default)]
    pub model: Option<String>,
    /// **§9's change record, declined** — `marion run --no-change-record`, and a root-only field on
    /// [`Self::repo`]'s pairing rule: permitted with `caller: None`, refused with `caller: Some(_)`.
    ///
    /// It is here because a root runs in the operator's **own checkout** rather than in a worktree
    /// marion made, so marion snapshots that tree at launch and at exit and the delta is the only
    /// record of what the run did (§9). Only the operator can say that snapshot must not be taken —
    /// of a directory too large to walk, or one they would rather marion did not read — and once the
    /// supervisor drives the root there is no other channel for them to say it on. A child has no
    /// such record at all: its writes are judged against its worktree, so the field would be an
    /// accept-and-ignore there, which §11 item 23 refuses.
    ///
    /// `Option<bool>`, not `bool`, so that "stated" and "unstated" are distinguishable on the wire
    /// and the pairing can be refused by name — a plain `false` from a caller and a `false` serde
    /// invented are the same bytes, and the refusal would then fire on the operator's default.
    /// Absent, for a root, is `false`: marion looks. That is the direction that produces a record
    /// rather than a silence, and §9's whole argument for the record is that its absence is
    /// indistinguishable from a write that escaped.
    #[serde(default)]
    pub no_change_record: Option<bool>,
    /// **§9's M3, asked for**: run this node in a terminal marion owns, so `node/attach` can serve
    /// its pane and a client can drive it by keystrokes.
    ///
    /// A second root-only field on [`Self::repo`]'s pairing rule, and it is refused with
    /// `caller: Some(_)` for a sharper reason than symmetry. A child is defined by its
    /// `TaskContract`: it is spawned to do a task and to `report`, under the wall clock the
    /// contract records. A pane is a TUI, which takes no turn at all until a human presses return —
    /// so a contracted child in a pane is a task that can only ever time out, and the contract
    /// would record that as the child's failure. A pane belongs to a **root**: a node an operator
    /// started and is watching.
    ///
    /// `Option<bool>` for exactly [`Self::no_change_record`]'s reason: absent and `false` must be
    /// distinguishable on the wire, or the refusal above fires on a caller's default. Absent is
    /// `false` — a node gets a pane because a run asked for one, never because the harness has one
    /// to give.
    #[serde(default)]
    pub pane: Option<bool>,
    /// **§5.4's `isolation`, and the mirror of [`Self::no_change_record`]'s pairing**: permitted
    /// with `caller: Some(_)`, refused with `caller: None`.
    ///
    /// A child's workspace is a choice its parent makes — §6.6's own worktree, or the caller's
    /// directory shared with it. A **root** has no such choice: it *is* the operator's checkout, in
    /// the tree they named with `--repo`, and §9's change record is built on exactly that. Accepting
    /// the key there and running in the checkout anyway would be the accept-and-ignore §11 item 23
    /// refuses; accepting `worktree` and honouring it would branch a root off HEAD into a directory
    /// the operator never asked marion to make.
    ///
    /// `Option`, so absent and stated are distinguishable and the pairing can be refused by name.
    /// Absent, for a child, resolves to [`Isolation::Worktree`] — see that type for why the
    /// resolution diverges from §3.1's stated default rather than silently removing containment
    /// from every caller that names nothing.
    ///
    /// `remote` is not representable: it is refused at the request edge with a sentence naming it,
    /// and is deliberately not an [`Isolation`] variant.
    #[serde(default)]
    pub isolation: Option<Isolation>,
    /// **§6.6's escape hatch**: may this child share a cwd with a live write-capable sibling?
    ///
    /// Only meaningful beside `isolation: SharedCwd`; the request edge refuses `true` under
    /// `worktree`, where marion gives the child its own tree and there is no guard to lift.
    ///
    /// `Option<bool>` for [`Self::no_change_record`]'s reason — absent and an explicit `false` must
    /// be distinguishable, because `false` is what marion does and must never be refused.
    #[serde(default)]
    pub allow_concurrent_writes: Option<bool>,
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
            prompt: "implement §6.3".into(),
            native_launch: None,
            caller: None,
            repo: Some("/r".into()),
            acceptance_criteria: vec![],
            writable_scope: vec![],
            timeout_secs: None,
            model: None,
            no_change_record: Some(true),
            pane: None,
            isolation: None,
            allow_concurrent_writes: None,
        });
        rt!(AgentSpawnParams {
            agent_type: "codex-impl".into(),
            prompt: "implement §6.3".into(),
            native_launch: None,
            caller: Some(SpawnCaller {
                agent_id: agent(),
                node_token: "tok-abc".into(),
            }),
            repo: None,
            acceptance_criteria: vec!["the suite is green".into()],
            writable_scope: vec!["src/**".into()],
            timeout_secs: Some(120),
            model: Some("sonnet".into()),
            no_change_record: None,
            pane: None,
            isolation: None,
            allow_concurrent_writes: None,
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
        // **Every pre-existing field is written, including the absent ones.** `native_launch` is
        // the one backward-compatible exception: its absence must preserve the legacy frame byte
        // for byte. A `skip_serializing_if` on `caller` would make absent and `caller: null` mean
        // the same thing on the wire, and those are the two halves of this method's distinction: a
        // client creating a root, and a node spawning a child. Stating `null` costs five bytes and
        // makes the legacy shape one thing.
        assert_eq!(
            serde_json::to_string(&AgentSpawnParams {
                agent_type: "codex-impl".into(),
                prompt: "go".into(),
                native_launch: None,
                caller: None,
                repo: Some("/r".into()),
                acceptance_criteria: vec![],
                writable_scope: vec![],
                timeout_secs: None,
                model: None,
                no_change_record: None,
                pane: None,
                isolation: None,
                allow_concurrent_writes: None,
            })
            .unwrap(),
            r#"{"agent_type":"codex-impl","prompt":"go","caller":null,"repo":"/r","acceptance_criteria":[],"writable_scope":[],"timeout_secs":null,"model":null,"no_change_record":null,"pane":null,"isolation":null,"allow_concurrent_writes":null}"#
        );
        assert_eq!(
            serde_json::to_string(&AgentSpawnParams {
                agent_type: "codex-impl".into(),
                prompt: "go".into(),
                native_launch: None,
                caller: Some(SpawnCaller {
                    agent_id: AgentId("a".into()),
                    node_token: "t".into(),
                }),
                repo: None,
                acceptance_criteria: vec!["c".into()],
                writable_scope: vec!["src/**".into()],
                timeout_secs: Some(60),
                model: Some("sonnet".into()),
                no_change_record: None,
                pane: None,
                isolation: None,
                allow_concurrent_writes: None,
            })
            .unwrap(),
            r#"{"agent_type":"codex-impl","prompt":"go","caller":{"agent_id":"a","node_token":"t"},"repo":null,"acceptance_criteria":["c"],"writable_scope":["src/**"],"timeout_secs":60,"model":"sonnet","no_change_record":null,"pane":null,"isolation":null,"allow_concurrent_writes":null}"#
        );
    }

    /// A client creating a root states no caller, and the five fields it did not state are the
    /// values §11 item 23's rule requires: empty, absent, and *not* a fabricated default that a
    /// caller would have no way to discover marion had chosen for it.
    #[test]
    fn a_client_creating_a_root_states_only_what_a_root_needs() {
        let p: AgentSpawnParams =
            serde_json::from_str(r#"{"agent_type":"codex-impl","prompt":"go"}"#).unwrap();
        assert_eq!(p.caller, None, "no caller means a client creating a root");
        assert_eq!(
            p.repo, None,
            "the deserializer permits it absent — a required-here `repo` would make the field \
             undeserializable for the `caller: Some(_)` half, where stating it is the error. The \
             pairing is the supervisor's to refuse, by name, and it does"
        );
        assert!(p.acceptance_criteria.is_empty());
        assert!(p.writable_scope.is_empty());
        assert_eq!(
            p.timeout_secs, None,
            "absent is absent: the supervisor resolves the bound from §3.1, and a number invented \
             here would be a second source of truth for it"
        );
        assert_eq!(p.model, None);
        assert_eq!(
            p.no_change_record, None,
            "unstated is unstated: the supervisor reads absence as `false` for a root, and the \
             pairing rule refuses a stated value from a caller by name"
        );
    }

    /// **The F2 shape, at the deserializer.** A caller states who it is and proves it. It does not
    /// state its agent type and it does not state its depth, because the supervisor already knows
    /// both from the `SpawnIntent` it wrote — and a caller that *states* them is a caller that can
    /// lie about them. Accepting either field would make §6.1 step 2's gates advisory.
    #[test]
    fn a_caller_cannot_state_its_own_depth_or_agent_type() {
        for (json, field) in [
            (r#"{"agent_id":"a","node_token":"t","depth":0}"#, "depth"),
            (
                r#"{"agent_id":"a","node_token":"t","agent_type":"codex-impl"}"#,
                "agent_type",
            ),
        ] {
            let e = serde_json::from_str::<SpawnCaller>(json).unwrap_err();
            assert!(e.to_string().contains(field), "must name the field: {e}");
        }
    }

    /// A caller that names itself and offers no proof is not a caller. The token has no `default`,
    /// so an omitted one is a deserialization failure rather than the empty string — which would
    /// otherwise be a credential every process on the machine already has.
    #[test]
    fn a_caller_without_a_token_is_not_a_caller() {
        let e = serde_json::from_str::<SpawnCaller>(r#"{"agent_id":"a"}"#).unwrap_err();
        assert!(e.to_string().contains("node_token"), "{e}");
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
                // `isolation` used to sit here, as a key the MCP schema declared and the socket did
                // not carry. It carries it now, so the slot needs a key that is *still* only in the
                // schema — otherwise this test would keep asserting a gap that has closed, which is
                // the same staleness §11 item 23 is about.
                r#"{"agent_type":"codex-impl","prompt":"go","name":"impl-auth"}"#,
                "name",
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

    /// **`remote` is not representable, and the deserializer is where that becomes a fact.**
    ///
    /// [`Isolation`] holds only workspaces marion builds, so a socket client cannot get `remote`
    /// into a `SpawnRequest` even by bypassing the MCP edge — where the refusal is a sentence in
    /// marion's voice ([`SpawnError::IsolationUnimplemented`]). Here it is a parse failure, and the
    /// assertion is that it names the two values that *are* served rather than merely saying no:
    /// the caller has to be able to fix the call.
    #[test]
    fn isolation_remote_is_not_a_value_this_type_can_hold() {
        let e = serde_json::from_str::<AgentSpawnParams>(
            r#"{"agent_type":"codex-impl","prompt":"go","isolation":"remote"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("remote"), "names the value it rejected: {e}");
        assert!(e.contains("worktree") && e.contains("shared-cwd"), "{e}");
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
