//! Supervisor→client notifications: the other half of `tree/subscribe` and `node/attach`.
//!
//! A JSON-RPC notification is a request with no `id` and therefore no response. That is the right
//! shape here for a reason beyond convenience: **the supervisor must not be able to block on a
//! client.** §5.7 is explicit that the supervisor's lifetime is not the client's and that it keeps
//! holding every channel with zero clients attached; a supervisor awaiting an acknowledgement from
//! a TUI that has been SIGKILLed would make §7.3.1's invariant — *"a crashed client MUST leave
//! every node exactly as it was"* — depend on the dead client answering.
//!
//! Six events, and the set is deliberately small. Each one exists because a specific client
//! behaviour is impossible without it, named in its doc; anything a client can derive from the
//! journal it already replayed is not here.
//!
//! Notification method names live in a **disjoint namespace** from [`crate::Method`] — asserted by
//! test. An overlap would make a frame's classification depend on whether it carried an `id`, and
//! a truncated line is exactly where that goes wrong.

use marion_core::contract::AgentId;
use marion_core::encoding::SystemTime;
use marion_core::ir::{Provenance, SrcSeq};
use marion_core::node::{NodeState, ReapState};
use serde::{Deserialize, Serialize};

use crate::model::{ElicitationRequestId, NodeSummary, PermissionRequestId, ResidentReason};

/// Adjacently tagged, exactly as [`crate::Call`] is, so a notification and a request are the same
/// shape minus the `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Event {
    /// A node appeared. §6.1 step 7 journals the spawn intent *before* the process exists, so a
    /// client that learned of nodes only from state changes would show a tree that is missing
    /// exactly the nodes currently being created — the ones an operator is most likely watching.
    #[serde(rename = "tree/node-added")]
    NodeAdded { node: NodeSummary, ts: SystemTime },

    /// §3.2's `state` moved. Carries `reap_state` alongside it because §7.6's gating rule is the
    /// *disjunction* of the two (`Exited(_)` **or** `reap_state ∈ {Orphaned, ReapedIdle}`), and a
    /// client that received them in separate messages could render a node as live between them.
    #[serde(rename = "node/state")]
    NodeState {
        agent_id: AgentId,
        state: NodeState,
        reap_state: ReapState,
        ts: SystemTime,
    },

    /// One event from a node's stream, for a client attached to it.
    ///
    /// `src_seq` is carried per event and not per batch: it is §4.2's loss evidence and §7.3.3's
    /// replay-to-subscribe seam, and a client stitching a replayed tail to a live stream compares
    /// exactly this field against the [`crate::ReplayPoint`] its attach returned. `Provenance`
    /// rides along for the reason §4.1 gives — *"what the UI keys on before claiming anything about
    /// loss"*.
    ///
    /// `payload` is opaque. marion is the courier for a harness's own event body; §4's full `Event`
    /// with its typed `Payload` lands with `events.jsonl`, and inventing a normalization here would
    /// mean this crate deciding a question §4 has already reserved.
    #[serde(rename = "node/event")]
    NodeEvent {
        agent_id: AgentId,
        ts: SystemTime,
        provenance: Provenance,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src_seq: Option<SrcSeq>,
        payload: serde_json::Value,
    },

    /// A node is asking for permission. §5.6's single queue across every harness that has a
    /// permission channel.
    ///
    /// **This is the notification §11 item 22 is about.** Today marion has no route from an arriving
    /// request to a person: it sleeps the node's `Blocked` bound and denies (§9). This event and
    /// `permission/reply` are that route; until both are wired, an operator sees nothing and the
    /// far side sees an `is_error: true` `tool_result` it routes around (S9).
    #[serde(rename = "permission/request")]
    PermissionRequest {
        request_id: PermissionRequestId,
        agent_id: AgentId,
        tool_name: String,
        /// The tool input, as the harness sent it. Opaque for the same reason as `payload` above.
        input: serde_json::Value,
        ts: SystemTime,
    },

    /// A structured input request (§2: ACP and Codex have these and they are **not** permissions).
    /// `request` is the harness's own schema — see [`crate::ElicitationResponse`] for why marion
    /// does not type it.
    #[serde(rename = "elicitation/request")]
    ElicitationRequest {
        request_id: ElicitationRequestId,
        agent_id: AgentId,
        request: serde_json::Value,
        ts: SystemTime,
    },

    /// §5.7's exit record, sent as it is journaled.
    ///
    /// *"Strictly it is not needed for correctness… but the record is what lets a later `marion`
    /// distinguish 'it finished its work and left' from 'it died'."* The same distinction is worth
    /// exactly as much to a live client: without this event, a supervisor that exited cleanly after
    /// its grace period and a supervisor that crashed are one closed socket.
    ///
    /// `held_by` is `None` on every exit, since §5.7 forbids exiting while any of its four
    /// conditions holds — it exists so a supervisor that finds itself unable to leave can say so
    /// with the same vocabulary, rather than a client inferring residency from silence.
    #[serde(rename = "supervisor/exiting")]
    SupervisorExiting {
        ts: SystemTime,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        held_by: Option<ResidentReason>,
    },
}

impl Event {
    /// The wire spelling, for the same reason [`crate::Method::as_str`] exists: one table.
    pub const fn method(&self) -> &'static str {
        match self {
            Event::NodeAdded { .. } => "tree/node-added",
            Event::NodeState { .. } => "node/state",
            Event::NodeEvent { .. } => "node/event",
            Event::PermissionRequest { .. } => "permission/request",
            Event::ElicitationRequest { .. } => "elicitation/request",
            Event::SupervisorExiting { .. } => "supervisor/exiting",
        }
    }

    /// Every notification method name. Used by the frame reader to reject an unknown one by name
    /// rather than as an anonymous parse failure.
    pub const METHODS: [&'static str; 6] = [
        "tree/node-added",
        "node/state",
        "node/event",
        "permission/request",
        "elicitation/request",
        "supervisor/exiting",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Method;
    use marion_core::contract::ExitStatus;
    use marion_core::encoding::Duration;
    use marion_core::harness::Harness;
    use marion_core::ir::{Completeness, EventId, Source, Transformation};

    fn ts() -> SystemTime {
        SystemTime::from_unix_millis(1_785_625_628_619)
    }

    fn every_event() -> Vec<Event> {
        vec![
            Event::NodeAdded {
                node: NodeSummary {
                    agent_id: AgentId("a".into()),
                    parent_id: None,
                    name: None,
                    agent_type: "codex-impl".into(),
                    harness: Harness::Codex,
                    depth: 0,
                    state: NodeState::Spawning,
                    reap_state: ReapState::Live,
                    timeout: Duration::from_secs(900),
                },
                ts: ts(),
            },
            Event::NodeState {
                agent_id: AgentId("a".into()),
                state: NodeState::Exited(ExitStatus::Ok),
                reap_state: ReapState::Live,
                ts: ts(),
            },
            Event::NodeEvent {
                agent_id: AgentId("a".into()),
                ts: ts(),
                provenance: Provenance {
                    source: Source::Transcript,
                    source_id: Some("uuid-1".into()),
                    observed_live: true,
                    authoritative: true,
                    completeness: Completeness::Complete,
                    transformation: Transformation::Native,
                },
                src_seq: Some(SrcSeq::Predecessor(EventId("uuid-0".into()))),
                payload: serde_json::json!({"type": "assistant"}),
            },
            Event::PermissionRequest {
                request_id: PermissionRequestId("r-1".into()),
                agent_id: AgentId("a".into()),
                tool_name: "mcp__marion__report".into(),
                input: serde_json::json!({"narrative": "done"}),
                ts: ts(),
            },
            Event::ElicitationRequest {
                request_id: ElicitationRequestId("e-1".into()),
                agent_id: AgentId("a".into()),
                request: serde_json::json!({"prompt": "which branch?"}),
                ts: ts(),
            },
            Event::SupervisorExiting {
                ts: ts(),
                held_by: None,
            },
        ]
    }

    #[test]
    fn events_round_trip() {
        for e in every_event() {
            let s = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e, "{s}");
        }
    }

    #[test]
    fn every_event_is_covered_and_names_itself() {
        assert_eq!(every_event().len(), Event::METHODS.len());
        for e in every_event() {
            assert!(Event::METHODS.contains(&e.method()), "{:?}", e.method());
            let wire = serde_json::to_value(&e).unwrap();
            assert_eq!(wire["method"].as_str().unwrap(), e.method());
        }
    }

    #[test]
    fn notification_names_never_collide_with_request_methods() {
        // If they did, a frame's meaning would depend on whether an `id` key survived, which is
        // precisely what a truncated or hand-written line gets wrong.
        for n in Event::METHODS {
            assert_eq!(Method::from_wire(n), None, "{n} is also a request method");
        }
    }

    #[test]
    fn events_pin_their_wire_shapes() {
        assert_eq!(
            serde_json::to_string(&Event::NodeState {
                agent_id: AgentId("a".into()),
                state: NodeState::Blocked(marion_core::node::BlockReason::Descendants),
                reap_state: ReapState::ReapedIdle,
                ts: ts(),
            })
            .unwrap(),
            r#"{"method":"node/state","params":{"agent_id":"a","state":{"Blocked":"Descendants"},"reap_state":"ReapedIdle","ts":"2026-08-01T23:07:08.619Z"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Event::SupervisorExiting {
                ts: ts(),
                held_by: Some(ResidentReason::SpawnOutstanding),
            })
            .unwrap(),
            r#"{"method":"supervisor/exiting","params":{"ts":"2026-08-01T23:07:08.619Z","held_by":"SpawnOutstanding"}}"#
        );
    }

    #[test]
    fn a_node_event_without_ordering_evidence_omits_the_field() {
        // §4.2 again, on the live leg: Codex app-server and Claude Code headless supply none, and
        // an `Ordinal(0)` here would tell a client it can detect loss when it cannot.
        let e = Event::NodeEvent {
            agent_id: AgentId("a".into()),
            ts: ts(),
            provenance: Provenance {
                source: Source::Protocol,
                source_id: None,
                observed_live: true,
                authoritative: false,
                completeness: Completeness::Unknown,
                transformation: Transformation::Normalized,
            },
            src_seq: None,
            payload: serde_json::Value::Null,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("src_seq"), "{s}");
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
    }
}
