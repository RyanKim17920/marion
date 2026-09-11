//! Supervisor→client notifications: the other half of `tree/subscribe` and `node/attach`.
//!
//! A JSON-RPC notification is a request with no `id` and therefore no response. That is the right
//! shape here for a reason beyond convenience: **the supervisor must not be able to block on a
//! client.** §5.7 is explicit that the supervisor's lifetime is not the client's and that it keeps
//! holding every channel with zero clients attached; a supervisor awaiting an acknowledgement from
//! a TUI that has been SIGKILLed would make §7.3.1's invariant — *"a crashed client MUST leave
//! every node exactly as it was"* — depend on the dead client answering.
//!
//! Eight events, and the set is deliberately small. Each one exists because a specific client
//! behaviour is impossible without it, named in its doc; anything a client can derive from the
//! journal it already replayed is not here.
//!
//! **This enum is not covered by [`crate::proto::Method::ALL`]'s pin.** `method.rs` asserts a length of
//! fifteen, the exact fifteen wire strings in §2's order, and non-collision with these names — and
//! it says fifteen *requests*. Notifications are a separate enum with a separate table, so adding
//! one here is not a change to the request surface and does not move that pin. The
//! `notification_names_never_collide_with_request_methods` test below is the join between the two.
//!
//! Notification method names live in a **disjoint namespace** from [`crate::proto::Method`] — asserted by
//! test. An overlap would make a frame's classification depend on whether it carried an `id`, and
//! a truncated line is exactly where that goes wrong.
//!
//! There is now a **third** table, [`crate::proto::Input`], and it is the client→supervisor direction. The
//! seam this module's doc describes above is what made it possible without moving the fifteen-pin;
//! what it costs is that "id-less" no longer implies "outbound", so the disjointness assertion is
//! three-way and lives in `input.rs` beside the newer table.

use crate::contract::AgentId;
use crate::encoding::SystemTime;
use crate::ir::{Provenance, SrcSeq};
use crate::node::{NodeState, ReapState};
use serde::{Deserialize, Serialize};

use crate::proto::PaneFrameV1;
use crate::proto::model::{ElicitationRequestId, NodeSummary, PermissionRequestId, ResidentReason};

/// Adjacently tagged, exactly as [`crate::proto::Call`] is, so a notification and a request are the same
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
    /// **`agent_seq` is the node's own `events.jsonl` ordinal, and it is the field §7.3.3's seam is
    /// actually stated in.** [`crate::proto::ReplayPoint`] counts `records` of exactly these ordinals, so
    /// an attach that hands a client `ResubscribeFrom(records: N)` and then sends it events with no
    /// ordinal has handed it a read point it cannot check anything against. It is carried
    /// separately from `src_seq` because the two answer different questions and only one of them is
    /// usually answerable: `src_seq` is the *harness's* ordering evidence, `None` on Codex
    /// app-server and on Claude Code `headless` — which is most of what marion runs — while
    /// `agent_seq` is **marion's own**, assigned by the single writer of that one file
    /// (`marion_supervisor::events::EventWriter`), and therefore always present and always dense.
    /// A client checking for a gap or a repeat across the replay/live join checks this one.
    ///
    /// `src_seq` is carried per event and not per batch: it is §4.2's loss evidence, and where a
    /// harness does supply it, it is the only thing that can report loss *upstream of marion* —
    /// which `agent_seq` cannot, because a frame marion never saw got no ordinal from marion.
    /// `Provenance` rides along for the reason §4.1 gives — *"what the UI keys on before claiming
    /// anything about loss"*.
    ///
    /// `payload` is opaque. marion is the courier for a harness's own event body; §4's full `Event`
    /// with its typed `Payload` lands with `events.jsonl`, and inventing a normalization here would
    /// mean this crate deciding a question §4 has already reserved.
    #[serde(rename = "node/event")]
    NodeEvent {
        agent_id: AgentId,
        agent_seq: u64,
        ts: SystemTime,
        provenance: Provenance,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src_seq: Option<SrcSeq>,
        payload: serde_json::Value,
    },

    /// **Raw terminal bytes from a node's pty**, for a client attached to a node with a display
    /// plane (§5.3, §3.4).
    ///
    /// **Bytes, not grid cells.** The grid is a derived *per-viewer* object: two clients on one node
    /// may have different window sizes, different scrollback offsets and different alternate-screen
    /// state, so a supervisor that shipped cells would have to pick one viewer's answer and be
    /// wrong for the other. The emulator (`marion-term`) therefore lives on the client's side of
    /// this seam, and what crosses it is the same byte stream the harness wrote.
    ///
    /// **Why this is not a [`Self::NodeEvent`] with a `Payload::Raw`.** §3.4 originally said
    /// terminal bytes land in `events.jsonl`; they do not, and §3.4 has been corrected. Three
    /// reasons, each independent: `Payload::Raw`'s own doc defines it as *a line of stdout*, which a
    /// mid-escape-sequence pty read is not; `EventReader::open_path` reads the whole file on every
    /// attach, so folding a terminal stream in would make every attach to every node pay for
    /// megabytes of escape sequences; and `event.rs:40` states that `mono_ns` exists to **align**
    /// the two streams, which is a statement that there are two. The recording lives in `pty.cast`,
    /// and `mono_ns` here is the same monotonic origin `events.jsonl` uses for that node.
    ///
    /// **`bytes` is a JSON string, asciicast-style, not base64 or hex.** Measured: all five
    /// committed captures in `tests/fixtures/s2/` are valid UTF-8 end to end. The 23 U+FFFD across
    /// nine regions in the `.cast` files are a *capture-host* defect — `extract.py` decoded each
    /// read chunk independently — exactly as `NOTES.txt` records. So a producer **MUST** buffer an
    /// incomplete trailing UTF-8 sequence across reads, which is S11's *"a `read()` is not a
    /// frame"* MUST in a third guise; `marion_supervisor::pty::Utf8Stream` is that buffer.
    ///
    /// `seq` is this pty stream's own dense ordinal, assigned by the single reader of that master.
    /// It is **not** `agent_seq`: the two streams are separate files with separate writers, and
    /// numbering them together would imply an ordering between them that no lock provides.
    #[serde(rename = "node/pty")]
    NodePty {
        agent_id: AgentId,
        seq: u64,
        mono_ns: u64,
        bytes: String,
    },

    /// One byte-exact record from the replayable pane stream. `seq` is dense from zero.
    #[serde(rename = "node/pane-frame")]
    NodePaneFrame(PaneFrameV1),

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
    /// `request` is the harness's own schema — see [`crate::proto::ElicitationResponse`] for why marion
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
    /// The wire spelling, for the same reason [`crate::proto::Method::as_str`] exists: one table.
    pub const fn method(&self) -> &'static str {
        match self {
            Event::NodeAdded { .. } => "tree/node-added",
            Event::NodeState { .. } => "node/state",
            Event::NodeEvent { .. } => "node/event",
            Event::NodePty { .. } => "node/pty",
            Event::NodePaneFrame(_) => "node/pane-frame",
            Event::PermissionRequest { .. } => "permission/request",
            Event::ElicitationRequest { .. } => "elicitation/request",
            Event::SupervisorExiting { .. } => "supervisor/exiting",
        }
    }

    /// Every notification method name. Used by the frame reader to reject an unknown one by name
    /// rather than as an anonymous parse failure.
    pub const METHODS: [&'static str; 8] = [
        "tree/node-added",
        "node/state",
        "node/event",
        "node/pty",
        "node/pane-frame",
        "permission/request",
        "elicitation/request",
        "supervisor/exiting",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ExitStatus;
    use crate::encoding::Duration;
    use crate::harness::Harness;
    use crate::ir::{Completeness, EventId, Source, Transformation};
    use crate::proto::{Input, Method, OpaquePaneBytesV1, PaneFrameKindV1, PaneFrameV1};

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
                    harness_version: None,
                    depth: 0,
                    state: NodeState::Spawning,
                    reap_state: ReapState::Live,
                    timeout: Duration::from_secs(900),
                    pane: false,
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
                agent_seq: 7,
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
            Event::NodePty {
                agent_id: AgentId("a".into()),
                seq: 3,
                mono_ns: 1_234_567,
                // A box-drawing glyph and an ESC, because those are the two things a naive
                // encoding gets wrong: the first is multibyte, the second is a control character.
                bytes: "\u{1b}[?1049h\u{256d}".into(),
            },
            Event::NodePaneFrame(PaneFrameV1::new(
                AgentId("a".into()),
                0,
                PaneFrameKindV1::Output {
                    bytes: OpaquePaneBytesV1::new([0xff]),
                },
            )),
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
            assert!(
                !Input::METHODS.contains(&n),
                "{n} is also a client→supervisor input"
            );
        }
    }

    #[test]
    fn events_pin_their_wire_shapes() {
        assert_eq!(
            serde_json::to_string(&Event::NodeState {
                agent_id: AgentId("a".into()),
                state: NodeState::Blocked(crate::node::BlockReason::Descendants),
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

    /// The terminal stream is text on the wire, and the escape characters survive the round trip.
    ///
    /// Not base64 and not hex: `tests/fixtures/s2/NOTES.txt` records that all five committed
    /// captures are valid UTF-8, and an encoding a human cannot read in a packet dump costs a third
    /// of the bytes' size for nothing. The `\u001b` spelling is what `serde_json` produces for ESC
    /// and what the committed `.cast` files carry.
    #[test]
    fn pty_bytes_ride_as_a_json_string_with_escapes_intact() {
        let e = Event::NodePty {
            agent_id: AgentId("a".into()),
            seq: 0,
            mono_ns: 0,
            bytes: "\u{1b}[2J\u{2500}".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""bytes":"\u001b[2J"#), "{s}");
        assert!(!s.contains("base64"), "{s}");
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
        // And `seq` is dense from zero, so it is never skipped the way `src_seq` is.
        assert!(s.contains(r#""seq":0"#), "{s}");
    }

    /// **`agent_seq` is never omitted, and zero is a real ordinal.**
    ///
    /// `src_seq` is skipped when absent because absent is what §4.2 requires it to say; `agent_seq`
    /// has no absent case — every event in an `events.jsonl` was numbered by the writer that
    /// appended it — so a `skip_serializing_if` on it would make the *first* event of every node
    /// indistinguishable on the wire from an event whose ordinal marion does not know. The first
    /// event of every node is exactly the one a re-attaching client is stitching against.
    #[test]
    fn a_node_events_own_ordinal_is_always_on_the_wire_including_zero() {
        let e = Event::NodeEvent {
            agent_id: AgentId("a".into()),
            agent_seq: 0,
            ts: ts(),
            provenance: Provenance::marion(),
            src_seq: None,
            payload: serde_json::json!({"type": "system"}),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""agent_seq":0"#), "{s}");
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
    }

    #[test]
    fn a_node_event_without_ordering_evidence_omits_the_field() {
        // §4.2 again, on the live leg: Codex app-server and Claude Code headless supply none, and
        // an `Ordinal(0)` here would tell a client it can detect loss when it cannot.
        let e = Event::NodeEvent {
            agent_id: AgentId("a".into()),
            agent_seq: 0,
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
