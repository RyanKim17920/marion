//! Setup: what `marion doctor` found for each harness, with the fix for anything not ready; where
//! this project keeps its state and agent types; the agent types themselves; and logins.

use super::GUTTER;
use super::start::{HarnessRow, harness_line, readiness};
use super::text::fit;
use super::theme::{Ready, Theme, bad, dim, good};
use super::widgets::{code_spans, expansion, section, span};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// One agent type this project can spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTypeRow {
    pub name: String,
    /// Its harness's readiness: a type is only as runnable as the harness under it.
    pub ready: Ready,
    /// Declared in the project's agents file rather than built in; drawn in the accent.
    pub custom: bool,
}

/// Everything Setup draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupView {
    pub harnesses: Vec<HarnessRow>,
    pub checking: bool,
    pub cursor: usize,
    /// Whether the selected harness shows its detail rows.
    pub expanded: bool,
    /// Where things are, label then path: `("state", "~/.local/state/marion")`.
    pub project: Vec<(String, String)>,
    pub agent_types: Vec<AgentTypeRow>,
    /// The project's agents file, as the operator would type it.
    pub agents_file: String,
    /// The last validation of the agents file: whether it passed, and its one-line answer.
    pub validation: Option<(bool, String)>,
    /// Signed-in state per harness, once `marion login` exists; `None` shows what to do meanwhile.
    pub logins: Option<Vec<(String, String)>>,
}

/// The note column's width, so the surfaces line up after it.
const NOTE_W: usize = 28;

pub fn render(v: &SetupView, theme: Theme, frame: usize, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width <= GUTTER * 2 {
        return;
    }
    let x = area.x + GUTTER;
    let w = (area.width - GUTTER * 2) as usize;
    let end = area.bottom();
    let mut y = area.y;
    let put = |buf: &mut Buffer, lx: u16, l: &Line, y: &mut u16| {
        if *y < end {
            buf.set_line(lx, *y, l, area.right().saturating_sub(lx));
        }
        *y += 1;
    };

    put(
        buf,
        x,
        &section("Harnesses", &readiness(&v.harnesses, v.checking)),
        &mut y,
    );
    y += 1;
    for (i, h) in v.harnesses.iter().enumerate() {
        let sel = i == v.cursor;
        let l = harness_line(h, sel, frame + i, theme, w + GUTTER as usize, Some(NOTE_W));
        put(buf, area.x, &l, &mut y);
        if sel && v.expanded {
            let mut rows: Vec<(String, Vec<Span>)> = h
                .detail
                .iter()
                .map(|(k, val)| (k.clone(), vec![span(val.clone(), Style::default())]))
                .collect();
            if let Some(fix) = &h.fix {
                rows.push(("fix".into(), code_spans(fix, Style::default(), theme)));
            }
            if matches!(h.ready, Ready::Attention) {
                rows.push((
                    String::new(),
                    vec![span("marion uses your login, never its own", dim())],
                ));
            }
            let ew = w.saturating_sub(2);
            for l in expansion(rows, 9, ew) {
                put(buf, x + 2, &l, &mut y);
            }
        }
    }

    if !v.project.is_empty() {
        y += 1;
        let mut l = Vec::new();
        for (i, (k, path)) in v.project.iter().enumerate() {
            if i > 0 {
                l.push(span(" · ", dim()));
            }
            l.push(span(format!("{k} "), dim()));
            l.push(span(path.clone(), Style::default()));
        }
        put(buf, x, &section("Project", ""), &mut y);
        put(buf, x, &Line::from(fit(l, w)), &mut y);
    }

    if !v.agent_types.is_empty() {
        y += 1;
        let note = format!(
            "{} · custom ones come from {}",
            v.agent_types.len(),
            v.agents_file
        );
        put(buf, x, &section("Agent types", &note), &mut y);
        // Whole `glyph name` groups, wrapped over as many rows as they need.
        let mut row: Vec<Span> = Vec::new();
        for t in &v.agent_types {
            let g = vec![
                span(t.ready.glyph(frame), t.ready.style(theme)),
                span(
                    format!(" {}", t.name),
                    if t.custom {
                        theme.accent()
                    } else {
                        Style::default()
                    },
                ),
            ];
            let gw: usize = g.iter().map(|s| s.width()).sum();
            let rw: usize = row.iter().map(|s| s.width()).sum();
            if !row.is_empty() && rw + 3 + gw > w {
                put(buf, x, &Line::from(std::mem::take(&mut row)), &mut y);
            } else if !row.is_empty() {
                row.push(Span::raw("   "));
            }
            row.extend(g);
        }
        if !row.is_empty() {
            put(buf, x, &Line::from(row), &mut y);
        }
        if let Some((ok, answer)) = &v.validation {
            let (glyph, style) = if *ok { ("✓", good()) } else { ("✗", bad()) };
            let l = vec![span(glyph, style), span(format!(" {answer}"), dim())];
            put(buf, x, &Line::from(fit(l, w)), &mut y);
        }
    }

    y += 1;
    put(buf, x, &section("Logins", ""), &mut y);
    match &v.logins {
        Some(rows) => {
            for (h, state) in rows {
                let l = vec![
                    span(super::text::pad(h, 10), Style::default()),
                    span(state.clone(), dim()),
                ];
                put(buf, x, &Line::from(fit(l, w)), &mut y);
            }
        }
        None => {
            let l = code_spans(
                "API keys: `marion login <provider>`; subscription logins stay with each harness's own CLI",
                dim(),
                theme,
            );
            put(buf, x, &Line::from(fit(l, w)), &mut y);
        }
    }
}
