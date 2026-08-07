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

use crate::Term;

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
        AnsiColor::Named(named) => match named {
            NamedColor::Black => Color::Black,
            NamedColor::Red => Color::Red,
            NamedColor::Green => Color::Green,
            NamedColor::Yellow => Color::Yellow,
            NamedColor::Blue => Color::Blue,
            NamedColor::Magenta => Color::Magenta,
            NamedColor::Cyan => Color::Cyan,
            NamedColor::White => Color::Gray,
            NamedColor::BrightBlack => Color::DarkGray,
            NamedColor::BrightRed => Color::LightRed,
            NamedColor::BrightGreen => Color::LightGreen,
            NamedColor::BrightYellow => Color::LightYellow,
            NamedColor::BrightBlue => Color::LightBlue,
            NamedColor::BrightMagenta => Color::LightMagenta,
            NamedColor::BrightCyan => Color::LightCyan,
            NamedColor::BrightWhite => Color::White,
            // Foreground/Background/Cursor and the dim/bright aliases have no ratatui equivalent;
            // the terminal's own default is the honest mapping.
            _ => Color::Reset,
        },
    }
}
