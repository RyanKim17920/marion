//! Grid → [`ratatui::buffer::Buffer`]. **Viewport only.**
//!
//! §5.3: `renderable_content()` delegates to `display_iter`, which covers
//! `-display_offset-1 ..= bottommost_line()`. Retained history is deliberately not here — it is
//! [`Term::scrollback_lines`](crate::Term::scrollback_lines)'s job, and mixing the two would make
//! a scrollback regression look like a rendering one.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

use crate::Term;

/// The same viewport, drawn into a caller's area — what a `Terminal<B>` needs.
///
/// [`render`] returns a fresh buffer sized to the terminal; this clips into whatever `area` the
/// layout gave, so L4.5's `TestBackend` driver and M3's real TUI use one code path. Cells outside
/// the grid are left as the caller drew them, not blanked.
impl Widget for &Term {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let src = render(self);
        for y in 0..area.height.min(src.area.height) {
            for x in 0..area.width.min(src.area.width) {
                buf[(area.x + x, area.y + y)] = src[(x, y)].clone();
            }
        }
    }
}

/// Render the viewport into a fresh buffer sized to the terminal.
pub fn render(term: &Term) -> Buffer {
    let inner = term.inner();
    let (cols, rows) = (inner.columns(), inner.screen_lines());
    let mut buf = Buffer::empty(Rect::new(0, 0, cols as u16, rows as u16));

    let content = inner.renderable_content();
    let offset = content.display_offset as i32;
    for cell in content.display_iter {
        let x = cell.point.column.0 as u16;
        let y = (cell.point.line.0 + offset) as u16;
        if x >= cols as u16 || y >= rows as u16 {
            continue;
        }
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        buf[(x, y)]
            .set_char(cell.c)
            .set_style(style_of(cell.fg, cell.bg, cell.flags));
    }
    buf
}

fn style_of(fg: AnsiColor, bg: AnsiColor, flags: Flags) -> Style {
    let style = Style::default().fg(convert(fg)).bg(convert(bg));
    let mut modifier = Modifier::empty();
    if flags.contains(Flags::BOLD) {
        modifier |= Modifier::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        modifier |= Modifier::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) {
        modifier |= Modifier::UNDERLINED;
    }
    if flags.contains(Flags::DIM) {
        modifier |= Modifier::DIM;
    }
    if flags.contains(Flags::INVERSE) {
        modifier |= Modifier::REVERSED;
    }
    if flags.contains(Flags::HIDDEN) {
        modifier |= Modifier::HIDDEN;
    }
    if flags.contains(Flags::STRIKEOUT) {
        modifier |= Modifier::CROSSED_OUT;
    }
    style.add_modifier(modifier)
}

fn convert(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => Color::Indexed(i),
        AnsiColor::Named(named) => convert_named(named),
    }
}

/// The sixteen ANSI names that have a ratatui colour, in ANSI order. Note the two off-by-one
/// names: ANSI's `White` is ratatui's `Gray`, and `BrightBlack` is `DarkGray`.
const NAMED: [(NamedColor, Color); 16] = [
    (NamedColor::Black, Color::Black),
    (NamedColor::Red, Color::Red),
    (NamedColor::Green, Color::Green),
    (NamedColor::Yellow, Color::Yellow),
    (NamedColor::Blue, Color::Blue),
    (NamedColor::Magenta, Color::Magenta),
    (NamedColor::Cyan, Color::Cyan),
    (NamedColor::White, Color::Gray),
    (NamedColor::BrightBlack, Color::DarkGray),
    (NamedColor::BrightRed, Color::LightRed),
    (NamedColor::BrightGreen, Color::LightGreen),
    (NamedColor::BrightYellow, Color::LightYellow),
    (NamedColor::BrightBlue, Color::LightBlue),
    (NamedColor::BrightMagenta, Color::LightMagenta),
    (NamedColor::BrightCyan, Color::LightCyan),
    (NamedColor::BrightWhite, Color::White),
];

fn convert_named(named: NamedColor) -> Color {
    NAMED
        .iter()
        .find(|(ansi, _)| *ansi == named)
        .map(|(_, color)| *color)
        // Foreground/Background/Cursor and the dim/bright aliases have no ratatui equivalent;
        // the terminal's own default is the honest mapping.
        .unwrap_or(Color::Reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table against the sixteen-arm `match` it replaced, name by name, plus the fallthrough
    /// for a name ratatui cannot express.
    #[test]
    fn every_ansi_name_keeps_its_ratatui_colour() {
        let expected = [
            (NamedColor::Black, Color::Black),
            (NamedColor::Red, Color::Red),
            (NamedColor::Green, Color::Green),
            (NamedColor::Yellow, Color::Yellow),
            (NamedColor::Blue, Color::Blue),
            (NamedColor::Magenta, Color::Magenta),
            (NamedColor::Cyan, Color::Cyan),
            (NamedColor::White, Color::Gray),
            (NamedColor::BrightBlack, Color::DarkGray),
            (NamedColor::BrightRed, Color::LightRed),
            (NamedColor::BrightGreen, Color::LightGreen),
            (NamedColor::BrightYellow, Color::LightYellow),
            (NamedColor::BrightBlue, Color::LightBlue),
            (NamedColor::BrightMagenta, Color::LightMagenta),
            (NamedColor::BrightCyan, Color::LightCyan),
            (NamedColor::BrightWhite, Color::White),
        ];
        for (ansi, color) in expected {
            assert_eq!(convert(AnsiColor::Named(ansi)), color, "{ansi:?}");
        }
        for unmapped in [
            NamedColor::Foreground,
            NamedColor::Background,
            NamedColor::Cursor,
            NamedColor::DimRed,
            NamedColor::BrightForeground,
        ] {
            assert_eq!(
                convert(AnsiColor::Named(unmapped)),
                Color::Reset,
                "{unmapped:?}"
            );
        }
    }
}
