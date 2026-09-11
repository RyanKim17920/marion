//! Client-side invariants shared by rendered attach and transparent native relay.

use marion_core::contract::AgentId;
use marion_core::proto::{Event, OpaquePaneBytesV1, PaneFrameKindV1};

pub(crate) enum PaneV1Action {
    Output(OpaquePaneBytesV1),
    Resize { cols: u16, rows: u16 },
    End,
    Ignore,
}

pub(crate) struct DecodedPaneV1 {
    pub(crate) next_seq: u64,
    pub(crate) action: PaneV1Action,
}

/// Whether an event carries display-plane bytes for `id`, on either pane protocol.
///
/// Both clients need this before their attach response has named a protocol, which is exactly
/// when they cannot yet decode a frame: the question is only whose display plane it belongs to.
pub(crate) fn pane_event_targets(id: &AgentId, event: &Event) -> bool {
    match event {
        Event::NodePaneFrame(frame) => frame.agent_id == *id,
        Event::NodePty { agent_id, .. } => agent_id == id,
        _ => false,
    }
}

/// Validate and decode one event from an already-negotiated pane-v1 stream.
pub(crate) fn decode_pane_v1_event(
    id: &AgentId,
    next_seq: u64,
    cut: u64,
    event: Event,
) -> Result<DecodedPaneV1, String> {
    match event {
        Event::NodePaneFrame(frame) if frame.agent_id == *id => {
            if frame.seq != next_seq {
                return Err(format!(
                    "node `{}`'s pane stream is not dense: expected sequence {next_seq}, received {}",
                    id.0, frame.seq
                ));
            }
            let following = next_seq
                .checked_add(1)
                .ok_or_else(|| format!("node `{}` exhausted its pane sequence", id.0))?;
            let action = match frame.frame {
                PaneFrameKindV1::Output { bytes } => PaneV1Action::Output(bytes),
                PaneFrameKindV1::Resize { cols, rows } => PaneV1Action::Resize { cols, rows },
                PaneFrameKindV1::End {} => {
                    if following < cut {
                        return Err(format!(
                            "node `{}` ended its pane stream at sequence {} before the advertised replay cut {cut}",
                            id.0, frame.seq
                        ));
                    }
                    PaneV1Action::End
                }
            };
            Ok(DecodedPaneV1 {
                next_seq: following,
                action,
            })
        }
        Event::NodePty { agent_id, .. } if agent_id == *id => Err(format!(
            "the supervisor mixed legacy node/pty into node `{}`'s negotiated pane-v1 stream",
            id.0
        )),
        // Journal state may precede the retained tail; only End terminates display bytes.
        Event::NodeState { agent_id, .. } if agent_id == *id => Ok(DecodedPaneV1 {
            next_seq,
            action: PaneV1Action::Ignore,
        }),
        _ => Ok(DecodedPaneV1 {
            next_seq,
            action: PaneV1Action::Ignore,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::proto::PaneFrameV1;

    fn pane_frame(agent: &str, seq: u64, frame: PaneFrameKindV1) -> Event {
        Event::NodePaneFrame(PaneFrameV1::new(AgentId(agent.into()), seq, frame))
    }

    #[test]
    fn raw_decoder_preserves_binary_output_and_advances_exactly_once() {
        let id = AgentId("root".into());
        let raw = [0x00, 0x80, 0xff, b'\n'];
        let decoded = decode_pane_v1_event(
            &id,
            7,
            8,
            pane_frame(
                "root",
                7,
                PaneFrameKindV1::Output {
                    bytes: OpaquePaneBytesV1::new(raw),
                },
            ),
        )
        .unwrap();

        assert_eq!(decoded.next_seq, 8);
        let PaneV1Action::Output(bytes) = decoded.action else {
            panic!("output frame decoded as another action");
        };
        assert_eq!(bytes.as_bytes(), raw);
    }

    #[test]
    fn raw_decoder_enforces_dense_cut_and_stream_identity() {
        let id = AgentId("root".into());
        let gap = decode_pane_v1_event(&id, 1, 0, pane_frame("root", 2, PaneFrameKindV1::End {}))
            .err()
            .expect("sequence gap must be rejected");
        assert!(gap.contains("not dense"), "{gap}");

        let early_end =
            decode_pane_v1_event(&id, 1, 4, pane_frame("root", 1, PaneFrameKindV1::End {}))
                .err()
                .expect("End before the replay cut must be rejected");
        assert!(
            early_end.contains("before the advertised replay cut 4"),
            "{early_end}"
        );

        let legacy = decode_pane_v1_event(
            &id,
            1,
            0,
            Event::NodePty {
                agent_id: id.clone(),
                seq: 0,
                mono_ns: 0,
                bytes: "legacy".into(),
            },
        )
        .err()
        .expect("legacy PTY must be rejected after pane-v1 negotiation");
        assert!(legacy.contains("mixed legacy"), "{legacy}");

        let foreign = decode_pane_v1_event(
            &id,
            1,
            0,
            pane_frame("sibling", 99, PaneFrameKindV1::End {}),
        )
        .unwrap();
        assert_eq!(foreign.next_seq, 1);
        assert!(matches!(foreign.action, PaneV1Action::Ignore));
    }
}
