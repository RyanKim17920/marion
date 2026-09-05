//! The fifteen methods' results.
//!
//! Tolerant of unknown fields, unlike [`crate::params`] — the crate doc argues why the asymmetry is
//! deliberate rather than an oversight. The short form: an ignored parameter changes what runs, an
//! ignored result field only narrows what is shown, and a client that refuses to parse a supervisor
//! one version newer than itself has turned an additive change into an outage.
//!
//! A result is a struct even where it holds one field. `node/get` returning a bare `NodeSummary`
//! would be shorter and would make the first added field a wire break for every client.

use marion_core::contract::{AgentId, TaskId};
use marion_core::node::{NodeState, ReapState};
use serde::{Deserialize, Serialize};

use crate::PaneReadyTokenV1;
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
///
/// `pane` is the **display plane's** half of the same attach, and it is `Option` because most
/// nodes have none: §3.4 implements `DisplayPlane` iff `display == NativePty`, and a headless node
/// attached to over this method is answered with a replay and nothing else. `None` is therefore a
/// fact about the node, not a failure of the attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAttachResult {
    pub node: NodeSummary,
    pub mode: AttachMode,
    /// `#[serde(default)]` for the reason the module doc gives for every result field: a client one
    /// version older must keep parsing, and a client that does not know about panes reading `None`
    /// is exactly right — it was never going to render one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneAttach>,
}

/// What a client attaching to a node **with** a display plane got.
///
/// # Why the write half is answered here and nowhere else
///
/// §5.3 gives one node one writer: two clients typing into one pty interleave at whatever
/// granularity their reads happen to have, and the pty echoes the mess back to both operators
/// identically, so neither can tell it from a harness misbehaving. The supervisor therefore leases
/// the write half, and **`node/attach`'s response is the one place a client can be told whether it
/// got it** — the inbound keystroke channel is a notification with no answer, so a refusal
/// delivered there would be a refusal nobody is listening for, repeated once per key held down.
///
/// `held_by` names the connection that has it rather than reporting a bare "busy", because §5.3's
/// refusal is required to be a sentence: a client told only that it may not type cannot tell a
/// colleague in the same node from a lease its own crashed predecessor never released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReadyDescriptorV1 {
    pub token: PaneReadyTokenV1,
    pub cut: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAttach {
    /// The pty's size **now**, as the supervisor set it before the child existed. A client uses it
    /// to decide whether its first act is a `node/resize`, and a client that renders without
    /// asking is rendering the geometry some earlier attacher chose.
    pub cols: u16,
    pub rows: u16,
    /// Whether this client may send `node/pty-write` and `node/resize` for this node.
    pub writable: bool,
    /// The connection holding the write half, when it is not this one. `None` when `writable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_ready: Option<PaneReadyDescriptorV1>,
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
    /// **The name of the file this run's contract will be written to**, for a child; `None` for a
    /// root, which has none (§9).
    ///
    /// A field and not a sixteenth method, and not something the caller reconstructs. §11 item 28
    /// step 5 makes the agent-facing synchronous `spawn` a *client-side composition* — this call,
    /// then `node/attach`, then read `agents/<agent_id>/contracts/<task_id>.json` — because a call
    /// that blocked until the contract existed would put a minutes-long request on this wire. The
    /// composing client therefore has to know which file to read, and only the supervisor can say:
    /// the id is minted inside `agent/spawn` from marion's own entropy so that two runs can never
    /// share a contract file, so a caller that "worked it out" would be minting a second one and
    /// reading a path nothing writes.
    ///
    /// `Option`, and the two values are the two node kinds rather than a presence flag: §9 gives a
    /// root no `TaskContract`, so `None` here is a fact about the node and not an omission.
    #[serde(default)]
    pub task_id: Option<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorRunResult {
    pub reports: Vec<HarnessReport>,
}

/// `node/resume` — the node relaunched into its **own** id (`plan-restart-resume.md` step 6).
///
/// `agent_id` is the id the caller asked for, echoed back so a client watching over
/// `tree/subscribe` knows the same node it lost is the one that came back — a resume that minted a
/// new id would be a spawn, and the whole point is that it is not. `spawn_generation` is the
/// lifetime count from replay: `2` on the first resume, more on later ones, and the field a caller
/// reads to tell "relaunched" from "was never lost". `state` is typically `Spawning`, the same
/// instant `agent/spawn` returns at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeResumeResult {
    pub agent_id: AgentId,
    pub state: NodeState,
    pub spawn_generation: u32,
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
            state: NodeState::Spawning,
            task_id: None,
        });
        rt!(AgentSpawnResult {
            agent_id: AgentId("a".into()),
            state: NodeState::Spawning,
            task_id: Some(TaskId("task-1".into())),
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

    /// **A spawn answered without a `task_id` is a spawn with no contract, never a parse failure.**
    ///
    /// Two callers land here and they must not be told apart by whether the frame deserializes: a
    /// root spawn, which has no `TaskContract` at all (§9), and a supervisor built before the field
    /// existed. Both mean "there is no contract file for you to read", which is exactly what the
    /// composing client (§11 item 28 step 5) has to branch on — so the absence is a value and the
    /// `default` is what makes it one.
    #[test]
    fn a_spawn_result_without_a_task_id_is_a_node_with_no_contract() {
        let r: AgentSpawnResult =
            serde_json::from_str(r#"{"agent_id":"a","state":"Spawning"}"#).unwrap();
        assert_eq!(r.task_id, None);
    }

    #[test]
    fn pane_attach_without_readiness_keeps_the_exact_legacy_wire_shape() {
        let legacy = PaneAttach {
            cols: 80,
            rows: 24,
            writable: true,
            held_by: None,
            pane_ready: None,
        };
        let json = r#"{"cols":80,"rows":24,"writable":true}"#;

        assert_eq!(serde_json::to_string(&legacy).unwrap(), json);
        assert_eq!(serde_json::from_str::<PaneAttach>(json).unwrap(), legacy);
    }

    #[test]
    fn pane_attach_readiness_round_trips_cut_boundaries() {
        for cut in [0, u64::MAX] {
            let pane = PaneAttach {
                cols: 80,
                rows: 24,
                writable: true,
                held_by: None,
                pane_ready: Some(PaneReadyDescriptorV1 {
                    token: PaneReadyTokenV1::new([
                        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
                        0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
                        0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
                    ]),
                    cut,
                }),
            };

            let json = serde_json::to_string(&pane).unwrap();
            let decoded = serde_json::from_str::<PaneAttach>(&json).unwrap();
            assert_eq!(decoded, pane);
            assert_eq!(decoded.pane_ready.unwrap().cut, cut);
        }
    }

    #[test]
    fn pane_attach_readiness_requires_both_token_and_cut() {
        for invalid in [
            r#"{"cols":80,"rows":24,"writable":true,"pane_ready":{"cut":0}}"#,
            r#"{"cols":80,"rows":24,"writable":true,"pane_ready":{"token":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="}}"#,
        ] {
            assert!(
                serde_json::from_str::<PaneAttach>(invalid).is_err(),
                "accepted incomplete pane readiness descriptor: {invalid}"
            );
        }
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
