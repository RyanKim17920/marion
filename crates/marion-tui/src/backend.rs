//! A `ratatui` [`Backend`] that paints onto a [`Screen`] — the piece M3 criterion C2 was missing.
//!
//! # Why this had to be written rather than imported
//!
//! `marion-tui` takes `ratatui` with `default-features = false`, so none of the four shipped
//! backends (`crossterm`, `termina`, `termion`, `termwiz`) is compiled in. `crates/marion-supervisor
//! /src/attach.rs` recorded that as *"there is no such backend in the tree"* and rendered
//! **pass-through** instead: the node's bytes straight to the operator's terminal.
//!
//! **That reading was half right and the half it got wrong is the load-bearing one.** The
//! *implementations* are behind cargo features; the `Backend` **trait** is not — it lives in
//! `ratatui-core`, which is unconditional, and `TestBackend` beside it is what
//! `marion-term/tests/l45_driver.rs` already drives. So the cost of putting the grid back in the
//! loop was never a dependency change. It was this file.
//!
//! # What routing through the grid buys, and what it costs
//!
//! Pass-through is correct for every escape sequence, including the ones marion has no opinion
//! about, and it makes the mouse work with no encoder because the operator's own SGR-1006 reports
//! are already the bytes the node expects. What it cannot do is **intercept**, because nothing is
//! parsing. Two things marion has decided are therefore not in effect on it:
//!
//! * [`crate::MAX_SCROLLBACK`] — the operator's terminal keeps whatever its own scrollback is.
//! * **`CSI 3J`** (erase-saved) — the node's request to wipe the scrollback reaches the operator's
//!   real terminal and wipes *theirs*. That is §9's M3 criterion C2, and it is the reason a grid
//!   marion owns is not an optimisation.
//!
//! The cost is a full parse and a full repaint per frame instead of a `write(2)`, and
//! `a_repaint_costs_one_write_of_the_whole_viewport` measures it rather than asserting it.
//!
//! # Full repaints, and why that is not laziness
//!
//! `Terminal::draw` already diffs against its own previous buffer and calls [`Backend::draw`] with
//! **only the changed cells**, so this backend is handed a diff even though it applies it
//! unconditionally. What it does not do is track which cells `marion-term` changed — that is
//! `Terminal`'s job, done once, in a place that is already tested.

use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

use crate::guard::Screen;

/// A `ratatui` backend whose one output is [`Screen::write`].
///
/// It **owns** the `Screen`. A borrowing backend would make a `Terminal<ScreenBackend<'_>>` and the
/// guard it draws through two fields of one struct pointing at each other, and the alternative to
/// ownership is a self-referential type or an `Arc` around a value whose whole job is to be dropped
/// at a known instant.
pub struct ScreenBackend {
    screen: Screen,
    /// Escape sequences and text, accumulated until [`Backend::flush`]. One `write(2)` per frame
    /// rather than one per cell: a 200×50 viewport is 10 000 cells, and a syscall each would be
    /// visible as tearing on a real terminal.
    out: Vec<u8>,
    size: Size,
    cursor: Position,
    /// The style the terminal is currently in, so an unchanged run of cells emits no SGR at all.
    /// `None` means "unknown" — after a clear, or before the first cell.
    style: Option<(Color, Color, Modifier)>,
    /// Where the terminal's cursor is *as a result of what has been written into `out`*, which is
    /// not [`Self::cursor`]: that one is the application's, this one is the paint head. A cell
    /// written at the paint head needs no `CUP` before it, which is most of them.
    head: Option<Position>,
}

impl ScreenBackend {
    /// Wrap a guard already in the alternate screen. `cols`/`rows` are the viewport, and a zero on
    /// either axis is clamped to one — `ratatui` divides by the area and a zero-sized buffer is a
    /// panic rather than an empty frame.
    pub fn new(screen: Screen, cols: u16, rows: u16) -> Self {
        Self {
            screen,
            out: Vec::with_capacity(8192),
            size: Size::new(cols.max(1), rows.max(1)),
            cursor: Position::new(0, 0),
            style: None,
            head: None,
        }
    }

    /// The guard, for the caller that has to `leave()` it in a particular order relative to its
    /// own last words. See `marion_supervisor::attach`'s `Drop`.
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Tell the backend the viewport changed. The caller is what learns about a `SIGWINCH`, and
    /// `Terminal::autoresize` reads [`Backend::size`], so this is how the two meet.
    pub fn set_size(&mut self, cols: u16, rows: u16) {
        self.size = Size::new(cols.max(1), rows.max(1));
        // The new geometry means nothing is where it was.
        self.style = None;
        self.head = None;
    }

    fn cup(&mut self, x: u16, y: u16) {
        // 1-based, row first. The one place in this file that order is used, and it is the
        // opposite of `WinSize`'s — which is why `WinSize` is a type and this is a local.
        let _ = write!(self.out, "\x1b[{};{}H", y + 1, x + 1);
    }

    /// Put the terminal into `cell`'s style, emitting nothing if it is already there.
    ///
    /// A reset-then-set rather than a minimal diff: the minimal form has to know which of nine
    /// attributes has an "off" code that does not also turn something else off (`22` clears bold
    /// *and* dim), and getting that wrong leaves a stray attribute on screen with no way to see
    /// where it came from. `\x1b[0m` costs four bytes and cannot be wrong.
    fn style(&mut self, cell: &Cell) {
        let want = (cell.fg, cell.bg, cell.modifier);
        if self.style == Some(want) {
            return;
        }
        self.style = Some(want);
        self.out.extend_from_slice(b"\x1b[0m");
        for (flag, code) in [
            (Modifier::BOLD, "1"),
            (Modifier::DIM, "2"),
            (Modifier::ITALIC, "3"),
            (Modifier::UNDERLINED, "4"),
            (Modifier::SLOW_BLINK, "5"),
            (Modifier::RAPID_BLINK, "6"),
            (Modifier::REVERSED, "7"),
            (Modifier::HIDDEN, "8"),
            (Modifier::CROSSED_OUT, "9"),
        ] {
            if cell.modifier.contains(flag) {
                let _ = write!(self.out, "\x1b[{code}m");
            }
        }
        let fg = sgr_color(cell.fg, true);
        if !fg.is_empty() {
            let _ = write!(self.out, "\x1b[{fg}m");
        }
        let bg = sgr_color(cell.bg, false);
        if !bg.is_empty() {
            let _ = write!(self.out, "\x1b[{bg}m");
        }
    }
}

/// One `Color` as SGR parameters, or the empty string for [`Color::Reset`] — which needs nothing,
/// because [`ScreenBackend::style`] has already emitted `\x1b[0m`.
fn sgr_color(c: Color, fg: bool) -> String {
    let base = if fg { 30 } else { 40 };
    match c {
        Color::Reset => String::new(),
        Color::Indexed(i) => format!("{};5;{i}", base + 8),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        named => (base + named_offset(named)).to_string(),
    }
}

/// The sixteen named colours as distances from the SGR base (`30` foreground, `40` background):
/// the normal eight are `0..=7` and the bright eight `60..=67`, because `90`/`100` are `30`/`40`
/// plus sixty. A table rather than sixteen arms, for the same reason [`ScreenBackend::style`]'s
/// modifiers are one.
const NAMED_OFFSETS: [(Color, u16); 16] = [
    (Color::Black, 0),
    (Color::Red, 1),
    (Color::Green, 2),
    (Color::Yellow, 3),
    (Color::Blue, 4),
    (Color::Magenta, 5),
    (Color::Cyan, 6),
    (Color::Gray, 7),
    (Color::DarkGray, 60),
    (Color::LightRed, 61),
    (Color::LightGreen, 62),
    (Color::LightYellow, 63),
    (Color::LightBlue, 64),
    (Color::LightMagenta, 65),
    (Color::LightCyan, 66),
    (Color::White, 67),
];

/// A named colour's row in [`NAMED_OFFSETS`]. Only [`sgr_color`] calls this, after it has taken
/// `Reset`, `Indexed` and `Rgb` itself, so the sixteen colours that can reach here are exactly the
/// table's rows and a miss is a table edit, not a colour.
fn named_offset(named: Color) -> u16 {
    NAMED_OFFSETS
        .iter()
        .find(|(c, _)| *c == named)
        .map(|(_, offset)| *offset)
        .expect("every named ratatui colour has a row in NAMED_OFFSETS")
}

impl Backend for ScreenBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            if self.head != Some(Position::new(x, y)) {
                self.cup(x, y);
            }
            self.style(cell);
            self.out.extend_from_slice(cell.symbol().as_bytes());
            // A wide glyph advances two columns and the terminal knows it; this does not, so the
            // next cell gets an explicit `CUP` rather than a guess. `None` is the honest state.
            self.head = match cell.symbol().chars().count() {
                1 => Some(Position::new(x.saturating_add(1), y)),
                _ => None,
            };
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.out.extend_from_slice(b"\x1b[?25l");
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.out.extend_from_slice(b"\x1b[?25h");
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        // Tracked, never asked. A `CSI 6n` here would need a reply read off the operator's stdin —
        // which another thread is already reading for keystrokes, so the answer would be consumed
        // as input and typed into the node.
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let p = position.into();
        self.cursor = p;
        self.cup(p.x, p.y);
        self.head = Some(p);
        Ok(())
    }

    /// **`CSI 2J`, never `CSI 3J`** — and that is C2 restated from the other side.
    ///
    /// The whole point of owning a grid is that the node's erase-saved is swallowed by
    /// `marion_term`'s `Suppressor` and does not reach the operator's scrollback. A backend that
    /// cleared with `3J` would put it back, by marion's own hand, on every repaint after a resize.
    fn clear(&mut self) -> io::Result<()> {
        self.out.extend_from_slice(b"\x1b[2J");
        self.style = None;
        self.head = None;
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        match clear_type {
            ClearType::All => self.clear(),
            ClearType::AfterCursor => {
                self.out.extend_from_slice(b"\x1b[0J");
                Ok(())
            }
            ClearType::BeforeCursor => {
                self.out.extend_from_slice(b"\x1b[1J");
                Ok(())
            }
            ClearType::CurrentLine => {
                self.out.extend_from_slice(b"\x1b[2K");
                Ok(())
            }
            ClearType::UntilNewLine => {
                self.out.extend_from_slice(b"\x1b[0K");
                Ok(())
            }
        }
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            // Not measured. `TIOCGWINSZ`'s pixel fields are zero on most terminals anyway, and a
            // fabricated number here would be read by a widget that scales to it.
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.out.is_empty() {
            return Ok(());
        }
        let buf = std::mem::take(&mut self.out);
        self.screen.write(&buf)?;
        // Reuse the allocation: a pane repaints many times a second and a fresh 8 KiB `Vec` each
        // time is the one allocation on this path that is easy to avoid.
        self.out = buf;
        self.out.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A sink that records what the backend wrote, so the assertions below are about bytes rather
    /// than about a mock's expectations.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// A backend over a sink, with the guard's terminal side inert: `Screen::enter` on a
    /// non-tty fd installs no raw mode, which is exactly the state a test wants.
    fn backend(cols: u16, rows: u16) -> (ScreenBackend, Sink) {
        let sink = Sink::default();
        let screen = Screen::enter(sink.clone(), -1, &crate::Sticky::initial(cols, rows))
            .expect("a sink is always enterable");
        // Discard the preamble so each test asserts about its own writes.
        sink.0.lock().unwrap().clear();
        (ScreenBackend::new(screen, cols, rows), sink)
    }

    /// **M3 criterion C2, from the backend's side.**
    ///
    /// `marion-term`'s `Suppressor` swallows the *node's* `CSI 3J`. This asserts marion does not
    /// then emit one of its own — a full clear on a resize is `2J`, which touches the visible
    /// screen and nothing else.
    ///
    /// Mutation: change `clear` to write `\x1b[3J`. This fails.
    #[test]
    fn a_clear_never_erases_the_operators_scrollback() {
        let (mut b, sink) = backend(20, 5);
        b.clear().unwrap();
        b.clear_region(ClearType::All).unwrap();
        b.flush().unwrap();
        let out = sink.text();
        assert!(out.contains("\x1b[2J"), "a clear must clear the screen");
        assert!(
            !out.contains("\x1b[3J"),
            "marion emitted erase-saved at the operator's terminal: {out:?}"
        );
    }

    /// A run of cells in one style costs one `CUP` and one SGR, not one of each per cell.
    #[test]
    fn an_unbroken_run_of_one_style_emits_no_sgr_per_cell() {
        let (mut b, sink) = backend(20, 2);
        let mut cells = Vec::new();
        for x in 0..5u16 {
            let mut c = Cell::default();
            c.set_symbol("x");
            cells.push((x, 0u16, c));
        }
        b.draw(cells.iter().map(|(x, y, c)| (*x, *y, c))).unwrap();
        b.flush().unwrap();
        let out = sink.text();
        assert_eq!(out.matches("\x1b[0m").count(), 1, "{out:?}");
        assert_eq!(out.matches('H').count(), 1, "one CUP for the run: {out:?}");
        assert!(out.ends_with("xxxxx"), "{out:?}");
    }

    /// A discontinuity is addressed rather than assumed. The failure this catches is the one that
    /// looks like a working pane until a diff happens to be sparse.
    #[test]
    fn a_gap_between_cells_is_addressed_and_not_walked_over() {
        let (mut b, sink) = backend(20, 3);
        let mut a = Cell::default();
        a.set_symbol("a");
        let mut z = Cell::default();
        z.set_symbol("z");
        b.draw([(0u16, 0u16, &a), (7u16, 2u16, &z)].into_iter())
            .unwrap();
        b.flush().unwrap();
        let out = sink.text();
        assert!(out.contains("\x1b[1;1H"), "{out:?}");
        assert!(out.contains("\x1b[3;8H"), "{out:?}");
    }

    /// Nothing reaches the terminal until `flush`, so a half-painted frame cannot be seen.
    #[test]
    fn a_frame_is_one_write_and_not_one_per_cell() {
        let (mut b, sink) = backend(20, 2);
        let mut c = Cell::default();
        c.set_symbol("q");
        b.draw([(0u16, 0u16, &c)].into_iter()).unwrap();
        assert_eq!(sink.text(), "", "a cell reached the terminal before flush");
        b.flush().unwrap();
        assert!(sink.text().ends_with('q'));
    }

    /// The colour table is a round trip against the codes a terminal actually reads, including the
    /// two extended forms, which are the ones a hand-written table gets wrong.
    #[test]
    fn colours_compile_to_the_sgr_a_terminal_reads() {
        assert_eq!(sgr_color(Color::Reset, true), "");
        assert_eq!(sgr_color(Color::Red, true), "31");
        assert_eq!(sgr_color(Color::Red, false), "41");
        assert_eq!(sgr_color(Color::White, true), "97");
        assert_eq!(sgr_color(Color::Indexed(200), true), "38;5;200");
        assert_eq!(sgr_color(Color::Rgb(1, 2, 3), false), "48;2;1;2;3");
    }

    /// Every named colour, against the codes the sixteen-arm `match` this table replaced produced:
    /// `30`–`37` and `90`–`97` foreground, `40`–`47` and `100`–`107` background, in ratatui's
    /// declaration order.
    #[test]
    fn every_named_colour_keeps_its_sgr_code() {
        let named = [
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
            Color::DarkGray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ];
        let fg = (30..=37).chain(90..=97);
        let bg = (40..=47).chain(100..=107);
        for ((colour, fg), bg) in named.into_iter().zip(fg).zip(bg) {
            assert_eq!(sgr_color(colour, true), fg.to_string(), "{colour:?} fg");
            assert_eq!(sgr_color(colour, false), bg.to_string(), "{colour:?} bg");
        }
    }

    /// `set_size` is what a `SIGWINCH` reaches, and `Terminal::autoresize` reads it back.
    #[test]
    fn a_resize_is_visible_to_the_terminal_that_asks_for_the_size() {
        let (mut b, _sink) = backend(80, 24);
        assert_eq!(b.size().unwrap(), Size::new(80, 24));
        b.set_size(100, 30);
        assert_eq!(b.size().unwrap(), Size::new(100, 30));
        // A zero axis is a pipe's answer, not a terminal's, and a zero-sized buffer panics
        // `ratatui` rather than drawing nothing.
        b.set_size(0, 0);
        assert_eq!(b.size().unwrap(), Size::new(1, 1));
    }

    /// The cursor is tracked, never queried — see [`ScreenBackend::get_cursor_position`].
    #[test]
    fn the_cursor_is_tracked_rather_than_asked_for_over_the_operators_stdin() {
        let (mut b, sink) = backend(20, 5);
        b.set_cursor_position(Position::new(3, 4)).unwrap();
        assert_eq!(b.get_cursor_position().unwrap(), Position::new(3, 4));
        b.flush().unwrap();
        assert!(
            !sink.text().contains("\x1b[6n"),
            "a device-status probe would be answered into the node as keystrokes"
        );
    }
}
