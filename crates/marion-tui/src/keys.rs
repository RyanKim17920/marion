//! Keystrokes back to the node, and the one sequence that must never reach it.
//!
//! # Why this is a filter and not an encoder
//!
//! The obvious shape for this module is a `KeyEvent` enum and a big `match` producing `ESC[A` for
//! Up, `ESC[1;5C` for Ctrl-Right, and so on. **That would be a second terminal, and it would be
//! wrong.** marion's own terminal is already in raw mode ([`crate::guard`]), so the bytes arriving
//! on marion's stdin are *the bytes the operator's terminal emits for those keys* — which is
//! exactly what the node, running under a pty on the other side, expects to read. Decoding them
//! into an enum and re-encoding them would introduce a translation step whose only possible
//! contribution is disagreement: with the operator's terminfo on one side and the node's
//! expectations on the other, every key marion failed to model would be a key that stopped working.
//!
//! Mode-dependent encodings are handled the same way and for the same reason. If the node enables
//! application cursor keys or bracketed paste, the fix is to **mirror that mode onto marion's own
//! terminal** — [`crate::guard::mirror_delta`] is where that happens for the mouse modes — so the
//! operator's terminal produces the right bytes at the source. A translation table here would be
//! the second parser [`crate::sticky`] refuses to be, in a different costume.
//!
//! So the only *encoding* in this crate is [`crate::mouse`]'s, which genuinely has no source:
//! marion's terminal reports a click to marion, and marion must re-emit it for the node.
//!
//! # What is left, and why it is not nothing
//!
//! A pure passthrough has one fatal gap: **there is no way out.** `marion attach` puts the
//! operator's terminal in raw mode and forwards every byte, including `^C`, `^D` and `^Z`, because
//! those belong to the node. An operator who wants to stop *looking* at a node — without killing
//! it, which is §7.3.1's whole invariant — has no keystroke left that marion can hear.
//!
//! So one sequence is reserved, and reserving it correctly is this module's entire job:
//!
//! * **`^]` is a prefix, not a command.** `^] d` detaches. `^] ^]` sends one literal `^]` to the
//!   node. `^] x` for any other `x` forwards **both** bytes, so a mistyped prefix loses no
//!   keystroke — the failure mode of a swallowing prefix is a key that silently does nothing,
//!   which is indistinguishable from a hung node.
//! * `^]` (0x1d, `GS`) rather than `^A` or `^B`: `^A` is start-of-line in every readline-ish
//!   editor and `^B` is tmux's own, so either would collide with something the operator uses
//!   constantly. `^]` is telnet's escape and neither committed harness binds it — no `\x1d` occurs
//!   in any of the five captures in `tests/fixtures/s2/`, which
//!   `the_reserved_prefix_appears_in_no_committed_capture` checks rather than assumes.
//!
//! # The prefix state outlives a `read()`
//!
//! `^]` and `d` are two keystrokes and therefore, normally, two reads. [`Keys`] is a struct and not
//! a function for exactly that reason: the pending-prefix bit has to survive a chunk boundary, or
//! the detach would only work when the operator typed both keys faster than marion could read. It
//! is the same "a `read()` is not a frame" fact that shapes [`crate::cast`], reaching a third part
//! of the client.

/// The reserved prefix: `^]`, `GS`, 0x1d.
pub const PREFIX: u8 = 0x1d;

/// The key that, after [`PREFIX`], ends the attach.
pub const DETACH_KEY: u8 = b'd';

/// What the read loop should do with what the operator typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write these bytes to the node's pty, unaltered.
    Forward(Vec<u8>),
    /// Stop attaching. **The node is left running** — §7.3.1: a client going away must leave every
    /// node exactly as it was.
    Detach,
}

/// The keystroke filter. One per attach.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Keys {
    /// A [`PREFIX`] has been seen and its second byte has not arrived yet — possibly because the
    /// chunk ended between them.
    pending: bool,
}

impl Keys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a prefix is half-typed. Diagnostic; a client may want to show an indicator.
    pub fn armed(&self) -> bool {
        self.pending
    }

    /// Split one `read()` from marion's stdin into things to do.
    ///
    /// Forwarded runs are **coalesced**: a 200-byte paste with no prefix in it yields exactly one
    /// [`Action::Forward`], not 200. That is not only cheaper — it preserves the operator's
    /// chunking, and a harness reading its stdin sees the same burst sizes it would have seen
    /// without marion in the middle.
    ///
    /// Anything after a [`Action::Detach`] in the same chunk is **discarded**, deliberately. Those
    /// bytes were typed at a node the operator has just stopped looking at; delivering them would
    /// be typing into a pane that is no longer on screen, which is the one outcome worse than
    /// losing them.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Action> {
        let mut out = Vec::new();
        let mut run: Vec<u8> = Vec::new();

        for &b in chunk {
            if self.pending {
                self.pending = false;
                match b {
                    DETACH_KEY => {
                        if !run.is_empty() {
                            out.push(Action::Forward(std::mem::take(&mut run)));
                        }
                        out.push(Action::Detach);
                        return out;
                    }
                    // `^] ^]` is how the operator sends a literal prefix through.
                    PREFIX => run.push(PREFIX),
                    // A mistyped prefix forwards both bytes rather than eating either.
                    other => {
                        run.push(PREFIX);
                        run.push(other);
                    }
                }
                continue;
            }
            if b == PREFIX {
                self.pending = true;
            } else {
                run.push(b);
            }
        }

        if !run.is_empty() {
            out.push(Action::Forward(run));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarded(actions: &[Action]) -> Vec<u8> {
        actions
            .iter()
            .flat_map(|a| match a {
                Action::Forward(b) => b.clone(),
                Action::Detach => Vec::new(),
            })
            .collect()
    }

    fn feed(k: &mut Keys, chunk: &[u8]) -> Vec<Action> {
        k.feed(chunk)
    }

    #[test]
    fn ordinary_keys_pass_through_untouched() {
        let mut k = Keys::new();
        // An arrow key, a control character and text. None of these is marion's business.
        let bytes = b"\x1b[Als -la\r\x03";
        assert_eq!(feed(&mut k, bytes), vec![Action::Forward(bytes.to_vec())]);
    }

    /// The passthrough is the design, so it is asserted as a *property* and not only on a sample:
    /// every byte that is not the prefix reaches the node, alone and in company.
    #[test]
    fn every_byte_but_the_prefix_reaches_the_node() {
        for b in 0u8..=255 {
            if b == PREFIX {
                continue;
            }
            let mut k = Keys::new();
            assert_eq!(
                forwarded(&feed(&mut k, &[b])),
                vec![b],
                "byte {b:#04x} did not reach the node"
            );
            assert!(!k.armed());
        }
    }

    #[test]
    fn the_prefix_alone_forwards_nothing_and_arms() {
        let mut k = Keys::new();
        assert_eq!(feed(&mut k, &[PREFIX]), vec![]);
        assert!(k.armed(), "the prefix must survive to the next read");
    }

    /// **The reason [`Keys`] is a struct.** `^]` and `d` are two keystrokes, so normally two
    /// `read()`s. A function-shaped filter would detach only when the operator typed both faster
    /// than marion could read — a race that would present as an intermittently unresponsive key.
    #[test]
    fn the_prefix_survives_a_chunk_boundary() {
        let mut k = Keys::new();
        assert_eq!(feed(&mut k, &[PREFIX]), vec![]);
        assert_eq!(feed(&mut k, b"d"), vec![Action::Detach]);
    }

    #[test]
    fn the_prefix_and_the_key_in_one_chunk_detach_just_the_same() {
        let mut k = Keys::new();
        assert_eq!(feed(&mut k, &[PREFIX, DETACH_KEY]), vec![Action::Detach]);
    }

    #[test]
    fn a_doubled_prefix_sends_one_literal_prefix_to_the_node() {
        let mut k = Keys::new();
        assert_eq!(
            feed(&mut k, &[PREFIX, PREFIX]),
            vec![Action::Forward(vec![PREFIX])]
        );
        assert!(!k.armed(), "the pair is consumed, not left half-typed");
    }

    /// A mistyped prefix must lose nothing. The alternative — swallowing both bytes — presents to
    /// the operator as a keystroke that silently did nothing, which is indistinguishable from a
    /// node that has hung.
    #[test]
    fn a_mistyped_prefix_forwards_both_bytes_rather_than_eating_either() {
        let mut k = Keys::new();
        assert_eq!(
            feed(&mut k, &[PREFIX, b'q']),
            vec![Action::Forward(vec![PREFIX, b'q'])]
        );
    }

    #[test]
    fn text_around_a_detach_keeps_its_order_and_the_tail_is_dropped() {
        let mut k = Keys::new();
        let actions = feed(&mut k, b"before\x1ddafter");
        assert_eq!(
            actions,
            vec![Action::Forward(b"before".to_vec()), Action::Detach],
            "what preceded the detach is delivered; what followed it is not"
        );
    }

    /// Coalescing is not cosmetic: it preserves the burst sizes a harness would have seen without
    /// marion in the middle, which is the same "a `read()` is not a frame" concern from the other
    /// direction.
    #[test]
    fn a_paste_with_no_prefix_in_it_is_one_forward_and_not_one_per_byte() {
        let mut k = Keys::new();
        let paste = "a long pasted line of text\r\nand a second one\r\n".as_bytes();
        let actions = feed(&mut k, paste);
        assert_eq!(
            actions.len(),
            1,
            "the paste was split into {}",
            actions.len()
        );
        assert_eq!(actions[0], Action::Forward(paste.to_vec()));
    }

    #[test]
    fn an_empty_read_does_nothing() {
        let mut k = Keys::new();
        assert_eq!(feed(&mut k, b""), vec![]);
        // And an empty read must not disarm a pending prefix.
        feed(&mut k, &[PREFIX]);
        assert_eq!(feed(&mut k, b""), vec![]);
        assert!(k.armed());
    }

    /// A multi-byte escape sequence split across reads must not be disturbed. `Keys` does not
    /// parse escapes at all, which is what makes this true — asserted so that a future version
    /// that *does* start parsing them fails here first.
    #[test]
    fn an_escape_sequence_split_across_reads_arrives_whole_and_in_order() {
        let mut k = Keys::new();
        let mut got = Vec::new();
        for part in [&b"\x1b"[..], b"[1;", b"5C"] {
            got.extend(forwarded(&feed(&mut k, part)));
        }
        assert_eq!(
            got,
            b"\x1b[1;5C".to_vec(),
            "Ctrl-Right was mangled in transit"
        );
    }

    /// The prefix must not collide with something a harness already uses, or an operator would
    /// find one key mysteriously dead inside Claude Code or Codex. Checked against the corpus
    /// rather than asserted from memory.
    #[test]
    fn the_reserved_prefix_appears_in_no_committed_capture() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/s2");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("the committed capture directory") {
            let path = entry.expect("a dir entry").path();
            if path.extension().is_none_or(|e| e != "bin") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("a committed capture");
            assert!(
                !bytes.contains(&PREFIX),
                "{} emits {PREFIX:#04x}, so reserving it would break that harness",
                path.display()
            );
            checked += 1;
        }
        assert_eq!(checked, 5, "expected the five committed .raw.bin captures");
    }
}
