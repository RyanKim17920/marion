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

use marion_tui::home::setup::AgentTypeRow;
use marion_tui::home::{
    self, AgentRow, AgentsView, Body, Footer, FormField, FormView, HarnessRow, HelpView, Hint, Ink,
    KeyRow, LineKind, LoginRow, Preview, ProfileRow, Ready, Screen, SetupView, StreamLine, Theme,
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
    let t = |n: &str, r: Ready, c: bool| AgentTypeRow {
        name: s(n),
        ready: r,
        custom: c,
    };
    SetupView {
        harnesses: hs,
        checking,
        cursor: 2,
        expanded: true,
        project: vec![
            (s("repo"), s(PROJECT)),
            (s("state"), s("~/.local/state/marion")),
            (s("agents"), s(".marion/agents.toml")),
        ],
        agent_types: vec![
            t("claude", Ready::Ready, false),
            t("claude-orchestrator", Ready::Ready, false),
            t("codex", Ready::Ready, false),
            t("qwen", Ready::Attention, false),
            t("opencode", Ready::Ready, false),
            t("reviewer", Ready::Ready, true),
            t("migrator", Ready::Ready, true),
        ],
        agents_file: s(".marion/agents.toml"),
        validation: Some((true, s(".marion/agents.toml: 2 types, valid"))),
        logins: vec![
            login("anthropic", "anthropic"),
            login("openrouter", "openrouter"),
            login("openrouter", "openrouter:work"),
        ],
        logins_note: s("keys in the macOS Keychain · `a` adds one · `x` removes the selected"),
        profiles: vec![
            profile("claude", "work", true, Ready::Ready, "logged in", None),
            profile(
                "codex",
                "cx",
                false,
                Ready::Attention,
                "logged out",
                Some("CODEX_HOME='/Users/me/.local/share/marion/profiles/codex/cx' codex login"),
            ),
            profile("opencode", "oc", false, Ready::Checking, "checking…", None),
        ],
        profiles_note: s(
            "`a` adds one · `u` makes the selected the default · `x` removes it, keeping its \
             directory · marion never logs in for you",
        ),
        notify: Some((true, "osascript".into())),
        form: None,
    }
}

fn profile(
    harness: &str,
    name: &str,
    default: bool,
    ready: Ready,
    note: &str,
    login: Option<&str>,
) -> ProfileRow {
    ProfileRow {
        harness: s(harness),
        name: s(name),
        default,
        ready,
        note: s(note),
        login: login.map(s),
        limit: None,
    }
}

fn login(provider: &str, id: &str) -> LoginRow {
    LoginRow {
        provider: s(provider),
        id: s(id),
        note: None,
    }
}

fn form_view(preview: bool) -> FormView {
    let f = |label: &str, value: &str, choice: bool, hint: &str| FormField {
        label: s(label),
        value: s(value),
        choice,
        hint: s(hint),
    };
    FormView {
        title: s("New agent type"),
        fields: vec![
            f("name", "reviewer", false, "letters, digits, - and _"),
            f("harness", "codex", true, ""),
            f("model", "", false, "the harness's default"),
            f(
                "description",
                "Reviews a diff and reports",
                false,
                "one line",
            ),
            f("tools", "read", true, ""),
            f("provider", "", false, "optional: an endpoint provider id"),
        ],
        field: if preview { 5 } else { 3 },
        error: None,
        file: s(".marion/agents.toml"),
        preview: if preview {
            vec![
                (' ', s("description = \"Moves data.\"")),
                (' ', s("")),
                ('+', s("[[agent]]")),
                ('+', s("name = \"reviewer\"")),
                ('+', s("harness = \"codex\"")),
                ('+', s("description = \"Reviews a diff and reports\"")),
                ('+', s("tools = [\"read\"]")),
            ]
        } else {
            Vec::new()
        },
    }
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
        ("enter", "output"),
        ("n", "new"),
        ("s", "steer"),
        ("x", "cancel"),
        ("?", "more"),
    ])
}

fn ended_keys() -> Footer {
    keys(&[
        ("enter", "output"),
        ("n", "new"),
        ("m", "merge"),
        ("d", "diff"),
        ("u", "resume"),
        ("o", "shell"),
        ("?", "more"),
    ])
}

fn setup_keys() -> Footer {
    keys(&[
        ("enter", "details"),
        ("r", "re-check"),
        ("e", "edit types"),
        ("n", "new type"),
        ("a", "add key"),
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
    let footer = keys(&[("n", "new"), (",", "setup"), ("?", "more")]);
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
        ("esc", "back"),
        ("m", "merge"),
        ("u", "resume"),
        ("?", "more"),
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

#[test]
fn setup_expanded_harness() {
    let v = setup_view(false);
    let input = setup_keys();
    insta::assert_snapshot!(at_every_size(&setup_screen(&v, input)));
}

/// A machine with most harnesses missing: the missing ones fold into one line, so the sections
/// below still show, and the one under the cursor keeps its own row.
#[test]
fn setup_folds_the_harnesses_that_are_not_installed() {
    let mut v = setup_view(false);
    v.expanded = false;
    for name in [
        "goose",
        "cline",
        "qwen",
        "acp kilo",
        "acp vibe",
        "acp vtcode",
    ] {
        v.harnesses.push(harness(
            name,
            None,
            Ready::Absent,
            "not installed",
            "headless",
            None,
        ));
    }
    v.cursor = v.harnesses.len() - 2;
    let input = setup_keys();
    insta::assert_snapshot!(report(&setup_screen(&v, input), 100, 30));
}

#[test]
fn setup_doctor_checking() {
    let v = setup_view(true);
    let input = setup_keys();
    insta::assert_snapshot!(report(&setup_screen(&v, input), 100, 30));
}

#[test]
fn setup_logins_listed() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.cursor = v.harnesses.len() + 2;
    let input = setup_keys();
    insta::assert_snapshot!(at_every_size(&setup_screen(&v, input)));
}

/// The profiles, by harness: the selected one is logged out, so its row shows the command that
/// logs in to it — for the operator to run — and the box echoes what `u` would run.
#[test]
fn setup_profiles_listed_with_the_login_command_shown() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.cursor = v.harnesses.len() + v.logins.len() + 1;
    v.profiles[0].limit = Some(s("last limit allowed_warning (five_hour)"));
    let input = setup_keys();
    insta::assert_snapshot!(at_every_size(&setup_screen(&v, input)));
}

/// **The notifications row, last on Setup**: on or off and how a notice is shown here, and the
/// box echoes the `marion notify` that Enter runs.
#[test]
fn setup_notifications_row_turns_them_off_on_enter() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.cursor = v.harnesses.len() + v.logins.len() + v.profiles.len();
    let input = setup_keys();
    insta::assert_snapshot!(report(&setup_screen(&v, input), 100, 40));
}

#[test]
fn setup_no_profiles_offers_to_add_one_and_no_keys() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.logins.clear();
    v.logins_note = s("no keys stored · `a` adds one with `marion login <provider>`");
    v.profiles.clear();
    v.profiles_note = s(
        "each harness uses its own login · `a` runs `marion profile add`, then you run the login it prints",
    );
    v.cursor = v.harnesses.len();
    let input = setup_keys();
    insta::assert_snapshot!(report(&setup_screen(&v, input), 100, 30));
}

#[test]
fn setup_profile_remove_waits_for_y() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.cursor = v.harnesses.len() + v.logins.len();
    let input = Footer::Confirm {
        question: s("remove the profile work?"),
        command: s("marion profile rm codex work"),
    };
    insta::assert_snapshot!(report(&setup_screen(&v, input), 100, 30));
}

#[test]
fn setup_form_open() {
    let mut v = setup_view(false);
    v.form = Some(form_view(false));
    let input = setup_keys();
    insta::assert_snapshot!(at_every_size(&setup_screen(&v, input)));
}

#[test]
fn setup_form_diff_waits_for_y() {
    let mut v = setup_view(false);
    v.form = Some(form_view(true));
    let input = Footer::Confirm {
        question: s("write reviewer to .marion/agents.toml?"),
        command: s("+5 -0 lines"),
    };
    insta::assert_snapshot!(report(&setup_screen(&v, input), 100, 30));
}
