//! The home screen at the three sizes that matter — 80x24 (the floor), 100x30 and 160x50 — over
//! fixture data, as text snapshots with a style census.
//!
//! # Why a census and not just the characters
//!
//! Half of what this screen says is style: the selected row is a reversed bar in the accent, a
//! blocked agent's glyph is yellow. A text-only snapshot would pass a screen
//! that lost every colour. The census is one line per distinct style with the number of cells that
//! wear it and the first few characters of them, so a style that moves, appears or disappears is a
//! diff a reviewer can read.
//!
//! # PNGs
//!
//! With `MARION_HOME_DUMP=<dir>` set, every screen is also written there as `<name>.json` — one
//! `[symbol, fg, bg, flags]` per cell — for a rasteriser to turn into dark- and light-theme images.

use marion_tui::home::setup::{ADD_LOGIN, TRUST};
use marion_tui::home::{
    self, AgentRow, AgentsView, Body, Footer, HarnessRow, HelpView, Hint, Ink, KeyRow, LineKind,
    LoginRow, Preview, ProfileRow, Ready, Screen, SetupLine, SetupView, StreamLine, Theme,
};
use marion_tui::tree::{self, Tone};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use std::collections::BTreeMap;

const SIZES: [(u16, u16); 3] = [(80, 24), (100, 30), (160, 50)];
const PROJECT: &str = "~/code/acme-api";
const PROMPT: &str =
    "Add rate limiting to /v1/orders (token bucket, 100 req/min per key) and cover it with tests";

// ------------------------------------------------------------------------------ fixture data

fn s(v: &str) -> String {
    v.to_string()
}

fn harness(
    name: &str,
    version: Option<&str>,
    ready: Ready,
    note: &str,
    surfaces: &str,
    fix: Option<&str>,
) -> HarnessRow {
    HarnessRow {
        name: s(name),
        version: version.map(s),
        ready,
        note: s(note),
        surfaces: s(surfaces),
        fix: fix.map(s),
        detail: vec![
            (s("binary"), format!("/usr/local/bin/{name}")),
            (s("version"), version.unwrap_or("—").to_string()),
        ],
    }
}

fn harnesses() -> Vec<HarnessRow> {
    vec![
        harness(
            "claude",
            Some("2.1.268"),
            Ready::Ready,
            "ready",
            "headless · pane · channel push",
            None,
        ),
        harness(
            "codex",
            Some("0.61.0"),
            Ready::Ready,
            "ready",
            "headless · pane · ACP",
            None,
        ),
        harness(
            "qwen",
            Some("0.9.1"),
            Ready::Attention,
            "sign-in needed",
            "headless · pane",
            Some("run `qwen` once and sign in"),
        ),
        harness(
            "opencode",
            Some("1.2.3"),
            Ready::Ready,
            "ready",
            "headless · pane · ACP",
            None,
        ),
        harness(
            "copilot",
            Some("0.0.339"),
            Ready::Broken,
            "too old, needs ≥ 0.0.350",
            "headless · ACP",
            Some("`brew upgrade copilot-cli`, then r to re-check"),
        ),
        harness(
            "agy",
            None,
            Ready::Absent,
            "not installed",
            "—",
            Some("install agy, then r to re-check"),
        ),
    ]
}

/// Setup as the supervisor lays it out: the installed harnesses with their logins under them, the
/// one folded line of those not installed, the keys, the files and notify. The cursor is on qwen,
/// expanded.
fn setup_view(checking: bool) -> SetupView {
    let mut hs = harnesses();
    hs[2].detail.push((s("auth"), s("not signed in")));
    if checking {
        for h in &mut hs[3..] {
            h.ready = Ready::Checking;
            h.note = s("checking…");
            h.version = None;
        }
    }
    let absent: Vec<String> = hs
        .iter()
        .filter(|h| h.ready == Ready::Absent)
        .map(|h| h.name.clone())
        .collect();
    let mut lines = vec![SetupLine::Group(s("harnesses"))];
    for h in hs.into_iter().filter(|h| h.ready != Ready::Absent) {
        let name = h.name.clone();
        lines.push(SetupLine::Harness(h));
        lines.extend(match name.as_str() {
            "claude" => vec![
                profile("work", true, Ready::Ready, "logged in", None),
                profile("personal", false, Ready::Ready, "logged in", None),
            ],
            "codex" => vec![profile(
                "cx",
                false,
                Ready::Attention,
                "logged out",
                Some("CODEX_HOME='/Users/me/.local/share/marion/profiles/codex/cx' codex login"),
            )],
            "opencode" => vec![profile("oc", false, Ready::Checking, "checking…", None)],
            _ => Vec::new(),
        });
    }
    if !absent.is_empty() {
        lines.push(SetupLine::Absent(absent));
    }
    lines.extend([
        SetupLine::Group(s("api keys")),
        login("anthropic", "anthropic"),
        login("openrouter", "openrouter"),
        login("openrouter", "openrouter:work"),
        SetupLine::Group(s("files")),
        SetupLine::File {
            what: s("agent types"),
            path: s(".marion/agents.toml"),
            check: Some((true, s("2 types, valid"))),
        },
        SetupLine::Group(s("notify")),
        SetupLine::Notify {
            on: true,
            shown_by: s("osascript"),
        },
    ]);
    let mut v = SetupView {
        lines,
        cursor: None,
        expanded: true,
    };
    select(
        &mut v,
        |l| matches!(l, SetupLine::Harness(h) if h.name == "qwen"),
    );
    v
}

/// Put the cursor on the first line `pick` takes.
fn select(v: &mut SetupView, pick: impl Fn(&SetupLine) -> bool) {
    v.cursor = Some(v.lines.iter().position(pick).expect("a line to select"));
}

fn profile(name: &str, default: bool, ready: Ready, note: &str, login: Option<&str>) -> SetupLine {
    SetupLine::Profile(ProfileRow {
        name: s(name),
        default,
        ready,
        note: s(note),
        login: login.map(s),
        limit: None,
    })
}

fn login(provider: &str, id: &str) -> SetupLine {
    SetupLine::Key(LoginRow {
        provider: s(provider),
        id: s(id),
        note: None,
    })
}

// ------------------------------------------------------------------------------ drawing

fn draw(screen: &Screen, w: u16, h: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).expect("TestBackend");
    t.draw(|f| f.render_widget(screen, f.area())).expect("draw");
    t.backend().buffer().clone()
}

fn rows_of(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            let mut skip = 0;
            for x in 0..buf.area.width {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let sym = buf[(x, y)].symbol();
                row.push_str(if sym.is_empty() { " " } else { sym });
                skip = unicode_width::UnicodeWidthStr::width(sym).saturating_sub(1);
            }
            row.truncate(row.trim_end().len());
            row
        })
        .collect()
}

/// One line per distinct non-blank style: `<style>  <cells>  "<sample>"`.
fn census(buf: &Buffer) -> String {
    let mut by: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            if c.symbol().trim().is_empty() {
                continue;
            }
            let e = by.entry(format!("{:?}", c.style())).or_default();
            e.0 += 1;
            if e.1.chars().count() < 24 {
                e.1.push_str(c.symbol());
            }
        }
    }
    by.into_iter()
        .map(|(style, (n, sample))| format!("{n:>5}  {style}  {sample:?}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn report(screen: &Screen, w: u16, h: u16) -> String {
    let buf = draw(screen, w, h);
    check(&buf);
    dump(&buf, w, h);
    format!(
        "--- {w}x{h} ---\n{}\n--- styles ---\n{}",
        rows_of(&buf).join("\n"),
        census(&buf)
    )
}

/// What every screen at every size must satisfy, whatever its snapshot says.
fn check(buf: &Buffer) {
    let text = rows_of(buf).join("\n");
    // Tokens, never money.
    assert!(
        !text
            .chars()
            .zip(text.chars().skip(1))
            .any(|(a, b)| a == '$' && b.is_ascii_digit()),
        "a price reached the screen:\n{text}"
    );
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            // Nothing paints a background: the terminal's own ground shows on dark and light alike.
            assert_eq!(c.bg, Color::Reset, "a background at ({x},{y}):\n{text}");
            // Status colours are ANSI-16; the only truecolour values are the accent and the tree's
            // own grey for an unknown state.
            if let Color::Rgb(..) | Color::Indexed(_) = c.fg {
                assert!(
                    [
                        home::theme::ACCENT_RGB,
                        home::theme::ACCENT_256,
                        tree::greyed().fg.unwrap()
                    ]
                    .contains(&c.fg),
                    "an off-palette colour {:?} at ({x},{y}):\n{text}",
                    c.fg
                );
            }
        }
    }
}

/// Cells as JSON for the rasteriser, when `MARION_HOME_DUMP` names a directory.
fn dump(buf: &Buffer, w: u16, h: u16) {
    let Some(dir) = std::env::var_os("MARION_HOME_DUMP") else {
        return;
    };
    let name = std::thread::current()
        .name()
        .unwrap_or("screen")
        .replace("::", "-");
    let tag = |c: Color| match c {
        Color::Reset => s("d"),
        Color::Indexed(n) => format!("i{n}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Black => s("a0"),
        Color::Red => s("a1"),
        Color::Green => s("a2"),
        Color::Yellow => s("a3"),
        Color::Blue => s("a4"),
        Color::Magenta => s("a5"),
        Color::Cyan => s("a6"),
        Color::Gray => s("a7"),
        Color::DarkGray => s("a8"),
        other => format!("{other:?}"),
    };
    let mut rows = Vec::new();
    for y in 0..h {
        let mut row = Vec::new();
        for x in 0..w {
            let c = &buf[(x, y)];
            let mut f = String::new();
            for (m, ch) in [
                (Modifier::BOLD, 'B'),
                (Modifier::DIM, 'D'),
                (Modifier::REVERSED, 'R'),
            ] {
                if c.modifier.contains(m) {
                    f.push(ch);
                }
            }
            row.push(serde_json::json!([c.symbol(), tag(c.fg), tag(c.bg), f]));
        }
        rows.push(serde_json::Value::Array(row));
    }
    let doc = serde_json::json!({"w": w, "h": h, "rows": rows});
    let path = std::path::Path::new(&dir).join(format!("{name}-{w}x{h}.json"));
    std::fs::create_dir_all(&dir).expect("dump dir");
    std::fs::write(path, doc.to_string()).expect("dump");
}

fn row(prefix: &str, tone: Tone, title: &str, who: &str, time: &str, tokens: u64) -> AgentRow {
    AgentRow {
        id: format!("01a093dc-{}", title.len()),
        prefix: s(prefix),
        tone,
        title: s(title),
        who: s(who),
        time: s(time),
        tokens: Some(tokens),
        dim: false,
    }
}

/// The forest of the design's mockups: a root whose children need the operator sorted first.
fn rows() -> Vec<AgentRow> {
    vec![
        row(
            "",
            Tone::Live,
            "ship the rate limiter",
            "claude opus",
            "12m",
            312_000,
        ),
        row(
            "├ ",
            Tone::Blocked,
            "wire the limiter into ci",
            "codex gpt-5",
            "3m",
            41_000,
        ),
        row(
            "├ ",
            Tone::Failed,
            "add bucket tests",
            "codex gpt-5",
            "5m",
            120_000,
        ),
        row(
            "├ ",
            Tone::Done,
            "add token-bucket limiter",
            "codex gpt-5",
            "7m",
            197_000,
        ),
        row(
            "└ ",
            Tone::Live,
            "implement the bucket",
            "codex gpt-5",
            "2m",
            88_000,
        ),
        row(
            "",
            Tone::Done,
            "review the orders api",
            "claude sonnet",
            "9m",
            54_000,
        ),
        row(
            "",
            Tone::Unknown,
            "port invoices to the new schema",
            "opencode",
            "4m",
            63_000,
        ),
    ]
}

fn stream(n: usize) -> Vec<StreamLine> {
    use LineKind::{Call, Done, Failed, Files, Said};
    let all = [
        (Call, "read src/limits/mod.rs"),
        (Call, "$ cargo test -q limits"),
        (Done, "ok 4.1s"),
        (Said, "I'll add a token bucket keyed by API key."),
        (Files, "~ crates/api/src/limits/bucket.rs"),
        (Call, "$ cargo build -q"),
        (Failed, "exit 101 3.2s"),
        (Call, "read .github/workflows/ci.yml"),
    ];
    (0..n)
        .map(|i| {
            let (kind, text) = all[i % all.len()];
            StreamLine {
                clock: format!("14:{:02}:{:02}", 24 + i * 7 / 60, (i * 7) % 60),
                kind,
                text: s(text),
            }
        })
        .collect()
}

fn live_preview() -> Preview {
    Preview {
        who: s("codex 91cd"),
        title: s("wire the limiter into ci"),
        needs: Some(s("wants to write .github/workflows/ci.yml")),
        facts: Vec::new(),
        stream: stream(9),
        scroll: 0,
        quiet: None,
    }
}

fn landed_preview() -> Preview {
    Preview {
        who: s("codex 3c33"),
        title: s("add token-bucket limiter"),
        needs: None,
        facts: vec![
            vec![
                (s("done in 6m41s"), Ink::Plain),
                (s("  184k in  12.9k out"), Ink::Dim),
            ],
            vec![(s("limiter in, tests green"), Ink::Plain)],
            vec![
                (s("marion/t-3c33"), Ink::Bold),
                (s("  +84"), Ink::Good),
                (s(" -12"), Ink::Bad),
                (s("  3 files"), Ink::Dim),
            ],
        ],
        stream: stream(5),
        scroll: 0,
        quiet: None,
    }
}

fn agents_view(cursor: usize, preview: Preview) -> AgentsView {
    AgentsView {
        rows: rows(),
        cursor,
        preview: Some(preview),
        full: false,
    }
}

fn keys(pairs: &[(&str, &str)]) -> Footer {
    Footer::Keys(pairs.iter().map(|(k, v)| Hint::new(*k, *v)).collect())
}

fn live_keys() -> Footer {
    keys(&[
        ("j/k", "move"),
        ("enter", "output"),
        ("n", "new"),
        ("x", "cancel"),
        (":", "command"),
        (",", "setup"),
        ("q", "quit"),
        ("?", "help"),
    ])
}

fn ended_keys() -> Footer {
    keys(&[
        ("j/k", "move"),
        ("enter", "output"),
        ("n", "new"),
        (":", "command"),
        (",", "setup"),
        ("q", "quit"),
        ("?", "help"),
    ])
}

fn setup_keys() -> Footer {
    keys(&[
        ("enter", "details"),
        ("a", ADD_LOGIN),
        ("r", "re-check"),
        ("esc", "back"),
    ])
}

const COUNTS: &str = "2 running  2 need you  1 to land";

fn screen<'a>(body: Body<'a>, footer: Footer) -> Screen<'a> {
    Screen {
        theme: Theme::TRUECOLOR,
        title: PROJECT,
        counts: COUNTS,
        body,
        footer,
        notice: None,
        frame: 0,
    }
}

fn setup_screen(v: &SetupView, footer: Footer) -> Screen<'_> {
    Screen {
        title: "setup",
        counts: "3 of 6 ready",
        ..screen(Body::Setup(v), footer)
    }
}

fn at_every_size(screen: &Screen) -> String {
    SIZES
        .iter()
        .map(|&(w, h)| report(screen, w, h))
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ------------------------------------------------------------------------------ the snapshots

/// The main screen: the list, sorted by what needs the operator, and the selected agent's preview
/// beside it from 100 columns, under it below that.
#[test]
fn agents_a_blocked_agent_selected() {
    let v = agents_view(1, live_preview());
    insta::assert_snapshot!(at_every_size(&screen(Body::Agents(&v), live_keys())));
}

/// A finished agent previews how it ended and the branch it left.
#[test]
fn agents_a_landed_agent_selected() {
    let v = agents_view(3, landed_preview());
    insta::assert_snapshot!(at_every_size(&screen(Body::Agents(&v), ended_keys())));
}

#[test]
fn agents_none_yet() {
    let v = AgentsView::default();
    let footer = keys(&[
        ("n", "new"),
        (":", "command"),
        (",", "setup"),
        ("q", "quit"),
        ("?", "help"),
    ]);
    let s = Screen {
        counts: "",
        ..screen(Body::Agents(&v), footer)
    };
    insta::assert_snapshot!(report(&s, 100, 30));
}

/// `enter` on a headless agent: its output takes the whole body, and `esc` is the way back.
#[test]
fn agents_output_full() {
    let mut v = agents_view(3, landed_preview());
    v.full = true;
    v.preview.as_mut().unwrap().stream = stream(40);
    let footer = keys(&[
        ("j/k", "move"),
        ("esc", "back"),
        ("n", "new"),
        (":", "command"),
        (",", "setup"),
        ("q", "quit"),
        ("?", "help"),
    ]);
    insta::assert_snapshot!(report(&screen(Body::Agents(&v), footer), 100, 30));
}

/// Scrolled back, the preview says how much is above what it shows.
#[test]
fn agents_output_scrolled_back() {
    let mut p = live_preview();
    p.stream = stream(60);
    p.scroll = 10;
    let v = agents_view(1, p);
    let buf = draw(&screen(Body::Agents(&v), live_keys()), 120, 32);
    let text = rows_of(&buf).join("\n");
    assert!(text.contains("above"), "{text}");
    insta::assert_snapshot!(report(&screen(Body::Agents(&v), live_keys()), 120, 32));
}

/// `n`: the new-task line takes the footer, naming the type, its model, and the line's own keys.
#[test]
fn agents_new_task_line() {
    let v = agents_view(1, live_preview());
    let footer = Footer::Input {
        target: s("new claude-orchestrator"),
        note: s("opus"),
        text: s(PROMPT),
        keys: vec![
            Hint::new("tab", "type"),
            Hint::new("↑↓", "model"),
            Hint::new("enter", "run"),
        ],
    };
    insta::assert_snapshot!(at_every_size(&screen(Body::Agents(&v), footer)));
}

#[test]
fn agents_steer_line() {
    let v = agents_view(1, live_preview());
    let footer = Footer::Input {
        target: s("steer codex 91cd"),
        note: String::new(),
        text: s("use the shared limiter crate"),
        keys: vec![Hint::new("enter", "send"), Hint::new("esc", "cancel")],
    };
    insta::assert_snapshot!(report(&screen(Body::Agents(&v), footer), 120, 32));
}

#[test]
fn agents_confirm_cancel() {
    let v = agents_view(4, live_preview());
    let footer = Footer::Confirm {
        question: s("cancel codex a1f0?"),
        command: s("marion cancel 01a093dc-a1f0"),
    };
    insta::assert_snapshot!(report(&screen(Body::Agents(&v), footer), 120, 32));
}

#[test]
fn help_page() {
    let k = |key: &str, verb: &str, cmd: &str| KeyRow {
        key: s(key),
        verb: s(verb),
        command: s(cmd),
    };
    let v = HelpView {
        scroll: 0,
        sections: vec![
            (
                s("agents"),
                vec![
                    k("enter", "attach a pane agent", "marion attach <id>"),
                    k("n", "new task", "marion run <type> --prompt <text>"),
                    k("s", "steer a running agent", "marion steer <id> <text>"),
                ],
            ),
            (s("input line"), vec![k("tab", "next agent type", "")]),
        ],
    };
    let sc = Screen {
        title: "keys",
        counts: "",
        ..screen(Body::Help(&v), keys(&[("esc", "back"), ("j/k", "scroll")]))
    };
    insta::assert_snapshot!(report(&sc, 100, 30));
}

/// The selection is one reversed bar in the accent, across the whole list column, and only on the
/// selected row.
#[test]
fn the_selection_is_one_reversed_bar() {
    let v = agents_view(2, live_preview());
    let buf = draw(&screen(Body::Agents(&v), live_keys()), 120, 32);
    let reversed = |y: u16| {
        (1..70)
            .filter(|&x| buf[(x, y)].modifier.contains(Modifier::REVERSED))
            .count()
    };
    assert_eq!(reversed(3), 69, "the bar spans the list");
    assert_eq!(reversed(2), 0);
    assert_eq!(reversed(4), 0);
    assert_eq!(
        buf[(4, 3)].fg,
        home::theme::ACCENT_RGB,
        "the failed glyph on the bar takes the bar's colour"
    );
}

/// The list is ordered for a check-in; `+N below` says rows are past the bottom, and the cursor's
/// row is always on screen.
#[test]
fn a_long_list_keeps_the_cursor_on_screen() {
    let mut v = agents_view(0, live_preview());
    let base = v.rows[4].clone();
    for i in 0..30 {
        let mut r = base.clone();
        r.title = format!("bucket shard {i}");
        v.rows.insert(5, r);
    }
    v.cursor = v.rows.len() - 1;
    let buf = draw(&screen(Body::Agents(&v), live_keys()), 80, 24);
    let text = rows_of(&buf).join("\n");
    assert!(text.contains("port invoices"), "{text}");
    v.cursor = 0;
    let buf = draw(&screen(Body::Agents(&v), live_keys()), 80, 24);
    assert!(rows_of(&buf).join("\n").contains("below"));
}

#[test]
fn no_color_draws_every_cell_without_colour_and_keeps_the_glyphs() {
    let v = agents_view(1, live_preview());
    let mut sc = screen(Body::Agents(&v), live_keys());
    sc.theme = Theme {
        mono: true,
        ..Theme::TRUECOLOR
    };
    let buf = draw(&sc, 120, 32);
    for cell in buf.content() {
        assert_eq!(
            (cell.fg, cell.bg),
            (ratatui::style::Color::Reset, ratatui::style::Color::Reset),
            "{:?}",
            cell.symbol()
        );
    }
    let text = rows_of(&buf).join("\n");
    for glyph in ["◐", "✗", "✓", "○"] {
        assert!(text.contains(glyph), "{glyph} is gone:\n{text}");
    }
}

/// A notice replaces the keys until the next key.
#[test]
fn a_notice_takes_the_footer() {
    let v = agents_view(1, live_preview());
    let mut sc = screen(Body::Agents(&v), live_keys());
    sc.notice = Some(s(
        "queued as m-1; reaches codex 91cd at its next tool round",
    ));
    let buf = draw(&sc, 120, 32);
    let last = rows_of(&buf).pop().unwrap();
    assert!(
        last.contains("queued as m-1") && !last.contains("enter output"),
        "{last}"
    );
}

/// The 256-colour fallback changes the accent and nothing else.
#[test]
fn the_indexed_theme_swaps_only_the_accent() {
    let v = agents_view(3, landed_preview());
    let mut sc = screen(Body::Agents(&v), ended_keys());
    let rgb = draw(&sc, 120, 32);
    sc.theme = Theme::INDEXED;
    let idx = draw(&sc, 120, 32);
    let mut swapped = 0;
    for (a, b) in rgb.content.iter().zip(&idx.content) {
        assert_eq!(a.symbol(), b.symbol());
        assert_eq!(a.modifier, b.modifier);
        if a.fg != b.fg {
            assert_eq!(
                (a.fg, b.fg),
                (home::theme::ACCENT_RGB, home::theme::ACCENT_256)
            );
            swapped += 1;
        }
    }
    assert!(swapped > 0, "the accent never appeared");
}

/// Every row carries its state glyph: a colour never stands for a state alone.
#[test]
fn every_row_carries_its_state_glyph() {
    let v = agents_view(0, live_preview());
    let buf = draw(&screen(Body::Agents(&v), live_keys()), 120, 32);
    let text = rows_of(&buf).join("\n");
    for r in rows() {
        let line = text
            .lines()
            .find(|l| {
                l.split('│')
                    .next()
                    .is_some_and(|list| list.contains(&r.title))
            })
            .unwrap_or_else(|| panic!("{} missing:\n{text}", r.title));
        let glyph = match r.tone {
            Tone::Live => continue,
            Tone::Unknown => "○",
            t => t.glyph(),
        };
        assert!(line.contains(glyph), "{} has no glyph: {line}", r.title);
    }
}

/// Setup with a harness's detail open under it, at every size.
#[test]
fn setup_expanded_harness() {
    let v = setup_view(false);
    insta::assert_snapshot!(at_every_size(&setup_screen(&v, setup_keys())));
}

/// **The one list at the sizes the design names**: 120x32, and the 80x24 floor, where the window
/// keeps the selected row in view.
#[test]
fn setup_one_list_at_120x32_and_80x24() {
    let mut v = setup_view(false);
    v.expanded = false;
    select(
        &mut v,
        |l| matches!(l, SetupLine::Harness(h) if h.name == "claude"),
    );
    let wide = report(&setup_screen(&v, setup_keys()), 120, 32);
    select(&mut v, |l| matches!(l, SetupLine::Notify { .. }));
    let footer = keys(&[("enter", "turn off"), ("r", "re-check"), ("esc", "back")]);
    let floor = report(&setup_screen(&v, footer), 80, 24);
    insta::assert_snapshot!(format!("{wide}\n\n{floor}"));
}

/// A machine with most harnesses missing: the missing ones fold into one line, so the groups
/// below still show.
#[test]
fn setup_folds_the_harnesses_that_are_not_installed() {
    let mut v = setup_view(false);
    v.expanded = false;
    let at = v
        .lines
        .iter()
        .position(|l| matches!(l, SetupLine::Absent(_)))
        .unwrap();
    v.lines[at] = SetupLine::Absent(
        [
            "agy",
            "goose",
            "cline",
            "acp kilo",
            "acp vibe",
            "acp vtcode",
        ]
        .map(s)
        .to_vec(),
    );
    insta::assert_snapshot!(report(&setup_screen(&v, setup_keys()), 100, 30));
}

#[test]
fn setup_doctor_checking() {
    let v = setup_view(true);
    insta::assert_snapshot!(report(&setup_screen(&v, setup_keys()), 100, 30));
}

#[test]
fn setup_keys_listed() {
    let mut v = setup_view(false);
    v.expanded = false;
    select(
        &mut v,
        |l| matches!(l, SetupLine::Key(k) if k.id == "openrouter:work"),
    );
    let footer = keys(&[
        ("a", "add key"),
        ("del", "remove"),
        ("r", "re-check"),
        ("esc", "back"),
    ]);
    insta::assert_snapshot!(at_every_size(&setup_screen(&v, footer)));
}

/// **A logged-out login shows the command that signs it in**, whole, under its row — for the
/// operator to run; marion never runs it.
#[test]
fn setup_logged_out_login_shows_its_command() {
    let mut v = setup_view(false);
    v.expanded = false;
    if let Some(SetupLine::Profile(p)) = v.lines.get_mut(2) {
        p.limit = Some(s("last limit allowed_warning (five_hour)"));
    }
    select(
        &mut v,
        |l| matches!(l, SetupLine::Profile(p) if p.name == "cx"),
    );
    let footer = keys(&[
        ("enter", "use"),
        ("a", ADD_LOGIN),
        ("del", "remove"),
        ("r", "re-check"),
        ("esc", "back"),
    ]);
    insta::assert_snapshot!(at_every_size(&setup_screen(&v, footer)));
}

/// **With nothing stored**: no logins under any harness, and one dim line in the keys' place.
#[test]
fn setup_no_logins_and_no_keys() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.lines
        .retain(|l| !matches!(l, SetupLine::Profile(_) | SetupLine::Key(_)));
    let at = v
        .lines
        .iter()
        .position(|l| *l == SetupLine::Group(s("api keys")))
        .unwrap();
    v.lines.insert(
        at + 1,
        SetupLine::Note(s("none stored (macOS Keychain) · `a` adds one")),
    );
    select(&mut v, |l| matches!(l, SetupLine::Note(_)));
    let footer = keys(&[("a", "add key"), ("r", "re-check"), ("esc", "back")]);
    insta::assert_snapshot!(report(&setup_screen(&v, footer), 100, 30));
}

#[test]
fn setup_login_remove_waits_for_y() {
    let mut v = setup_view(false);
    v.expanded = false;
    select(
        &mut v,
        |l| matches!(l, SetupLine::Profile(p) if p.name == "personal"),
    );
    let input = Footer::Confirm {
        question: s("remove the login claude personal?"),
        command: s("marion profile rm claude personal"),
    };
    insta::assert_snapshot!(report(&setup_screen(&v, input), 100, 30));
}

/// **A file that fails its check, and notifications off**: the file's row says why in red, and
/// the notify row is dim with the key that turns it on.
#[test]
fn setup_a_failing_file_and_notify_off() {
    let mut v = setup_view(false);
    v.expanded = false;
    for l in &mut v.lines {
        match l {
            SetupLine::File { check, .. } => {
                *check = Some((false, s("line 4: unknown key `harnes`")));
            }
            SetupLine::Notify { on, .. } => *on = false,
            _ => {}
        }
    }
    select(&mut v, |l| matches!(l, SetupLine::Notify { .. }));
    let footer = keys(&[("enter", "turn on"), ("r", "re-check"), ("esc", "back")]);
    insta::assert_snapshot!(report(&setup_screen(&v, footer), 100, 30));
}

/// **The repository row, last on Setup**: a repository with harness config of its own that the
/// operator has not answered about is marked `◐` and names the files held back, and the footer
/// offers the one key that trusts it (after a `y`).
#[test]
fn setup_repository_row_waits_for_the_operators_answer() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.lines.push(SetupLine::Group(s("repository")));
    v.lines.push(SetupLine::Repo {
        answer: None,
        files: s(".mcp.json, .claude/settings.json"),
    });
    select(&mut v, |l| matches!(l, SetupLine::Repo { .. }));
    let footer = keys(&[("enter", TRUST), ("r", "re-check"), ("esc", "back")]);
    insta::assert_snapshot!(report(&setup_screen(&v, footer), 100, 30));
}

/// A key the store could not read keeps its row, with why in red after its id.
#[test]
fn setup_a_key_the_store_could_not_read() {
    let mut v = setup_view(false);
    v.expanded = false;
    for l in &mut v.lines {
        if let SetupLine::Key(k) = l
            && k.id == "openrouter"
        {
            k.note = Some(s("the Keychain refused: user canceled"));
        }
    }
    select(
        &mut v,
        |l| matches!(l, SetupLine::Key(k) if k.id == "openrouter"),
    );
    let footer = keys(&[
        ("a", "add key"),
        ("del", "remove"),
        ("r", "re-check"),
        ("esc", "back"),
    ]);
    insta::assert_snapshot!(report(&setup_screen(&v, footer), 100, 30));
}

/// **`/` search within one project**: the rows that match `bucket`, and dim above them the root
/// they sit under; the line counts the matches.
#[test]
fn agents_search_filters_and_keeps_ancestors_dim() {
    let all = rows();
    let mut kept: Vec<AgentRow> = [0, 2, 3, 4].into_iter().map(|i| all[i].clone()).collect();
    kept[0].dim = true;
    let v = AgentsView {
        rows: kept,
        cursor: 1,
        ..agents_view(0, live_preview())
    };
    let footer = Footer::Input {
        target: s("search"),
        note: s("3 matches"),
        text: s("bucket"),
        keys: vec![
            Hint::new("↑↓", "next"),
            Hint::new("enter", "go"),
            Hint::new("esc", "back"),
        ],
    };
    insta::assert_snapshot!(at_every_size(&screen(Body::Agents(&v), footer)));
}
