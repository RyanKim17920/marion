//! Setup: what `marion doctor` found for each harness, with the fix for anything not ready; where
//! this project keeps its state and agent types; the agent types themselves; the provider keys
//! `marion login` has stored; and profiles. `n` opens the agent-type form in place of all that.

use super::GUTTER;
use super::start::{HarnessRow, harness_line, name_col, readiness};
use super::text::{clip, fit, pad, width};
use super::theme::{CARET, Ready, Theme, bad, bold, dim, good};
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

/// One stored provider key, by its credential id. **Never the key**: the id is all a listing has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRow {
    pub provider: String,
    /// The credential id `marion logout` takes (`openrouter`, `openrouter:work`).
    pub id: String,
    /// Why the store could not say, when it could not.
    pub note: Option<String>,
}

/// One profile `marion profile add` made: its harness, its name, its login as the harness's own
/// read-only probe answered, and the last usage-limit reading a child's stream left for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub harness: String,
    pub name: String,
    /// The harness's default: what its nodes run on when nothing names another.
    pub default: bool,
    /// Signed in, signed out (the operator's to fix), or still being asked.
    pub ready: Ready,
    /// The probe's answer in words: `logged in`, `logged out`, `checking…`, or why it could not say.
    pub note: String,
    /// The command that signs it in, shown under the selected row while it is signed out. Shown,
    /// never run: a login is the operator's.
    pub login: Option<String>,
    pub limit: Option<String>,
}

/// One field of the agent-type form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField {
    pub label: String,
    pub value: String,
    /// Picked with ←→ from a fixed list rather than typed.
    pub choice: bool,
    /// Shown dim while the value is empty.
    pub hint: String,
}

/// The agent-type form, and — once enter has been pressed — the change it would make to the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormView {
    pub title: String,
    pub fields: Vec<FormField>,
    /// The field being edited.
    pub field: usize,
    /// Why the last preview was refused (the loader's own words).
    pub error: Option<String>,
    /// The file it writes, as the operator would type it.
    pub file: String,
    /// `+`/`-`/` ` lines of the change, waiting for `y`; empty until previewed.
    pub preview: Vec<(char, String)>,
}

/// Everything Setup draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupView {
    pub harnesses: Vec<HarnessRow>,
    pub checking: bool,
    /// Into the harnesses, then on into [`Self::logins`], then [`Self::profiles`] — or, while there
    /// are none, the one row that adds the first.
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
    /// The stored provider keys, by credential id.
    pub logins: Vec<LoginRow>,
    /// One dim line under them: where keys are kept, or why they could not be listed.
    pub logins_note: String,
    /// The profiles, by harness then name.
    pub profiles: Vec<ProfileRow>,
    /// One dim line under them: what the keys do, or why they could not be listed.
    pub profiles_note: String,
    /// Desktop notifications: on or off, and how a notice is shown here. One row, after the
    /// profiles (or the row that adds the first); `None` until read.
    pub notify: Option<(bool, String)>,
    /// The agent-type form, drawn in place of everything else while it is open.
    pub form: Option<FormView>,
}

/// The note column's width, so the surfaces line up after it.
const NOTE_W: usize = 28;
/// The form's label column.
const FORM_LABEL_W: usize = 14;

pub fn render(v: &SetupView, theme: Theme, frame: usize, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width <= GUTTER * 2 {
        return;
    }
    let w = (area.width - GUTTER * 2) as usize;
    let (lines, selected) = match &v.form {
        Some(f) => (form_lines(f, theme, w), (0, 0)),
        None => body_lines(v, theme, frame, w),
    };
    // A window over the lines that keeps the selected row (and its expansion) in view.
    let rows = area.height as usize;
    let skip = if lines.len() <= rows {
        0
    } else {
        selected.1.saturating_sub(rows).min(selected.0)
    };
    for (y, (indent, l)) in (area.y..area.bottom()).zip(lines.iter().skip(skip)) {
        let lx = area.x + indent;
        buf.set_line(lx, y, l, area.right().saturating_sub(lx));
    }
}

/// Every line of the normal body, each with its indent from the area's left edge, and the range of
/// lines the selected row covers.
fn body_lines<'a>(
    v: &SetupView,
    theme: Theme,
    frame: usize,
    w: usize,
) -> (Vec<(u16, Line<'a>)>, (usize, usize)) {
    let mut out: Vec<(u16, Line)> = Vec::new();
    let mut selected = (0, 0);
    let g = GUTTER;
    let blank = |out: &mut Vec<(u16, Line)>| out.push((0, Line::default()));

    out.push((
        g,
        section("Harnesses", &readiness(&v.harnesses, v.checking)),
    ));
    blank(&mut out);
    let name_w = name_col(&v.harnesses);
    // Harnesses that are not installed fold into one line, so twenty-odd rows of "not installed"
    // do not push the sections below off the screen; the one under the cursor still shows in place.
    // One absent harness keeps its own row: a fold of one says no more and reads worse.
    let fold = v
        .harnesses
        .iter()
        .enumerate()
        .filter(|(i, h)| h.ready == Ready::Absent && *i != v.cursor)
        .count()
        > 1;
    let mut absent: Vec<&str> = Vec::new();
    for (i, h) in v.harnesses.iter().enumerate() {
        let sel = i == v.cursor;
        if fold && h.ready == Ready::Absent && !sel {
            absent.push(&h.name);
            continue;
        }
        let start = out.len();
        let l = harness_line(
            h,
            name_w,
            sel,
            frame + i,
            theme,
            w + g as usize,
            Some(NOTE_W),
        );
        out.push((0, l));
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
            for l in expansion(rows, 9, w.saturating_sub(2)) {
                out.push((g + 2, l));
            }
        }
        if sel {
            selected = (start, out.len());
        }
    }
    if !absent.is_empty() {
        let l = vec![
            Span::raw("  "),
            span(Ready::Absent.glyph(frame), Ready::Absent.style(theme)),
            span(format!(" {} not installed  ", absent.len()), dim()),
            span(absent.join(" · "), dim()),
        ];
        out.push((0, Line::from(fit(l, w + g as usize))));
    }

    if !v.project.is_empty() {
        blank(&mut out);
        let mut l = Vec::new();
        for (i, (k, path)) in v.project.iter().enumerate() {
            if i > 0 {
                l.push(span(" · ", dim()));
            }
            l.push(span(format!("{k} "), dim()));
            l.push(span(path.clone(), Style::default()));
        }
        out.push((g, section("Project", "")));
        out.push((g, Line::from(fit(l, w))));
    }

    if !v.agent_types.is_empty() {
        blank(&mut out);
        let note = format!(
            "{} · custom ones come from {}",
            v.agent_types.len(),
            v.agents_file
        );
        out.push((g, section("Agent types", &note)));
        // Whole `glyph name` groups, wrapped over as many rows as they need.
        let mut row: Vec<Span> = Vec::new();
        for t in &v.agent_types {
            let grp = vec![
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
            let gw: usize = grp.iter().map(|s| s.width()).sum();
            let rw: usize = row.iter().map(|s| s.width()).sum();
            if !row.is_empty() && rw + 3 + gw > w {
                out.push((g, Line::from(std::mem::take(&mut row))));
            } else if !row.is_empty() {
                row.push(Span::raw("   "));
            }
            row.extend(grp);
        }
        if !row.is_empty() {
            out.push((g, Line::from(row)));
        }
        if let Some((ok, answer)) = &v.validation {
            let (glyph, style) = if *ok { ("✓", good()) } else { ("✗", bad()) };
            let l = vec![span(glyph, style), span(format!(" {answer}"), dim())];
            out.push((g, Line::from(fit(l, w))));
        }
    }

    blank(&mut out);
    let count = match v.logins.len() {
        0 => String::new(),
        1 => "1 key".to_string(),
        n => format!("{n} keys"),
    };
    out.push((g, section("API keys", &count)));
    let provider_w = v
        .logins
        .iter()
        .map(|l| l.provider.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 16);
    for (i, login) in v.logins.iter().enumerate() {
        let sel = v.harnesses.len() + i == v.cursor;
        if sel {
            selected = (out.len(), out.len() + 1);
        }
        let mut l = vec![
            span(if sel { CARET } else { " " }, theme.key()),
            Span::raw(" "),
            span("● ", good()),
            span(
                pad(&login.provider, provider_w + 2),
                if sel { bold() } else { Style::default() },
            ),
            span(login.id.clone(), dim()),
        ];
        if let Some(note) = &login.note {
            l.push(span(format!("  {note}"), bad()));
        }
        out.push((0, Line::from(fit(l, w + g as usize))));
    }
    if !v.logins_note.is_empty() {
        let l = code_spans(&v.logins_note, dim(), theme);
        out.push((g, Line::from(fit(l, w))));
    }

    blank(&mut out);
    let count = match v.profiles.len() {
        0 => String::new(),
        1 => "1 login".to_string(),
        n => format!("{n} logins"),
    };
    out.push((g, section("Logins", &count)));
    let first = v.harnesses.len() + v.logins.len();
    let harness_w = column(v.profiles.iter().map(|p| p.harness.as_str()), 8, 12);
    let profile_w = column(v.profiles.iter().map(|p| p.name.as_str()), 8, 20);
    for (i, p) in v.profiles.iter().enumerate() {
        let sel = first + i == v.cursor;
        let start = out.len();
        let mut l = vec![
            span(if sel { CARET } else { " " }, theme.key()),
            Span::raw(" "),
            span(
                format!("{} ", p.ready.glyph(frame + i)),
                p.ready.style(theme),
            ),
            span(pad(&p.harness, harness_w + 2), dim()),
            span(
                pad(&p.name, profile_w + 2),
                if sel { bold() } else { Style::default() },
            ),
        ];
        if p.default {
            l.push(span("default · ", theme.accent()));
        }
        l.push(span(p.note.clone(), p.ready.note_style()));
        if let Some(limit) = &p.limit {
            l.push(span(format!(" · {limit}"), dim()));
        }
        out.push((0, Line::from(fit(l, w + g as usize))));
        if let (true, Some(login)) = (sel, &p.login) {
            // The whole command, broken across rows where it must be: a clipped one cannot be run.
            let lw = w.saturating_sub(4).max(1);
            const LABEL: &str = "log in yourself: ";
            if width(LABEL) + width(login) <= lw {
                let l = vec![span(LABEL, dim()), span(login.clone(), theme.key())];
                out.push((g + 4, Line::from(l)));
            } else {
                out.push((g + 4, Line::from(span(LABEL.trim_end(), dim()))));
                for part in break_at(login, lw) {
                    out.push((g + 4, Line::from(span(part, theme.key()))));
                }
            }
        }
        if sel {
            selected = (start, out.len());
        }
    }
    if v.profiles.is_empty() {
        let sel = first == v.cursor;
        if sel {
            selected = (out.len(), out.len() + 1);
        }
        let l = vec![
            span(if sel { CARET } else { " " }, theme.key()),
            Span::raw(" "),
            span("+ add a profile", if sel { bold() } else { dim() }),
        ];
        out.push((0, Line::from(fit(l, w + g as usize))));
    }
    if !v.profiles_note.is_empty() {
        let l = code_spans(&v.profiles_note, dim(), theme);
        out.push((g, Line::from(fit(l, w))));
    }

    if let Some((on, shown_by)) = &v.notify {
        blank(&mut out);
        out.push((g, section("Notifications", "")));
        let sel = first + v.profiles.len().max(1) == v.cursor;
        if sel {
            selected = (out.len(), out.len() + 1);
        }
        let (glyph, style, word) = if *on {
            ("● ", good(), "on")
        } else {
            ("○ ", dim(), "off")
        };
        let l = vec![
            span(if sel { CARET } else { " " }, theme.key()),
            Span::raw(" "),
            span(glyph, style),
            span(pad(word, 5), if sel { bold() } else { Style::default() }),
            span(
                format!("when an agent ends or needs you · shown by {shown_by}"),
                dim(),
            ),
        ];
        out.push((0, Line::from(fit(l, w + g as usize))));
    }
    (out, selected)
}

/// `s` in rows of at most `w` columns, broken anywhere — for a command that has no spaces to wrap
/// at and must be shown whole.
fn break_at(s: &str, w: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    for c in s.chars() {
        let row = rows.last_mut().expect("never empty");
        if !row.is_empty() && width(row) + width(c.encode_utf8(&mut [0; 4])) > w {
            rows.push(String::new());
        }
        rows.last_mut().expect("never empty").push(c);
    }
    rows
}

/// A column as wide as its widest entry, held between `min` and `max`.
fn column<'a>(entries: impl Iterator<Item = &'a str>, min: usize, max: usize) -> usize {
    entries
        .map(|e| e.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(min, max)
}

/// The agent-type form: one row per field, the one being edited marked, then the loader's
/// refusal or the diff waiting for `y`.
fn form_lines<'a>(f: &FormView, theme: Theme, w: usize) -> Vec<(u16, Line<'a>)> {
    let g = GUTTER;
    let mut out: Vec<(u16, Line)> = vec![(g, section(&f.title, &f.file)), (0, Line::default())];
    for (i, field) in f.fields.iter().enumerate() {
        let sel = i == f.field;
        let mut l = vec![
            span(if sel { CARET } else { " " }, theme.key()),
            Span::raw(" "),
            span(
                pad(&field.label, FORM_LABEL_W),
                if sel { bold() } else { dim() },
            ),
        ];
        let vw = w.saturating_sub(FORM_LABEL_W + 4);
        if field.choice {
            l.push(span("‹ ", if sel { theme.key() } else { dim() }));
            l.push(span(clip(&field.value, vw), Style::default()));
            l.push(span(" ›", if sel { theme.key() } else { dim() }));
        } else if field.value.is_empty() {
            if sel {
                l.push(span("▏", theme.accent()));
            }
            l.push(span(field.hint.clone(), dim()));
        } else {
            l.push(span(clip(&field.value, vw), Style::default()));
            if sel {
                l.push(span("▏", theme.accent()));
            }
        }
        out.push((0, Line::from(fit(l, w + g as usize))));
    }
    if let Some(e) = &f.error {
        out.push((0, Line::default()));
        let l = vec![span("✗ ", bad()), span(e.clone(), bad())];
        out.push((g, Line::from(fit(l, w))));
    }
    if !f.preview.is_empty() {
        out.push((0, Line::default()));
        out.push((g, section("Change", &f.file)));
        for (tag, text) in &f.preview {
            let style = match tag {
                '+' => good(),
                '-' => bad(),
                _ => dim(),
            };
            let l = vec![span(format!("{tag} "), style), span(text.clone(), style)];
            out.push((g, Line::from(fit(l, w))));
        }
    }
    out
}
