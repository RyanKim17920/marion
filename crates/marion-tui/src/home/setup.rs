//! Setup, on `,`: one list under dim lowercase group headers — the harnesses as `marion doctor`
//! found them with each one's logins indented under it, the stored API keys, the project's files,
//! notifications, and whether this repository's own harness config is trusted. Every row takes single keys, which the footer names for the selected row.
//!
//! The supervisor decides the rows and their order ([`SetupLine`]); this draws them, the selected
//! one as the agents list draws its own, with a harness's detail or a logged-out login's command
//! under it.

use super::GUTTER;
use super::text::{fit, pad, width};
use super::theme::{Ready, Theme, bad, dim, good, warn};
use super::widgets::{code_spans, expansion, row_bar, span};
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
    /// What to do about it, with the command in backticks: ``run `qwen` once and sign in``.
    pub fix: Option<String>,
    /// The detail rows `enter` shows, label then value: `binary`, `version`, `auth`.
    pub detail: Vec<(String, String)>,
}

/// One stored provider key, by its credential id. **Never the key**: the id is all a listing has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRow {
    pub provider: String,
    /// The credential id `marion key rm` takes (`openrouter`, `openrouter:work`).
    pub id: String,
    /// Why the store could not say, when it could not.
    pub note: Option<String>,
}

/// One login (profile) `marion profile add` made, drawn under its harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub name: String,
    /// The harness's default: what its nodes run on when nothing names another.
    pub default: bool,
    /// Signed in, signed out (the operator's to fix), or still being asked.
    pub ready: Ready,
    /// The probe's answer in words: `logged in`, `logged out`, `checking…`, or why it could not say.
    pub note: String,
    /// The command that signs it in, while it is signed out. Shown, never run: a login is the
    /// operator's.
    pub login: Option<String>,
    pub limit: Option<String>,
}

/// One line of Setup, in the order it shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupLine {
    /// A dim lowercase header: `harnesses`, `api keys`, `files`, `notify`, `repository`.
    Group(String),
    Harness(HarnessRow),
    /// A login, indented under its harness.
    Profile(ProfileRow),
    /// The harnesses that are not installed, folded into one line by name.
    Absent(Vec<String>),
    Key(LoginRow),
    /// A dim line in a group's place: none stored yet and the key that adds one, or why the group
    /// could not be listed.
    Note(String),
    /// A project file: what it is, where, and its last check (`None` until checked).
    File {
        what: String,
        path: String,
        check: Option<(bool, String)>,
    },
    /// Desktop notifications: on or off, and how a notice is shown here.
    Notify {
        on: bool,
        shown_by: String,
    },
    /// This repository's own harness configuration and the operator's answer about it.
    Repo {
        /// `Some(true)` trusted, `Some(false)` declined, `None` never answered — which needs them.
        answer: Option<bool>,
        /// The files it holds, as the operator would type them (`.mcp.json, .claude/settings.json`);
        /// empty where it holds none now.
        files: String,
    },
}

/// Everything Setup draws.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupView {
    pub lines: Vec<SetupLine>,
    /// The selected line, an index into [`Self::lines`].
    pub cursor: Option<usize>,
    /// The selected harness shows its detail rows.
    pub expanded: bool,
}

/// The name column's floor and ceiling: wide enough for `acp claude-agent-acp`, never wider.
const NAME_W: usize = 13;
const NAME_MAX: usize = 26;
const VERSION_W: usize = 10;
/// The note column, so the surfaces line up after it.
const NOTE_W: usize = 28;

/// The name column for `lines`: wide enough for the longest harness, provider or file label and a
/// gap, within [`NAME_W`]..=[`NAME_MAX`], so a long name is never clipped against what follows.
pub fn name_col(lines: &[SetupLine]) -> usize {
    lines
        .iter()
        .filter_map(|l| match l {
            SetupLine::Harness(h) => Some(width(&h.name)),
            SetupLine::Key(k) => Some(width(&k.provider)),
            SetupLine::File { what, .. } => Some(width(what)),
            _ => None,
        })
        .map(|w| w + 2)
        .max()
        .unwrap_or(0)
        .clamp(NAME_W, NAME_MAX)
}

/// `3 of 6 ready`, or with `· checking…` while the probe runs: Setup's status-line counts.
pub fn readiness(rows: &[HarnessRow], checking: bool) -> String {
    let ok = rows.iter().filter(|h| h.ready == Ready::Ready).count();
    let mut s = format!("{ok} of {} ready", rows.len());
    if checking {
        s.push_str(" · checking…");
    }
    s
}

pub fn render(v: &SetupView, theme: Theme, frame: usize, area: Rect, buf: &mut Buffer) {
    if area.height == 0 || area.width <= GUTTER * 2 {
        return;
    }
    let w = (area.width - GUTTER) as usize;
    let (lines, selected) = body_lines(v, theme, frame, w);
    // A window over the lines that keeps the selected row (and what it shows under it) in view.
    let rows = area.height as usize;
    let skip = if lines.len() <= rows {
        0
    } else {
        selected.1.saturating_sub(rows).min(selected.0)
    };
    for (y, l) in (area.y..area.bottom()).zip(lines.iter().skip(skip)) {
        buf.set_line(area.x + 1, y, l, w as u16);
    }
}

/// Every line, and the range of lines the selected row covers.
fn body_lines<'a>(
    v: &SetupView,
    theme: Theme,
    frame: usize,
    w: usize,
) -> (Vec<Line<'a>>, (usize, usize)) {
    let mut out: Vec<Line> = Vec::new();
    let mut selected = (0, 0);
    let name_w = name_col(&v.lines);
    for (i, line) in v.lines.iter().enumerate() {
        let sel = v.cursor == Some(i);
        let start = out.len();
        let f = frame + i;
        let cells: Vec<Span> = match line {
            SetupLine::Group(g) => {
                if !out.is_empty() {
                    out.push(Line::default());
                }
                out.push(Line::from(vec![Span::raw(" "), span(g.clone(), dim())]));
                continue;
            }
            SetupLine::Harness(h) => vec![
                span(h.ready.glyph(f), h.ready.style(theme)),
                Span::raw(" "),
                span(pad(&h.name, name_w), Style::default()),
                span(pad(h.version.as_deref().unwrap_or("—"), VERSION_W), dim()),
                span(pad(&h.note, NOTE_W), h.ready.note_style()),
                span(h.surfaces.clone(), dim()),
            ],
            SetupLine::Profile(p) => {
                let mut c = vec![
                    Span::raw("  "),
                    span(p.ready.glyph(f), p.ready.style(theme)),
                    Span::raw(" "),
                    span(pad(&p.name, name_w.saturating_sub(2)), Style::default()),
                ];
                if p.default {
                    c.push(span("default · ", theme.accent()));
                }
                c.push(span(p.note.clone(), p.ready.note_style()));
                if let Some(limit) = &p.limit {
                    c.push(span(format!(" · {limit}"), dim()));
                }
                c
            }
            SetupLine::Absent(names) => vec![
                span(Ready::Absent.glyph(f), Ready::Absent.style(theme)),
                span(format!(" {} not installed  ", names.len()), dim()),
                span(names.join(" · "), dim()),
            ],
            SetupLine::Key(k) => {
                let mut c = vec![
                    span("● ", good()),
                    span(pad(&k.provider, name_w), Style::default()),
                    span(k.id.clone(), dim()),
                ];
                if let Some(note) = &k.note {
                    c.push(span(format!("  {note}"), bad()));
                }
                c
            }
            SetupLine::Note(n) => {
                let mut c = vec![Span::raw("  ")];
                c.extend(code_spans(n, dim(), theme));
                c
            }
            SetupLine::File { what, path, check } => {
                let (glyph, style) = match check {
                    Some((true, _)) => ("✓", good()),
                    Some((false, _)) => ("✗", bad()),
                    None => ("○", dim()),
                };
                let mut c = vec![
                    span(glyph, style),
                    Span::raw(" "),
                    span(pad(what, name_w), Style::default()),
                    span(path.clone(), dim()),
                ];
                if let Some((ok, said)) = check {
                    c.push(span(format!("  {said}"), if *ok { dim() } else { bad() }));
                }
                c
            }
            SetupLine::Notify { on, shown_by } => {
                let (glyph, style, word) = if *on {
                    ("●", good(), "on")
                } else {
                    ("○", dim(), "off")
                };
                vec![
                    span(glyph, style),
                    Span::raw(" "),
                    span(pad(word, name_w), Style::default()),
                    span(
                        format!("when an agent ends or needs you · shown by {shown_by}"),
                        dim(),
                    ),
                ]
            }
            SetupLine::Repo { answer, files } => {
                let (glyph, style, word) = match answer {
                    Some(true) => ("●", good(), "trusted"),
                    Some(false) => ("○", dim(), "declined"),
                    None => ("◐", warn(), "not asked"),
                };
                let note = match (answer, files.is_empty()) {
                    (_, true) => "no harness config of its own now".to_string(),
                    (Some(true), false) => format!("nodes on your login load {files}"),
                    (_, false) => format!("held back from nodes on your login: {files}"),
                };
                vec![
                    span(glyph, style),
                    Span::raw(" "),
                    span(pad(word, name_w), Style::default()),
                    span(note, dim()),
                ]
            }
        };
        let mut l = vec![Span::raw(" ")];
        l.extend(cells);
        out.push(row_bar(l, w, sel, theme));
        if sel {
            out.extend(under(line, v.expanded, theme, w));
            selected = (start, out.len());
        }
    }
    (out, selected)
}

/// What the selected row shows under it: a harness's detail and fix once `enter` has opened it,
/// and a logged-out login's whole command — broken across rows where it must be, since a clipped
/// one cannot be run.
fn under<'a>(line: &SetupLine, expanded: bool, theme: Theme, w: usize) -> Vec<Line<'a>> {
    let pad_left = |l: Line<'a>, by: usize| {
        let mut s = vec![Span::raw(" ".repeat(by))];
        s.extend(l.spans);
        Line::from(s)
    };
    match line {
        SetupLine::Harness(h) if expanded => {
            let mut rows: Vec<(String, Vec<Span>)> = h
                .detail
                .iter()
                .map(|(k, val)| (k.clone(), vec![span(val.clone(), Style::default())]))
                .collect();
            if let Some(fix) = &h.fix {
                rows.push(("fix".into(), code_spans(fix, Style::default(), theme)));
            }
            expansion(rows, 9, w.saturating_sub(4))
                .into_iter()
                .map(|l| pad_left(l, 3))
                .collect()
        }
        SetupLine::Profile(ProfileRow {
            login: Some(cmd), ..
        }) => {
            let lw = w.saturating_sub(5).max(1);
            break_at(cmd, lw)
                .into_iter()
                .map(|part| Line::from(vec![Span::raw("     "), span(part, theme.key())]))
                .collect()
        }
        _ => Vec::new(),
    }
    .into_iter()
    .map(|l| Line::from(fit(l.spans, w)))
    .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(name: &str) -> SetupLine {
        SetupLine::Harness(HarnessRow {
            name: name.into(),
            version: None,
            ready: Ready::Absent,
            note: String::new(),
            surfaces: String::new(),
            fix: None,
            detail: Vec::new(),
        })
    }

    #[test]
    fn the_name_column_fits_the_longest_name_within_bounds() {
        assert_eq!(
            name_col(&[harness("codex")]),
            NAME_W,
            "short names keep the floor"
        );
        assert_eq!(
            name_col(&[harness("codex"), harness("acp claude-agent-acp")]),
            "acp claude-agent-acp".len() + 2,
            "a long name gets its width and a gap, never clipped against its version"
        );
        assert_eq!(name_col(&[harness(&"x".repeat(60))]), NAME_MAX);
    }

    #[test]
    fn a_long_command_breaks_into_rows_and_keeps_every_character() {
        let cmd = "CODEX_HOME='/Users/me/.local/share/marion/profiles/codex/cx' codex login";
        let rows = break_at(cmd, 20);
        assert!(rows.iter().all(|r| width(r) <= 20));
        assert_eq!(rows.concat(), cmd);
    }
}
