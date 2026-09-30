//! Help (`?` or F1): every key, grouped by the screen it works on, each with the command it stands
//! for.
//!
//! Laid out to fit: two columns wherever the screen is wide enough for them (a section is never split
//! across the two), the commands only where a column has room for them, and the whole page scrolled
//! with j/k where even that does not fit. The section for the tab help was opened from comes first.

use super::GUTTER;
use super::text::{fit, pad};
use super::theme::{Theme, dim};
use super::widgets::{section, span};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

/// One key: what to press, what it does, and the CLI command it is (empty when it is not one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRow {
    pub key: String,
    pub verb: String,
    pub command: String,
}

/// The key table, by section. The supervisor's key handler owns it, so this page cannot list a key
/// that does nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelpView {
    pub sections: Vec<(String, Vec<KeyRow>)>,
    /// Rows scrolled down, when the page does not fit; clamped here to what there is.
    pub scroll: usize,
}

const KEY_W: usize = 7;
const VERB_W: usize = 30;
/// Below this width, one column.
const TWO_COLUMNS: usize = 70;
/// The gap between the two columns.
const GAP: usize = 2;

/// One section's lines: its title, then one row per key.
fn section_lines<'a>(
    title: &str,
    keys: &[KeyRow],
    w: usize,
    commands: bool,
    theme: Theme,
) -> Vec<Line<'a>> {
    let mut out = vec![section(title, "")];
    for k in keys {
        let verb = if commands {
            pad(&k.verb, VERB_W)
        } else {
            k.verb.clone()
        };
        let mut l: Vec<Span> = vec![
            span(pad(&k.key, KEY_W), theme.key()),
            span(verb, Default::default()),
        ];
        if commands && !k.command.is_empty() {
            l.push(span(format!("$ {}", k.command), dim()));
        }
        out.push(Line::from(fit(l, w)));
    }
    out
}

pub fn render(v: &HelpView, theme: Theme, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width <= GUTTER * 2 {
        return;
    }
    let x = area.x + GUTTER;
    let w = (area.width - GUTTER * 2) as usize;
    let two = w >= TWO_COLUMNS;
    let col_w = if two { (w - GAP) / 2 } else { w };
    let commands = col_w >= KEY_W + VERB_W + 20;
    let blocks: Vec<Vec<Line>> = v
        .sections
        .iter()
        .map(|(t, keys)| section_lines(t, keys, col_w, commands, theme))
        .collect();
    // Two columns: fill the first up to half the lines, never splitting a section.
    let total: usize = blocks.iter().map(Vec::len).sum();
    let mut columns: Vec<Vec<Line>> = vec![Vec::new()];
    for b in blocks {
        let first_full = columns.len() == 1 && {
            let c = &columns[0];
            !c.is_empty() && c.len() + b.len() > total.div_ceil(2)
        };
        if two && first_full {
            columns.push(b);
        } else if let Some(last) = columns.last_mut() {
            last.extend(b);
        }
    }
    let tall = columns.iter().map(Vec::len).max().unwrap_or(0);
    let rows = area.height as usize;
    let overflow = tall > rows;
    let shown = if overflow {
        rows.saturating_sub(1)
    } else {
        rows
    };
    let scroll = v.scroll.min(tall.saturating_sub(shown));
    for (i, col) in columns.iter().enumerate() {
        let cx = x + (i * (col_w + GAP)) as u16;
        for (y, l) in (area.y..).zip(col.iter().skip(scroll).take(shown)) {
            buf.set_line(cx, y, l, col_w as u16);
        }
    }
    if overflow {
        let below = tall - scroll - shown;
        let mut l = Vec::new();
        if scroll > 0 {
            l.push(span(format!("↑ {scroll} above  "), dim()));
        }
        if below > 0 {
            l.push(span(format!("↓ {below} below  "), dim()));
        }
        l.push(span("j/k", theme.key()));
        l.push(span(" scroll", dim()));
        buf.set_line(x, area.y + shown as u16, &Line::from(fit(l, w)), w as u16);
    }
}
