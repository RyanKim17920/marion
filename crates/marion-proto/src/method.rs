//! The fifteen methods, and the two enums that keep a method's name, its params and its result from
//! drifting apart.
//!
//! [`Call`] is the request side: a method **and** its parameters, in one value. There is no way to
//! hold `Method::NodePrompt` next to a `NodeSteerParams`, which is the mistake a `(String, Value)`
//! pair invites and a dispatcher then has to re-check by hand.
//!
//! [`MethodResult`] is the response side, and it is deliberately *not* part of the wire envelope.
//! JSON-RPC does not put the method on a response — correlation is by `id` — so a response type
//! that carried one would be inventing a field marion does not control. The envelope therefore
//! carries the result as JSON (see [`crate::Outcome`]) and the typing happens at the seam where the
//! pending-request map already knows which method the `id` belongs to:
//! [`Method::decode_result`].

use serde::{Deserialize, Serialize};

use crate::error::RpcError;
use crate::params::{
    AgentSpawnParams, DoctorRunParams, ElicitationReplyParams, NodeAttachParams, NodeCancelParams,
    NodeDetachParams, NodeGetParams, NodeKillParams, NodePromptParams, NodeRenameParams,
    NodeSteerParams, POLICY_SET_UNSPECIFIED, PermissionReplyParams, SessionQuitParams,
    TreeSubscribeParams, UnspecifiedPolicy,
};
use crate::result::{
    AgentSpawnResult, DeliveryResult, DoctorRunResult, NodeAttachResult, NodeCancelResult,
    NodeDetachResult, NodeGetResult, NodeKillResult, NodeRenameResult, ReplyResult,
    SessionQuitResult, TreeSubscribeResult,
};

/// §2's client↔supervisor method list.
///
/// **Fifteen.** §10's prose says fourteen and is stale: §2 lines 87–89 enumerate fifteen, and line
/// 91 documents `session/quit` immediately below the list as the addition. [`Method::ALL`] is
/// pinned to fifteen by test so the count cannot drift back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Method {
    TreeSubscribe,
    NodeGet,
    NodeAttach,
    NodeDetach,
    NodePrompt,
    NodeSteer,
    NodeCancel,
    NodeKill,
    NodeRename,
    PermissionReply,
    ElicitationReply,
    /// Declared by §2, specified nowhere. See [`UnspecifiedPolicy`]: the name exists, no call to it
    /// can be constructed or parsed, and the refusal is a sentence.
    PolicySet,
    AgentSpawn,
    DoctorRun,
    /// M2+. §2: *"the only method on this list that can end the supervisor"*.
    SessionQuit,
}

impl Method {
    /// The wire spelling. The one place the strings live — `Call`'s serde renames below must match,
    /// and a test walks [`Method::ALL`] against them so a rename in one place fails in the other.
    pub const fn as_str(self) -> &'static str {
        match self {
            Method::TreeSubscribe => "tree/subscribe",
            Method::NodeGet => "node/get",
            Method::NodeAttach => "node/attach",
            Method::NodeDetach => "node/detach",
            Method::NodePrompt => "node/prompt",
            Method::NodeSteer => "node/steer",
            Method::NodeCancel => "node/cancel",
            Method::NodeKill => "node/kill",
            Method::NodeRename => "node/rename",
            Method::PermissionReply => "permission/reply",
            Method::ElicitationReply => "elicitation/reply",
            Method::PolicySet => "policy/set",
            Method::AgentSpawn => "agent/spawn",
            Method::DoctorRun => "doctor/run",
            Method::SessionQuit => "session/quit",
        }
    }

    pub const ALL: [Method; 15] = [
        Method::TreeSubscribe,
        Method::NodeGet,
        Method::NodeAttach,
        Method::NodeDetach,
        Method::NodePrompt,
        Method::NodeSteer,
        Method::NodeCancel,
        Method::NodeKill,
        Method::NodeRename,
        Method::PermissionReply,
        Method::ElicitationReply,
        Method::PolicySet,
        Method::AgentSpawn,
        Method::DoctorRun,
        Method::SessionQuit,
    ];

    pub fn from_wire(s: &str) -> Option<Method> {
        Method::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// **The only method that can end the supervisor** (§2). A predicate rather than a comment,
    /// because a dispatcher that must special-case the one lifetime-ending verb should ask the
    /// protocol rather than match on a name.
    pub const fn can_end_the_supervisor(self) -> bool {
        matches!(self, Method::SessionQuit)
    }

    /// Type the JSON body of a response, given the method its `id` was pending on.
    ///
    /// The `Err` cases are the two honest ones: a `policy/set` result that cannot exist, and a body
    /// that did not parse — reported as a sentence naming the method, because "invalid type:
    /// string" with no context is unactionable in a client that has fifteen of these.
    pub fn decode_result(self, body: &serde_json::Value) -> Result<MethodResult, RpcError> {
        fn take<T: serde::de::DeserializeOwned>(
            m: Method,
            v: &serde_json::Value,
        ) -> Result<T, RpcError> {
            serde_json::from_value(v.clone()).map_err(|e| {
                RpcError::invalid_request(format!("{} result did not parse: {e}", m.as_str()))
            })
        }
        Ok(match self {
            Method::TreeSubscribe => MethodResult::TreeSubscribe(take(self, body)?),
            Method::NodeGet => MethodResult::NodeGet(take(self, body)?),
            Method::NodeAttach => MethodResult::NodeAttach(take(self, body)?),
            Method::NodeDetach => MethodResult::NodeDetach(take(self, body)?),
            Method::NodePrompt => MethodResult::NodePrompt(take(self, body)?),
            Method::NodeSteer => MethodResult::NodeSteer(take(self, body)?),
            Method::NodeCancel => MethodResult::NodeCancel(take(self, body)?),
            Method::NodeKill => MethodResult::NodeKill(take(self, body)?),
            Method::NodeRename => MethodResult::NodeRename(take(self, body)?),
            Method::PermissionReply => MethodResult::PermissionReply(take(self, body)?),
            Method::ElicitationReply => MethodResult::ElicitationReply(take(self, body)?),
            Method::PolicySet => {
                return Err(RpcError::unimplemented(
                    "policy/set",
                    POLICY_SET_UNSPECIFIED,
                    "§2",
                ));
            }
            Method::AgentSpawn => MethodResult::AgentSpawn(take(self, body)?),
            Method::DoctorRun => MethodResult::DoctorRun(take(self, body)?),
            Method::SessionQuit => MethodResult::SessionQuit(take(self, body)?),
        })
    }
}

/// A method **with** its parameters.
///
/// Adjacently tagged, which is what makes the wire `{"method":…,"params":…}` — JSON-RPC's shape —
/// rather than serde's default externally-tagged `{"tree/subscribe":…}`. `params` is present on
/// every variant, including the empty one, because an optional `params` key would give a
/// dispatcher two spellings of "no arguments" to handle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Call {
    #[serde(rename = "tree/subscribe")]
    TreeSubscribe(TreeSubscribeParams),
    #[serde(rename = "node/get")]
    NodeGet(NodeGetParams),
    #[serde(rename = "node/attach")]
    NodeAttach(NodeAttachParams),
    #[serde(rename = "node/detach")]
    NodeDetach(NodeDetachParams),
    #[serde(rename = "node/prompt")]
    NodePrompt(NodePromptParams),
    #[serde(rename = "node/steer")]
    NodeSteer(NodeSteerParams),
    #[serde(rename = "node/cancel")]
    NodeCancel(NodeCancelParams),
    #[serde(rename = "node/kill")]
    NodeKill(NodeKillParams),
    #[serde(rename = "node/rename")]
    NodeRename(NodeRenameParams),
    #[serde(rename = "permission/reply")]
    PermissionReply(PermissionReplyParams),
    #[serde(rename = "elicitation/reply")]
    ElicitationReply(ElicitationReplyParams),
    /// Uninhabited: the variant exists so `policy/set` is a name the protocol knows, and no value
    /// of it can be built or parsed.
    #[serde(rename = "policy/set")]
    PolicySet(UnspecifiedPolicy),
    #[serde(rename = "agent/spawn")]
    AgentSpawn(AgentSpawnParams),
    #[serde(rename = "doctor/run")]
    DoctorRun(DoctorRunParams),
    #[serde(rename = "session/quit")]
    SessionQuit(SessionQuitParams),
}

impl Call {
    pub fn method(&self) -> Method {
        match self {
            Call::TreeSubscribe(_) => Method::TreeSubscribe,
            Call::NodeGet(_) => Method::NodeGet,
            Call::NodeAttach(_) => Method::NodeAttach,
            Call::NodeDetach(_) => Method::NodeDetach,
            Call::NodePrompt(_) => Method::NodePrompt,
            Call::NodeSteer(_) => Method::NodeSteer,
            Call::NodeCancel(_) => Method::NodeCancel,
            Call::NodeKill(_) => Method::NodeKill,
            Call::NodeRename(_) => Method::NodeRename,
            Call::PermissionReply(_) => Method::PermissionReply,
            Call::ElicitationReply(_) => Method::ElicitationReply,
            Call::PolicySet(p) => match *p {},
            Call::AgentSpawn(_) => Method::AgentSpawn,
            Call::DoctorRun(_) => Method::DoctorRun,
            Call::SessionQuit(_) => Method::SessionQuit,
        }
    }
}

/// A typed response body. Never appears on the wire in this shape — see the module doc.
#[derive(Debug, Clone, PartialEq)]
pub enum MethodResult {
    TreeSubscribe(TreeSubscribeResult),
    NodeGet(NodeGetResult),
    NodeAttach(NodeAttachResult),
    NodeDetach(NodeDetachResult),
    NodePrompt(DeliveryResult),
    NodeSteer(DeliveryResult),
    NodeCancel(NodeCancelResult),
    NodeKill(NodeKillResult),
    NodeRename(NodeRenameResult),
    PermissionReply(ReplyResult),
    ElicitationReply(ReplyResult),
    PolicySet(UnspecifiedPolicy),
    AgentSpawn(AgentSpawnResult),
    DoctorRun(DoctorRunResult),
    SessionQuit(SessionQuitResult),
}

impl MethodResult {
    pub fn method(&self) -> Method {
        match self {
            MethodResult::TreeSubscribe(_) => Method::TreeSubscribe,
            MethodResult::NodeGet(_) => Method::NodeGet,
            MethodResult::NodeAttach(_) => Method::NodeAttach,
            MethodResult::NodeDetach(_) => Method::NodeDetach,
            MethodResult::NodePrompt(_) => Method::NodePrompt,
            MethodResult::NodeSteer(_) => Method::NodeSteer,
            MethodResult::NodeCancel(_) => Method::NodeCancel,
            MethodResult::NodeKill(_) => Method::NodeKill,
            MethodResult::NodeRename(_) => Method::NodeRename,
            MethodResult::PermissionReply(_) => Method::PermissionReply,
            MethodResult::ElicitationReply(_) => Method::ElicitationReply,
            MethodResult::PolicySet(p) => match *p {},
            MethodResult::AgentSpawn(_) => Method::AgentSpawn,
            MethodResult::DoctorRun(_) => Method::DoctorRun,
            MethodResult::SessionQuit(_) => Method::SessionQuit,
        }
    }

    /// The JSON body a [`crate::Outcome::Result`] carries. Infallible: every inner type is plain
    /// data, and serialization of plain data into `Value` cannot fail.
    pub fn to_body(&self) -> serde_json::Value {
        fn v<T: Serialize>(t: &T) -> serde_json::Value {
            serde_json::to_value(t).expect("a result type is plain data and cannot fail to encode")
        }
        match self {
            MethodResult::TreeSubscribe(r) => v(r),
            MethodResult::NodeGet(r) => v(r),
            MethodResult::NodeAttach(r) => v(r),
            MethodResult::NodeDetach(r) => v(r),
            MethodResult::NodePrompt(r) | MethodResult::NodeSteer(r) => v(r),
            MethodResult::NodeCancel(r) => v(r),
            MethodResult::NodeKill(r) => v(r),
            MethodResult::NodeRename(r) => v(r),
            MethodResult::PermissionReply(r) | MethodResult::ElicitationReply(r) => v(r),
            MethodResult::PolicySet(p) => match *p {},
            MethodResult::AgentSpawn(r) => v(r),
            MethodResult::DoctorRun(r) => v(r),
            MethodResult::SessionQuit(r) => v(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::params::*;
    use crate::result::*;
    use marion_core::contract::{AgentId, ExitStatus};
    use marion_core::encoding::{Duration, Millis};
    use marion_core::harness::Harness;
    use marion_core::node::{BlockReason, NodeState, ReapState};

    fn agent(s: &str) -> AgentId {
        AgentId(s.to_string())
    }

    fn node() -> NodeSummary {
        NodeSummary {
            agent_id: agent("a"),
            parent_id: Some(agent("root")),
            name: None,
            agent_type: "codex-impl".into(),
            harness: Harness::Codex,
            depth: 1,
            state: NodeState::Idle,
            reap_state: ReapState::Live,
            timeout: Duration::from_secs(900),
        }
    }

    /// Every method with a representative call and result. The single place a new method has to be
    /// added, which is what makes the count test below meaningful.
    fn every_method() -> Vec<(Call, MethodResult)> {
        vec![
            (
                Call::TreeSubscribe(TreeSubscribeParams {}),
                MethodResult::TreeSubscribe(TreeSubscribeResult {
                    nodes: vec![node()],
                    read_point: ReplayPoint {
                        records: 4,
                        src_seq: None,
                    },
                }),
            ),
            (
                Call::NodeGet(NodeGetParams {
                    agent_id: agent("a"),
                }),
                MethodResult::NodeGet(NodeGetResult { node: node() }),
            ),
            (
                Call::NodeAttach(NodeAttachParams {
                    agent_id: agent("a"),
                }),
                MethodResult::NodeAttach(NodeAttachResult {
                    node: node(),
                    mode: AttachMode::ResubscribeFrom(ReplayPoint {
                        records: 9,
                        src_seq: Some(marion_core::ir::SrcSeq::Ordinal(3)),
                    }),
                }),
            ),
            (
                Call::NodeDetach(NodeDetachParams {
                    agent_id: agent("a"),
                }),
                MethodResult::NodeDetach(NodeDetachResult {
                    state: NodeState::Running,
                    reap_state: ReapState::Live,
                }),
            ),
            (
                Call::NodePrompt(NodePromptParams {
                    agent_id: agent("a"),
                    text: "go".into(),
                }),
                MethodResult::NodePrompt(DeliveryResult {
                    delivered_as: Delivery::Prompt,
                    state: NodeState::Running,
                    resumed: false,
                }),
            ),
            (
                Call::NodeSteer(NodeSteerParams {
                    agent_id: agent("a"),
                    text: "stop".into(),
                }),
                MethodResult::NodeSteer(DeliveryResult {
                    delivered_as: Delivery::Steer,
                    state: NodeState::Running,
                    resumed: false,
                }),
            ),
            (
                Call::NodeCancel(NodeCancelParams {
                    agent_id: agent("a"),
                }),
                MethodResult::NodeCancel(NodeCancelResult {
                    state: NodeState::Exited(ExitStatus::Cancelled),
                }),
            ),
            (
                Call::NodeKill(NodeKillParams {
                    agent_id: agent("a"),
                }),
                MethodResult::NodeKill(NodeKillResult {
                    state: NodeState::Exited(ExitStatus::Cancelled),
                }),
            ),
            (
                Call::NodeRename(NodeRenameParams {
                    agent_id: agent("a"),
                    name: "impl".into(),
                }),
                MethodResult::NodeRename(NodeRenameResult {
                    previous_name: Some("old".into()),
                    name: "impl".into(),
                }),
            ),
            (
                Call::PermissionReply(PermissionReplyParams {
                    request_id: PermissionRequestId("r-1".into()),
                    decision: PermissionDecision::Allow,
                }),
                MethodResult::PermissionReply(ReplyResult {
                    outcome: ReplyOutcome::Delivered {
                        state: NodeState::Running,
                    },
                }),
            ),
            (
                Call::ElicitationReply(ElicitationReplyParams {
                    request_id: ElicitationRequestId("e-1".into()),
                    response: ElicitationResponse::Provided(serde_json::json!({"b": "main"})),
                }),
                MethodResult::ElicitationReply(ReplyResult {
                    outcome: ReplyOutcome::Stale {
                        reason: "the node exited before the answer arrived".into(),
                    },
                }),
            ),
            (
                // The **child** shape, deliberately, and not the root's. This is the only place a
                // `Call` travels through a whole `Frame` and back, so the case worth spending it on
                // is the one carrying the most structure — a `SpawnCaller` nested inside an
                // `Option` inside the params.
                Call::AgentSpawn(AgentSpawnParams {
                    no_change_record: None,
                    agent_type: "codex-impl".into(),
                    prompt: "implement §6.3".into(),
                    caller: Some(crate::params::SpawnCaller {
                        agent_id: agent("parent"),
                        node_token: "tok-9f2c".into(),
                    }),
                    // Absent, and that is the shape: a caller does not state its repository, the
                    // supervisor knows it. See `AgentSpawnParams::repo`.
                    repo: None,
                    acceptance_criteria: vec!["the suite is green".into()],
                    writable_scope: vec!["src/**".into()],
                    timeout_secs: Some(900),
                    model: Some("sonnet".into()),
                }),
                MethodResult::AgentSpawn(AgentSpawnResult {
                    agent_id: agent("a"),
                    state: NodeState::Spawning,
                }),
            ),
            (
                Call::DoctorRun(DoctorRunParams {
                    mode: ProbeMode::Adapter,
                    harness: Some(Harness::ClaudeCode),
                }),
                MethodResult::DoctorRun(DoctorRunResult {
                    reports: vec![HarnessReport {
                        harness: Harness::ClaudeCode,
                        harness_version: Some("2.1.220".into()),
                        adapter_check: Some(true),
                        notes: vec![],
                        elapsed: Millis(std::time::Duration::from_millis(412)),
                    }],
                }),
            ),
            (
                Call::SessionQuit(SessionQuitParams {
                    disposition: QuitDisposition::DEFAULT,
                }),
                MethodResult::SessionQuit(SessionQuitResult {
                    outcome: QuitOutcome::ReapedAndDetached {
                        reaped: vec![agent("a")],
                        detached: vec![agent("b")],
                        gate_exposed: vec![agent("b")],
                        guidance: DetachGuidance {
                            reattach: "run `marion` in this project root".into(),
                            stop_fleet: "run `marion kill --all`".into(),
                        },
                        supervisor: SupervisorDisposition::Resident(ResidentReason::BlockedNode),
                    },
                }),
            ),
        ]
    }

    #[test]
    fn the_method_list_is_fifteen() {
        assert_eq!(
            Method::ALL.len(),
            15,
            "§2 lines 87-89 list fifteen; §10's 'fourteen' is stale"
        );
        let mut names: Vec<&str> = Method::ALL.iter().map(|m| m.as_str()).collect();
        assert_eq!(
            names,
            [
                "tree/subscribe",
                "node/get",
                "node/attach",
                "node/detach",
                "node/prompt",
                "node/steer",
                "node/cancel",
                "node/kill",
                "node/rename",
                "permission/reply",
                "elicitation/reply",
                "policy/set",
                "agent/spawn",
                "doctor/run",
                "session/quit",
            ],
            "the list, in §2's order"
        );
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "two methods share a wire name");
    }

    #[test]
    fn every_method_has_a_representative_call_and_result() {
        // `policy/set` is the one method with no constructible call — that is its specification,
        // not a gap in this fixture.
        assert_eq!(
            every_method().len(),
            Method::ALL.len() - 1,
            "a method was added without a call/result fixture"
        );
    }

    #[test]
    fn a_call_agrees_with_its_method_name_in_both_directions() {
        for (call, _) in every_method() {
            let m = call.method();
            let wire = serde_json::to_value(&call).unwrap();
            assert_eq!(
                wire["method"].as_str().unwrap(),
                m.as_str(),
                "Call's serde rename and Method::as_str disagree for {m:?}"
            );
            assert_eq!(Method::from_wire(m.as_str()), Some(m));
        }
        assert_eq!(Method::from_wire("node/frobnicate"), None);
    }

    #[test]
    fn calls_round_trip() {
        for (call, _) in every_method() {
            let s = serde_json::to_string(&call).unwrap();
            let back: Call = serde_json::from_str(&s).unwrap();
            assert_eq!(back, call, "round trip failed: {s}");
        }
    }

    #[test]
    fn results_round_trip_through_the_untyped_body() {
        // This is the `as_wire`/`from_wire` shape that found a real defect in `6803b5b`: the body
        // leaves the type system and must come back identical.
        for (_, res) in every_method() {
            let m = res.method();
            let body = res.to_body();
            let back = m.decode_result(&body).unwrap();
            assert_eq!(back, res, "{} did not round trip", m.as_str());
            assert_eq!(back.method(), m);
        }
    }

    #[test]
    fn a_call_pins_its_wire_shape() {
        // JSON-RPC's `{"method":…,"params":…}`, not serde's default external tagging. A stray
        // `#[serde(untagged)]` or a missing rename would change this line and nothing else.
        assert_eq!(
            serde_json::to_string(&Call::NodeSteer(NodeSteerParams {
                agent_id: agent("a"),
                text: "stop".into()
            }))
            .unwrap(),
            r#"{"method":"node/steer","params":{"agent_id":"a","text":"stop"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Call::TreeSubscribe(TreeSubscribeParams {})).unwrap(),
            r#"{"method":"tree/subscribe","params":{}}"#
        );
    }

    #[test]
    fn policy_set_is_a_name_the_protocol_knows_and_a_call_it_cannot_make() {
        assert!(Method::ALL.contains(&Method::PolicySet));
        assert_eq!(Method::from_wire("policy/set"), Some(Method::PolicySet));

        let e = serde_json::from_str::<Call>(r#"{"method":"policy/set","params":{"deny":"all"}}"#)
            .unwrap_err();
        assert!(
            e.to_string().contains("policy/set names no policy"),
            "the refusal must reach the caller as a sentence: {e}"
        );

        let e = Method::PolicySet
            .decode_result(&serde_json::json!({}))
            .unwrap_err();
        assert_eq!(e.kind(), Some(crate::FailureKind::Unimplemented));
        assert!(e.message.contains("accept-and-ignore"));
    }

    #[test]
    fn only_session_quit_can_end_the_supervisor() {
        let enders: Vec<Method> = Method::ALL
            .into_iter()
            .filter(|m| m.can_end_the_supervisor())
            .collect();
        assert_eq!(enders, vec![Method::SessionQuit], "§2");
    }

    #[test]
    fn a_result_body_for_the_wrong_method_is_refused_with_the_method_named() {
        // The client's pending-request map is what supplies the method; if it supplies the wrong
        // one, the error has to say which one failed or a client with fifteen in flight cannot act
        // on it.
        let body = MethodResult::NodeGet(NodeGetResult { node: node() }).to_body();
        let e = Method::DoctorRun.decode_result(&body).unwrap_err();
        assert!(
            e.message.starts_with("doctor/run result did not parse"),
            "{e}"
        );
    }

    #[test]
    fn a_steer_result_is_not_silently_accepted_as_a_prompt_result() {
        // Both carry `DeliveryResult`, so only the enum variant distinguishes them. This asserts
        // the variant survives the round trip rather than collapsing.
        let steer = MethodResult::NodeSteer(DeliveryResult {
            delivered_as: Delivery::Steer,
            state: NodeState::Blocked(BlockReason::Permission),
            resumed: false,
        });
        let back = Method::NodePrompt.decode_result(&steer.to_body()).unwrap();
        assert_ne!(back, steer, "the method, not the payload, is the identity");
        assert_eq!(back.method(), Method::NodePrompt);
    }
}
