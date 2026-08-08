//! Replay the committed `tests/fixtures/s2` pty captures through the grid.
//!
//! Every number asserted here was *measured* from these captures, not carried over from §5.3 —
//! several of §5.3's figures are self-flagged as unverified (§11 item 10) and this file is the
//! first Rust to check them.

mod fixtures;

use marion_term::{Options, Size, Term, assert_scrollback_lines, render};

/// Replay a capture whole, applying its resizes at the byte offsets the asciicast records.
fn replay(name: &str, suppress: bool) -> Term {
    replay_chunked(name, suppress, usize::MAX)
}

/// Replay a capture in chunks of at most `chunk` bytes. `usize::MAX` means "whole".
fn replay_chunked(name: &str, suppress: bool, chunk: usize) -> Term {
    let cap = fixtures::load(name);
    let mut term = Term::with_options(
        Size::new(cap.cols, cap.rows),
        Options {
            suppress_erase_saved: suppress,
            ..Options::default()
        },
    );
    for (bytes, resize) in cap.segments() {
        if chunk == usize::MAX {
            term.advance(bytes);
        } else {
            for piece in bytes.chunks(chunk) {
                term.advance(piece);
            }
        }
        if let Some((cols, rows)) = resize {
            term.resize(Size::new(cols, rows));
        }
    }
    term
}

/// Everything a replay produced, as text — the snapshot payload and the equivalence key.
fn transcript(term: &Term) -> String {
    let stats = term.stats();
    let mut out = String::new();
    out.push_str(&format!(
        "size: {}x{}\nframes: {} ({} with a CUP)\nerase-saved: {} suppressed, {} honoured\n\
         history: {} rows, {} non-blank\n",
        term.size().cols,
        term.size().rows,
        stats.frames,
        stats.frames_with_cup,
        stats.suppressed_erase_saved,
        stats.honoured_erase_saved,
        term.history_size(),
        term.scrollback_len(),
    ));
    out.push_str("\n--- scrollback (oldest first) ---\n");
    for line in term.scrollback_lines() {
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str("\n--- viewport ---\n");
    for line in term.viewport_lines() {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Snapshots: one per committed capture.
// ---------------------------------------------------------------------------------------------

macro_rules! snapshot_test {
    ($test:ident, $capture:expr) => {
        #[test]
        fn $test() {
            insta::assert_snapshot!(transcript(&replay($capture, true)));
        }
    };
}

snapshot_test!(snapshot_claude_boot_exit, fixtures::CLAUDE_BOOT_EXIT);
snapshot_test!(
    snapshot_claude_boot_help_status_resize,
    fixtures::CLAUDE_BOOT_HELP_STATUS_RESIZE
);
snapshot_test!(
    snapshot_codex_145_diff_resize,
    fixtures::CODEX_145_DIFF_RESIZE
);
snapshot_test!(snapshot_codex_146_14row, fixtures::CODEX_146_14ROW);
snapshot_test!(snapshot_codex_146_resize, fixtures::CODEX_146_RESIZE);

// ---------------------------------------------------------------------------------------------
// The load-bearing one: scrollback survives Codex's resize, and only because of suppression.
// ---------------------------------------------------------------------------------------------

/// `codex-cli-0.146.0-boot-status-help-resize` resizes twice (120x40 → 100x24 → 140x45) and emits
/// `ESC[r ESC[0m ESC[H ESC[2J ESC[3J ESC[H` each time. Its cast is byte-identical to its raw.bin,
/// so the resizes land at the right byte offsets.
///
/// **§11 item 10, half one.** Retained history across that session:
/// suppressed 16 rows / 9 non-blank, honoured 1 row / 0 non-blank.
#[test]
fn scrollback_survives_codex_resize() {
    let kept = replay(fixtures::CODEX_146_RESIZE, true);
    assert_eq!(
        kept.stats().suppressed_erase_saved,
        2,
        "one CSI 3J per resize"
    );
    assert_eq!(kept.history_size(), 16);
    assert_scrollback_lines(&kept, 9);

    let lost = replay(fixtures::CODEX_146_RESIZE, false);
    assert_eq!(lost.stats().honoured_erase_saved, 2);
    assert_eq!(
        lost.history_size(),
        1,
        "clear_history() runs on the last resize"
    );
    assert_scrollback_lines(&lost, 0);
}

// ---------------------------------------------------------------------------------------------
// §9's M3 criterion C1: the alt-screen switch, including the leg that never arrives
// ---------------------------------------------------------------------------------------------

/// **The switch is handled, and the *restore* is not depended on.**
///
/// §9's C1 names *"alt-screen switch handled"* and §5.3 measures where: Claude Code enters
/// `?1049h` at byte **67** on `claude-2.1.220-boot-exit` and at byte **1900** on
/// `claude-2.1.220-boot-help-status-resize` — the run with the trust dialog, which is the clause
/// about pre-alt-screen output on the main screen. The two captures differ in exactly the way that
/// matters: the first exits cleanly and emits its `?1049l` at 5866; the second **never emits one
/// at all**. So the restore leg is frequently not exercised, and an emulator that only left the
/// alternate screen when told to would be right on one capture and wrong on the other.
///
/// This asserts the transition at both ends and in both directions, which is what makes it an
/// assertion about *handling* rather than about the byte being present:
///
/// * before the switch the grid is on the main screen (so the trust dialog is painted there);
/// * after it the grid is on the alternate screen;
/// * on the capture that restores, the grid comes **back**;
/// * on the capture that does not, the grid stays — it does not guess.
///
/// Why this lives here and not in `marion-supervisor/tests/pane_attach.rs` with the rest of C1:
/// **claude 2.1.225 no longer takes the switch at all.** Probed on 2026-08-08 under a marion pane
/// and again bare, at 100x30 through a boot, a trust dialog, a submitted turn and a resize: zero
/// `?1049h`, zero mouse modes, and only `?1004`, `?2004`, `?2026` and `?2031`. §5.3's reading is a
/// 2.1.220 reading and this corpus is where it is still live, so the clause is pinned against the
/// bytes it was measured from. The live test asserts the other half — that marion's grid agrees
/// with whichever screen the node's own bytes put it on.
#[test]
fn the_alt_screen_switch_is_handled_including_the_restore_that_never_arrives() {
    for (name, at, restores) in [
        (fixtures::CLAUDE_BOOT_EXIT, 67usize, true),
        (fixtures::CLAUDE_BOOT_HELP_STATUS_RESIZE, 1900usize, false),
    ] {
        let cap = fixtures::load(name);
        assert!(
            cap.raw[at..].starts_with(b"\x1b[?1049h"),
            "{name}: §5.3 puts the switch at byte {at} and it is not there. The offsets below are \
             read off this capture, so a capture that moved makes every one of them meaningless"
        );

        let mut term = Term::with_options(
            Size::new(cap.cols, cap.rows),
            Options {
                suppress_erase_saved: true,
                ..Options::default()
            },
        );
        term.advance(&cap.raw[..at]);
        assert!(
            !term.on_alt_screen(),
            "{name}: the grid was already on the alternate screen before byte {at}, so nothing \
             this capture paints first is pre-alt-screen output"
        );

        term.advance(&cap.raw[at..at + b"\x1b[?1049h".len()]);
        assert!(
            term.on_alt_screen(),
            "{name}: the grid did not switch buffers on `?1049h`"
        );

        term.advance(&cap.raw[at + b"\x1b[?1049h".len()..]);
        assert_eq!(
            term.on_alt_screen(),
            !restores,
            "{name}: the grid ends on the wrong screen. This capture {} a `?1049l`",
            if restores { "sends" } else { "never sends" }
        );
    }
}

/// **§11 item 10, half two — the `121 → 2` claim.**
///
/// `codex-cli-0.145.0-boot-status-help-diff-resize` retains **121** history rows with `CSI 3J`
/// suppressed and **25** without. 121 is exactly the figure §5.3 cites. The collapse target is
/// **not** 2: the last `CSI 3J` lands mid-session, and the 25 rows scrolled up afterwards survive
/// it. Recorded here as the measured value.
///
/// This capture's cast is one of the three damaged ones, so it replays at a fixed 120x40 with no
/// resize applied; the `CSI 3J`s are in the byte stream regardless, which is what this asserts.
#[test]
fn erase_saved_collapses_history_on_codex_145() {
    let kept = replay(fixtures::CODEX_145_DIFF_RESIZE, true);
    let lost = replay(fixtures::CODEX_145_DIFF_RESIZE, false);
    assert_eq!((kept.history_size(), lost.history_size()), (121, 25));
    assert_eq!(kept.stats().suppressed_erase_saved, 2);
    assert_eq!(lost.stats().honoured_erase_saved, 2);
}

/// Ground truth from `s2/scrollattr.py`: the 14-row capture performs `2 + 92 = 94` top-anchored
/// scroll-ups, all of which alacritty must rotate into history. It emits no `CSI 3J` at all, so
/// this measures retention alone — §5.3's "alacritty's 94", now derived.
#[test]
fn retained_history_matches_scrollattr_ground_truth() {
    let term = replay(fixtures::CODEX_146_14ROW, true);
    assert_eq!(
        term.size(),
        Size::new(100, 14),
        "cast header, not extract.py's 120x40 default"
    );
    assert_eq!(
        term.history_size(),
        94,
        "2 + 92 top-anchored scroll-up lines"
    );
    assert_eq!(
        term.stats().suppressed_erase_saved,
        0,
        "this capture emits no CSI 3J"
    );
}

/// A `CSI 3J` split across four reads is still one `CSI 3J`. This is why suppression lives in the
/// `Handler` and not in a byte filter over the pty stream.
#[test]
fn erase_saved_suppressed_when_split_across_reads() {
    for suppress in [true, false] {
        let mut term = Term::with_options(
            Size::new(20, 3),
            Options {
                suppress_erase_saved: suppress,
                ..Options::default()
            },
        );
        term.advance(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert_eq!(
            term.history_size(),
            2,
            "two lines rotated out of a 3-row viewport"
        );

        for byte in [&b"\x1b"[..], b"[", b"3", b"J"] {
            term.advance(byte);
        }
        if suppress {
            assert_eq!(term.history_size(), 2);
            assert_eq!(term.stats().suppressed_erase_saved, 1);
        } else {
            assert_eq!(term.history_size(), 0, "clear_history() ran");
            assert_eq!(term.stats().honoured_erase_saved, 1);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// S11 MUST #1: a read() is not a frame.
// ---------------------------------------------------------------------------------------------

/// Feeding a capture one byte at a time must produce exactly the state feeding it whole produces.
///
/// This is the assertion vte 0.15 fails if you take its callbacks at face value: it invokes
/// `unset_private_mode(SyncUpdate)` 9 times for `claude-boot-exit`'s 8 brackets when fed whole and
/// 16 times when fed byte-wise, which is why `Stats::frames` is edge-triggered.
#[test]
fn byte_at_a_time_matches_whole_buffer() {
    for name in fixtures::ALL {
        assert_eq!(
            transcript(&replay_chunked(name, true, 1)),
            transcript(&replay(name, true)),
            "{name}: byte-at-a-time replay diverged from whole-buffer replay"
        );
    }
}

/// The same property at chunk sizes that actually split escape sequences at awkward places.
#[test]
fn arbitrary_chunking_matches_whole_buffer() {
    for name in fixtures::ALL {
        let whole = transcript(&replay(name, true));
        for chunk in [2usize, 3, 7, 64, 4096] {
            assert_eq!(
                transcript(&replay_chunked(name, true, chunk)),
                whole,
                "{name}: {chunk}-byte chunking diverged from whole-buffer replay"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The "assert now" boundary is the DECSET 2026 bracket, never a CUP.
// ---------------------------------------------------------------------------------------------

/// §5.3 measured Claude frames that contain no cursor movement at all — 1 of 8 in `boot-exit` and
/// 3 of 19 in `boot-help-status-resize`. Both figures reproduce exactly, so a harness that keyed
/// "assert now" on CUP would miss those frames entirely and fire on the wrong ones elsewhere.
#[test]
fn frame_boundary_is_the_sync_bracket_not_a_cup() {
    for (name, frames, with_cup) in [
        (fixtures::CLAUDE_BOOT_EXIT, 8, 7),
        (fixtures::CLAUDE_BOOT_HELP_STATUS_RESIZE, 19, 16),
        (fixtures::CODEX_145_DIFF_RESIZE, 52, 52),
        (fixtures::CODEX_146_14ROW, 45, 45),
        (fixtures::CODEX_146_RESIZE, 50, 50),
    ] {
        let stats = replay(name, true).stats();
        assert_eq!(
            (stats.frames, stats.frames_with_cup),
            (frames, with_cup),
            "{name}"
        );
    }

    // Stated as a property so the mutation is unambiguous: on both Claude captures the bracket
    // count and the CUP-frame count are different numbers.
    for name in [
        fixtures::CLAUDE_BOOT_EXIT,
        fixtures::CLAUDE_BOOT_HELP_STATUS_RESIZE,
    ] {
        let stats = replay(name, true).stats();
        assert!(
            stats.frames_with_cup < stats.frames,
            "{name}: a CUP heuristic would be indistinguishable from the bracket here"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Fixture invariants and the render adapter.
// ---------------------------------------------------------------------------------------------

/// `NOTES.txt`: the two 0.146.0 captures' casts match their raw.bin exactly; the other three do
/// not (9 damaged regions, 23 U+FFFD). The resize-offset derivation depends on which.
#[test]
fn cast_matches_raw_only_for_the_clean_captures() {
    for name in fixtures::ALL {
        let cap = fixtures::load(name);
        let clean = name.starts_with("codex-cli-0.146.0");
        assert_eq!(cap.cast_matches_raw, clean, "{name}");
        assert_eq!(cap.cast_output_bytes() == cap.raw, clean, "{name}");
    }
}

/// The adapter is viewport-only, and the viewport is what `render` puts in the buffer.
#[test]
fn render_covers_the_viewport_and_nothing_else() {
    let term = replay(fixtures::CODEX_146_14ROW, true);
    let buf = render(&term);
    assert_eq!(buf.area.width as usize, term.size().cols);
    assert_eq!(buf.area.height as usize, term.size().rows);
    assert!(
        term.history_size() > 0,
        "this capture has history the buffer must not contain"
    );

    let rendered: Vec<String> = (0..buf.area.height)
        .map(|y| {
            let mut row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap())
                .collect();
            row.truncate(row.trim_end().len());
            row
        })
        .collect();
    assert_eq!(rendered, term.viewport_lines());
}
