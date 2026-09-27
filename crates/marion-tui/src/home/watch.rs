//! Watch: the forest, one node per row, and the selected node expanded in place — what it is
//! doing, its tokens (totals, a rate sparkline, how full its context is) and its result (the branch
//! it landed and the `git merge` that takes it).

use super::GUTTER;
use super::text::{clip, fit, pad, rpad, spans_width, tokens, width};
use super::theme::{CARET, Theme, bad, bold, dim, good, spinner, warn};
use super::widgets::{centred, code_spans, context_bar, expansion, rule, section, span, sparkline};
use crate::tree::Tone;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// One node, as a forest row. Rendered text throughout: the supervisor projects, this draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    /// The whole agent id. Not drawn on the row (a UUID is wider than the name column); carried
    /// so a caller can map a row back to its node.
    pub id: String,
    /// Tree connectors, from [`crate::tree::Tree::prefix`].
    pub prefix: String,
    pub harness: String,
    /// The agent type, or the node's name where it has one.
    pub kind: String,
    /// The short id the operator types (`3c33`).
    pub short: String,
    pub tone: Tone,
    /// `2m17s` while it runs, `1h ago` once it has ended: already worded by the caller.
    pub elapsed: String,
    pub tokens: Option<u64>,
    /// One line: what it is doing now, or how it ended.
    pub doing: String,
}

/// The selected node, expanded. Every field is optional because a node reports them at different
/// times: a spawning node has no activity, a running one no result, a harness without usage
/// reporting no tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expanded {
    /// What it was asked to do.
    pub task: Option<String>,
    /// The latest activity the supervisor peeked (`cargo test · 142 passed`).
    pub activity: Option<String>,
    /// Why it is waiting on the operator, when it is.
    pub needs: Option<String>,
    pub tokens: Option<TokenView>,
    pub result: Option<ResultView>,
    /// Where it works: its worktree, or the operator's checkout for a root.
    pub workspace: Option<String>,
}

/// Token usage for one node. Counts only, never a price.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenView {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    /// Tokens per window, oldest first: the sparkline.
    pub rate: Vec<u64>,
    /// The window each `rate` sample covers, worded (`30s`).
    pub window: String,
    /// How full the context window is, in thousandths, when the harness says.
    pub context_permille: Option<u32>,
}

/// How a node ended, and what it left.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultView {
    /// One line: `landed`, `exit 101: 3 tests failed`.
    pub summary: String,
    pub tone: Option<Tone>,
    pub branch: Option<String>,
    pub added: u32,
    pub removed: u32,
    pub files: u32,
    /// The command that takes the branch, for the operator to copy.
    pub merge: Option<String>,
}

/// One line of the forest-wide activity feed, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedRow {
    pub clock: String,
    pub tone: Tone,
    pub short: String,
    pub text: String,
}

/// Everything Watch draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchView {
    /// Whether a supervisor answered. `false` is its own empty state: an empty forest would read
    /// as "no agents" rather than as "nothing is running here".
    pub supervisor: bool,
    pub rows: Vec<NodeRow>,
    pub cursor: usize,
    pub expanded: Option<Expanded>,
    pub running: usize,
    pub attention: usize,
    pub feed: Vec<FeedRow>,
    /// The filter the operator typed after `/`, when one is on.
    pub filter: Option<String>,
}

/// Below this width the tokens column goes; the expanded block still carries them.
const WIDE: usize = 90;
/// The expansion's label column.
const LABEL_W: usize = 17;
/// The spinner-and-elapsed column: `⠋ 12m04s`.
const STATUS_W: usize = 9;

pub fn render(v: &WatchView, theme: Theme, frame: usize, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width <= GUTTER * 2 {
        return;
    }
    let x = area.x + GUTTER;
    let w = (area.width - GUTTER * 2) as usize;
    if !v.supervisor {
        let y = area.y + area.height / 3;
        centred(
            buf,
            area,
            y,
            Line::from(span("No supervisor is running for this project.", bold())),
        );
        centred(
            buf,
            area,
            y + 2,
            Line::from(code_spans(
                "Start a task on the Start tab, or run `marion run <type> --prompt …` in a shell.",
                dim(),
                theme,
            )),
        );
        return;
    }
    let mut note = format!("{} · {} running", v.rows.len(), v.running);
    if v.attention > 0 {
        note.push_str(&format!(" · {} need you", v.attention));
    }
    if let Some(f) = &v.filter {
        note.push_str(&format!(" · /{f}"));
    }
    buf.set_line(x, area.y, &section("Nodes", &note), w as u16);
    if v.rows.is_empty() {
        let y = area.y + area.height / 3;
        centred(buf, area, y, Line::from(span("No nodes yet.", bold())));
        centred(
            buf,
            area,
            y + 2,
            Line::from(code_spans(
                "Start a task on the Start tab, or run `marion run` in a shell.",
                dim(),
                theme,
            )),
        );
        return;
    }

    // Every line of the list, each with the column it starts at, then a window over them that
    // keeps the selected node and its whole expansion on screen.
    let wide = w >= WIDE;
    let name_w = 30.min(w / 2);
    let mut lines: Vec<(u16, Line)> = Vec::new();
    let mut selected = (0, 0);
    for (i, n) in v.rows.iter().enumerate() {
        let sel = i == v.cursor;
        let start = lines.len();
        let (branch, through) = connectors(&n.prefix);
        lines.push((
            area.x,
            row(n, &branch, sel, i, frame, name_w, wide, w, theme),
        ));
        if sel {
            if let Some(e) = &v.expanded {
                let ew = w.saturating_sub(width(&through) + 2);
                for l in expansion(expanded_rows(e, ew, theme), LABEL_W, ew) {
                    // The tree's own lines carry on through the block, so the rows under it still
                    // read as siblings of the one above it.
                    let mut spans = vec![span(format!("  {through}  "), dim())];
                    spans.extend(l.spans);
                    lines.push((area.x, Line::from(spans)));
                }
            }
            selected = (start, lines.len());
        }
    }
    let list_top = area.y + 2;
    let list_h = (area.bottom().saturating_sub(list_top)) as usize;
    let skip = window(lines.len(), selected, list_h);
    let mut y = list_top;
    for (lx, l) in lines.iter().skip(skip).take(list_h) {
        buf.set_line(*lx, y, l, area.right().saturating_sub(*lx));
        y += 1;
    }

    // The feed takes whatever rows the list left, and only if it gets at least two of them.
    if y + 5 <= area.bottom() && !v.feed.is_empty() {
        rule(buf, x, y + 1, w as u16);
        buf.set_line(x, y + 2, &section("Activity", ""), w as u16);
        for (fy, f) in (y + 4..area.bottom()).zip(&v.feed) {
            let l = vec![
                span(format!("{}  ", f.clock), dim()),
                span(f.tone.glyph(), f.tone.style()),
                span(format!(" {}  ", f.short), Style::default()),
                span(f.text.clone(), dim()),
            ];
            buf.set_line(x, fy, &Line::from(fit(l, w)), w as u16);
        }
    }
}

/// The first line to draw so that lines `sel.0..sel.1` are visible in `rows` rows: scrolled only
/// as far as needed, and showing the top of the block when the block is taller than the window.
fn window(total: usize, sel: (usize, usize), rows: usize) -> usize {
    if total <= rows {
        return 0;
    }
    sel.1.saturating_sub(rows).min(sel.0)
}

/// `tree::Tree`'s two-column connectors, widened to three for a row that has room (`├─ `), and
/// the same levels as they continue past the row (`│  `), for the lines of its expansion.
fn connectors(prefix: &str) -> (String, String) {
    let chars: Vec<char> = prefix.chars().collect();
    let mut branch = String::new();
    let mut through = String::new();
    for unit in chars.chunks(2) {
        let (b, t) = match unit[0] {
            '├' => ("├─ ", "│  "),
            '└' => ("└─ ", "   "),
            '│' => ("│  ", "│  "),
            _ => ("   ", "   "),
        };
        branch.push_str(b);
        through.push_str(t);
    }
    (branch, through)
}

#[allow(clippy::too_many_arguments)]
fn row<'a>(
    n: &NodeRow,
    prefix: &str,
    sel: bool,
    i: usize,
    frame: usize,
    name_w: usize,
    wide: bool,
    w: usize,
    theme: Theme,
) -> Line<'a> {
    let name_style = if sel { bold() } else { Style::default() };
    let meta_style = if sel { Style::default() } else { dim() };
    // The kind is the one field that gives way, so the harness and the short id always survive.
    let fixed = width(prefix) + 2 + width(&n.harness) + 1 + 1 + width(&n.short);
    let kind = clip(&n.kind, name_w.saturating_sub(fixed + 1));
    let mut name = vec![
        span(prefix.to_string(), dim()),
        span(n.tone.glyph(), n.tone.style()),
        Span::raw(" "),
        span(n.harness.clone(), name_style),
        Span::raw(" "),
        span(kind, meta_style),
        Span::raw(" "),
        span(n.short.clone(), meta_style),
    ];
    let nw = spans_width(&name);
    name.push(Span::raw(" ".repeat(name_w.saturating_sub(nw) + 1)));
    let status = if n.tone == Tone::Live {
        vec![
            span(spinner(frame + i * 3), theme.accent()),
            span(
                format!(" {}", rpad(&n.elapsed, STATUS_W - 2)),
                theme.accent(),
            ),
        ]
    } else {
        vec![span(format!("  {}", rpad(&n.elapsed, STATUS_W - 2)), dim())]
    };
    let mut l = vec![
        span(if sel { CARET } else { " " }, theme.key()),
        Span::raw(" "),
    ];
    l.extend(name);
    l.extend(status);
    if wide {
        let t = n.tokens.map(tokens).unwrap_or_default();
        l.push(span(format!("  {}", rpad(&t, 6)), dim()));
    }
    let doing_style = match n.tone {
        Tone::Blocked => warn(),
        Tone::Failed => bad(),
        _ => dim(),
    };
    l.push(Span::raw("   "));
    l.push(span(n.doing.clone(), doing_style));
    Line::from(fit(l, w + GUTTER as usize))
}

/// The expansion's rows: label, then values; an empty label continues the row above.
fn expanded_rows<'a>(e: &Expanded, w: usize, theme: Theme) -> Vec<(String, Vec<Span<'a>>)> {
    let mut rows: Vec<(String, Vec<Span>)> = Vec::new();
    let value_w = w.saturating_sub(2 + LABEL_W);
    let mut doing = Vec::new();
    if let Some(t) = &e.task {
        doing.push(vec![span(t.clone(), Style::default())]);
    }
    if let Some(a) = &e.activity {
        doing.push(vec![
            span("last ", dim()),
            span(a.clone(), Style::default()),
        ]);
    }
    if let Some(n) = &e.needs {
        doing.push(vec![span("needs you: ", warn()), span(n.clone(), warn())]);
    }
    push_block(&mut rows, "What it's doing", doing);

    if let Some(t) = &e.tokens {
        let mut block = vec![vec![
            span(tokens(t.input), Style::default()),
            span(" in · ", dim()),
            span(tokens(t.output), Style::default()),
            span(" out · ", dim()),
            span(tokens(t.cached), Style::default()),
            span(" cached", dim()),
        ]];
        let bar_w = 16.min(value_w.saturating_sub(18));
        if !t.rate.is_empty() && bar_w > 0 {
            let peak = t.rate.iter().copied().max().unwrap_or(0);
            block.push(vec![
                span(pad(&sparkline(&t.rate, bar_w), bar_w), theme.accent()),
                span(format!("  peak {} / {}", tokens(peak), t.window), dim()),
            ]);
        }
        if let Some(p) = t.context_permille
            && bar_w > 0
        {
            block.push(context_bar(p, bar_w, theme));
        }
        push_block(&mut rows, "Tokens", block);
    }

    if let Some(r) = &e.result {
        let mut block = Vec::new();
        let summary_style = match r.tone {
            Some(Tone::Failed) => bad(),
            Some(Tone::Blocked) => warn(),
            _ => Style::default(),
        };
        match &r.branch {
            Some(b) => {
                let mut l = vec![span(b.clone(), bold())];
                if r.added + r.removed > 0 {
                    l.push(span(format!("  +{}", r.added), good()));
                    l.push(span(format!(" −{}", r.removed), bad()));
                }
                if r.files > 0 {
                    let s = if r.files == 1 { "" } else { "s" };
                    l.push(span(format!(" · {} file{s}", r.files), dim()));
                }
                block.push(l);
                if !r.summary.is_empty() {
                    block.push(vec![span(r.summary.clone(), summary_style)]);
                }
            }
            None => block.push(vec![span(r.summary.clone(), summary_style)]),
        }
        if let Some(m) = &r.merge {
            block.push(vec![
                span(m.clone(), Style::default()),
                span("   c", theme.key()),
                span(" copies", dim()),
            ]);
        }
        push_block(&mut rows, "Result", block);
    }

    if let Some(ws) = &e.workspace {
        push_block(&mut rows, "Workspace", vec![vec![span(ws.clone(), dim())]]);
    }
    rows
}

/// Label the first line of `block`, continue the rest under it.
fn push_block<'a>(rows: &mut Vec<(String, Vec<Span<'a>>)>, name: &str, block: Vec<Vec<Span<'a>>>) {
    for (i, l) in block.into_iter().enumerate() {
        rows.push((
            if i == 0 {
                name.to_string()
            } else {
                String::new()
            },
            l,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connectors_widen_and_continue() {
        assert_eq!(connectors(""), (String::new(), String::new()));
        assert_eq!(connectors("├ "), ("├─ ".into(), "│  ".into()));
        assert_eq!(connectors("│ └ "), ("│  └─ ".into(), "│     ".into()));
        assert_eq!(connectors("  ├ "), ("   ├─ ".into(), "   │  ".into()));
    }

    #[test]
    fn the_window_keeps_the_selected_block_in_view() {
        assert_eq!(window(5, (2, 3), 10), 0, "everything fits");
        assert_eq!(window(20, (2, 3), 10), 0, "near the top: no scroll");
        assert_eq!(window(20, (15, 18), 10), 8, "scrolled just far enough");
        assert_eq!(
            window(20, (5, 18), 10),
            5,
            "a block taller than the window shows its top"
        );
    }
}
