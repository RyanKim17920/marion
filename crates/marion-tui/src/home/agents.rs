//! The agents page: one row per agent on the left, children indented under their parent, and the
//! selected agent previewed on the right — what it is doing, or how it ended and what it left.
//! Below [`SPLIT_MIN`] columns the preview stacks under the list. `enter` on a headless agent
//! gives its output the whole body ([`AgentsView::full`]).

use super::text::{clip, clip_left, fit, pad, rpad, tokens, width};
use super::theme::{Theme, bad, bold, dim, good, spinner, warn};
use super::widgets::{centred, row_bar, span};
use crate::tree::Tone;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// One row of the list. Rendered text throughout: the supervisor projects, this draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    /// The agent id, or a race or workflow header's row id. Not drawn; carried so a caller can map
    /// a row back to what it is.
    pub id: String,
    /// Tree connectors, from [`crate::tree::Tree::row`].
    pub prefix: String,
    pub tone: Tone,
    /// The task's first line, or the agent type where there is no task.
    pub title: String,
    /// The harness and the model, where one was asked for: `codex gpt-5`.
    pub who: String,
    /// How long it has run (or ran): `12m`. Empty where no start was recorded.
    pub time: String,
    pub tokens: Option<u64>,
    /// Shown only for where a search's match sits under it: drawn dim, never selected.
    pub dim: bool,
}

/// How a word in the preview is drawn. The supervisor says which; this says what each looks like.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Ink {
    #[default]
    Plain,
    Dim,
    Bold,
    Good,
    Bad,
    Warn,
}

impl Ink {
    fn style(self) -> Style {
        match self {
            Ink::Plain => Style::default(),
            Ink::Dim => dim(),
            Ink::Bold => bold(),
            Ink::Good => good(),
            Ink::Bad => bad(),
            Ink::Warn => warn(),
        }
    }
}

/// One line of an agent's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLine {
    pub clock: String,
    pub kind: LineKind,
    pub text: String,
}

/// What an output line is, which says how it is drawn and clipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LineKind {
    /// A tool call: clipped at its end.
    #[default]
    Call,
    /// A call that changed files (`~ src/a.rs`): clipped from the left of its paths, so the file
    /// name stays in view.
    Files,
    /// A call that finished well (`ok 3.2s`).
    Done,
    /// A call that did not (`exit 1 40ms`).
    Failed,
    /// Text it wrote, rather than a tool call: dim.
    Said,
}

/// The selected agent, previewed. Every part is optional: a spawning agent has no output yet, a
/// running one no result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    /// `codex 91cd`, then the title.
    pub who: String,
    pub title: String,
    /// What it is waiting on the operator for, when it is.
    pub needs: Option<String>,
    /// Facts about it, one line each: how it ended, its branch, its review, its budget.
    pub facts: Vec<Vec<(String, Ink)>>,
    /// Its output so far, oldest first.
    pub stream: Vec<StreamLine>,
    /// Lines up from the newest the output is scrolled; 0 follows the end.
    pub scroll: usize,
    /// Why there is no output to show, when there is none (`no output yet`, `reading`).
    pub quiet: Option<String>,
}

/// Everything the agents page draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentsView {
    pub rows: Vec<AgentRow>,
    pub cursor: usize,
    pub preview: Option<Preview>,
    /// The selected agent's output fills the body: `enter` on a headless agent.
    pub full: bool,
}

/// From this width the preview sits beside the list; below it, under the list.
pub const SPLIT_MIN: u16 = 100;
/// The list's share of a split screen, in percent.
const LIST_SHARE: u16 = 60;
/// The stacked preview's least share of the body's height, in percent: it takes every row the list
/// does not need beyond that.
const STACKED_SHARE: u16 = 40;
/// The `harness model` column.
const WHO_W: usize = 14;
/// The run-time column: `12m`, `1h04`.
const TIME_W: usize = 5;
/// The tokens column: `312k`.
const TOKENS_W: usize = 5;
/// Below this list width the tokens column goes; the preview still carries them.
const TOKENS_MIN: usize = 56;
/// Below this list width the `harness model` column goes too.
const WHO_MIN: usize = 40;

pub fn render(v: &AgentsView, theme: Theme, frame: usize, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    if v.rows.is_empty() {
        centred(
            buf,
            area,
            area.y + area.height / 3,
            Line::from(span("no agents", dim())),
        );
        return;
    }
    if v.full
        && let Some(p) = &v.preview
    {
        let inner = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        preview(p, inner, buf);
        return;
    }
    let (list, pane) = split(area, v.rows.len());
    list_rows(v, theme, frame, list, buf);
    match pane {
        Pane::Beside(r) => {
            for y in r.top()..r.bottom() {
                buf[(r.x, y)].set_symbol("│").set_style(dim());
            }
            if let Some(p) = &v.preview {
                let inner = Rect::new(r.x + 2, r.y, r.width.saturating_sub(3), r.height);
                preview(p, inner, buf);
            }
        }
        Pane::Below(r) => {
            for x in r.left()..r.right() {
                buf[(x, r.y)].set_symbol("─").set_style(dim());
            }
            if let Some(p) = &v.preview {
                let inner = Rect::new(
                    r.x + 1,
                    r.y + 1,
                    r.width.saturating_sub(2),
                    r.height.saturating_sub(1),
                );
                preview(p, inner, buf);
            }
        }
        Pane::None => {}
    }
}

/// Where the preview goes: beside the list (its first column the `│`), below it (its first row
/// the `─`), or nowhere on a body too small to hold both.
enum Pane {
    Beside(Rect),
    Below(Rect),
    None,
}

fn split(area: Rect, rows: usize) -> (Rect, Pane) {
    if area.width >= SPLIT_MIN {
        let list_w = share(area.width, LIST_SHARE);
        let list = Rect::new(area.x, area.y, list_w, area.height);
        let pane = Rect::new(area.x + list_w, area.y, area.width - list_w, area.height);
        return (list, Pane::Beside(pane));
    }
    let spare = area
        .height
        .saturating_sub(u16::try_from(rows).unwrap_or(u16::MAX));
    let pane_h = share(area.height, STACKED_SHARE).max(spare);
    // A preview of under three rows (the rule, a title, one line) says nothing: the list keeps them.
    if pane_h < 3 {
        return (area, Pane::None);
    }
    let list = Rect::new(area.x, area.y, area.width, area.height - pane_h);
    let pane = Rect::new(area.x, list.bottom(), area.width, pane_h);
    (list, Pane::Below(pane))
}

/// `pct` percent of `n`, worked in `u32`: a terminal more than 1092 columns wide would overflow
/// `u16` on the way.
fn share(n: u16, pct: u16) -> u16 {
    (u32::from(n) * u32::from(pct) / 100) as u16
}

/// The list: a window over the rows that keeps the cursor in view, the selected row in reverse.
fn list_rows(v: &AgentsView, theme: Theme, frame: usize, area: Rect, buf: &mut Buffer) {
    let rows = area.height as usize;
    let skip = window(v.rows.len(), v.cursor, rows);
    // One column of margin on the left, the bar starting there; one before the `│` on the right.
    let w = area.width.saturating_sub(2) as usize;
    for (y, (i, r)) in (area.y..area.bottom()).zip(v.rows.iter().enumerate().skip(skip)) {
        let sel = i == v.cursor;
        let line = row(r, sel, i, frame, w, theme);
        buf.set_line(area.x + 1, y, &line, w as u16);
    }
    let below = v.rows.len().saturating_sub(skip + rows);
    if below > 0 && rows > 1 {
        let note = format!("+{below} below");
        let x = (area.x + 1 + w as u16).saturating_sub(width(&note) as u16);
        buf.set_string(x, area.bottom() - 1, note, dim());
    }
}

/// The first row to draw so that `cursor` is visible in `rows` rows, scrolled no further than it
/// has to be.
fn window(total: usize, cursor: usize, rows: usize) -> usize {
    if total <= rows || rows == 0 {
        return 0;
    }
    (cursor + 1).saturating_sub(rows).min(total - rows)
}

/// `<tree> <glyph> <title>  <harness model>  <time>  <tokens>`, the numbers right-aligned, in `w`
/// columns. The title gives way first, then the tokens column, then the `harness model` one.
fn row<'a>(r: &AgentRow, sel: bool, i: usize, frame: usize, w: usize, theme: Theme) -> Line<'a> {
    let glyph = match r.tone {
        Tone::Live => spinner(frame + i * 3),
        Tone::Unknown => "○",
        t => t.glyph(),
    };
    let who_on = w >= WHO_MIN;
    let tokens_on = w >= TOKENS_MIN;
    let right_w =
        2 + TIME_W + if who_on { WHO_W + 2 } else { 0 } + if tokens_on { TOKENS_W + 1 } else { 0 };
    let lead_w = 1 + width(&r.prefix) + 2;
    let title_w = w.saturating_sub(lead_w + right_w);
    let mut right = Vec::new();
    if who_on {
        right.push(span(format!("  {}", pad(&r.who, WHO_W)), dim()));
    }
    right.push(span(format!("  {}", rpad(&r.time, TIME_W)), dim()));
    if tokens_on {
        let t = r.tokens.map(tokens).unwrap_or_default();
        right.push(span(format!(" {}", rpad(&t, TOKENS_W)), dim()));
    }
    let mut l = vec![
        Span::raw(" "),
        span(r.prefix.clone(), dim()),
        span(glyph, glyph_style(r.tone)),
        Span::raw(" "),
        span(
            pad(&r.title, title_w),
            if sel { bold() } else { Style::default() },
        ),
    ];
    l.extend(right);
    if r.dim {
        for s in &mut l {
            s.style = dim();
        }
    }
    row_bar(l, w, sel, theme)
}

/// The state glyph's colour: the tone's own, except that a stop with no verdict is dim rather than
/// the unmeasured grey, so it recedes with the finished rows.
fn glyph_style(tone: Tone) -> Style {
    match tone {
        Tone::Unknown => dim(),
        t => t.style(),
    }
}

/// The preview, in `area`: the agent and its title, what it needs, its facts, then as much of its
/// output as fits, the newest at the bottom unless scrolled back.
fn preview(p: &Preview, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let w = area.width as usize;
    let mut lines: Vec<Line> = vec![Line::from(fit(
        vec![
            span(p.who.clone(), bold()),
            Span::raw("  "),
            span(p.title.clone(), Style::default()),
        ],
        w,
    ))];
    if let Some(n) = &p.needs {
        lines.push(Line::from(fit(vec![span(n.clone(), warn())], w)));
    }
    if !p.facts.is_empty() {
        lines.push(Line::default());
        for f in &p.facts {
            let spans = f
                .iter()
                .map(|(t, ink)| span(t.clone(), ink.style()))
                .collect();
            lines.push(Line::from(fit(spans, w)));
        }
    }
    lines.push(Line::default());
    let rows = area.height as usize;
    let room = rows.saturating_sub(lines.len());
    let (out, above, below) = output(p, room, w);
    lines.extend(out);
    for (y, l) in (area.y..area.bottom()).zip(&lines) {
        buf.set_line(area.x, y, l, area.width);
    }
    let note = [(above, "above"), (below, "below")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, at)| format!("+{n} {at}"))
        .collect::<Vec<_>>()
        .join("  ");
    if !note.is_empty() && rows > 1 {
        let x = area.right().saturating_sub(width(&note) as u16);
        buf.set_string(x, area.bottom() - 1, note, dim());
    }
}

/// At most `room` rows of output, and how many lines are above and below them. Output that does
/// not fit leaves the last row free for the note that says so.
fn output<'a>(p: &Preview, room: usize, w: usize) -> (Vec<Line<'a>>, usize, usize) {
    if room == 0 {
        return (Vec::new(), 0, 0);
    }
    if p.stream.is_empty() {
        let why = p.quiet.clone().unwrap_or_else(|| "no output yet".into());
        return (vec![Line::from(span(why, dim()))], 0, 0);
    }
    let total = p.stream.len();
    let shown = if total > room && room > 1 {
        room - 1
    } else {
        room
    };
    let scroll = p.scroll.min(total.saturating_sub(shown));
    let end = total - scroll;
    let start = end.saturating_sub(shown);
    let lines = p.stream[start..end]
        .iter()
        .map(|l| {
            let clock = format!("{}  ", l.clock);
            let rest = w.saturating_sub(width(&clock));
            let text = match l.kind {
                LineKind::Said => span(clip(&l.text, rest), dim()),
                LineKind::Files => span(keep_end(&l.text, rest), Style::default()),
                LineKind::Done => span(clip(&l.text, rest), good()),
                LineKind::Failed => span(clip(&l.text, rest), bad()),
                LineKind::Call => span(clip(&l.text, rest), Style::default()),
            };
            Line::from(vec![span(clock, dim()), text])
        })
        .collect();
    (lines, start, scroll)
}

/// `text` in `w` columns with its first word kept and the rest clipped from the left: `~ ` and
/// then the end of a path, which names the file.
fn keep_end(text: &str, w: usize) -> String {
    match text.split_once(' ') {
        Some((head, rest)) if width(head) + 2 < w => {
            format!("{head} {}", clip_left(rest, w - width(head) - 1))
        }
        _ => clip(text, w),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_split_holds_on_a_terminal_wider_than_u16_math_allows() {
        let (list, pane) = split(Rect::new(0, 0, u16::MAX, u16::MAX), 3);
        assert_eq!(list.width, share(u16::MAX, LIST_SHARE));
        assert!(matches!(pane, Pane::Beside(p) if p.width == u16::MAX - list.width));
        let (list, pane) = split(Rect::new(0, 0, 80, u16::MAX), 3);
        assert!(matches!(pane, Pane::Below(p) if p.bottom() == u16::MAX && list.height < 4));
    }

    /// A changed file keeps its name in view however deep its path: the path is clipped from the
    /// left, after the `~` that says what the line is.
    #[test]
    fn a_changed_file_keeps_its_name_when_its_path_is_clipped() {
        let deep = "~ crates/marion-supervisor/src/home/view.rs";
        assert_eq!(keep_end(deep, 60), deep);
        assert_eq!(keep_end(deep, 20), "~ …/src/home/view.rs");
        assert_eq!(
            keep_end("~ a/very/deep/lib.rs +2 more", 18),
            "~ …/lib.rs +2 more"
        );
        assert_eq!(width(&keep_end(deep, 20)), 20);
        assert_eq!(keep_end(deep, 3), "~ …");
    }

    #[test]
    fn the_window_keeps_the_cursor_in_view_and_scrolls_no_further() {
        assert_eq!(window(5, 2, 10), 0, "everything fits");
        assert_eq!(window(20, 2, 10), 0, "near the top: no scroll");
        assert_eq!(window(20, 15, 10), 6, "scrolled just far enough");
        assert_eq!(window(20, 19, 10), 10, "the last row at the bottom");
    }

    /// Wide enough, the preview sits beside the list; narrower, under it, with every row the list
    /// does not need and never under its share; too short for a useful preview, the list keeps the
    /// whole body.
    #[test]
    fn the_preview_moves_under_the_list_below_the_split_width() {
        let (list, pane) = split(Rect::new(0, 1, 120, 30), 40);
        assert_eq!(list.width, 72);
        assert!(matches!(pane, Pane::Beside(r) if r.x == 72 && r.width == 48));
        let (list, pane) = split(Rect::new(0, 1, 80, 22), 40);
        assert_eq!(list.height, 14);
        assert!(matches!(pane, Pane::Below(r) if r.y == 15 && r.height == 8));
        let (list, pane) = split(Rect::new(0, 1, 80, 22), 3);
        assert_eq!(list.height, 3, "a short list keeps only its rows");
        assert!(matches!(pane, Pane::Below(r) if r.y == 4 && r.height == 19));
        let (list, pane) = split(Rect::new(0, 1, 80, 5), 40);
        assert_eq!(list.height, 5);
        assert!(matches!(pane, Pane::None));
    }
}
