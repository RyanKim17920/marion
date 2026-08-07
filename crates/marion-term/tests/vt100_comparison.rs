//! §11 item 10, half two: the `vt100`-vs-`alacritty` scrollback comparison, derived.
//!
//! §5.3 disqualifies `vt100` on the strength of a spike that retained **0** scrollback lines
//! against alacritty's 94. Nothing in the tree re-derived that 0, so it stood as an uncommitted
//! measurement — the exact shape of claim §11 item 10 exists to list. This file derives it from
//! the same committed `tests/fixtures/s2/*.raw.bin` captures the alacritty half uses.
//!
//! `vt100` is a **dev-dependency of this crate only**. It is evidence, never a component: marion
//! ships one VT and it is `alacritty_terminal`.
//!
//! **A number alone would not be worth committing.** Three different faults produce a `0` here —
//! vt100 modelling no scrollback at all, this file measuring it wrongly, and the cause §5.3
//! actually alleges (history dropped under a top-anchored *partial* DECSTBM region). The first two
//! are ruled out below before the third is asserted.

mod fixtures;

use marion_term::{Options, Size, Term};

/// Retained scrollback: `(rows, non-blank rows)`.
///
/// `vt100` exposes no `history_size()`. It exposes `set_scrollback`, which clamps to the real
/// scrollback length, and `scrollback()`, which reports where it landed — so an unsatisfiable
/// request reads back the length. Rows are then read one at a time by scrolling to each.
fn vt100_retained(p: &mut vt100::Parser, cols: u16) -> (usize, usize) {
    p.screen_mut().set_scrollback(usize::MAX);
    let rows = p.screen().scrollback();
    let non_blank = (1..=rows)
        .filter(|&n| {
            p.screen_mut().set_scrollback(n);
            !p.screen().contents_between(0, 0, 0, cols).trim().is_empty()
        })
        .count();
    (rows, non_blank)
}

/// Replay one capture into both emulators, at the same size and with the same resizes.
///
/// alacritty runs with `CSI 3J` suppressed, which is marion's shipping policy and the kinder of
/// the two columns for vt100 — vt100 does not implement `CSI 3J` at all
/// (`Screen::ed` handles modes 0/1/2 and drops the rest), so suppressed-alacritty is the only
/// like-for-like comparison. `vt100_ignores_erase_saved_so_the_comparison_is_like_for_like`
/// pins that.
fn replay_both(name: &str) -> (Term, vt100::Parser) {
    let cap = fixtures::load(name);
    let mut alac = Term::with_options(Size::new(cap.cols, cap.rows), Options::default());
    let mut vt = vt100::Parser::new(cap.rows as u16, cap.cols as u16, 10_000);
    for (bytes, resize) in cap.segments() {
        alac.advance(bytes);
        vt.process(bytes);
        if let Some((cols, rows)) = resize {
            alac.resize(Size::new(cols, rows));
            vt.screen_mut().set_size(rows as u16, cols as u16);
        }
    }
    (alac, vt)
}

// ---------------------------------------------------------------------------------------------
// Fault 1 and 2: vt100 does model scrollback, and this file can read it.
// ---------------------------------------------------------------------------------------------

/// Twelve lines through a 6-row screen with **no** scroll region set. Both emulators retain 7.
///
/// Without this, every `0` below would be equally well explained by `vt100` having no scrollback
/// at all, or by [`vt100_retained`] being wrong.
#[test]
fn vt100_does_model_scrollback_when_nothing_restricts_the_region() {
    let (vt, alac) = scroll_twelve_lines(None);
    assert_eq!(vt, (7, 7), "vt100 retains history when it is allowed to");
    assert_eq!(
        alac, 7,
        "and alacritty agrees exactly, so 7 is not an artefact"
    );
}

// ---------------------------------------------------------------------------------------------
// Fault 3: the cause §5.3 alleges, isolated.
// ---------------------------------------------------------------------------------------------

/// The same twelve lines under four DECSTBMs. Only the region changes.
///
/// `vt100`'s `Grid::scroll_up` pushes a displaced row into scrollback only when
/// `!scroll_region_active()`, and `scroll_region_active()` is
/// `scroll_top != 0 || scroll_bottom != rows - 1`. So **top-anchored is not enough** — the region
/// must also reach the last row. `alacritty` requires only the top anchor.
///
/// The `1;5` row is the whole finding: identical bytes, identical top anchor, and vt100 keeps
/// nothing while alacritty keeps more than it did unrestricted (8, not 7 — a 5-row region rotates
/// an extra line out of a 6-row screen).
#[test]
fn vt100_drops_history_under_a_top_anchored_partial_region_and_alacritty_does_not() {
    // (DECSTBM, vt100 retained, alacritty retained)
    let table = [
        (None, 7, 7),
        (Some((1, 6)), 7, 7), // top-anchored *and* full-height: vt100 keeps it
        (Some((1, 5)), 0, 8), // top-anchored, partial: the disagreement
        (Some((2, 6)), 0, 0), // not top-anchored: §5.3's "both drop it" correction
    ];
    for (region, want_vt, want_alac) in table {
        let (vt, alac) = scroll_twelve_lines(region);
        assert_eq!(
            (vt.0, alac),
            (want_vt, want_alac),
            "DECSTBM {region:?}: vt100 kept {} rows, alacritty {alac}",
            vt.0
        );
    }
}

/// vt100 never sees `CSI 3J`, so its column needs no suppressed/honoured split.
///
/// This is what makes the table's vt100 column comparable to alacritty's *suppressed* column
/// rather than its honoured one: vt100 is already behaving as if suppression were on.
#[test]
fn vt100_ignores_erase_saved_so_the_comparison_is_like_for_like() {
    let mut vt = vt100::Parser::new(6, 20, 10_000);
    let mut honouring = Term::with_options(
        Size::new(20, 6),
        Options {
            suppress_erase_saved: false,
            ..Options::default()
        },
    );
    for i in 0..12 {
        let line = format!("line{i}\r\n");
        vt.process(line.as_bytes());
        honouring.advance(line.as_bytes());
    }
    assert_eq!(
        (vt100_retained(&mut vt, 20).0, honouring.history_size()),
        (7, 7)
    );

    vt.process(b"\x1b[3J");
    honouring.advance(b"\x1b[3J");
    assert_eq!(
        (vt100_retained(&mut vt, 20).0, honouring.history_size()),
        (7, 0),
        "vt100 drops CSI 3J on the floor; alacritty honouring it calls clear_history()"
    );
}

// ---------------------------------------------------------------------------------------------
// The table.
// ---------------------------------------------------------------------------------------------

/// **§11 item 10's vt100 column, measured.** Confirms §5.3's verdict for two of the three
/// captures and sharpens the third: vt100 does not retain 0 *everywhere*, it retains 26 on
/// codex 0.145.0 against alacritty's 121.
///
/// | capture | alacritty (`3J` suppressed) | vt100 |
/// |---|---|---|
/// | codex 0.146.0 14-row heavy-history | 94 (82 non-blank) | 0 |
/// | codex 0.145.0 boot-status-help-diff-resize | 121 | 26 (21 non-blank) |
/// | codex 0.146.0 boot-status-help-resize | 16 (9 non-blank) | 0 |
///
/// The alacritty figures are asserted here too, in the same expression, so the two columns cannot
/// drift apart into separate tests that each pass against a different replay.
#[test]
fn vt100_retains_less_than_alacritty_on_every_capture_with_history() {
    for (name, alac_rows, vt_rows, vt_non_blank) in [
        (fixtures::CODEX_146_14ROW, 94, 0, 0),
        (fixtures::CODEX_145_DIFF_RESIZE, 121, 26, 21),
        (fixtures::CODEX_146_RESIZE, 16, 0, 0),
    ] {
        let (alac, mut vt) = replay_both(name);
        let (cols, rows) = (alac.size().cols, alac.size().rows);
        assert_eq!(
            vt.screen().size(),
            (rows as u16, cols as u16),
            "{name}: the two emulators must end the replay the same size, or the retention \
             figures are not comparable"
        );
        assert_eq!(
            (alac.history_size(), vt100_retained(&mut vt, cols as u16)),
            (alac_rows, (vt_rows, vt_non_blank)),
            "{name}"
        );
        assert!(
            alac.stats().top_anchored_partial_regions > 0,
            "{name}: vt100's loss here is only attributable to the region shape if the capture \
             actually sets one"
        );
    }
}

/// The two Claude captures retain 0 in **both** emulators — and that 0 has nothing to do with
/// scroll regions, because they set none.
///
/// Recorded so the table above cannot be misread as "vt100 always returns 0 and the region
/// explanation is decoration". Claude Code 2.1.220 spends its whole session on the alternate
/// screen (§5.3), which has no scrollback in either emulator by construction.
#[test]
fn the_claude_captures_lose_history_for_a_different_reason_entirely() {
    for name in [
        fixtures::CLAUDE_BOOT_EXIT,
        fixtures::CLAUDE_BOOT_HELP_STATUS_RESIZE,
    ] {
        let (alac, mut vt) = replay_both(name);
        let cols = alac.size().cols as u16;
        assert_eq!(
            (alac.history_size(), vt100_retained(&mut vt, cols).0),
            (0, 0),
            "{name}"
        );
        assert_eq!(
            alac.stats().top_anchored_partial_regions,
            0,
            "{name}: no DECSTBM here, so the region explanation must not be reached for"
        );
    }
}

/// Feed twelve lines through a 6-row screen under an optional 1-based `CSI top;bottom r`.
///
/// Returns `(vt100 (rows, non-blank), alacritty rows)`. Same bytes to both, in the same order.
fn scroll_twelve_lines(region: Option<(u16, u16)>) -> ((usize, usize), usize) {
    const COLS: u16 = 20;
    let mut vt = vt100::Parser::new(6, COLS, 10_000);
    let mut alac = Term::with_options(Size::new(COLS as usize, 6), Options::default());
    let mut feed = |bytes: &[u8]| {
        vt.process(bytes);
        alac.advance(bytes);
    };
    if let Some((top, bottom)) = region {
        feed(format!("\x1b[{top};{bottom}r").as_bytes());
    }
    for i in 0..12 {
        feed(format!("line{i}\r\n").as_bytes());
    }
    (vt100_retained(&mut vt, COLS), alac.history_size())
}
