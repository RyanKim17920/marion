//! The fifteen methods' results.
//!
//! Tolerant of unknown fields, unlike [`crate::params`] — the crate doc argues why the asymmetry is
//! deliberate rather than an oversight. The short form: an ignored parameter changes what runs, an
//! ignored result field only narrows what is shown, and a client that refuses to parse a supervisor
//! one version newer than itself has turned an additive change into an outage.
//!
//! A result is a struct even where it holds one field. `node/get` returning a bare `NodeSummary`
//! would be shorter and would make the first added field a wire break for every client.

use marion_core::contract::AgentId;
use marion_core::node::{NodeState, ReapState};
use serde::{Deserialize, Serialize};

use crate::model::{
    AttachMode, Delivery, HarnessReport, NodeSummary, QuitOutcome, ReplayPoint, ReplyOutcome,
};

/// `tree/subscribe` — the snapshot, and the point live notifications begin from.
///
/// The snapshot and the read point travel together because a snapshot without one has the same
/// seam problem §7.3.3 identifies for re-attach: a client that renders a tree and *then* starts
/// listening has a window it cannot account for. Here the answer is the same as there — the
/// snapshot is as of `read_point`, and everything after arrives as a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSubscribeResult {
    pub nodes: Vec<NodeSummary>,
    pub read_point: ReplayPoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeGetResult {
    pub node: NodeSummary,
}

/// `node/attach`. The mode is §7.3.3's per-node answer; see [`AttachMode`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAttachResult {
    pub node: NodeSummary,
    pub mode: AttachMode,
}

/// `node/detach` — returns the node's state, which is the *evidence* that detaching did nothing.
///
/// An empty acknowledgement was the alternative and is weaker. §6.2 and §7.3.1 both rest on
/// detaching being inert; a result that hands back the unchanged state lets a client assert it,
/// and lets an operator reading a log see that a detach and a reap are different events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDetachResult {
    pub state: NodeState,
    pub reap_state: ReapState,
}

/// `node/prompt` and `node/steer` share this result — and only this result. The params are
/// separate types (see [`crate::params::NodeSteerParams`]) because the two calls are refused under
/// different conditions; the *answers* are genuinely the same question: which verb was performed,
/// and what state did the node move to.
///
/// `delivered_as` is present rather than implied because §6.3's resume path makes it
/// non-obvious: a `node/prompt` against an `Exited(_)` node performed `continue_()` **then**
/// `prompt()` as one atomic registry operation, and a client that renders "prompted" without
/// knowing a resume happened will show a fresh turn on a session that was reopened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub delivered_as: Delivery,
    pub state: NodeState,
    /// True iff the node was terminal and §6.3's atomic `continue_()` + `prompt()` ran.
    #[serde(default)]
    pub resumed: bool,
}

/// `node/cancel`. §6.7 requires a cancel to reach a process that exists; where it does, the node's
/// terminal is `Cancelled`, and that classification is the node's state here rather than a
/// separate boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCancelResult {
    pub state: NodeState,
}

/// `node/kill`. Same shape as cancel and a different type, because §6.7 gives them different
/// preconditions and §7.3.2's disposition (a) is built from this one only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeKillResult {
    pub state: NodeState,
}

/// `node/rename`.
///
/// `previous_name` is returned so a client can undo, and so a journal reader can see the rename as
/// a transition rather than a fact. Nothing here reports `allow_peers`: a bound grant did not move
/// (§2, §5.4) and an unbound one is not a property of this call — it resolves at the *peer's* next
/// call, which may never come.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRenameResult {
    #[serde(default)]
    pub previous_name: Option<String>,
    pub name: String,
}

/// `permission/reply` and `elicitation/reply`. See [`ReplyOutcome`] for why `Stale` is a variant
/// and not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyResult {
    pub outcome: ReplyOutcome,
}

/// `agent/spawn`.
///
/// Returns the id and the state, **not a `TaskContract`**. §5.4 rejects `report` on a root and
/// `wait` has no contract to return there, so a root has none to hand back — the contract belongs
/// to the parent↔child relationship, and a client-spawned root has no parent. The client watches
/// the run through `tree/subscribe` and `node/attach`, which is the same path it uses for every
/// other node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpawnResult {
    pub agent_id: AgentId,
    /// Typically `Spawning` — §6.1 step 7 journals the intent, starts the process, journals the
    /// confirmation, and this returns once the node exists in the registry.
    pub state: NodeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorRunResult {
    pub reports: Vec<HarnessReport>,
}

/// `session/quit`. See [`QuitOutcome`] — one variant per disposition, so a detach cannot be
/// reported without its guidance and a kill cannot report reaped nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionQuitResult {
    pub outcome: QuitOutcome,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DetachGuidance, ResidentReason, SupervisorDisposition};
    use marion_core::contract::ExitStatus;

    #[test]
    fn results_round_trip() {
        macro_rules! rt {
            ($v:expr) => {{
                let v = $v;
                let s = serde_json::to_string(&v).unwrap();
                assert_eq!(v, serde_json::from_str(&s).unwrap(), "round trip: {s}");
            }};
        }
        rt!(NodeDetachResult {
            state: NodeState::Running,
            reap_state: ReapState::Live
        });
        rt!(DeliveryResult {
            delivered_as: Delivery::Steer,
            state: NodeState::Running,
            resumed: false
        });
        rt!(DeliveryResult {
            delivered_as: Delivery::Prompt,
            state: NodeState::Running,
            resumed: true
        });
        rt!(NodeCancelResult {
            state: NodeState::Exited(ExitStatus::Cancelled)
        });
        rt!(NodeKillResult {
            state: NodeState::Exited(ExitStatus::Cancelled)
        });
        rt!(NodeRenameResult {
            previous_name: None,
            name: "impl".into()
        });
        rt!(ReplyResult {
            outcome: ReplyOutcome::Delivered {
                state: NodeState::Running
            }
        });
        rt!(AgentSpawnResult {
            agent_id: AgentId("a".into()),
            state: NodeState::Spawning
        });
        rt!(DoctorRunResult { reports: vec![] });
        rt!(SessionQuitResult {
            outcome: QuitOutcome::ReapedAndDetached {
                reaped: vec![AgentId("a".into())],
                detached: vec![AgentId("b".into())],
                gate_exposed: vec![AgentId("b".into())],
                guidance: DetachGuidance {
                    reattach: "run `marion` here".into(),
                    stop_fleet: "run `marion kill --all`".into()
                },
                supervisor: SupervisorDisposition::Resident(ResidentReason::BlockedNode),
            }
        });
    }

    #[test]
    fn a_result_tolerates_a_field_it_has_never_heard_of() {
        // The forward-compatibility half of the crate's unknown-field decision. A supervisor one
        // version ahead must not break a client that is one behind.
        let r: NodeCancelResult = serde_json::from_str(
            r#"{"state":"Running","signalled_at":"2026-08-05T00:00:00.000Z"}"#,
        )
        .unwrap();
        assert_eq!(r.state, NodeState::Running);
    }

    #[test]
    fn results_pin_their_wire_shapes() {
        assert_eq!(
            serde_json::to_string(&DeliveryResult {
                delivered_as: Delivery::Prompt,
                state: NodeState::Running,
                resumed: true
            })
            .unwrap(),
            r#"{"delivered_as":"Prompt","state":"Running","resumed":true}"#
        );
        assert_eq!(
            serde_json::to_string(&NodeDetachResult {
                state: NodeState::Idle,
                reap_state: ReapState::Live
            })
            .unwrap(),
            r#"{"state":"Idle","reap_state":"Live"}"#
        );
        assert_eq!(
            serde_json::to_string(&ReplyResult {
                outcome: ReplyOutcome::Delivered {
                    state: NodeState::Running
                }
            })
            .unwrap(),
            r#"{"outcome":{"Delivered":{"state":"Running"}}}"#
        );
    }

    #[test]
    fn a_resumed_prompt_says_so() {
        // §6.3's resume is invisible in the state alone: the node is Running either way. A client
        // rendering "new turn" on a session that was reopened is showing the wrong history.
        let fresh = DeliveryResult {
            delivered_as: Delivery::Prompt,
            state: NodeState::Running,
            resumed: false,
        };
        let resumed = DeliveryResult {
            resumed: true,
            ..fresh.clone()
        };
        assert_ne!(fresh, resumed);
        assert_eq!(fresh.state, resumed.state);
    }
}
