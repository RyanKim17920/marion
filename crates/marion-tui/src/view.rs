//! The whole terminal is one node's grid.
//!
//! # Why this module is nearly empty, and must stay that way
//!
//! `marion-term` already has the renderer: `impl Widget for &Term`, the same one L4.5's
//! `TestBackend` driver snapshots. **A second renderer here would be the real cost of this
//! increment** — the snapshot suite would stop covering what ships, and every colour, wide-char
//! and blank-trimming decision would exist in two places to disagree in one. So [`Pane`] is a
//! newtype whose `render` is a delegation and a comment, and the only thing it adds is the
//! *sizing* question, which the widget deliberately does not answer.
//!
//! # What sizing means when there is no layout engine
//!
//! `marion attach` gives the node the entire terminal — no criterion *bullet* in §9's M3 names a
//! tree, and the count this comment used to cite for that (*"a pane three times and a tree zero
//! times"*) was wrong in both figures: it is **pane 5×, tree 1×**, the one being M3's title. The
//! decision stands on the bullets; the arithmetic under it did not.
//!
//! That makes an attach's geometry a single decision rather than a layout: the node's
//! pty is set to marion's own terminal size, and the grid that comes back is exactly that size.
//! [`fits`] is the guard for the window between a `SIGWINCH` and the resize taking effect, during
//! which the grid marion holds is still the old size and something must decide what to do with the
//! mismatch.
//!
//! **Mismatch clips, it never scales and never errors.** A grid wider than the area loses its
//! right-hand columns for the one frame it takes the node to repaint; a grid narrower leaves the
//! remainder as the caller drew it, which for a full-area attach is the cleared screen.
//! `marion-term`'s widget already does exactly this — the two `min`s in its loop — so clipping is
//! not a policy this module implements, it is one it declines to override.

use marion_term::Term;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

/// One node's grid, drawn over the whole terminal.
///
/// Holds a borrow rather than owning: the grid is fed by the read loop and painted by the draw
/// loop, and a `Pane` that owned a copy would be painting a snapshot of a screen that had already
/// moved on.
/// No `Debug`: `marion_term::Term` has none, and deriving one here would mean printing a grid —
/// which is [`marion_term::Term::viewport_lines`]'s job and not a formatter's.
#[derive(Clone, Copy)]
pub struct Pane<'a>(pub &'a Term);

impl Widget for Pane<'_> {
    /// **One line, and that is the whole design.** See the module doc: `marion-term`'s widget is
    /// the render path L4.5 snapshots, so this must reach it rather than re-implement it.
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.0.render(area, buf);
    }
}

/// The pty size marion should ask for, given its own terminal.
///
/// The identity function, written down because it is a **decision** and not an absence of one: the
/// whole terminal is one node, so there is no chrome to subtract, no border, no status line and no
/// tree column. **It stays an identity now that [`crate::tree`] exists**, because the tree is a
/// separate screen rather than a column subtracted from an attach: `marion tree` and `marion
/// attach` are two verbs, and the second still hands the node everything. [`crate::tree::split`] is
/// where the tree screen's own arithmetic lives, and it is the one place a caller should reach for
/// it — a caller doing it inline would be the thing that had to be found.
///
/// Zero is passed through rather than clamped. A zero-sized terminal is a real state — a window
/// dragged to nothing, or a `TIOCGWINSZ` on a pipe — and inventing an 80x24 here would set the
/// node's pty to a size the operator is not looking at.
pub fn pty_size(terminal: (u16, u16)) -> (u16, u16) {
    terminal
}

/// Whether `term`'s grid is exactly the size of the area it is about to be drawn into.
///
/// Diagnostic, not a gate. A `false` means a resize is in flight and this frame will clip, which
/// is expected and transient; a `false` that *persists* means the resize never reached the node
/// and is the bug worth surfacing.
pub fn fits(term: &Term, area: Rect) -> bool {
    let size = term.size();
    size.cols == area.width as usize && size.rows == area.height as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_term::Size;

    fn term(cols: usize, rows: usize, text: &str) -> Term {
        let mut t = Term::with_options(Size::new(cols, rows), crate::grid_options());
        t.advance(text.as_bytes());
        t
    }

    fn draw<W: Widget>(w: W, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        w.render(area, &mut buf);
        buf
    }

    /// **The property this module exists to hold: there is exactly one renderer.**
    ///
    /// If `Pane` ever grows a paint of its own, this fails. It is a stronger statement than
    /// "`Pane` renders something correct" — it says `Pane` renders *the same thing* the L4.5
    /// snapshot suite is measuring, which is what stops the shipped TUI drifting away from the
    /// tests that certify it.
    #[test]
    fn a_pane_is_the_marion_term_widget_and_not_a_second_renderer() {
        let t = term(20, 5, "\x1b[1mbold\x1b[0m\r\nplain \x1b[31mred\x1b[0m");
        let area = Rect::new(0, 0, 20, 5);
        assert_eq!(
            draw(Pane(&t), area),
            draw(&t, area),
            "Pane diverged from `impl Widget for &Term` — there are now two renderers"
        );
    }

    /// Delegation must survive being drawn somewhere other than the origin, since that is the one
    /// way a re-implementation usually differs: it forgets to offset.
    #[test]
    fn the_delegation_holds_at_a_non_zero_origin() {
        let t = term(10, 3, "abc\r\ndef");
        let area = Rect::new(4, 2, 10, 3);
        assert_eq!(draw(Pane(&t), area), draw(&t, area));
    }

    #[test]
    fn a_full_area_attach_subtracts_no_chrome() {
        // No border, no status line, no tree column: M3's pane is the whole terminal.
        assert_eq!(pty_size((140, 45)), (140, 45));
        assert_eq!(
            pty_size((0, 0)),
            (0, 0),
            "a zero terminal is reported, not invented away"
        );
    }

    #[test]
    fn fits_is_true_only_at_the_exact_size() {
        let t = term(20, 5, "");
        assert!(fits(&t, Rect::new(0, 0, 20, 5)));
        assert!(
            !fits(&t, Rect::new(0, 0, 21, 5)),
            "a resize in flight does not fit"
        );
        assert!(!fits(&t, Rect::new(0, 0, 20, 4)));
        // And the origin is not part of the question — a pane may be placed anywhere.
        assert!(fits(&t, Rect::new(7, 9, 20, 5)));
    }

    /// A grid larger than its area clips instead of panicking. This is the frame between a
    /// `SIGWINCH` and the node repainting, and an index-out-of-bounds here would take the client
    /// down every time the operator dragged their window smaller.
    #[test]
    fn a_grid_larger_than_its_area_clips_rather_than_panicking() {
        let t = term(40, 10, "x".repeat(40).as_str());
        let area = Rect::new(0, 0, 12, 3);
        let buf = draw(Pane(&t), area);
        assert_eq!(buf.area, area);
        assert_eq!(
            buf[(11, 0)].symbol(),
            "x",
            "the visible columns are the grid's"
        );
    }

    /// And the other direction: a grid smaller than its area leaves the remainder alone rather
    /// than reading past the end of the source buffer.
    #[test]
    fn a_grid_smaller_than_its_area_leaves_the_remainder_untouched() {
        let t = term(4, 2, "ab");
        let area = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(area);
        for y in 0..4 {
            for x in 0..10 {
                buf[(x, y)].set_char('.');
            }
        }
        Pane(&t).render(area, &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "a");
        assert_eq!(
            buf[(9, 3)].symbol(),
            ".",
            "outside the grid is the caller's, not blanked"
        );
    }
}
