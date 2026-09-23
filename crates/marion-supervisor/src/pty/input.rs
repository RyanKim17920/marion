//! **What the node's terminal will do with bytes typed into it** — the facts a writer other than
//! the operator (turn delivery's paste, `crate::paste`) has to know before it types.
//!
//! One of them is read off the node's own output: whether it asked for **bracketed paste**
//! (`CSI ? 2004 h`). A paste into a terminal that did not ask for it arrives as typed keys, and a
//! newline inside the message then submits half of it; S31 measured codex turning an unbracketed
//! burst's CR into a newline, so marion never types a paste the node cannot tell from typing.

/// Longest parameter list kept for one private-mode CSI. Real mode sets are a handful of short
/// numbers; anything longer is not one marion needs to read, and a bound keeps a hostile stream
/// from growing this.
const MAX_PARAMS: usize = 64;

/// The DEC private mode for bracketed paste.
const BRACKETED_PASTE: &[u8] = b"2004";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    /// After `ESC`.
    Escape,
    /// After `ESC [`: a private marker may follow.
    CsiEntry,
    /// Inside `ESC [ ?`, collecting parameters.
    Private,
    /// Inside a CSI marion does not read; waiting for its final byte.
    Ignore,
}

/// **DECSET/DECRST 2004, followed across reads.** A small state machine over the output stream —
/// only the sequences that change the mode are recognised, and a read boundary anywhere inside one
/// is carried, because a `read()` is not a frame (S11).
#[derive(Debug, Default)]
pub(crate) struct ModeScan {
    state: Scan,
    params: Vec<u8>,
    overflowed: bool,
    bracketed_paste: bool,
}

impl ModeScan {
    /// Whether the node has asked for bracketed paste and not since withdrawn it.
    pub(crate) fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            self.step(b);
        }
    }

    fn step(&mut self, b: u8) {
        // `ESC` restarts a sequence from anywhere, as a terminal's parser does; `CAN`/`SUB`
        // abandon one.
        if b == 0x1b {
            self.state = Scan::Escape;
            return;
        }
        if b == 0x18 || b == 0x1a {
            self.state = Scan::Ground;
            return;
        }
        self.state = match self.state {
            Scan::Ground => Scan::Ground,
            Scan::Escape => match b {
                b'[' => Scan::CsiEntry,
                // RIS: a full reset, which clears every DEC private mode.
                b'c' => {
                    self.bracketed_paste = false;
                    Scan::Ground
                }
                _ => Scan::Ground,
            },
            Scan::CsiEntry => match b {
                b'?' => {
                    self.params.clear();
                    self.overflowed = false;
                    Scan::Private
                }
                0x40..=0x7e => Scan::Ground,
                _ => Scan::Ignore,
            },
            Scan::Private => match b {
                b'0'..=b'9' | b';' => {
                    if self.params.len() < MAX_PARAMS {
                        self.params.push(b);
                    } else {
                        self.overflowed = true;
                    }
                    Scan::Private
                }
                b'h' | b'l' if !self.overflowed => {
                    if self
                        .params
                        .split(|&c| c == b';')
                        .any(|p| p == BRACKETED_PASTE)
                    {
                        self.bracketed_paste = b == b'h';
                    }
                    Scan::Ground
                }
                0x40..=0x7e => Scan::Ground,
                // An intermediate (`$` in DECRQM, `CSI ? 2004 $ p`) makes it another command.
                _ => Scan::Ignore,
            },
            Scan::Ignore => match b {
                0x40..=0x7e => Scan::Ground,
                _ => Scan::Ignore,
            },
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(chunks: &[&[u8]]) -> bool {
        let mut scan = ModeScan::default();
        for c in chunks {
            scan.feed(c);
        }
        scan.bracketed_paste()
    }

    /// Every split point of a set, and of a reset after it, lands on the same answer.
    #[test]
    fn a_mode_change_is_seen_at_every_read_boundary() {
        let set = b"ab\x1b[?1004;2004hcd";
        for at in 0..=set.len() {
            assert!(scanned(&[&set[..at], &set[at..]]), "split at {at}");
        }
        let reset = b"\x1b[?2004l";
        for at in 0..=reset.len() {
            assert!(
                !scanned(&[set, &reset[..at], &reset[at..]]),
                "split at {at}"
            );
        }
    }

    #[test]
    fn sequences_that_are_not_decset_2004_leave_the_mode_alone() {
        assert!(!scanned(&[b"\x1b[2004h"]), "not private");
        assert!(!scanned(&[b"\x1b[?20045h"]), "another number");
        assert!(!scanned(&[b"\x1b[?2004$p"]), "a query");
        assert!(
            !scanned(&[b"\x1b[?20\x1b[m04h"]),
            "interrupted by another sequence"
        );
        assert!(!scanned(&[b"\x1b[?20\x1804h"]), "cancelled");
        assert!(scanned(&[b"\x1b]0;title\x07\x1b[?2004h"]), "after an OSC");
    }

    /// A parameter list past the bound is dropped whole rather than read truncated.
    #[test]
    fn an_overlong_parameter_list_is_not_read() {
        let mut long = b"\x1b[?".to_vec();
        long.extend(std::iter::repeat_n(b'1', MAX_PARAMS));
        long.extend(b";2004h");
        assert!(!scanned(&[&long]));
        assert!(
            scanned(&[&long, b"\x1b[?2004h"]),
            "and the scanner recovers"
        );
    }
}
