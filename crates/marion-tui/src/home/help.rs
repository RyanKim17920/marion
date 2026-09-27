//! `?`: every key, grouped by the screen it works on, each with the command it stands for.

use super::GUTTER;
use super::text::{fit, pad};
use super::theme::{Theme, dim};
use super::widgets::{section, span};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

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
}

const KEY_W: usize = 9;
const VERB_W: usize = 22;

pub fn render(v: &HelpView, theme: Theme, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width <= GUTTER * 2 {
        return;
    }
    let x = area.x + GUTTER;
    let w = (area.width - GUTTER * 2) as usize;
    let mut y = area.y;
    for (title, keys) in &v.sections {
        if y >= area.bottom() {
            break;
        }
        buf.set_line(x, y, &section(title, ""), w as u16);
        y += 1;
        for k in keys {
            if y >= area.bottom() {
                break;
            }
            let mut l = vec![
                span(pad(&k.key, KEY_W), theme.key()),
                span(pad(&k.verb, VERB_W), Default::default()),
            ];
            if !k.command.is_empty() {
                l.push(span(format!("$ {}", k.command), dim()));
            }
            buf.set_line(x, y, &Line::from(fit(l, w)), w as u16);
            y += 1;
        }
        y += 1;
    }
}
