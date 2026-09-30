//! Watch: the forest, one node per row, and the selected node expanded in place — what it is
//! doing, its tokens (totals, a rate sparkline, how full its context is) and its result (the branch
//! it landed and the `git merge` that takes it).

use super::GUTTER;
use super::text::{clip, clip_left, fit, pad, rpad, spans_width, tokens, width, wrap};
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
    /// A parent's subtree spend, already worded (`Σ184k`, or `Σ1.2M/5M` against a tree budget),
    /// and whether it is past the budget's warn line. `None` on a leaf, whose subtree is itself.
    pub subtree: Option<(String, bool)>,
    /// One line: what it is doing now, or how it ended.
    pub doing: String,
}

/// The selected node, expanded. Every field is optional because a node reports them at different
/// times: a spawning node has no stream, a running one no result, a harness without usage
/// reporting no tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expanded {
    /// Still running: its stream section reads "Running" rather than "Ran".
    pub live: bool,
    /// What marion sent it. A root has no contract and so no task.
    pub task: Option<TaskView>,
    /// Messages queued for it (steers), oldest first, already worded.
    pub messages: Vec<MessageView>,
    /// Everything it has run, oldest first: the live action stream.
    pub stream: Vec<StreamLine>,
    /// How many lines up from the newest the stream window is scrolled; 0 follows the end.
    pub stream_scroll: usize,
    /// Why marion cannot show a stream for this node, when it cannot (not the same as "nothing").
    pub stream_unread: Option<String>,
    /// Why it is waiting on the operator, when it is.
    pub needs: Option<String>,
    pub tokens: Option<TokenView>,
    /// A parent's subtree, in one worded line (`Σ1.3M tokens · 14 files · 12m04s wall · 6 nodes
    /// (2 running)`, and its tree budget), and whether it is past the budget's warn line.
    pub subtree: Option<(String, bool)>,
    pub result: Option<ResultView>,
    /// Where it works: its worktree, or the operator's checkout for a root.
    pub workspace: Option<String>,
    /// An endpoint node's `provider:model · route`, already worded.
    pub endpoint: Option<String>,
    /// What its harness can be asked to do on its surfaces, each with whether marion has measured
    /// it there: the unmeasured ones are drawn in [`crate::tree::greyed`], never hidden — M5's
    /// "greying out what they cannot do", decided by the supervisor as the tree screen's strip was.
    pub caps: Vec<(String, bool)>,
}

/// The task as the node received it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskView {
    pub prompt: String,
    /// What marion appended (its report instruction): drawn dim, as marion's words.
    pub appended: Option<String>,
    pub acceptance: Vec<String>,
    pub verification: Vec<String>,
}

/// One queued message: when, and the rest already worded (`operator · 41 bytes · delivered`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    pub clock: String,
    pub text: String,
}

/// One line of the action stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamLine {
    pub clock: String,
    pub kind: LineKind,
    pub text: String,
}

/// What a stream line is, which says how it is drawn when it does not fit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LineKind {
    /// A tool call: clipped at its end.
    #[default]
    Call,
    /// A call that changed files (`~ src/a.rs`): clipped from the left of its paths, so the file
    /// name stays in view.
    Files,
    /// A call that finished well (`✓ 3.2s`).
    Done,
    /// A call that did not (`exit 1 · 40ms`).
    Failed,
    /// Text it wrote, rather than a tool call: drawn dim.
    Said,
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
    /// The whole forest's spend, already worded (`Σ2.1M`), for the header. `None` where no node
    /// reported a figure.
    pub total: Option<String>,
    /// Whether a supervisor answered. `false` is its own empty state: an empty forest would read
    /// as "no agents" rather than as "nothing is running here".
    pub supervisor: bool,
    pub rows: Vec<NodeRow>,
    pub cursor: usize,
    pub expanded: Option<Expanded>,
    pub running: usize,
    pub attention: usize,
    /// The first node's own words on what it needs, where the supervisor gave some — a held boot
    /// dialog's one action — drawn after the count so the operator reads it without selecting.
    pub attention_note: Option<String>,
    pub feed: Vec<FeedRow>,
    /// The filter the operator typed after `/`, when one is on.
    pub filter: Option<String>,
    /// The selected agent's stream fills the screen (Enter on a headless agent), rather than the
    /// forest with it expanded in place.
    pub full_stream: bool,
}

/// Below this width the tokens column goes; the expanded block still carries them.
const WIDE: usize = 90;
/// The subtree column beside the tokens: `Σ1.2M/5M`.
const SUBTREE_W: usize = 9;
/// The expansion's label column.
const LABEL_W: usize = 17;
/// The spinner-and-elapsed column: `⠋ 12m04s`.
const STATUS_W: usize = 9;
/// Rows of prompt the Task section shows before it ellipsises.
const PROMPT_ROWS: usize = 3;
/// The stream window's height, when the screen has room for it.
const STREAM_MIN: usize = 3;
const STREAM_MAX: usize = 12;

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
    if let Some(total) = &v.total {
        note.push_str(&format!(" · {total}"));
    }
    if v.attention > 0 {
        note.push_str(&format!(" · {} need you", v.attention));
        if let Some(what) = &v.attention_note {
            note.push_str(&format!(": {what}"));
        }
    }
    if let Some(f) = &v.filter {
        note.push_str(&format!(" · /{f}"));
    }
    if v.full_stream
        && let (Some(n), Some(e)) = (v.rows.get(v.cursor), &v.expanded)
    {
        let title = format!("{} {} · {}", n.kind, n.short, n.doing);
        buf.set_line(x, area.y, &section("Stream", &title), w as u16);
        let h = area.height.saturating_sub(2) as usize;
        for (y, l) in (area.y + 2..area.bottom()).zip(stream_block(e, h, w, theme)) {
            buf.set_line(x, y, &Line::from(fit(l, w)), w as u16);
        }
        return;
    }
    buf.set_line(x, area.y, &section("Agents", &note), w as u16);
    if v.rows.is_empty() {
        let y = area.y + area.height / 3;
        centred(buf, area, y, Line::from(span("No agents yet.", bold())));
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
    // The subtree column is drawn only where some row has something in it.
    let sub_col = wide && v.rows.iter().any(|r| r.subtree.is_some());
    let name_w = 30.min(w / 2);
    let mut lines: Vec<(u16, Line)> = Vec::new();
    let mut selected = (0, 0);
    for (i, n) in v.rows.iter().enumerate() {
        let sel = i == v.cursor;
        let start = lines.len();
        let (branch, through) = connectors(&n.prefix);
        lines.push((
            area.x,
            row(n, &branch, sel, i, frame, name_w, (wide, sub_col), w, theme),
        ));
        if sel {
            if let Some(e) = &v.expanded {
                let ew = w.saturating_sub(width(&through) + 2);
                // The stream window gets the rows the rest of the list and block leave, within
                // bounds: the one part of the block whose height is the screen's to choose.
                // Compact when the full block would push the other rows off a short screen.
                let room = list_rows(area).saturating_sub(v.rows.len());
                let compact = expanded_rows(e, ew, theme, 0, false).len() + STREAM_MIN > room;
                let fixed = expanded_rows(e, ew, theme, 0, compact).len();
                let stream_h = room.saturating_sub(fixed).clamp(STREAM_MIN, STREAM_MAX);
                let block = expanded_rows(e, ew, theme, stream_h, compact);
                for l in expansion(block, LABEL_W, ew) {
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
    let list_h = list_rows(area);
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

/// Rows the node list has: the body under its header and a blank row.
fn list_rows(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
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
    (wide, sub_col): (bool, bool),
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
    if sub_col {
        let (sub, over) = n.subtree.clone().unwrap_or_default();
        l.push(span(
            format!(" {}", rpad(&sub, SUBTREE_W)),
            if over { warn() } else { dim() },
        ));
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

/// The expansion's rows: label, then values; an empty label continues the row above. The Running
/// section gets `stream_h` rows of window (none when 0, which is how the caller measures the rest).
fn expanded_rows<'a>(
    e: &Expanded,
    w: usize,
    theme: Theme,
    stream_h: usize,
    compact: bool,
) -> Vec<(String, Vec<Span<'a>>)> {
    let mut rows: Vec<(String, Vec<Span>)> = Vec::new();
    let value_w = w.saturating_sub(2 + LABEL_W);

    if let Some(t) = &e.task {
        let mut block = Vec::new();
        let keep = if compact { 1 } else { PROMPT_ROWS };
        let mut prompt = wrap(&t.prompt, value_w);
        if prompt.len() > keep {
            prompt.truncate(keep);
            let last = prompt.last_mut().expect("keep > 0");
            *last = clip(&format!("{last} …"), value_w);
        }
        block.extend(prompt.into_iter().map(|l| vec![span(l, Style::default())]));
        if compact {
            // The criteria and checks as counts, one row; the full list shows on a taller window.
            let mut summary = Vec::new();
            if !t.acceptance.is_empty() {
                let n = t.acceptance.len();
                summary.push(format!(
                    "✓ {n} {}",
                    if n == 1 { "criterion" } else { "criteria" }
                ));
            }
            if !t.verification.is_empty() {
                let n = t.verification.len();
                summary.push(format!("$ {n} check{}", if n == 1 { "" } else { "s" }));
            }
            if !summary.is_empty() {
                block.push(vec![span(summary.join(" · "), dim())]);
            }
        } else {
            if let Some(a) = &t.appended {
                block.push(vec![span("+ marion: ", dim()), span(a.clone(), dim())]);
            }
            for a in &t.acceptance {
                block.push(vec![span("✓ ", dim()), span(a.clone(), Style::default())]);
            }
            for v in &t.verification {
                block.push(vec![span("$ ", dim()), span(v.clone(), Style::default())]);
            }
        }
        push_block(&mut rows, "Task", block);
    }

    if !e.messages.is_empty() {
        // Compact keeps the latest steer only: the one an operator just sent.
        let shown = if compact { e.messages.len() - 1 } else { 0 };
        let block = e.messages[shown..]
            .iter()
            .map(|m| {
                vec![
                    span(format!("{}  ", m.clock), dim()),
                    span(m.text.clone(), dim()),
                ]
            })
            .collect();
        push_block(&mut rows, "Steers", block);
    }

    if let Some(n) = &e.needs {
        // What it needs, then (after the last ` · `) what the operator can do: a row each, so the
        // direction is not the part a narrow screen clips.
        let block = match n.rsplit_once(" · ") {
            Some((what, then)) => vec![
                vec![span(what.to_string(), warn())],
                vec![span(then.to_string(), dim())],
            ],
            None => vec![vec![span(n.clone(), warn())]],
        };
        push_block(&mut rows, "Needs you", block);
    }

    if let Some(m) = &e.endpoint {
        push_block(
            &mut rows,
            "Model",
            vec![vec![span(m.clone(), Style::default())]],
        );
    }

    let running = if e.live { "Running" } else { "Ran" };
    if stream_h > 0 {
        push_block(
            &mut rows,
            running,
            stream_block(e, stream_h, value_w, theme),
        );
    } else {
        // Measuring the rest of the block: the section's label row still counts.
        push_block(&mut rows, running, vec![vec![]]);
    }

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
        // One sample is a lone block, not a trend: the line waits for a second one.
        if !compact && t.rate.len() >= 2 && bar_w > 0 {
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

    if let Some((line, over)) = &e.subtree {
        let style = if *over { warn() } else { Style::default() };
        let block = wrap(line, value_w)
            .into_iter()
            .map(|l| vec![span(l, style)])
            .collect();
        push_block(&mut rows, "Subtree", block);
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

    if !e.caps.is_empty() {
        let mut l = Vec::new();
        for (i, (name, on)) in e.caps.iter().enumerate() {
            if i > 0 {
                l.push(span(" · ", dim()));
            }
            let style = if *on {
                Style::default()
            } else {
                crate::tree::greyed()
            };
            l.push(span(name.clone(), style));
        }
        push_block(&mut rows, "Can", vec![l]);
    }

    if let Some(ws) = e.workspace.as_ref().filter(|_| !compact) {
        push_block(
            &mut rows,
            "Workspace",
            vec![vec![
                span(ws.clone(), dim()),
                span("   o", theme.key()),
                span(" opens a shell", dim()),
            ]],
        );
    }
    rows
}

/// The Running section: a window of `h` stream lines, the newest at the bottom unless scrolled,
/// and a dim line saying what is above and below it when anything is.
fn stream_block<'a>(e: &Expanded, h: usize, w: usize, theme: Theme) -> Vec<Vec<Span<'a>>> {
    if let Some(why) = &e.stream_unread {
        return vec![vec![span(why.clone(), dim())]];
    }
    if e.stream.is_empty() {
        return vec![vec![span("nothing recorded yet", dim())]];
    }
    let total = e.stream.len();
    let overflow = total > h;
    // One row of the window goes to the position line when the stream does not fit.
    let rows = if overflow {
        h.saturating_sub(1).max(1)
    } else {
        h
    };
    let scroll = e.stream_scroll.min(total.saturating_sub(rows));
    let end = total - scroll;
    let start = end.saturating_sub(rows);
    let mut block: Vec<Vec<Span>> = e.stream[start..end]
        .iter()
        .map(|l| {
            let clock = format!("{}  ", l.clock);
            let text = match l.kind {
                LineKind::Said => span(format!("“{}”", l.text), dim()),
                LineKind::Files => span(
                    keep_end(&l.text, w.saturating_sub(width(&clock))),
                    Style::default(),
                ),
                LineKind::Done => span(l.text.clone(), good()),
                LineKind::Failed => span(l.text.clone(), bad()),
                LineKind::Call => span(l.text.clone(), Style::default()),
            };
            vec![span(clock, dim()), text]
        })
        .collect();
    if overflow {
        let mut pos = vec![span(format!("↑ {start} earlier"), dim())];
        if scroll > 0 {
            pos.push(span(format!(" · ↓ {scroll} newer"), dim()));
            pos.push(span("   J", theme.key()));
            pos.push(span(" follows", dim()));
        } else {
            pos.push(span("   K", theme.key()));
            pos.push(span(" scrolls back", dim()));
        }
        block.push(pos);
    }
    block
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
        // Too narrow for the head and any of the path: the whole line clipped as any other.
        assert_eq!(keep_end(deep, 3), "~ …");
    }

    #[test]
    fn connectors_widen_and_continue() {
        assert_eq!(connectors(""), (String::new(), String::new()));
        assert_eq!(connectors("├ "), ("├─ ".into(), "│  ".into()));
        assert_eq!(connectors("│ └ "), ("│  └─ ".into(), "│     ".into()));
        assert_eq!(connectors("  ├ "), ("   ├─ ".into(), "   │  ".into()));
    }

    /// An endpoint node's expansion carries a `Model` section with `provider:model · route`; a node
    /// without one has no such section.
    #[test]
    fn an_endpoint_nodes_expansion_names_its_model_and_route() {
        let text = |e: &Expanded| {
            expanded_rows(e, 80, Theme::TRUECOLOR, 0, false)
                .into_iter()
                .map(|(label, spans)| {
                    let value: String = spans.iter().map(|s| s.content.to_string()).collect();
                    format!("{label}|{value}")
                })
                .collect::<Vec<_>>()
        };
        let plain = Expanded::default();
        assert!(!text(&plain).iter().any(|r| r.starts_with("Model|")));
        let e = Expanded {
            endpoint: Some("groq:llama-3.3-70b · translated".into()),
            ..Expanded::default()
        };
        assert!(
            text(&e).contains(&"Model|groq:llama-3.3-70b · translated".to_string()),
            "{:?}",
            text(&e)
        );
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
