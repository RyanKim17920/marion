//! The sticky-mode preamble: six fixed sequences, scanned literally.
//!
//! # Why a tail alone is always wrong
//!
//! A `pty.cast` tail is not a self-describing stream. Some terminal state is set **once, near the
//! start of the session, and never mentioned again** — and a replay that begins after that point
//! has no way to learn it from the bytes it was given. Measured on the committed captures:
//!
//! | capture | `?1049h` | `?1049l` | mouse modes |
//! |---|---|---|---|
//! | `claude-2.1.220-boot-exit` | byte 67 | byte 5866 | `?1000/1002/1003/1006`, set at 98-122 |
//! | `claude-2.1.220-boot-help-status-resize` | byte 1900 | **never** | set at 1931-1955, **never unset** |
//! | `codex-cli-0.145.0-…-diff-resize` | byte 38963 | byte 43372 | none |
//! | `codex-cli-0.146.0-14row-…` | none | none | none |
//! | `codex-cli-0.146.0-boot-status-help-resize` | none | none | none |
//!
//! Both Claude captures enter the alternate screen and stay there for essentially the whole
//! session, so a tail cut anywhere in the body renders alt-screen content on the **main** screen:
//! the harness's absolute-addressed repaint lands on top of the user's shell scrollback, and
//! nothing in the tail ever says `?1049h` again to correct it.
//!
//! *(A correction worth recording, because the increment brief and an earlier reading of §5.3 both
//! get it slightly wrong in a way that would mis-target a test: it is **not** true that Claude
//! "enters at byte 67 and never leaves". `boot-exit` enters at 67 and **does** leave, at 5866 —
//! it is the clean-`/exit` capture, and §5.3 L1147 says the closing `?1049l` is present exactly
//! when the session exited cleanly. The capture that never leaves is `boot-help-status-resize`,
//! which enters at **1900**, after the trust dialog. Two different captures, two different
//! offsets. The conclusion — a bare tail is wrong for Claude Code — survives either way, and in
//! fact holds for both: a cut between 67 and 5866 is equally mid-alt-screen.)*
//!
//! # Why this is a scan and not a parser
//!
//! [`Sticky::scan`] tracks a **fixed set of six** DECSET/DECRST numbers and the current size. It
//! does not maintain a cursor, a grid, a charset, or an SGR stack — `marion_term::Term` does all
//! of that, and duplicating any of it here would create a second VT whose disagreements with the
//! first are undiagnosable. The scan answers exactly one question: *what modes would already be
//! on if you had watched from the beginning?*
//!
//! The set is closed deliberately, and §5.3 L1221 is the authority for its membership: `?1049`
//! buffer switching and mouse modes `?1000/?1002/?1003/?1006` are required; **`?47` and `?1047`
//! are explicitly not needed for these two harnesses** and are therefore not tracked. `?2026`
//! (synchronized output) is deliberately absent too — it is a *bracket*, not a sticky mode, and a
//! preamble that opened one without closing it would leave the emulator buffering forever.

/// The private modes a replay must restore before a tail can be applied.
///
/// `Copy` and eight bytes wide: this is scanned across every record of a session, and an
/// allocation per record would put the scan on the wrong side of the cost that made a tail
/// attractive in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sticky {
    /// `?1049` — the alternate screen. The one whose absence corrupts the *user's* scrollback and
    /// not merely the node's rendering.
    pub alt_screen: bool,
    /// `?1000` — X11 button-event tracking.
    pub mouse_button: bool,
    /// `?1002` — button-event tracking with drag.
    pub mouse_drag: bool,
    /// `?1003` — any-event (motion) tracking.
    pub mouse_motion: bool,
    /// `?1006` — SGR extended coordinates. Not a tracking mode of its own: it changes the
    /// *encoding* of every report the three above produce. See [`crate::mouse`].
    pub mouse_sgr: bool,
    /// The size the pty was last set to, columns first.
    pub cols: u16,
    pub rows: u16,
}

impl Sticky {
    /// The state a session starts in: main screen, no mouse, at the header's size.
    pub fn initial(cols: u16, rows: u16) -> Self {
        Self {
            alt_screen: false,
            mouse_button: false,
            mouse_drag: false,
            mouse_motion: false,
            mouse_sgr: false,
            cols,
            rows,
        }
    }

    /// Fold one chunk of harness output into the state.
    ///
    /// **Chunk-boundary caveat, stated rather than hidden:** this is a literal scan over one
    /// string, so a `?1049h` split across two `o` records is not seen. That is sound *here* and
    /// nowhere else, for a reason specific to this input: a `pty.cast` `o` record is one whole
    /// `read()` from the master, and the scan is run over **every** record from the session start,
    /// so the only sequences it can miss are ones the kernel itself split. Across all five
    /// committed captures no tracked sequence is split — asserted by
    /// `no_tracked_sequence_straddles_a_record_boundary_in_any_capture`, which is what turns this
    /// from an assumption into a measurement. If a future capture breaks it, that test fails
    /// rather than the preamble silently losing a mode.
    pub fn absorb(&mut self, chunk: &str) {
        let b = chunk.as_bytes();
        let mut i = 0;
        while let Some(m) = next_private_mode(b, i) {
            for param in m.params.split(|&c| c == b';') {
                self.apply(param, m.set);
            }
            i = m.next;
        }
    }

    /// The tracked set, and nothing else. An untracked parameter is ignored rather than recorded:
    /// see the module header for why membership is closed and who decides it.
    fn apply(&mut self, param: &[u8], set: bool) {
        match param {
            b"1049" => self.alt_screen = set,
            b"1000" => self.mouse_button = set,
            b"1002" => self.mouse_drag = set,
            b"1003" => self.mouse_motion = set,
            b"1006" => self.mouse_sgr = set,
            _ => {}
        }
    }

    /// Record a resize, so the preamble can restore the size the tail was painted at.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
    }

    /// The bytes that put a fresh emulator into this state.
    ///
    /// **Only `h` sequences, never `l`.** The emulator this is fed to is constructed fresh for the
    /// attach, so every tracked mode is already off; emitting `?1049l` to express "not on the alt
    /// screen" would be a no-op at best, and at worst — for a client that reuses a grid across
    /// re-attaches — an instruction to *leave* a screen it was legitimately on. Restoring state
    /// means asserting what is true, not narrating what is false.
    ///
    /// The size is **not** in here: it is applied with [`marion_term::Term::resize`], because a
    /// terminal's dimensions are a property of the grid and not something a stream can request.
    pub fn preamble(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // Alt screen first: the mouse modes are independent of it, but a reader tracing this
        // output should see the buffer chosen before anything is drawn into it.
        if self.alt_screen {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        if self.mouse_button {
            out.extend_from_slice(b"\x1b[?1000h");
        }
        if self.mouse_drag {
            out.extend_from_slice(b"\x1b[?1002h");
        }
        if self.mouse_motion {
            out.extend_from_slice(b"\x1b[?1003h");
        }
        if self.mouse_sgr {
            out.extend_from_slice(b"\x1b[?1006h");
        }
        out
    }
}

/// One `CSI ? <params> <h|l>` the scanner found: the parameter bytes as they appeared, whether it
/// sets or resets, and where scanning resumes.
///
/// Recognising a sequence and deciding what it *means* are two jobs, and they are separated here
/// because only the second one is allowed to grow: the tracked set is closed (see the module
/// header), while this half is just "where is the next private-mode sequence" and has no opinion
/// about which numbers matter.
struct PrivateMode<'a> {
    params: &'a [u8],
    set: bool,
    next: usize,
}

/// The next `CSI ? <params> <h|l>` at or after `from`, or `None` when the chunk holds no more.
fn next_private_mode(b: &[u8], from: usize) -> Option<PrivateMode<'_>> {
    let mut i = from;
    // `i + 3 < b.len()`: an introducer with nothing after it cannot be a complete sequence, and
    // the chunk-boundary caveat on `absorb` is why a partial one is dropped rather than carried.
    while i + 3 < b.len() {
        if !is_private_csi(b, i) {
            i += 1;
            continue;
        }
        let start = i + 3;
        let end = params_end(b, start);
        let Some(set) = final_byte(b, end) else {
            i += 1;
            continue;
        };
        return Some(PrivateMode {
            params: &b[start..end],
            set,
            next: end + 1,
        });
    }
    None
}

/// Does a `CSI ?` introducer start at `i`? Callers guarantee `i + 2` is in bounds.
fn is_private_csi(b: &[u8], i: usize) -> bool {
    b[i] == 0x1b && b[i + 1] == b'[' && b[i + 2] == b'?'
}

/// The end of the parameter run starting at `from`. Parameters are digits and `;`.
fn params_end(b: &[u8], from: usize) -> usize {
    let mut j = from;
    while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
        j += 1;
    }
    j
}

/// `Some(true)` for `h`, `Some(false)` for `l`, and `None` for anything else — including the end
/// of the chunk, which is a sequence the scan never completes rather than one it half-applies.
fn final_byte(b: &[u8], j: usize) -> Option<bool> {
    match b.get(j) {
        Some(b'h') => Some(true),
        Some(b'l') => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> Sticky {
        let mut st = Sticky::initial(80, 24);
        st.absorb(s);
        st
    }

    #[test]
    fn alt_screen_is_sticky_across_the_whole_session() {
        assert!(scan("\x1b[?1049h and then a great deal of painting").alt_screen);
    }

    #[test]
    fn leaving_the_alt_screen_clears_it() {
        assert!(!scan("\x1b[?1049h body \x1b[?1049l").alt_screen);
    }

    #[test]
    fn the_four_mouse_modes_are_tracked_separately() {
        let st = scan("\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
        assert!(st.mouse_button && st.mouse_drag && st.mouse_motion && st.mouse_sgr);
        let st = scan("\x1b[?1000h\x1b[?1006h");
        assert!(st.mouse_button && st.mouse_sgr);
        assert!(
            !st.mouse_drag && !st.mouse_motion,
            "unset modes must stay unset"
        );
    }

    #[test]
    fn a_multi_parameter_decset_sets_every_parameter() {
        // Neither committed harness emits this form, but the grammar allows it and a scan that
        // read only the first parameter would drop three modes silently.
        let st = scan("\x1b[?1000;1002;1006h");
        assert!(st.mouse_button && st.mouse_drag && st.mouse_sgr);
    }

    #[test]
    fn untracked_private_modes_do_not_disturb_the_tracked_ones() {
        // ?47 and ?1047 are explicitly out of scope (§5.3 L1221) and must not be mistaken for
        // ?1049 by a sloppy prefix match.
        let st = scan("\x1b[?47h\x1b[?1047h\x1b[?2004h\x1b[?25l");
        assert!(!st.alt_screen, "?47 and ?1047 are not ?1049");
    }

    #[test]
    fn a_bare_csi_without_the_question_mark_is_not_a_private_mode() {
        // CSI 1049 h with no '?' is not DECSET. Matching it would be a real bug: `ESC[4h` is
        // insert mode, and a scan confusing the two would toggle the alt screen on ordinary text.
        assert!(!scan("\x1b[1049h").alt_screen);
    }

    #[test]
    fn the_preamble_asserts_only_what_is_true() {
        let mut st = Sticky::initial(80, 24);
        assert!(
            st.preamble().is_empty(),
            "a fresh session needs no preamble"
        );
        st.alt_screen = true;
        st.mouse_sgr = true;
        assert_eq!(st.preamble(), b"\x1b[?1049h\x1b[?1006h".to_vec());
    }

    #[test]
    fn the_preamble_never_emits_a_reset() {
        // See `preamble`'s doc: the grid is fresh, so `l` is at best a no-op and at worst wrong.
        let st = scan("\x1b[?1049h\x1b[?1000h\x1b[?1049l\x1b[?1000l");
        let p = String::from_utf8(st.preamble()).unwrap();
        assert!(!p.contains('l'), "found a reset in the preamble: {p:?}");
        assert!(p.is_empty());
    }

    #[test]
    fn a_preamble_round_trips_through_its_own_scan() {
        // The preamble's whole contract: feeding it to a fresh scanner reproduces the state.
        let st = scan("\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        let mut back = Sticky::initial(st.cols, st.rows);
        back.absorb(&String::from_utf8(st.preamble()).unwrap());
        assert_eq!(back, st);
    }

    #[test]
    fn a_truncated_escape_at_the_end_of_a_chunk_does_not_panic_or_match() {
        for tail in ["\x1b", "\x1b[", "\x1b[?", "\x1b[?1", "\x1b[?1049"] {
            let st = scan(tail);
            assert!(!st.alt_screen, "incomplete {tail:?} must not set a mode");
        }
    }

    #[test]
    fn resize_is_recorded_columns_first() {
        let mut st = Sticky::initial(80, 24);
        st.resize(140, 45);
        assert_eq!((st.cols, st.rows), (140, 45));
    }
}
