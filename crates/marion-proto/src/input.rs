//! Client→supervisor notifications: the **inbound** half of a display plane (§5.2, §5.3).
//!
//! # Why this is not a sixteenth method
//!
//! [`crate::Method::ALL`] is pinned at fifteen and the pin exists to force this conversation, so
//! composition over the fifteen was tried first and each route is closed for a reason that is
//! written down somewhere other than here:
//!
//! * **`node/prompt` and `node/steer`** are `{agent_id, text}` and look like the answer. They are
//!   not. `params.rs` refuses to merge the two *with each other* precisely so their handlers stay
//!   distinct, because §6.3 makes the choice between them a function of the node's state; a
//!   keystroke is not a turn and has no state precondition at all. Routing bytes through either
//!   would collapse §6.3's rule into the byte path. (§3.4's derivation table does say a
//!   *degenerate* control plane's `prompt` routes to `write_keys` — that is a harness-side
//!   adaptation for a `TerminalInput` node whose only way to receive a *turn* is the keyboard, not
//!   a client channel for arbitrary bytes.)
//! * **`node/attach`** is `{agent_id}` with `deny_unknown_fields`. Widening it with `cols`/`rows`
//!   would carry the geometry at attach time and nothing after it, which is half a solution to
//!   resize and none at all to keystrokes.
//! * The remaining twelve are tree, lifecycle, permission and session verbs with no byte channel.
//!
//! What *is* available is the seam [`crate::notify`]'s own module doc names: **notifications are a
//! separate enum with a separate table, and `method.rs`'s pin is fifteen _requests_.** The envelope
//! is direction-agnostic — [`crate::Frame::from_line`] classifies an id-less frame as a
//! notification on whichever side reads it — so the inbound direction needs a table of its own and
//! nothing else. This is that table. `Method::ALL` does not move.
//!
//! # Why a notification rather than a request, on the merits
//!
//! §5.7's argument for the outbound direction is that *the supervisor must not be able to block on
//! a client*. The inbound direction has the mirror of it, and it is stronger: a keystroke has no
//! answer. There is nothing a supervisor could put in a response that an operator wants — the pty
//! echo **is** the acknowledgement, and it arrives as [`crate::Event::NodePty`] whether marion
//! answers or not. A request would put a socket round trip between a key press and the next key
//! press, so a client that awaited each one would type at the speed of the supervisor's event loop,
//! and one that did not await would have re-invented a notification with an unread `id`.
//!
//! A refusal still has to be sayable — a second attacher gets no write half (§5.3) — and it is,
//! once, at `node/attach`: that method's answer is where a client learns whether it may type. A
//! per-keystroke refusal would be one line of error per key held down.
//!
//! # Two, and why not one
//!
//! A resize is not a keystroke wearing different bytes. `write_keys` and `resize` are separate
//! operations in §5.2's `DisplayPlane` trait for a measured reason recorded in
//! `marion_supervisor::pty::PtyHost::resize`: a resize is `TIOCSWINSZ` plus a `SIGWINCH` to the
//! foreground process group and an `r` record in `pty.cast`, in that order, and none of the three
//! is a write to the master. Folding them into one message would give the supervisor a byte string
//! it had to parse to find out which of two unrelated syscalls to make.

use marion_core::contract::AgentId;
use serde::{Deserialize, Serialize};

use crate::{OpaquePaneBytesV1, PaneReadyTokenV1};

/// Maximum decoded payload accepted from one interactive terminal read.
pub const MAX_PANE_INPUT_BYTES: usize = 16 * 1024;

fn deserialize_pane_input_bytes<'de, D>(deserializer: D) -> Result<OpaquePaneBytesV1, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let bytes = OpaquePaneBytesV1::deserialize(deserializer)?;
    if bytes.as_bytes().len() > MAX_PANE_INPUT_BYTES {
        return Err(serde::de::Error::custom(format!(
            "pane input exceeds {MAX_PANE_INPUT_BYTES} decoded bytes"
        )));
    }
    Ok(bytes)
}

/// Byte-exact terminal input for a negotiated pane writer.
///
/// This is deliberately separate from legacy [`Input::NodePtyWrite`]. Structured callers keep
/// their UTF-8 text contract, while an interactive terminal may forward every byte its tty
/// produced without inventing replacement characters or buffering for a Unicode boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodePaneWriteV1 {
    pub agent_id: AgentId,
    #[serde(deserialize_with = "deserialize_pane_input_bytes")]
    pub bytes: OpaquePaneBytesV1,
}

/// Strict parameters for the version-one pane replay readiness notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodePaneReadyV1 {
    pub agent_id: AgentId,
    pub token: PaneReadyTokenV1,
    pub cut: u64,
}

/// Adjacently tagged, exactly as [`crate::Call`] and [`crate::Event`] are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Input {
    /// Keystrokes into a node's pty, from the one client holding its write half.
    ///
    /// **`bytes` is a JSON string, not base64** — the same encoding [`crate::Event::NodePty`] uses,
    /// and for the same reason: the outbound direction settled it against the committed asciicast
    /// captures, and an inbound direction that disagreed would make one node's session two
    /// encodings.
    ///
    /// The obligation that comes with a `String` is the one S11 states as *"a `read()` is not a
    /// frame"*, now on the **client's** side of the seam. A client reads its own tty in raw mode,
    /// so a multibyte keystroke or a pasted run can be split across two reads, and a producer
    /// therefore MUST buffer an incomplete trailing UTF-8 sequence rather than emit it.
    /// `marion_supervisor::pty::Utf8Stream` is the outbound buffer; a client needs its own.
    ///
    /// Bytes are written to the master **unaltered**. `marion_tui::keys` is a filter and not an
    /// encoder — marion's terminal is already raw, so what the operator typed is what the node
    /// expects — and the one thing it removes is its own `^]` prefix.
    #[serde(rename = "node/pty-write")]
    NodePtyWrite { agent_id: AgentId, bytes: String },

    /// Opaque bytes from an interactive terminal holding a pane's write lease.
    #[serde(rename = "node/pane-write")]
    NodePaneWrite(NodePaneWriteV1),

    /// The attached client's window changed size, or has just announced its size for the first
    /// time.
    ///
    /// **`cols` and `rows`, not a byte string**, matching §5.2's
    /// `resize(&self, h: &PtyHandle, cols: u16, rows: u16)`. `u16` is the width of `winsize`'s
    /// `ws_col`/`ws_row`, so the wire type is the kernel's and a value that cannot be delivered
    /// cannot be sent.
    ///
    /// Sent on `SIGWINCH` **and** once at attach. The second is not redundant: the master's size
    /// was fixed before the child existed (`PtyMaster::open`), by a supervisor that had no way to
    /// know what terminal would eventually attach, so a client that only spoke on `SIGWINCH` would
    /// render a node painted at somebody else's geometry until the operator happened to drag a
    /// window edge.
    #[serde(rename = "node/resize")]
    NodeResize {
        agent_id: AgentId,
        cols: u16,
        rows: u16,
    },

    /// The client has consumed the pane replay through `cut` and may join the live stream.
    #[serde(rename = "node/pane-ready")]
    NodePaneReady(NodePaneReadyV1),
}

impl Input {
    /// The wire spelling. One table, as [`crate::Method::as_str`] and [`crate::Event::method`].
    pub const fn method(&self) -> &'static str {
        match self {
            Input::NodePtyWrite { .. } => "node/pty-write",
            Input::NodePaneWrite(_) => "node/pane-write",
            Input::NodeResize { .. } => "node/resize",
            Input::NodePaneReady(_) => "node/pane-ready",
        }
    }

    /// The node this is aimed at. Every inbound notification names one, because there is no
    /// session-wide keyboard: §7.3.3's split is by node, and a supervisor holding several pty nodes
    /// would otherwise have to guess which pane an operator was typing into.
    pub const fn agent_id(&self) -> &AgentId {
        match self {
            Input::NodePtyWrite { agent_id, .. } | Input::NodeResize { agent_id, .. } => agent_id,
            Input::NodePaneWrite(params) => &params.agent_id,
            Input::NodePaneReady(params) => &params.agent_id,
        }
    }

    /// Every inbound notification name, for the frame reader — so an unknown one is refused **by
    /// name** rather than as an anonymous parse failure.
    pub const METHODS: [&'static str; 4] = [
        "node/pty-write",
        "node/pane-write",
        "node/resize",
        "node/pane-ready",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, Method, PaneReadyTokenV1};

    fn pane_ready_token() -> PaneReadyTokenV1 {
        PaneReadyTokenV1::new([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ])
    }

    fn every_input() -> Vec<Input> {
        vec![
            Input::NodePtyWrite {
                agent_id: AgentId("a".into()),
                // An ESC and a multibyte glyph, the two things a naive encoding gets wrong — the
                // same pair `Event::NodePty`'s fixture uses, so the two directions are exercised
                // against one hazard rather than two.
                bytes: "\u{1b}[A\u{256d}".into(),
            },
            Input::NodePaneWrite(NodePaneWriteV1 {
                agent_id: AgentId("a".into()),
                bytes: OpaquePaneBytesV1::new([0x00, 0x80, 0xff]),
            }),
            Input::NodeResize {
                agent_id: AgentId("a".into()),
                cols: 140,
                rows: 40,
            },
            Input::NodePaneReady(NodePaneReadyV1 {
                agent_id: AgentId("a".into()),
                token: pane_ready_token(),
                cut: 9,
            }),
        ]
    }

    #[test]
    fn inputs_round_trip() {
        for i in every_input() {
            let s = serde_json::to_string(&i).unwrap();
            assert_eq!(serde_json::from_str::<Input>(&s).unwrap(), i, "{s}");
        }
    }

    #[test]
    fn every_input_is_covered_and_names_itself() {
        assert_eq!(every_input().len(), Input::METHODS.len());
        for i in every_input() {
            assert!(Input::METHODS.contains(&i.method()), "{}", i.method());
            let wire = serde_json::to_value(&i).unwrap();
            assert_eq!(wire["method"].as_str().unwrap(), i.method());
            assert_eq!(i.agent_id(), &AgentId("a".into()));
        }
    }

    #[test]
    fn inputs_pin_their_wire_shapes() {
        assert_eq!(
            serde_json::to_string(&Input::NodePtyWrite {
                agent_id: AgentId("a".into()),
                bytes: "\u{1b}q".into(),
            })
            .unwrap(),
            // The ESC is `\u001b` on the wire, not a raw byte: serde escapes C0 controls, which is
            // what keeps `Frame::to_line`'s one-frame-one-line property true for a keystroke.
            r#"{"method":"node/pty-write","params":{"agent_id":"a","bytes":"\u001bq"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Input::NodeResize {
                agent_id: AgentId("a".into()),
                cols: 140,
                rows: 40,
            })
            .unwrap(),
            r#"{"method":"node/resize","params":{"agent_id":"a","cols":140,"rows":40}}"#
        );
        assert_eq!(
            serde_json::to_string(&Input::NodePaneReady(NodePaneReadyV1 {
                agent_id: AgentId("a".into()),
                token: pane_ready_token(),
                cut: 9,
            }))
            .unwrap(),
            r#"{"method":"node/pane-ready","params":{"agent_id":"a","token":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=","cut":9}}"#
        );
    }

    #[test]
    fn pane_write_preserves_arbitrary_terminal_bytes() {
        let input = Input::NodePaneWrite(NodePaneWriteV1 {
            agent_id: AgentId("a".into()),
            bytes: crate::OpaquePaneBytesV1::new([0x00, 0x80, 0xff, b'\n']),
        });
        let wire = serde_json::to_string(&input).unwrap();
        assert_eq!(
            wire,
            r#"{"method":"node/pane-write","params":{"agent_id":"a","bytes":"AID/Cg=="}}"#
        );
        assert_eq!(serde_json::from_str::<Input>(&wire).unwrap(), input);
    }

    #[test]
    fn pane_write_rejects_a_decoded_payload_over_the_named_bound() {
        let oversized = Input::NodePaneWrite(NodePaneWriteV1 {
            agent_id: AgentId("a".into()),
            bytes: OpaquePaneBytesV1::new(vec![0; MAX_PANE_INPUT_BYTES + 1]),
        });
        let wire = serde_json::to_string(&oversized).unwrap();
        assert!(serde_json::from_str::<Input>(&wire).is_err());
    }

    #[test]
    fn pane_ready_requires_a_valid_token_and_cut() {
        for invalid in [
            r#"{"method":"node/pane-ready","params":{"agent_id":"a","cut":9}}"#,
            r#"{"method":"node/pane-ready","params":{"agent_id":"a","token":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="}}"#,
            r#"{"method":"node/pane-ready","params":{"agent_id":"a","token":"AA==","cut":9}}"#,
            r#"{"method":"node/pane-ready","params":{"agent_id":"a","token":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=","cut":-1}}"#,
            r#"{"method":"node/pane-ready","params":{"agent_id":"a","token":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=","cut":9,"cutt":9}}"#,
        ] {
            assert!(
                serde_json::from_str::<Input>(invalid).is_err(),
                "accepted malformed pane readiness input: {invalid}"
            );
        }
    }

    /// **The pin that keeps keystrokes off the request surface.**
    ///
    /// A keystroke transport arriving as a request method would have grown [`Method::ALL`]; this
    /// asserts from the other side that the inbound table is disjoint from it. The count moves only
    /// when a genuine request method lands (`node/resume` took it from fifteen to sixteen), never
    /// because an `Input` name leaked in — which the loop below is what proves.
    #[test]
    fn the_inbound_table_is_not_part_of_the_request_surface() {
        assert_eq!(Method::ALL.len(), 16, "§2's fifteen plus node/resume");
        for n in Input::METHODS {
            assert_eq!(
                Method::from_wire(n),
                None,
                "{n} became a request method; the point of a notification is that it has no answer"
            );
        }
    }

    /// Three namespaces on one socket, pairwise disjoint.
    ///
    /// `notify.rs` already asserts events against methods. The third table makes that insufficient:
    /// an inbound name colliding with an outbound one would make a frame's *direction* — not merely
    /// its kind — depend on which end happened to read it, and both ends use the same parser.
    #[test]
    fn inbound_names_collide_with_neither_events_nor_methods() {
        for n in Input::METHODS {
            assert!(
                !Event::METHODS.contains(&n),
                "{n} is also a supervisor→client event"
            );
        }
        let mut all: Vec<&str> = Input::METHODS
            .iter()
            .chain(Event::METHODS.iter())
            .chain(
                Method::ALL
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .iter(),
            )
            .copied()
            .collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(
            all.len(),
            n,
            "two wire names on one socket are the same string"
        );
        assert_eq!(Method::from_wire("node/pane-ready"), None);
        assert!(!Event::METHODS.contains(&"node/pane-ready"));
    }

    /// `node/pty-write` and `node/pty` are one character apart, and the reader distinguishes them
    /// by exact match rather than by prefix. Asserted because a prefix match is the natural way to
    /// write that reader and it would route every keystroke into the outbound table.
    #[test]
    fn the_write_name_is_not_the_read_name_with_a_suffix_the_reader_may_ignore() {
        assert!("node/pty-write".starts_with("node/pty"));
        assert!(!Event::METHODS.contains(&"node/pty-write"));
        assert!(!Input::METHODS.contains(&"node/pty"));
    }
}
