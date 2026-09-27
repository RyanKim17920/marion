//! The home screen's reusable pieces: the top bar, section headers, the `└` expansion block, the
//! token sparkline and context bar, the bottom input box and the hint row.
//!
//! Each is a function of plain data and a [`Theme`], drawn straight into a `Buffer` or returned as
//! `Line`s for the caller to place; none of them knows what a node or a harness is.

use super::text::{clip, clip_left, fit, fit_groups, lr, pad, spans_width, wrap};
use super::theme::{CARET, Theme, bold, dim, label};
use super::{Hint, Input, Tab};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Widget};

/// `s`, as an owned span in `style`.
pub fn span<'a>(s: impl Into<String>, style: Style) -> Span<'a> {
    Span::styled(s.into(), style)
}

/// A single dim horizontal rule, `w` columns from `(x, y)`, clipped to the buffer.
pub fn rule(buf: &mut Buffer, x: u16, y: u16, w: u16) {
    let area = buf.area;
    if y >= area.bottom() {
        return;
    }
    for cx in x..x.saturating_add(w).min(area.right()) {
        buf[(cx, y)].set_symbol("─").set_style(dim());
    }
}

/// A section header: the label in dim bold capitals, then a dim note three columns on.
pub fn section<'a>(name: &str, note: &str) -> Line<'a> {
    let mut s = vec![span(name.to_uppercase(), label())];
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

/// The top bar: `◆ marion  <project>` on the left, the tabs on the right, a rule beneath.
///
/// `attention` is the forest's "N need you" count, drawn after the Watch tab as `◐N` whenever it is
/// non-zero, so an operator on Start or Setup still sees that a node is waiting.
pub struct TopBar<'a> {
    pub project: &'a str,
    pub tab: Tab,
    pub attention: usize,
    pub theme: Theme,
}

impl TopBar<'_> {
    /// Rows the bar takes: the title row and the rule.
    pub const HEIGHT: u16 = 2;
}

impl Widget for TopBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width < 4 {
            return;
        }
        let w = area.width as usize;
        let mut tabs = Vec::new();
        for (i, t) in Tab::ALL.iter().enumerate() {
            let style = if *t == self.tab {
                self.theme.key()
            } else {
                dim()
            };
            tabs.push(span(t.title(), style));
            if *t == Tab::Watch && self.attention > 0 {
                tabs.push(span(format!(" ◐{}", self.attention), super::theme::warn()));
            }
            tabs.push(Span::raw(if i + 1 < Tab::ALL.len() { "   " } else { "  " }));
        }
        let left = vec![
            span(" ◆ ", self.theme.accent()),
            span("marion", bold()),
            span(format!("  {}", self.project), dim()),
        ];
        buf.set_line(area.x, area.y, &lr(left, tabs, w), area.width);
        if area.height > 1 {
            rule(buf, area.x + 1, area.y + 1, area.width - 2);
        }
    }
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
                s.push(span(pad(&lab.to_uppercase(), label_w), label()));
            }
            s.extend(val);
            Line::from(fit(s, w))
        })
        .collect()
}

/// Eighths of a cell, lowest to highest.
const BARS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// One row of block glyphs, one per sample, scaled to the largest sample; the newest `w` samples
/// when there are more than fit. A zero sample is a space, so an idle stretch reads as a gap.
pub fn sparkline(samples: &[u64], w: usize) -> String {
    let tail = &samples[samples.len().saturating_sub(w)..];
    let max = tail.iter().copied().max().unwrap_or(0);
    tail.iter()
        .map(|&v| {
            if v == 0 || max == 0 {
                " "
            } else {
                // 1..=8, rounding up so the smallest non-zero sample still shows.
                let level = (v * 8).div_ceil(max).clamp(1, 8) as usize;
                BARS[level - 1]
            }
        })
        .collect()
}

/// A `w`-cell bar, `permille`/1000 of it in the accent and the rest dim, then `  41% of context`.
pub fn context_bar<'a>(permille: u32, w: usize, theme: Theme) -> Vec<Span<'a>> {
    let permille = permille.min(1000) as usize;
    let filled = (w * permille + 500) / 1000;
    vec![
        span("━".repeat(filled), theme.accent()),
        span("━".repeat(w - filled), dim()),
        span(format!("  {}% of context", (permille + 5) / 10), dim()),
    ]
}

/// The bottom input box: a rounded border around one to three rows.
pub struct InputBox<'a> {
    pub input: &'a Input,
    pub theme: Theme,
}

impl InputBox<'_> {
    /// Rows of text inside the border at `inner_w` columns: a prompt grows to three rows while it
    /// is being typed, everything else is one row.
    fn text_rows(input: &Input, inner_w: usize) -> u16 {
        match input {
            Input::Prompt { text, .. } => {
                let n = wrap(text, inner_w.saturating_sub(4)).len();
                n.clamp(1, 3) as u16
            }
            _ => 1,
        }
    }

    /// The box's height, border included, at a terminal `width` columns wide.
    pub fn height(input: &Input, width: u16) -> u16 {
        2 + Self::text_rows(input, width.saturating_sub(4) as usize)
    }
}

impl Widget for InputBox<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height < 3 || area.width < 8 {
            return;
        }
        let border = match self.input {
            Input::Confirm { .. } => self.theme.accent(),
            _ => dim(),
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border);
        let inner = block.inner(area);
        block.render(area, buf);
        let w = inner.width as usize;
        let caret = span(format!(" {CARET} "), self.theme.key());
        let lines: Vec<Line> = match self.input {
            Input::Prompt {
                text,
                placeholder,
                focused,
            } => {
                if text.is_empty() {
                    let mut s = vec![caret];
                    if *focused {
                        s.push(span("▏", self.theme.accent()));
                    }
                    s.push(span(placeholder.clone(), dim()));
                    vec![Line::from(fit(s, w))]
                } else {
                    let wrapped = wrap(text, w.saturating_sub(4));
                    let rows = inner.height as usize;
                    // The typing point is the end: when the prompt outgrows the box, the first
                    // lines scroll off rather than the ones being typed.
                    let skip = wrapped.len().saturating_sub(rows);
                    let last = wrapped.len() - 1;
                    wrapped
                        .iter()
                        .enumerate()
                        .skip(skip)
                        .map(|(i, l)| {
                            let lead = if i == skip {
                                caret.clone()
                            } else {
                                Span::raw("   ")
                            };
                            let mut s = vec![lead, span(l.clone(), Style::default())];
                            if i == last && *focused {
                                s.push(span("▏", self.theme.accent()));
                            }
                            Line::from(fit(s, w))
                        })
                        .collect()
                }
            }
            Input::Command { line, note } => vec![lr(
                vec![caret, span(line.clone(), dim())],
                vec![span(note.clone(), dim()), Span::raw(" ")],
                w,
            )],
            Input::Compose { target, text } => {
                let head = vec![
                    span(format!(" {target} "), bold()),
                    span(format!("{CARET} "), self.theme.key()),
                ];
                let hw = spans_width(&head);
                let room = w.saturating_sub(hw + 1 + 22);
                // The end of a long message: that is where the typing is.
                let shown = clip_left(text, room);
                let mut left = head;
                left.push(span(shown, Style::default()));
                left.push(span("▏", self.theme.accent()));
                vec![lr(
                    left,
                    vec![span("enter sends · esc cancels", dim()), Span::raw(" ")],
                    w,
                )]
            }
            Input::Confirm { question, command } => vec![lr(
                vec![
                    span(" ", Style::default()),
                    span(question.clone(), bold()),
                    span(format!("   $ {command}"), dim()),
                ],
                vec![
                    span("y", self.theme.key()),
                    span(" yes · ", dim()),
                    span("any key", bold()),
                    span(" no ", dim()),
                ],
                w,
            )],
        };
        for (i, l) in lines.iter().take(inner.height as usize).enumerate() {
            buf.set_line(inner.x, inner.y + i as u16, l, inner.width);
        }
    }
}

/// The hint row: `key verb` pairs, dot separated, keys bold and everything dim, with `? shortcuts`
/// pinned right. Pairs that do not fit are dropped whole from the end.
pub struct HintRow<'a> {
    pub hints: &'a [Hint],
}

impl Widget for HintRow<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let w = area.width as usize;
        let groups: Vec<Vec<Span>> = self
            .hints
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let mut g = Vec::new();
                if i > 0 {
                    g.push(span(" · ", dim()));
                }
                g.push(span(h.key.clone(), label()));
                g.push(span(format!(" {}", h.verb), dim()));
                g
            })
            .collect();
        let right = vec![span("? ", label()), span("shortcuts  ", dim())];
        let mut left = vec![Span::raw("  ")];
        left.extend(fit_groups(
            groups,
            w.saturating_sub(4 + spans_width(&right)),
        ));
        buf.set_line(area.x, area.y, &lr(left, right, w), area.width);
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
    use crate::home::text::width;

    #[test]
    fn sparkline_scales_to_the_peak_and_keeps_the_newest_samples() {
        assert_eq!(sparkline(&[0, 1, 2, 4, 8], 5), " ▁▂▄█");
        assert_eq!(sparkline(&[8, 8, 1, 8], 2), "▁█");
        assert_eq!(sparkline(&[], 10), "");
        assert_eq!(sparkline(&[0, 0], 10), "  ");
    }

    #[test]
    fn context_bar_fills_by_fraction_and_says_the_percentage() {
        let s = context_bar(410, 16, Theme::TRUECOLOR);
        assert_eq!(width(&s[0].content), 7);
        assert_eq!(width(&s[1].content), 9);
        assert_eq!(s[2].content, "  41% of context");
        let full = context_bar(2000, 4, Theme::TRUECOLOR);
        assert_eq!(width(&full[0].content), 4);
        assert_eq!(full[2].content, "  100% of context");
    }

    #[test]
    fn code_spans_lift_the_command_out_of_the_sentence() {
        let s = code_spans("run `gemini` once", Style::default(), Theme::TRUECOLOR);
        let text: Vec<_> = s.iter().map(|x| x.content.as_ref()).collect();
        assert_eq!(text, ["run ", "gemini", " once"]);
        assert_eq!(s[1].style, Theme::TRUECOLOR.key());
    }
}
