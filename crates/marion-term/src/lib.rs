//! A VT screen model for marion's display plane (§5.3).
//!
//! Three things live here and nothing else:
//!
//! * [`Term`] — a grid fed by a **streaming** byte parser. A `read()` is not a frame; an escape
//!   sequence may be split across any number of `advance` calls and the result must be identical
//!   to feeding the same bytes whole.
//! * `CSI 3J` (erase-saved-lines) suppression, implemented as a [`vte::ansi::Handler`] wrapper
//!   that overrides exactly one case and delegates the other seventy-odd. Codex emits
//!   `ESC[r ESC[0m ESC[H ESC[2J ESC[3J ESC[H` on *every* resize; honouring the `3J` would call
//!   `Grid::clear_history` and take the transcript with it.
//! * [`render`] — a viewport-only adapter to a [`ratatui::buffer::Buffer`].
//!
//! Retained history is *not* in the viewport: `renderable_content()` delegates to `display_iter`,
//! which runs `-display_offset-1 ..= bottommost_line()`. Reading scrollback therefore needs
//! negative [`Line`] indexing, which is what [`scrollback_lines`](Term::scrollback_lines) does.

mod handler;
mod render;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term as ATerm};
use alacritty_terminal::vte::ansi::Processor;

pub use alacritty_terminal::vte;
pub use handler::Suppressor;
pub use render::render;

/// Terminal dimensions, the minimum [`Dimensions`] impl `Term::new`/`Term::resize` need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: usize,
    pub rows: usize,
}

impl Size {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Bytes one retained grid cell costs, taken from the type rather than from a comment.
///
/// A client choosing a scrollback bound is really choosing a memory bound, and the conversion
/// factor is `columns × CELL_BYTES` per row. Writing that factor down as a literal is how it goes
/// stale: `alacritty_terminal::term::cell::Cell` is `#[repr(Rust)]` and its size is a property of
/// the version in `Cargo.lock`, not a constant of the format. **Measured 24 on
/// `alacritty_terminal` 0.26**, which is the ~3 kB a row at the 140 columns §5.3's captures resize
/// to. Taking it from `size_of` means a version bump moves the client's memory arithmetic with it
/// instead of silently invalidating it.
///
/// This is retained-cell cost only. It excludes the `Row` header and any `CellExtra` a hyperlink
/// or an underline colour allocates on the side, so a bound derived from it is a floor — which is
/// the safe direction for a cap.
pub const CELL_BYTES: usize = std::mem::size_of::<Cell>();

/// How the screen should behave. Only the choices a caller can reasonably differ on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Suppress `CSI 3J`. Default `true` — see the module docs.
    ///
    /// `false` exists so a test can *measure* what honouring it costs rather than asserting that
    /// it costs something; §11 item 10 asks for exactly that number.
    pub suppress_erase_saved: bool,
    /// Scrollback capacity handed to `alacritty_terminal`.
    pub scrolling_history: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            suppress_erase_saved: true,
            scrolling_history: 10_000,
        }
    }
}

/// Counters the [`Suppressor`] maintains as the stream is parsed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Completed DECSET 2026 (synchronized output) brackets — the "assert now" boundary.
    ///
    /// Keyed on the bracket and never on cursor movement: §5.3 measured 1 of 8 and 3 of 19 frames
    /// with no CUP at all in the two Claude captures, so a CUP heuristic is already refuted.
    pub frames: usize,
    /// Frames that contained at least one CUP. Recorded only to keep that refutation testable.
    pub frames_with_cup: usize,
    /// `CSI 3J`s swallowed rather than passed to `Grid::clear_history`.
    pub suppressed_erase_saved: usize,
    /// `CSI 3J`s that reached the grid (always 0 when suppression is on).
    pub honoured_erase_saved: usize,
    /// DECSTBM regions that are top-anchored (`top == 1`) but stop short of the last row.
    ///
    /// This is the exact shape under which the two emulators disagree: `vt100`'s
    /// `Grid::scroll_up` pushes a displaced row into scrollback only when
    /// `!scroll_region_active()`, and its `scroll_region_active()` is
    /// `scroll_top != 0 || scroll_bottom != rows - 1` — so a top-anchored *partial* region
    /// silently discards history, while alacritty rotates it in. Counted so the
    /// vt100-vs-alacritty comparison can assert its own *cause* and not just its result.
    pub top_anchored_partial_regions: usize,
    cup_in_frame: bool,
    in_frame: bool,
}

impl Stats {
    /// Whether a DECSET 2026 bracket is currently **open**.
    ///
    /// Exposed because a viewer needs it and cannot derive it: `alacritty_terminal` deliberately
    /// drops `SyncUpdate` on the floor (`term/mod.rs` answers `NamedPrivateMode::SyncUpdate => ()`
    /// in both `set_private_mode` and `unset_private_mode`), so `TermMode` never carries the bit
    /// and there is nothing to query on the grid. The alternative for a client is to scan the byte
    /// stream for `?2026h`/`?2026l` itself — a second parser, disagreeing with this one at exactly
    /// the chunk boundaries [`Suppressor::unset_private_mode`] documents.
    ///
    /// A client withholds its paint while this is true: a half-applied frame is the thing
    /// synchronized output exists to hide.
    pub fn in_frame(&self) -> bool {
        self.in_frame
    }
}

/// A VT screen: alacritty's grid, a streaming parser, and the `CSI 3J` policy.
pub struct Term {
    inner: ATerm<VoidListener>,
    parser: Processor<StickyTimeout>,
    stats: Stats,
    options: Options,
}

impl Term {
    pub fn new(size: Size) -> Self {
        Self::with_options(size, Options::default())
    }

    pub fn with_options(size: Size, options: Options) -> Self {
        let config = Config {
            scrolling_history: options.scrolling_history,
            ..Config::default()
        };
        Self {
            inner: ATerm::new(config, &size, VoidListener),
            parser: Processor::default(),
            stats: Stats::default(),
            options,
        }
    }

    /// Feed bytes. **Streaming**: any split of the same byte sequence produces the same grid.
    pub fn advance(&mut self, bytes: &[u8]) {
        let mut sink = Suppressor {
            term: &mut self.inner,
            stats: &mut self.stats,
            suppress: self.options.suppress_erase_saved,
        };
        self.parser.advance(&mut sink, bytes);
    }

    /// Apply a pty resize (`TIOCSWINSZ` + `SIGWINCH` in a real session).
    pub fn resize(&mut self, size: Size) {
        self.inner.resize(size);
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The underlying emulator, for callers that need more than the two accessors below.
    pub fn inner(&self) -> &ATerm<VoidListener> {
        &self.inner
    }

    pub fn size(&self) -> Size {
        Size::new(self.inner.columns(), self.inner.screen_lines())
    }

    /// Retained history above the viewport, oldest first, trailing blanks trimmed.
    ///
    /// This is the half `renderable_content()` cannot reach.
    pub fn scrollback_lines(&self) -> Vec<String> {
        let grid = self.inner.grid();
        let history = grid.history_size();
        (0..history)
            .map(|i| line_text(self.inner.grid(), Line(-(history as i32) + i as i32)))
            .collect()
    }

    /// The viewport, top row first, trailing blanks trimmed.
    pub fn viewport_lines(&self) -> Vec<String> {
        let grid = self.inner.grid();
        let offset = grid.display_offset() as i32;
        (0..grid.screen_lines())
            .map(|i| line_text(grid, Line(i as i32 - offset)))
            .collect()
    }

    /// Retained history rows, blank ones included.
    pub fn history_size(&self) -> usize {
        self.inner.grid().history_size()
    }

    /// Whether the grid is currently on the **alternate screen** (`?1049h`).
    ///
    /// §9's M3 criterion C1 asks that the alt-screen switch be *handled*, and until this existed
    /// there was no way to ask: a caller could read the cells and see that they had changed, which
    /// a repaint on the main screen produces too. This is the distinction itself.
    ///
    /// It is also the fact behind §5.3's reading that C2's subject must be codex — an alternate
    /// screen has no scrollback, so a `CSI 3J` assertion against a harness that is on one passes
    /// because there was nothing to lose.
    pub fn on_alt_screen(&self) -> bool {
        self.inner
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
    }

    /// Non-blank retained history lines. The figure §5.3's scrollback claim is about.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback_lines()
            .iter()
            .filter(|l| !l.is_empty())
            .count()
    }
}

fn line_text(grid: &alacritty_terminal::grid::Grid<Cell>, line: Line) -> String {
    let mut text = String::new();
    for col in 0..grid.columns() {
        let cell = &grid[Point::new(line, Column(col))];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(cell.c);
    }
    text.truncate(text.trim_end().len());
    text
}

/// A [`vte::ansi::Timeout`] that never expires on its own.
///
/// vte buffers everything between `?2026h` and `?2026l` and replays it in one go at the `l`. A
/// wall-clock timeout would let a long frame flush early and split it; for fixture replay there is
/// no wall clock at all. So the bracket — and only the bracket — ends the frame.
#[derive(Debug, Default)]
pub struct StickyTimeout {
    pending: bool,
}

impl vte::ansi::Timeout for StickyTimeout {
    fn set_timeout(&mut self, _duration: std::time::Duration) {
        self.pending = true;
    }

    fn clear_timeout(&mut self) {
        self.pending = false;
    }

    fn pending_timeout(&self) -> bool {
        self.pending
    }
}

/// The custom assertion L4.5 names (§8).
///
/// Asserts the count of non-blank retained scrollback lines, and prints the lines themselves on
/// failure — a bare `assert_eq!(94, 2)` says nothing about *which* history was lost.
#[track_caller]
pub fn assert_scrollback_lines(term: &Term, expected: usize) {
    let lines = term.scrollback_lines();
    let actual = lines.iter().filter(|l| !l.is_empty()).count();
    if actual != expected {
        let sample: Vec<&str> = lines
            .iter()
            .filter(|l| !l.is_empty())
            .map(String::as_str)
            .take(5)
            .collect();
        panic!(
            "scrollback: expected {expected} non-blank retained lines, found {actual} \
             (history_size={}, suppressed_3j={}, honoured_3j={}, frames={})\n  first: {sample:#?}",
            term.inner.grid().history_size(),
            term.stats.suppressed_erase_saved,
            term.stats.honoured_erase_saved,
            term.stats.frames,
        );
    }
}
