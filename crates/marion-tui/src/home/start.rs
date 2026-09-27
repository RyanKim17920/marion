//! Start: which harnesses are ready, what kind of node to run and on which model, recent runs to
//! pick up again, and — on the row above the box — the `marion run` command all of that is.

use super::GUTTER;
use super::text::{fit, lr, pad, rpad, shell_line};
use super::theme::{CARET, Ready, Theme, bad, bold, dim};
use super::widgets::{code_spans, expansion, rule, section, span};
use crate::tree::Tone;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// One harness as `marion doctor` found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessRow {
    pub name: String,
    /// The version it reported, or `None` when it is not installed or has not answered.
    pub version: Option<String>,
    pub ready: Ready,
    /// One short phrase: `ready`, `sign-in needed`, `too old, needs ≥ 0.0.350`.
    pub note: String,
    /// The surfaces marion can drive it on, worded (`headless · pane · ACP`).
    pub surfaces: String,
    /// What to do about it, with the command in backticks: ``run `gemini` once and sign in``.
    pub fix: Option<String>,
    /// Setup's detail rows, label then value: `binary`, `version`, `auth`.
    pub detail: Vec<(String, String)>,
}

/// A recent run in this project, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentRow {
    pub when: String,
    pub tone: Tone,
    /// `claude opus`: the type and the model it ran on.
    pub who: String,
    pub prompt: String,
    /// One word: `running`, `landed`, `failed`.
    pub outcome: String,
}

/// Everything Start draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartView {
    pub harnesses: Vec<HarnessRow>,
    /// The doctor probe is still running: rows may be [`Ready::Checking`].
    pub checking: bool,
    pub cursor: usize,
    /// The agent types the selected harness offers, each with its grant in one phrase:
    /// `("claude", "edits in a worktree")`, `("claude-orchestrator", "read-only, delegates")`.
    pub kinds: Vec<(String, String)>,
    pub kind: usize,
    /// `default` first, then models this project has run, most recent first.
    pub models: Vec<String>,
    pub model: usize,
    /// Run options, label then value: `("timeout", "20m")`.
    pub options: Vec<(String, String)>,
    pub recent: Vec<RecentRow>,
    /// The `marion run` argv the current choices add up to.
    pub echo: Vec<String>,
}

/// The harness name and version columns.
const NAME_W: usize = 10;
const VERSION_W: usize = 10;
/// Labels of the choice rows (`TYPE`, `MODEL`, `OPTIONS`).
const CHOICE_W: usize = 10;

pub fn render(v: &StartView, theme: Theme, frame: usize, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width <= GUTTER * 2 {
        return;
    }
    let x = area.x + GUTTER;
    let w = (area.width - GUTTER * 2) as usize;
    let end = area.bottom();
    let put = |buf: &mut Buffer, lx: u16, y: u16, l: &Line| {
        if y < end.saturating_sub(1) {
            buf.set_line(lx, y, l, area.right().saturating_sub(lx));
        }
    };

    buf.set_line(
        x,
        area.y,
        &section("Harness", &readiness(&v.harnesses, v.checking)),
        w as u16,
    );
    let mut y = area.y + 2;
    for (i, h) in v.harnesses.iter().enumerate() {
        let sel = i == v.cursor;
        put(
            buf,
            area.x,
            y,
            &harness_line(h, sel, frame + i, theme, w + GUTTER as usize, None),
        );
        y += 1;
        if sel
            && h.ready != Ready::Ready
            && let Some(fix) = &h.fix
        {
            let fix = vec![("fix".into(), code_spans(fix, Style::default(), theme))];
            for l in expansion(fix, 5, w.saturating_sub(2)) {
                put(buf, x + 2, y, &l);
                y += 1;
            }
        }
    }
    y += 1;

    if !v.kinds.is_empty() {
        put(buf, x, y, &choices("Type", &v.kinds, v.kind, w, theme));
        y += 1;
    }
    if !v.models.is_empty() {
        let pairs: Vec<(String, String)> = v
            .models
            .iter()
            .map(|m| (m.clone(), String::new()))
            .collect();
        put(buf, x, y, &choices("Model", &pairs, v.model, w, theme));
        y += 1;
    }
    if !v.options.is_empty() {
        let mut l = vec![span(pad("OPTIONS", CHOICE_W), super::theme::label())];
        for (i, (k, val)) in v.options.iter().enumerate() {
            if i > 0 {
                l.push(span(" · ", dim()));
            }
            l.push(span(format!("{k} "), dim()));
            l.push(span(val.clone(), Style::default()));
        }
        put(buf, x, y, &Line::from(fit(l, w)));
        y += 1;
    }

    // The command, on the body's last row: right above the box it will run from.
    if !v.echo.is_empty() && area.height > 2 {
        let echo = vec![
            span("$ ", theme.accent()),
            span(shell_line(&v.echo, w - 2), dim()),
        ];
        buf.set_line(x, end - 1, &Line::from(fit(echo, w)), w as u16);
    }

    // Recent runs in whatever rows are left, if at least one fits under its header.
    y += 1;
    if y + 4 <= end.saturating_sub(2) && !v.recent.is_empty() {
        rule(buf, x, y, w as u16);
        put(
            buf,
            x,
            y + 1,
            &section("Recent", "marion resume <id> picks one up"),
        );
        for (ry, r) in (y + 3..end.saturating_sub(2)).zip(&v.recent) {
            let outcome_style = if r.tone == Tone::Failed { bad() } else { dim() };
            let l = lr(
                vec![
                    span(pad(&r.when, 6), dim()),
                    span(r.tone.glyph(), r.tone.style()),
                    span(format!(" {}", pad(&r.who, 17)), Style::default()),
                    span(r.prompt.clone(), dim()),
                ],
                vec![span(rpad(&r.outcome, 7), outcome_style)],
                w,
            );
            buf.set_line(x, ry, &l, w as u16);
        }
    }
}

/// `3 of 6 ready`, or `checking…` while the probe runs.
pub fn readiness(rows: &[HarnessRow], checking: bool) -> String {
    let ok = rows.iter().filter(|h| h.ready == Ready::Ready).count();
    if checking {
        format!("{ok} of {} ready · checking…", rows.len())
    } else {
        format!("{ok} of {} ready", rows.len())
    }
}

/// A harness row: caret, glyph, name, version, note, and — when `surfaces_at` is given — the
/// surfaces from that column on. Shared with Setup, which shows the surfaces.
pub fn harness_line<'a>(
    h: &HarnessRow,
    sel: bool,
    frame: usize,
    theme: Theme,
    w: usize,
    surfaces_at: Option<usize>,
) -> Line<'a> {
    let mut l = vec![
        span(if sel { CARET } else { " " }, theme.key()),
        Span::raw(" "),
        span(h.ready.glyph(frame), h.ready.style(theme)),
        Span::raw(" "),
        span(
            pad(&h.name, NAME_W),
            if sel { bold() } else { Style::default() },
        ),
        span(pad(h.version.as_deref().unwrap_or("—"), VERSION_W), dim()),
    ];
    match surfaces_at {
        Some(col) => {
            l.push(span(pad(&h.note, col), h.ready.note_style()));
            l.push(span(h.surfaces.clone(), dim()));
        }
        None => l.push(span(h.note.clone(), h.ready.note_style())),
    }
    Line::from(fit(l, w))
}

/// `LABEL     chosen   other   other`: the chosen value in the accent, bold; each value's phrase
/// dim after it.
fn choices<'a>(
    name: &str,
    values: &[(String, String)],
    chosen: usize,
    w: usize,
    theme: Theme,
) -> Line<'a> {
    let mut l = vec![span(
        pad(&name.to_uppercase(), CHOICE_W),
        super::theme::label(),
    )];
    for (i, (v, phrase)) in values.iter().enumerate() {
        if i > 0 {
            l.push(Span::raw("   "));
        }
        l.push(span(
            v.clone(),
            if i == chosen { theme.key() } else { dim() },
        ));
        if !phrase.is_empty() {
            l.push(span(format!(" {phrase}"), dim()));
        }
    }
    Line::from(fit(l, w))
}
