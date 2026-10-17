//! The home screen's reusable pieces: the status line, the footer line (keys, the input line and
//! the confirm line), section headers and the `└` expansion block.
//!
//! Each is a function of plain data and a [`Theme`], drawn straight into a `Buffer` or returned as
//! `Line`s for the caller to place; none of them knows what a node or a harness is.

use super::text::{clip, clip_left, fit, lr, pad, spans_width};
use super::theme::{Theme, bold, dim, label};
use super::{Footer, Hint};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

/// `s`, as an owned span in `style`.
pub fn span<'a>(s: impl Into<String>, style: Style) -> Span<'a> {
    Span::styled(s.into(), style)
}

/// A section header: the label in dim bold, as written, then a dim note three columns on.
pub fn section<'a>(name: &str, note: &str) -> Line<'a> {
    let mut s = vec![span(name.to_string(), label())];
    if !note.is_empty() {
        s.push(span(format!("   {note}"), dim()));
    }
    Line::from(s)
}

/// Text with `` `code` `` segments: the segments are drawn in the accent, bold, and without their
/// backticks — the command an operator is meant to copy stands out of the sentence around it.
pub fn code_spans<'a>(text: &str, base: Style, theme: Theme) -> Vec<Span<'a>> {
    text.split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(i, part)| {
            if i % 2 == 1 {
                span(part, theme.key())
            } else {
                span(part, base)
            }
        })
        .collect()
}

/// An indented `└` block under a selected row, like a tool result under a tool call: a label
/// column of `label_w` and the values beside it. A row with an empty label continues the one above.
pub fn expansion<'a>(
    rows: Vec<(String, Vec<Span<'a>>)>,
    label_w: usize,
    w: usize,
) -> Vec<Line<'a>> {
    rows.into_iter()
        .enumerate()
        .map(|(i, (lab, val))| {
            let lead = if i == 0 { "└ " } else { "  " };
            let mut s = vec![span(lead, dim())];
            if lab.is_empty() {
                s.push(Span::raw(" ".repeat(label_w)));
            } else {
                s.push(span(pad(&lab, label_w), label()));
            }
            s.extend(val);
            Line::from(fit(s, w))
        })
        .collect()
}

/// A list row fitted to `w` columns and padded to the edge; the selected one is one bar in the
/// accent, every span reversed and its own colour dropped, since a red glyph reversed is a red block
/// in the middle of the bar. The glyph still says the state. Bold stays bold.
pub fn row_bar<'a>(spans: Vec<Span<'a>>, w: usize, sel: bool, theme: Theme) -> Line<'a> {
    let mut l = fit(spans, w);
    let used = spans_width(&l);
    l.push(Span::raw(" ".repeat(w.saturating_sub(used))));
    if sel {
        let bar = theme.accent().add_modifier(Modifier::REVERSED);
        for s in &mut l {
            let bold = s.style.add_modifier.contains(Modifier::BOLD);
            s.style = if bold {
                bar.add_modifier(Modifier::BOLD)
            } else {
                bar
            };
        }
    }
    Line::from(l)
}

/// The status line: `marion  <title>` on the left, the counts on the right.
pub struct StatusLine<'a> {
    pub title: &'a str,
    pub counts: &'a str,
}

impl Widget for StatusLine<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width < 4 {
            return;
        }
        let left = vec![
            Span::raw(" "),
            span("marion", bold()),
            span(format!("  {}", self.title), dim()),
        ];
        let right = vec![span(self.counts.to_string(), dim()), Span::raw(" ")];
        buf.set_line(
            area.x,
            area.y,
            &lr(left, right, area.width as usize),
            area.width,
        );
    }
}

/// The footer line: the keys, a notice in their place, the input line or the confirm line.
pub struct FooterLine<'a> {
    pub footer: &'a Footer,
    /// What the last key did, or why it did nothing, shown in place of the keys until the next
    /// key: a key that silently does nothing reads as broken.
    pub notice: Option<&'a str>,
    pub theme: Theme,
}

/// `key verb` pairs, the key in the accent and the verb dim, two spaces apart.
fn hint_groups<'a>(hints: &[Hint], theme: Theme) -> Vec<Vec<Span<'a>>> {
    hints
        .iter()
        .map(|h| {
            vec![
                span(h.key.clone(), theme.key()),
                span(format!(" {}  ", h.verb), dim()),
            ]
        })
        .collect()
}

/// As many of `hints` as fit in `w` columns, dropped from the right, except that the last hint
/// (`? more`) is kept whenever anything is dropped: it is how the operator finds the rest.
pub fn keys_that_fit<'a>(hints: &[Hint], w: usize, theme: Theme) -> Vec<Span<'a>> {
    let groups = hint_groups(hints, theme);
    let total: usize = groups.iter().map(|g| spans_width(g)).sum();
    if total <= w {
        return groups.into_iter().flatten().collect();
    }
    let Some((last, rest)) = groups.split_last() else {
        return Vec::new();
    };
    let room = w.saturating_sub(spans_width(last));
    let mut out = Vec::new();
    let mut used = 0;
    for g in rest {
        let gw = spans_width(g);
        if used + gw > room {
            break;
        }
        used += gw;
        out.extend(g.iter().cloned());
    }
    out.extend(last.iter().cloned());
    out
}

impl Widget for FooterLine<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width < 4 {
            return;
        }
        let w = area.width as usize;
        let line = match (self.footer, self.notice) {
            (Footer::Keys(_), Some(n)) => {
                Line::from(fit(vec![Span::raw(" "), span(n.to_string(), bold())], w))
            }
            (Footer::Keys(hints), None) => {
                let mut l = vec![Span::raw(" ")];
                l.extend(keys_that_fit(hints, w.saturating_sub(1), self.theme));
                Line::from(l)
            }
            (
                Footer::Input {
                    target,
                    note,
                    text,
                    keys,
                },
                _,
            ) => {
                let mut head = vec![Span::raw(" "), span(target.clone(), bold())];
                if !note.is_empty() {
                    head.push(span(format!(" {note}"), dim()));
                }
                head.push(span(" › ", self.theme.key()));
                let right = keys_that_fit(keys, w / 2, self.theme);
                // The end of a long line: that is where the typing is.
                let room = w.saturating_sub(spans_width(&head) + spans_width(&right) + 2);
                head.push(span(clip_left(text, room), Style::default()));
                head.push(span("▏", self.theme.accent()));
                lr(head, right, w)
            }
            (Footer::Confirm { question, command }, _) => {
                let right = vec![span("y", self.theme.key()), span(" / any key ", dim())];
                lr(
                    vec![
                        Span::raw(" "),
                        span(question.clone(), bold()),
                        span(format!("  $ {command}"), dim()),
                    ],
                    right,
                    w,
                )
            }
        };
        buf.set_line(area.x, area.y, &line, area.width);
    }
}

/// A sentence centred in `area`, for the screens' empty states: dim, clipped to the width.
pub fn centred(buf: &mut Buffer, area: Rect, y: u16, line: Line) {
    if y >= area.bottom() {
        return;
    }
    let lw = line.width().min(area.width as usize) as u16;
    let x = area.x + (area.width - lw) / 2;
    buf.set_line(x, y, &line, area.width - (x - area.x));
}

/// `clip` for a span, keeping its style.
pub fn clipped<'a>(s: &str, w: usize, style: Style) -> Span<'a> {
    span(clip(s, w), style)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

    /// Keys that do not fit are dropped from the right, but `? more` stays: it is the way to the
    /// rest of them.
    #[test]
    fn the_footer_drops_keys_from_the_right_and_keeps_more_last() {
        let hints = [
            Hint::new("enter", "attach"),
            Hint::new("n", "new"),
            Hint::new("s", "steer"),
            Hint::new("?", "more"),
        ];
        let t = Theme::TRUECOLOR;
        assert_eq!(
            text(&keys_that_fit(&hints, 80, t)),
            "enter attach  n new  s steer  ? more  "
        );
        assert_eq!(
            text(&keys_that_fit(&hints, 24, t)),
            "enter attach  ? more  "
        );
        assert_eq!(text(&keys_that_fit(&hints, 8, t)), "? more  ");
    }

    #[test]
    fn code_spans_lift_the_command_out_of_the_sentence() {
        let s = code_spans("run `qwen` once", Style::default(), Theme::TRUECOLOR);
        let text: Vec<_> = s.iter().map(|x| x.content.as_ref()).collect();
        assert_eq!(text, ["run ", "qwen", " once"]);
        assert_eq!(s[1].style, Theme::TRUECOLOR.key());
    }
}
