//! The home screen's palette and glyphs: one accent, the terminal's own ANSI-16 colours for
//! status, and `DIM` for everything that is context rather than content.
//!
//! # Why the status colours are ANSI-16 and the accent is not
//!
//! An ANSI-16 colour is remapped by the terminal's theme — that is what makes one byte stream read
//! on a dark and a light background alike — so every **status** colour is one of them, and every
//! status is also a glyph (see [`crate::tree::Tone`] and [`Ready`]): an operator on a monochrome
//! terminal, or one who cannot tell red from green, reads the glyph.
//!
//! The accent is the exception, because it marks *where you are* (the selected row, the active
//! tab, the prompt caret) rather than *what state something is in*, and no ANSI-16 colour is free
//! for that. It is `rgb(193,95,60)`, chosen for about 4:1 contrast against both `#ffffff` and
//! `#0d1117`, because a truecolour value is **not** remapped and must survive either background by
//! itself. A terminal that does not advertise truecolour gets `Indexed(173)`, the nearest entry of
//! the 256-colour cube.

use ratatui::style::{Color, Modifier, Style};

/// The accent, where the terminal takes 24-bit colour.
pub const ACCENT_RGB: Color = Color::Rgb(193, 95, 60);
/// The accent's nearest 256-colour cube entry, for terminals that do not advertise truecolour.
pub const ACCENT_256: Color = Color::Indexed(173);

/// Braille spinner frames, the ones cargo and most CLIs use. Text glyphs, one column, never emoji.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The selection caret, drawn in the accent at the left edge of the selected row.
pub const CARET: &str = "❯";

/// Which accent the terminal can show. Everything else in the palette is the same either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub truecolor: bool,
}

impl Theme {
    /// The truecolour theme, which the snapshots and PNG renders use.
    pub const TRUECOLOR: Theme = Theme { truecolor: true };
    /// The 256-colour fallback.
    pub const INDEXED: Theme = Theme { truecolor: false };

    /// Read `COLORTERM` the way terminals advertise 24-bit colour (`truecolor` or `24bit`).
    pub fn from_env() -> Theme {
        Theme::from_colorterm(std::env::var("COLORTERM").ok().as_deref())
    }

    /// [`Theme::from_env`] without the environment, for tests.
    pub fn from_colorterm(value: Option<&str>) -> Theme {
        let truecolor = matches!(
            value.map(str::to_ascii_lowercase).as_deref(),
            Some("truecolor" | "24bit")
        );
        Theme { truecolor }
    }

    pub fn accent_color(self) -> Color {
        if self.truecolor {
            ACCENT_RGB
        } else {
            ACCENT_256
        }
    }

    /// The accent foreground: where you are.
    pub fn accent(self) -> Style {
        Style::default().fg(self.accent_color())
    }

    /// The accent, bold: the caret, the active tab, a key that acts on the selection.
    pub fn key(self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }
}

/// Context rather than content: labels, units, separators, anything the eye should skip.
pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// The one thing on a row an operator is looking for.
pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Section headers and expansion labels: dim bold capitals, which is how small caps read at
/// terminal size. (Unicode small capitals are missing from most terminal fonts and would render in
/// a fallback face.)
pub fn label() -> Style {
    dim().add_modifier(Modifier::BOLD)
}

/// Text that says the operator is needed: a sign-in, a permission, a question.
pub fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

/// Text that says something failed.
pub fn bad() -> Style {
    Style::default().fg(Color::Red)
}

/// Text that says something went well (lines added, a passing check).
pub fn good() -> Style {
    Style::default().fg(Color::Green)
}

/// A harness's readiness, from `marion doctor`'s answer: the glyph is the fact, the colour repeats
/// it where there is colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    /// Installed, admitted, signed in.
    Ready,
    /// Usable once the operator does something: sign in, answer a prompt.
    Attention,
    /// Installed but unusable as it stands: too old, too new, a failed probe.
    Broken,
    /// Not installed.
    Absent,
    /// The check has not answered yet.
    Checking,
}

impl Ready {
    /// One column, distinct per kind. [`Ready::Checking`] is a spinner frame, see [`spinner`].
    pub fn glyph(self, frame: usize) -> &'static str {
        match self {
            Ready::Ready => "●",
            Ready::Attention => "◐",
            Ready::Broken => "✗",
            Ready::Absent => "○",
            Ready::Checking => spinner(frame),
        }
    }

    pub fn style(self, theme: Theme) -> Style {
        match self {
            Ready::Ready => Style::default().fg(Color::Green),
            Ready::Attention => warn(),
            Ready::Broken => bad(),
            Ready::Absent => dim(),
            Ready::Checking => theme.accent(),
        }
    }

    /// The style of the note beside the glyph: only the kinds that need the operator are coloured,
    /// so the eye lands on them.
    pub fn note_style(self) -> Style {
        match self {
            Ready::Attention => warn(),
            Ready::Broken => bad(),
            _ => dim(),
        }
    }
}

/// The spinner frame for animation tick `frame`.
pub fn spinner(frame: usize) -> &'static str {
    SPINNER[frame % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolour_is_read_from_colorterm_and_nothing_else() {
        assert!(Theme::from_colorterm(Some("truecolor")).truecolor);
        assert!(Theme::from_colorterm(Some("24bit")).truecolor);
        assert!(Theme::from_colorterm(Some("TrueColor")).truecolor);
        assert!(!Theme::from_colorterm(Some("256color")).truecolor);
        assert!(!Theme::from_colorterm(None).truecolor);
        assert_eq!(Theme::TRUECOLOR.accent_color(), ACCENT_RGB);
        assert_eq!(Theme::INDEXED.accent_color(), ACCENT_256);
    }

    /// Every readiness kind is told apart by its glyph alone, and every status colour is one the
    /// terminal theme remaps.
    #[test]
    fn readiness_glyphs_are_distinct_and_colours_are_ansi_16() {
        let kinds = [Ready::Ready, Ready::Attention, Ready::Broken, Ready::Absent];
        let glyphs: std::collections::BTreeSet<_> = kinds.iter().map(|k| k.glyph(0)).collect();
        assert_eq!(glyphs.len(), kinds.len());
        for k in kinds {
            assert!(
                !matches!(
                    k.style(Theme::TRUECOLOR).fg,
                    Some(Color::Rgb(..) | Color::Indexed(_))
                ),
                "{k:?} is not an ANSI-16 colour"
            );
        }
        assert_ne!(Ready::Checking.glyph(0), Ready::Checking.glyph(1));
    }

    /// The accent's truecolour value clears 3:1 against both a white and a near-black ground.
    #[test]
    fn the_accent_reads_on_dark_and_light_grounds() {
        fn lum(c: (u8, u8, u8)) -> f64 {
            let ch = |v: u8| {
                let v = f64::from(v) / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * ch(c.0) + 0.7152 * ch(c.1) + 0.0722 * ch(c.2)
        }
        let ratio = |a: f64, b: f64| (a.max(b) + 0.05) / (a.min(b) + 0.05);
        let accent = lum((193, 95, 60));
        assert!(ratio(accent, lum((255, 255, 255))) >= 3.0);
        assert!(ratio(accent, lum((0x0d, 0x11, 0x17))) >= 3.0);
    }
}
