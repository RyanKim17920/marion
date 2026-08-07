//! **L4.5 — the self-hosted TUI driver** (§8). This is the layer that gates commits.
//!
//! §8 names three parts and they are all here: `TestBackend` + `insta` + `assert_scrollback_lines`,
//! with **DECSET 2026 brackets as the "assert now" signal**. No AI is in the loop and nothing here
//! spawns a process, so the whole file runs in milliseconds and a `pre-commit` hook can afford it.
//!
//! **What "marion hosts marion" means here.** It is the level's *name*. The driver replays recorded
//! harness bytes into marion's own grid and renders them through the same widget M3's TUI will use.
//! Standing up a second marion to satisfy the phrase would put the *supervisor* in the failure
//! path of a *rendering* assertion — precisely the ambiguity §8 gives as the reason L7 must not
//! gate.
//!
//! **`TestBackend` and `assert_scrollback_lines` are deliberately not the same assertion.** The
//! snapshot is of marion's rendered [`Buffer`](ratatui::buffer::Buffer) — the viewport, with
//! styles. `assert_scrollback_lines` is over the emulator's *retained history*, which the viewport
//! by construction does not contain. A style regression breaks the first and not the second; a
//! `CSI 3J` leaking through breaks the second and not the first.
//! [`the_two_assertions_cannot_substitute_for_each_other`] pins that.
//!
//! **The boundary is the bracket and only the bracket.** §5.3 measured Claude frames with no
//! cursor movement in them at all — 1 of 8 and 3 of 19 — so a CUP-keyed driver would miss frames
//! and fire on the wrong ones. And `unset_private_mode(SyncUpdate)` is *not* a frame counter: vte
//! calls it a number of times that depends on `read()` chunking, so
//! [`Stats::frames`](marion_term::Stats::frames) is edge-triggered on set→unset and this driver
//! renders when that counter moves, never when the callback fires.

mod fixtures;

use marion_term::{Options, Size, Term, assert_scrollback_lines};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

/// One replay, rendered at every frame boundary.
struct Session {
    term: Term,
    /// The screen as it stood at each completed DECSET 2026 bracket, one line each.
    ledger: Vec<String>,
    /// The last frame drawn, in full.
    last: Buffer,
}

/// Replay a capture, drawing through `Terminal<TestBackend>` at each "assert now" signal.
///
/// Bytes go in **one at a time**, and a frame is declared when
/// [`Stats::frames`](marion_term::Stats::frames) *increments*. Byte-at-a-time is a legitimate
/// chunking rather than a special case: `byte_at_a_time_matches_whole_buffer` in `replay.rs`
/// already proves the grid it produces is identical to feeding the capture whole, and it is the
/// finest granularity at which a boundary can be observed without asking vte to tell us — which
/// §5.3 established it cannot do reliably.
fn drive(name: &str) -> Session {
    let cap = fixtures::load(name);
    let mut term = Term::with_options(Size::new(cap.cols, cap.rows), Options::default());
    let mut terminal =
        Terminal::new(TestBackend::new(cap.cols as u16, cap.rows as u16)).expect("TestBackend");
    let mut ledger = Vec::new();
    let mut previous: Option<Buffer> = None;

    for (bytes, resize) in cap.segments() {
        for byte in bytes {
            let before = term.stats().frames;
            term.advance(std::slice::from_ref(byte));
            if term.stats().frames != before {
                let line = draw(&mut terminal, &term, previous.as_ref());
                previous = Some(terminal.backend().buffer().clone());
                ledger.push(line);
            }
        }
        if let Some((cols, rows)) = resize {
            term.resize(Size::new(cols, rows));
            terminal.resize(area(cols, rows)).expect("resize");
        }
    }
    let last = terminal.backend().buffer().clone();
    Session { term, ledger, last }
}

fn area(cols: usize, rows: usize) -> ratatui::layout::Rect {
    ratatui::layout::Rect::new(0, 0, cols as u16, rows as u16)
}

/// Draw the grid through the backend and summarise the result in one ledger line.
///
/// A full screen per frame would be 174 screens across the five captures. The summary keeps the
/// snapshot reviewable while still being a fingerprint: `changed` is the number of cells this
/// frame repainted, which is the thing that would move if frame *segmentation* drifted, and the
/// trailing text is the **last** non-blank row, which is where a TUI's newest output lands. The
/// final frame is snapshotted in full separately.
fn draw(terminal: &mut Terminal<TestBackend>, term: &Term, previous: Option<&Buffer>) -> String {
    terminal
        .draw(|frame| frame.render_widget(term, frame.area()))
        .expect("draw");
    let buf = terminal.backend().buffer();
    let rows = rows_of(buf);
    let changed = match previous {
        Some(prev) if prev.area == buf.area => prev
            .content()
            .iter()
            .zip(buf.content())
            .filter(|(a, b)| a != b)
            .count(),
        _ => buf.content().len(),
    };
    let last = rows
        .iter()
        .rfind(|r| !r.is_empty())
        .map_or("<blank>", String::as_str);
    format!(
        "{}x{} history={:>3} nonblank={:>2} changed={:>5} | {}",
        buf.area.width,
        buf.area.height,
        term.history_size(),
        rows.iter().filter(|r| !r.is_empty()).count(),
        changed,
        last.chars().take(64).collect::<String>(),
    )
}

fn rows_of(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let mut row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            row.truncate(row.trim_end().len());
            row
        })
        .collect()
}

/// The final frame in full, plus a census of the styles in it.
///
/// The style census is the half `replay.rs`'s text transcript cannot carry, and the only thing in
/// the tree that exercises `render::style_of` and `render::convert` at all — a bad
/// `NamedColor` → `Color` mapping is invisible to every other test here.
fn final_frame(session: &Session) -> String {
    let mut out = String::new();
    for row in rows_of(&session.last) {
        out.push_str(&row);
        out.push('\n');
    }
    out.push_str("\n--- styles, most cells first ---\n");
    let mut census: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for cell in session.last.content() {
        *census.entry(format!("{:?}", cell.style())).or_default() += 1;
    }
    let mut census: Vec<_> = census.into_iter().collect();
    census.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (style, count) in census {
        out.push_str(&format!("{count:>6}  {style}\n"));
    }
    out
}

/// The whole snapshot payload: the frame ledger, then the last frame in full.
fn report(session: &Session) -> String {
    format!(
        "frames: {}\n\n--- ledger, one line per DECSET 2026 bracket ---\n{}\n\n\
         --- final frame ---\n{}",
        session.ledger.len(),
        session.ledger.join("\n"),
        final_frame(session),
    )
}

// ---------------------------------------------------------------------------------------------
// The snapshots. One per capture: every frame boundary, then the last frame in full.
// ---------------------------------------------------------------------------------------------

macro_rules! l45 {
    ($test:ident, $capture:expr) => {
        #[test]
        fn $test() {
            let report = report(&drive($capture));
            insta::assert_snapshot!(report);
        }
    };
}

l45!(l45_claude_boot_exit, fixtures::CLAUDE_BOOT_EXIT);
l45!(
    l45_claude_boot_help_status_resize,
    fixtures::CLAUDE_BOOT_HELP_STATUS_RESIZE
);
l45!(l45_codex_145_diff_resize, fixtures::CODEX_145_DIFF_RESIZE);
l45!(l45_codex_146_14row, fixtures::CODEX_146_14ROW);
l45!(l45_codex_146_resize, fixtures::CODEX_146_RESIZE);

// ---------------------------------------------------------------------------------------------
// Properties of the driver itself. These are what stop the snapshots above from being vacuous.
// ---------------------------------------------------------------------------------------------

/// The driver draws exactly once per completed bracket — no more, no fewer, on every capture.
///
/// A snapshot of 45 identical blank screens would pass review and assert nothing. This is the
/// assertion that says the ledger has the shape it claims to.
#[test]
fn one_draw_per_sync_bracket_on_every_capture() {
    for name in fixtures::ALL {
        let session = drive(name);
        assert_eq!(
            session.ledger.len(),
            session.term.stats().frames,
            "{name}: draws and completed DECSET 2026 brackets must be the same number"
        );
        assert!(
            session.term.stats().frames > 0,
            "{name}: a capture with no brackets would make this level's signal untestable"
        );
    }
}

/// A CUP-keyed driver would draw a different number of times on the Claude captures.
///
/// §5.3's measurement (1 of 8, 3 of 19 frames with no cursor movement) is asserted in `replay.rs`;
/// what this adds is that the *driver* is the thing keyed on the bracket. Were `draw` moved to
/// `goto`, the ledger lengths here would stop matching `frames`.
#[test]
fn the_signal_is_the_bracket_and_a_cup_would_not_do() {
    for name in [
        fixtures::CLAUDE_BOOT_EXIT,
        fixtures::CLAUDE_BOOT_HELP_STATUS_RESIZE,
    ] {
        let session = drive(name);
        let stats = session.term.stats();
        assert!(
            stats.frames_with_cup < stats.frames,
            "{name}: this capture no longer distinguishes the two signals"
        );
        assert_eq!(session.ledger.len(), stats.frames, "{name}");
    }
}

/// The `TestBackend` snapshot and `assert_scrollback_lines` fail for **different** reasons.
///
/// Shown by exhibiting one fault each way, not by inspecting the buffer for history text — box
/// rules and blank frame edges recur legitimately in both views, so "no scrollback line appears
/// in the viewport" is false for reasons that have nothing to do with a leak.
///
/// * A `CSI 3J` reaching the grid destroys the transcript and leaves **every cell identical**.
///   Only `assert_scrollback_lines` moves.
/// * A colour change repaints the buffer and leaves the retained history **byte-identical**.
///   Only the snapshot moves.
///
/// If either half ever stopped holding, one of the two assertions would be redundant and a
/// regression in the concern it owns could hide inside the other's diff.
#[test]
fn the_two_assertions_cannot_substitute_for_each_other() {
    use marion_term::render;

    // The real capture, so the numbers below are not a toy: 14 viewport rows, 82 non-blank
    // history rows, and §8's named assertion holding over the second of those.
    let session = drive(fixtures::CODEX_146_14ROW);
    assert_scrollback_lines(&session.term, 82);
    assert_eq!(session.last.area.height, 14, "the buffer is the viewport");

    let scrolled = b"one\r\ntwo\r\nthree\r\nfour\r\nfive";
    let mut kept = Term::new(Size::new(20, 3));
    let mut lost = Term::with_options(
        Size::new(20, 3),
        Options {
            suppress_erase_saved: false,
            ..Options::default()
        },
    );
    for term in [&mut kept, &mut lost] {
        term.advance(scrolled);
        term.advance(b"\x1b[3J");
    }
    assert_eq!(
        render(&kept),
        render(&lost),
        "CSI 3J must not change one rendered cell, or the snapshot would catch it too"
    );
    assert_ne!(
        (kept.history_size(), lost.history_size()),
        (0, 0),
        "and it must change the scrollback, or assert_scrollback_lines would catch nothing"
    );
    assert_eq!((kept.history_size(), lost.history_size()), (2, 0));

    let mut recoloured = Term::new(Size::new(20, 3));
    recoloured.advance(b"\x1b[31m");
    recoloured.advance(scrolled);
    assert_eq!(
        recoloured.scrollback_lines(),
        kept.scrollback_lines(),
        "colour must not change the retained text, or assert_scrollback_lines would see it"
    );
    assert_ne!(
        render(&recoloured),
        render(&kept),
        "and it must change the buffer, or the TestBackend snapshot would see nothing"
    );
}

/// The widget clips into the area it is given and leaves the rest of the buffer alone.
///
/// M3 renders the grid inside a pane, not over the whole screen. Without this, a grid one row
/// taller than its pane would silently overwrite whatever the layout drew below it.
#[test]
fn the_widget_stays_inside_its_area() {
    use ratatui::widgets::Widget;

    let mut term = Term::new(Size::new(10, 3));
    term.advance(b"aaaaaaaaaa\r\nbbbbbbbbbb\r\ncccccccccc");

    let mut buf = Buffer::filled(area(14, 5), ratatui::buffer::Cell::new("."));
    (&term).render(ratatui::layout::Rect::new(2, 1, 6, 2), &mut buf);

    assert_eq!(
        rows_of(&buf),
        vec![
            "..............",
            "..aaaaaa......",
            "..bbbbbb......",
            "..............",
            "..............",
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// The gate is part of the level, so the gate is tested too.
// ---------------------------------------------------------------------------------------------

/// `.githooks/pre-commit` must actually name **this** target, and must expect **this many** tests.
///
/// The hook reads back its own pass count so it cannot silently no-op — but that check is only as
/// good as its threshold, and a threshold in a shell script drifts the moment a test is added or
/// the target is renamed. This is the other half of that latch: rename the target, delete a test,
/// or point the hook somewhere else, and the gate fails loudly instead of passing vacuously.
///
/// It also asserts the hook does **not** reach for the whole workspace. §8 is explicit that L7
/// must never gate a commit, and `cargo test --workspace` in a pre-commit hook is exactly how that
/// happens by accident once an L7 target exists.
#[test]
fn the_gate_names_this_target_and_only_this_target() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let hook = root.join(".githooks/pre-commit");
    let script = std::fs::read_to_string(&hook).expect(".githooks/pre-commit must exist");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&hook).expect("stat").permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "the hook is not executable ({mode:o}); git would skip it without a word"
        );
    }

    assert!(
        script.contains("TARGET=l45_driver"),
        "the hook does not name this target"
    );
    assert!(
        !script.contains("--workspace") && !script.contains("cargo test\n"),
        "the hook must run L4.5 by name; a blanket run would sweep in L7 the day it exists"
    );

    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/l45_driver.rs"),
    )
    .expect("this file");
    // Column-zero `#[test]` plus one per `l45!` invocation — the macro's own indented `#[test]`
    // is not at column zero, so neither branch double-counts.
    let tests = source.matches("\n#[test]\n").count() + source.matches("\nl45!(").count();
    let declared: usize = script
        .lines()
        .find_map(|l| l.strip_prefix("MIN_TESTS="))
        .expect("MIN_TESTS")
        .trim()
        .parse()
        .expect("MIN_TESTS is a number");
    assert_eq!(
        declared, tests,
        "the hook expects {declared} tests and this file defines {tests}; a gate that expects \
         fewer tests than exist will pass while some of them are missing"
    );
}
