//! Where the node's byte stream stands, so marion's status row is painted **between** the node's
//! escape sequences and never inside one.
//!
//! A pane frame is whatever one pty read returned, and a read can end anywhere: in the middle of
//! a CSI, an OSC, a UTF-8 scalar, or a `?2026` synchronized frame. Bytes marion wrote at such a
//! point would end the node's sequence early and the rest of it would print as text — the
//! `30;6HPassed.` opencode's composer showed. [`StreamPosition`] is the smallest parser that can
//! say whether the stream is at a boundary; it keeps no grid and never alters the bytes it reads.
//!
//! It also reports the node's scroll-region resets ([`Mark`]), because while marion holds the
//! last row the operator's scroll region must stop above it, and a node that resets its own
//! region to "the whole screen" would otherwise scroll marion's row into its text.

/// A node sequence that changed the operator's scroll region, and the offset just past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    /// `CSI top ; bottom r` (DECSTBM). `bottom` is `None` when defaulted, which means the last row
    /// of the operator's screen. DECSTBM homes the cursor.
    ScrollRegion {
        end: usize,
        top: u16,
        bottom: Option<u16>,
    },
    /// `ESC c` (RIS): every margin reset, the cursor homed.
    HardReset { end: usize },
    /// `CSI ! p` (DECSTR): every margin reset, the cursor left where it was.
    SoftReset { end: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    /// OSC, DCS, SOS, PM or APC: data up to a string terminator. `bel` says whether BEL ends it
    /// too, which xterm allows only for OSC.
    String {
        bel: bool,
    },
    /// An `ESC` inside a string: `\` finishes the terminator, anything else starts a new escape.
    StringEscape,
}

/// Longest CSI parameter string kept for inspection. Longer ones are still tracked to their final
/// byte; only their meaning is not read.
const MAX_PARAMS: usize = 64;

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const BEL: u8 = 0x07;

#[derive(Debug, Default)]
pub(super) struct StreamPosition {
    state: State,
    /// UTF-8 continuation bytes still owed by the scalar the last byte began.
    utf8_owed: u8,
    params: Vec<u8>,
    overflowed: bool,
    /// `?`, `<`, `=` or `>` as the first CSI byte.
    private: Option<u8>,
    intermediates: Vec<u8>,
    /// Inside `CSI ? 2026 h … CSI ? 2026 l`. The terminal holds the frame, so a row painted now
    /// would be applied with the node's half-finished frame.
    synchronized: bool,
    /// The node saved the cursor (`ESC 7`, `CSI s`) and has not restored it. The row's own save
    /// and restore would overwrite the node's saved position, so it waits for the node's restore.
    cursor_saved: bool,
}

impl StreamPosition {
    /// Whether marion may write to the operator's terminal now without landing inside anything
    /// the node has started.
    pub(super) fn at_boundary(&self) -> bool {
        self.state == State::Ground
            && self.utf8_owed == 0
            && !self.synchronized
            && !self.cursor_saved
    }

    /// Read `bytes` — the next pane frame — and return the scroll-region changes in it.
    pub(super) fn advance(&mut self, bytes: &[u8]) -> Vec<Mark> {
        let mut marks = Vec::new();
        for (index, &byte) in bytes.iter().enumerate() {
            if let Some(mark) = self.byte(byte, index + 1) {
                marks.push(mark);
            }
        }
        marks
    }

    fn byte(&mut self, byte: u8, end: usize) -> Option<Mark> {
        match self.state {
            State::Ground => {
                self.ground(byte);
                None
            }
            State::Escape => self.escape(byte, end),
            State::EscapeIntermediate => {
                match byte {
                    ESC => self.state = State::Escape,
                    CAN | SUB | 0x30..=0x7e => self.state = State::Ground,
                    _ => {}
                }
                None
            }
            State::Csi => self.csi(byte, end),
            State::String { bel } => {
                match byte {
                    ESC => self.state = State::StringEscape,
                    CAN | SUB => self.state = State::Ground,
                    BEL if bel => self.state = State::Ground,
                    _ => {}
                }
                None
            }
            State::StringEscape => {
                if byte == b'\\' {
                    self.state = State::Ground;
                    None
                } else {
                    self.state = State::Escape;
                    self.escape(byte, end)
                }
            }
        }
    }

    fn ground(&mut self, byte: u8) {
        self.utf8_owed = match byte {
            ESC => {
                self.state = State::Escape;
                0
            }
            0xc2..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf4 => 3,
            0x80..=0xbf => self.utf8_owed.saturating_sub(1),
            _ => 0,
        };
    }

    fn escape(&mut self, byte: u8, end: usize) -> Option<Mark> {
        self.state = State::Ground;
        match byte {
            b'[' => {
                self.params.clear();
                self.intermediates.clear();
                self.private = None;
                self.overflowed = false;
                self.state = State::Csi;
            }
            b']' => self.state = State::String { bel: true },
            b'P' | b'X' | b'^' | b'_' => self.state = State::String { bel: false },
            0x20..=0x2f => self.state = State::EscapeIntermediate,
            b'7' => self.cursor_saved = true,
            b'8' => self.cursor_saved = false,
            b'c' => {
                self.cursor_saved = false;
                self.synchronized = false;
                return Some(Mark::HardReset { end });
            }
            ESC => self.state = State::Escape,
            // C0 controls execute without ending the escape; CAN and SUB cancel it.
            CAN | SUB => {}
            0x00..=0x1f | 0x7f => self.state = State::Escape,
            _ => {}
        }
        None
    }

    fn csi(&mut self, byte: u8, end: usize) -> Option<Mark> {
        match byte {
            b'<'..=b'?' if self.params.is_empty() && self.private.is_none() && !self.overflowed => {
                self.private = Some(byte);
            }
            0x30..=0x3f => {
                if self.params.len() < MAX_PARAMS {
                    self.params.push(byte);
                } else {
                    self.overflowed = true;
                }
            }
            0x20..=0x2f => {
                if self.intermediates.len() < MAX_PARAMS {
                    self.intermediates.push(byte);
                }
            }
            0x40..=0x7e => {
                self.state = State::Ground;
                return self.dispatch(byte, end);
            }
            ESC => self.state = State::Escape,
            CAN | SUB => self.state = State::Ground,
            _ => {}
        }
        None
    }

    fn dispatch(&mut self, last: u8, end: usize) -> Option<Mark> {
        if self.overflowed {
            return None;
        }
        match (last, self.private, self.intermediates.as_slice()) {
            (b'h' | b'l', Some(b'?'), []) => {
                if self.numbers().any(|n| n == Some(2026)) {
                    self.synchronized = last == b'h';
                }
                None
            }
            (b'r', None, []) => {
                let mut numbers = self.numbers();
                let top = numbers.next().flatten().unwrap_or(1).max(1);
                let bottom = numbers.next().flatten().filter(|bottom| *bottom != 0);
                Some(Mark::ScrollRegion { end, top, bottom })
            }
            (b's', None, []) if self.params.is_empty() => {
                self.cursor_saved = true;
                None
            }
            (b'u', None, []) if self.params.is_empty() => {
                self.cursor_saved = false;
                None
            }
            (b'p', None, [b'!']) => {
                self.cursor_saved = false;
                self.synchronized = false;
                Some(Mark::SoftReset { end })
            }
            _ => None,
        }
    }

    /// The CSI's `;`-separated parameters, `None` where one is empty or not a number.
    fn numbers(&self) -> impl Iterator<Item = Option<u16>> + '_ {
        self.params
            .split(|byte| *byte == b';')
            .map(|field| std::str::from_utf8(field).ok()?.parse().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::{Mark, StreamPosition};

    fn after(chunks: &[&[u8]]) -> StreamPosition {
        let mut position = StreamPosition::default();
        for chunk in chunks {
            position.advance(chunk);
        }
        position
    }

    #[test]
    fn text_and_complete_sequences_leave_the_stream_at_a_boundary() {
        assert!(after(&[]).at_boundary());
        assert!(after(&[b"plain text\r\n"]).at_boundary());
        assert!(after(&[b"\x1b[30;6H\x1b[38;5;208mPassed.\x1b[0m"]).at_boundary());
        assert!(after(&[b"\x1b]0;title\x07", b"\x1b]8;;http://x\x1b\\link"]).at_boundary());
        assert!(after(&[b"\x1bP+q544e\x1b\\", b"\x1b(B"]).at_boundary());
        assert!(after(&["héllo ✓ 🦀".as_bytes()]).at_boundary());
    }

    #[test]
    fn a_frame_that_ends_inside_a_sequence_is_not_a_boundary_until_it_finishes() {
        for (first, rest) in [
            (&b"\x1b"[..], &b"[30;6H"[..]),
            (b"\x1b[30;", b"6H"),
            (b"\x1b[?20", b"04h"),
            (b"\x1b]0;tit", b"le\x07"),
            (b"\x1b]0;title\x1b", b"\\"),
            (b"\x1bP+q54", b"4e\x1b\\"),
            (b"\x1b(", b"B"),
            (&"✓".as_bytes()[..1], &"✓".as_bytes()[1..]),
            (&"🦀".as_bytes()[..2], &"🦀".as_bytes()[2..]),
        ] {
            let mut position = StreamPosition::default();
            position.advance(first);
            assert!(!position.at_boundary(), "{first:?} ends mid-sequence");
            position.advance(rest);
            assert!(position.at_boundary(), "{first:?} + {rest:?} is complete");
        }
    }

    /// A BEL ends an OSC but is data inside a DCS; CAN cancels whatever was open.
    #[test]
    fn string_terminators_follow_the_string_kind() {
        assert!(!after(&[b"\x1bPdata\x07"]).at_boundary());
        assert!(after(&[b"\x1bPdata\x18"]).at_boundary());
        assert!(after(&[b"\x1b[12\x1a"]).at_boundary());
    }

    #[test]
    fn a_synchronized_frame_is_not_a_boundary_until_it_closes() {
        let mut position = after(&[b"\x1b[?2026h\x1b[1;1Hdraw"]);
        assert!(!position.at_boundary());
        position.advance(b"\x1b[?2026l");
        assert!(position.at_boundary());
        // 2026 among other modes counts too.
        assert!(!after(&[b"\x1b[?25;2026h"]).at_boundary());
    }

    #[test]
    fn a_saved_cursor_is_not_a_boundary_until_the_node_restores_it() {
        assert!(!after(&[b"\x1b7\x1b[5;1Hx"]).at_boundary());
        assert!(after(&[b"\x1b7\x1b[5;1Hx\x1b8"]).at_boundary());
        assert!(!after(&[b"\x1b[s"]).at_boundary());
        assert!(after(&[b"\x1b[s", b"\x1b[u"]).at_boundary());
        // Kitty's keyboard protocol and DECSLRM share the final bytes, not the meaning.
        assert!(after(&[b"\x1b[?u\x1b[>1u\x1b[<u\x1b[1;80s"]).at_boundary());
    }

    #[test]
    fn scroll_region_changes_are_marked_just_past_their_final_byte() {
        let mut position = StreamPosition::default();
        let bytes = b"ab\x1b[r\x1b[5;20r\x1b[3r\x1b[;0r\x1bc\x1b[!p\x1b[?1;2r";
        assert_eq!(
            position.advance(bytes),
            vec![
                Mark::ScrollRegion {
                    end: 5,
                    top: 1,
                    bottom: None
                },
                Mark::ScrollRegion {
                    end: 12,
                    top: 5,
                    bottom: Some(20)
                },
                Mark::ScrollRegion {
                    end: 16,
                    top: 3,
                    bottom: None
                },
                Mark::ScrollRegion {
                    end: 21,
                    top: 1,
                    bottom: None
                },
                Mark::HardReset { end: 23 },
                Mark::SoftReset { end: 27 },
            ]
        );
        // A region split across two frames is marked in the frame that finishes it.
        let mut position = StreamPosition::default();
        assert!(position.advance(b"\x1b[1;").is_empty());
        assert_eq!(
            position.advance(b"9r"),
            vec![Mark::ScrollRegion {
                end: 2,
                top: 1,
                bottom: Some(9)
            }]
        );
    }
}
