//! When to paint.
//!
//! # The edge, and the three things that look like it
//!
//! A client that repainted on every `read()` would tear: a pty read is not a frame (S11's MUST),
//! and a TUI's repaint routinely spans several reads. A client that repainted on a timer would
//! either lag or burn CPU. The signal that actually exists in the stream is the **DECSET 2026
//! synchronized-output bracket**, which both harnesses emit around every frame — across all five
//! committed captures the `h`/`l` alternation is strict, with zero violations and zero unclosed
//! brackets (§5.3 L1229).
//!
//! Three near-misses, each already measured and each wrong:
//!
//! 1. **Not a CUP.** Keying on cursor movement fails on frames that do not move the cursor:
//!    `claude-boot-exit` has **1 of 8** such frames and `claude-boot-help-status-resize` **3 of
//!    19**. Those frames would never paint.
//! 2. **Not the `unset_private_mode` callback count.** vte 0.15 calls it a number of times that
//!    depends on how the bytes were *chunked* — 9 calls for `claude-boot-exit`'s 8 brackets when
//!    fed whole, **16** when fed one byte at a time, because `Processor::stop_sync_internal`
//!    re-parses the buffered ESU and then reports the mode itself. Counting callbacks makes the
//!    frame rate a function of `read()` sizes.
//! 3. **Not "the grid changed".** Cheap to test and wrong in the other direction: a frame that
//!    repaints identical content still ends a bracket, and a diff-based trigger would drop it,
//!    which matters because the cursor may have moved within it.
//!
//! `marion_term::Stats::frames` is already edge-triggered on set→unset for exactly these reasons.
//! This module's whole job is to hold that number and answer "has it moved?", so the client never
//! reaches for the tempting alternatives.
//!
//! # The straggler
//!
//! A bracket-only trigger would never paint a stream that contains **no** brackets. That is not
//! hypothetical for the first moments of a session — every capture has plaintext boot output
//! before its first `?2026h` — and it is the whole story for a harness that never synchronizes.
//! So [`Redraw::should_paint`] also fires when bytes arrived and *no* bracket is open: the frame
//! is over in the only sense available. When a bracket **is** open the paint is withheld, which is
//! what makes the bracket useful at all — a half-applied frame is the thing synchronized output
//! exists to hide.

use marion_term::Term;

/// Tracks the frame edge across feeds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Redraw {
    seen_frames: usize,
    painted: usize,
    /// Bytes have arrived that no paint has shown yet.
    dirty: bool,
}

impl Redraw {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the grid should be painted after feeding a chunk.
    ///
    /// **True on a closed bracket and on nothing else.** In particular this does *not* paint
    /// merely because bytes arrived: feeding `\x1b[?2026h` one byte at a time passes through six
    /// states in which no bracket is open yet and the escape sequence is half-parsed, and a
    /// "bytes arrived and no bracket is open" rule paints in every one of them. Measured on the
    /// first draft of this module: the same three-bracket stream produced 1 paint fed whole and
    /// **24** fed byte-at-a-time — the chunk dependence this module exists to avoid, reintroduced
    /// by the straggler rule rather than by the frame counter.
    ///
    /// Unsynchronized output is handled by [`Redraw::on_idle`] instead, which is a question about
    /// the *stream*, not about a chunk.
    pub fn on_feed(&mut self, term: &Term, bytes_fed: usize) -> bool {
        if bytes_fed > 0 {
            self.dirty = true;
        }
        let frames = term.stats().frames;
        if frames > self.seen_frames {
            self.seen_frames = frames;
            self.dirty = false;
            self.painted += 1;
            return true;
        }
        false
    }

    /// Whether to paint now that the stream has gone quiet.
    ///
    /// Call when the read loop finds nothing to read. This is what paints a harness that never
    /// synchronizes at all, and the plaintext boot output every capture emits before its first
    /// `?2026h`. It withholds while a bracket is open — a pause in the middle of a frame is a slow
    /// harness, not a finished frame, and showing it is exactly the tearing synchronized output
    /// exists to prevent.
    pub fn on_idle(&mut self, term: &Term) -> bool {
        if self.dirty && !in_open_bracket(term) {
            self.dirty = false;
            self.painted += 1;
            return true;
        }
        false
    }

    /// The stream proved no later closing bracket can arrive. Flush any dirty tail exactly once,
    /// including a child that died while synchronized output was open.
    pub fn on_end(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        self.dirty = false;
        self.painted += 1;
        true
    }

    /// Completed brackets observed. The number §5.3's per-capture figures are about.
    pub fn frames(&self) -> usize {
        self.seen_frames
    }

    /// Paints issued. Diagnostic: a client painting far more often than it has frames is keyed on
    /// the wrong thing, which is the failure this module exists to prevent.
    pub fn paints(&self) -> usize {
        self.painted
    }
}

/// Whether a synchronized-output bracket is currently open.
///
/// This asks `marion_term`, which is the only thing that knows: `alacritty_terminal` drops
/// `SyncUpdate` without recording it, so the grid's own `TermMode` never carries the bit. See
/// `marion_term::Stats::in_frame`.
fn in_open_bracket(term: &Term) -> bool {
    term.stats().in_frame()
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_term::{Size, Term};

    fn term() -> Term {
        Term::new(Size::new(20, 5))
    }

    fn feed(t: &mut Term, r: &mut Redraw, bytes: &[u8]) -> bool {
        t.advance(bytes);
        r.on_feed(t, bytes.len())
    }

    #[test]
    fn a_completed_bracket_paints() {
        let (mut t, mut r) = (term(), Redraw::new());
        assert!(feed(&mut t, &mut r, b"\x1b[?2026hhello\x1b[?2026l"));
        assert_eq!(r.frames(), 1);
    }

    #[test]
    fn an_open_bracket_withholds_the_paint_until_it_closes() {
        // This is the entire point of synchronized output: no half-applied frame is shown.
        let (mut t, mut r) = (term(), Redraw::new());
        assert!(
            !feed(&mut t, &mut r, b"\x1b[?2026hpartial"),
            "must not paint mid-frame"
        );
        assert!(!r.on_idle(&t), "even a pause mid-bracket must not paint");
        assert!(
            feed(&mut t, &mut r, b" more\x1b[?2026l"),
            "the close is the edge"
        );
        assert_eq!(r.frames(), 1);
        assert_eq!(r.paints(), 1, "one bracket, one paint");
    }

    /// Mutation: treat a terminal stream `End` as an ordinary idle edge. A child that dies with
    /// synchronized output open can never send the closing DECSET, so its final dirty bytes would
    /// remain invisible even though the durable stream has proved there is no later frame.
    #[test]
    fn terminal_end_flushes_dirty_output_even_when_a_frame_was_cut_open() {
        let (mut t, mut r) = (term(), Redraw::new());
        assert!(!feed(&mut t, &mut r, b"\x1b[?2026hfinal partial frame"));
        assert!(
            !r.on_idle(&t),
            "ordinary quiet must still withhold an open frame"
        );
        assert!(r.on_end(), "terminal End is the final paint edge");
        assert!(!r.on_end(), "a second End cannot repaint the same bytes");
        assert_eq!(r.paints(), 1);
    }

    /// §5.3 measured frames with no CUP; keying on cursor movement would never paint them.
    #[test]
    fn a_frame_that_moves_no_cursor_still_paints() {
        let (mut t, mut r) = (term(), Redraw::new());
        // Text and an SGR change, no CUP anywhere.
        assert!(feed(
            &mut t,
            &mut r,
            b"\x1b[?2026h\x1b[1mbold\x1b[0m\x1b[?2026l"
        ));
        assert_eq!(t.stats().frames, 1);
        assert_eq!(
            t.stats().frames_with_cup,
            0,
            "the fixture for this test has no CUP"
        );
        assert_eq!(r.paints(), 1);
    }

    /// The counterpart: a CUP without a bracket must not be mistaken for a frame boundary.
    #[test]
    fn a_cup_alone_is_not_a_frame() {
        let (mut t, mut r) = (term(), Redraw::new());
        feed(&mut t, &mut r, b"\x1b[?2026h");
        // A CUP inside an open bracket: the cursor moved, the frame did not end.
        assert!(
            !feed(&mut t, &mut r, b"\x1b[3;3Hx"),
            "a CUP mid-bracket is not an edge"
        );
        assert_eq!(r.frames(), 0);
    }

    const THREE_FRAMES: &[u8] =
        b"\x1b[?2026hone\x1b[?2026l\x1b[?2026htwo\x1b[?2026l\x1b[?2026hthree\x1b[?2026l";

    /// The frame **count** must not depend on how the bytes were chunked. This is the property
    /// that refutes counting `unset_private_mode` callbacks — vte reports 9 for
    /// `claude-boot-exit`'s 8 brackets fed whole and 16 fed byte-at-a-time, so a callback-counting
    /// client would paint at a rate set by `read()` sizes.
    ///
    /// The *paint* count is deliberately **not** asserted equal: a client that reads a large chunk
    /// containing three brackets paints once, and coalescing is the correct behaviour, not a bug.
    /// What must hold is that a paint never exceeds the frames that justified it.
    #[test]
    fn the_frame_count_does_not_depend_on_how_the_bytes_were_chunked() {
        let (mut t, mut r) = (term(), Redraw::new());
        feed(&mut t, &mut r, THREE_FRAMES);
        let (whole_frames, whole_paints) = (r.frames(), r.paints());

        let (mut t, mut r) = (term(), Redraw::new());
        for b in THREE_FRAMES.iter() {
            feed(&mut t, &mut r, std::slice::from_ref(b));
        }
        let (byte_frames, byte_paints) = (r.frames(), r.paints());

        assert_eq!(whole_frames, 3, "three brackets");
        assert_eq!(
            whole_frames, byte_frames,
            "chunking must not change the frame count"
        );
        assert_eq!(
            whole_paints, 1,
            "one read carrying three frames paints once"
        );
        assert_eq!(
            byte_paints, 3,
            "one paint per bracket when they arrive separately"
        );
        assert!(whole_paints <= whole_frames && byte_paints <= byte_frames);
    }

    /// The safety property that holds however the stream is split: **no paint is ever issued while
    /// a bracket is open.** A client that painted mid-bracket would show a half-applied frame,
    /// which is the one thing synchronized output exists to prevent.
    #[test]
    fn no_paint_is_ever_issued_while_a_bracket_is_open() {
        let (mut t, mut r) = (term(), Redraw::new());
        for b in THREE_FRAMES.iter() {
            if feed(&mut t, &mut r, std::slice::from_ref(b)) {
                assert!(!t.stats().in_frame(), "painted inside an open bracket");
            }
            if r.on_idle(&t) {
                assert!(!t.stats().in_frame(), "idle-painted inside an open bracket");
            }
        }
        assert_eq!(r.frames(), 3);
    }

    #[test]
    fn unsynchronized_output_paints_when_the_stream_goes_quiet() {
        // Boot text before the first ?2026h, and harnesses that never synchronize at all.
        let (mut t, mut r) = (term(), Redraw::new());
        assert!(
            !feed(&mut t, &mut r, b"plain boot text"),
            "no bracket closed"
        );
        assert!(
            r.on_idle(&t),
            "a quiet stream with unshown bytes must paint"
        );
        assert_eq!(r.frames(), 0, "no brackets were involved");
        assert_eq!(r.paints(), 1);
    }

    #[test]
    fn idle_does_not_repaint_what_was_already_shown() {
        let (mut t, mut r) = (term(), Redraw::new());
        feed(&mut t, &mut r, b"\x1b[?2026hhi\x1b[?2026l");
        assert_eq!(r.paints(), 1);
        assert!(!r.on_idle(&t), "the bracket already painted these bytes");
        assert!(!r.on_idle(&t));
        assert_eq!(r.paints(), 1);
    }

    #[test]
    fn an_empty_feed_never_paints() {
        let (mut t, mut r) = (term(), Redraw::new());
        assert!(!feed(&mut t, &mut r, b""));
        assert!(!r.on_idle(&t), "nothing arrived, so nothing is unshown");
        assert_eq!(r.paints(), 0);
    }
}
