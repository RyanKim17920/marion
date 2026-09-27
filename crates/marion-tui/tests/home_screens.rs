//! The home screen at the three sizes that matter — 80x24 (the floor), 100x30 and 160x50 — over
//! fixture data, as text snapshots with a style census.
//!
//! # Why a census and not just the characters
//!
//! Half of what this screen says is style: the selected row is the accent, a blocked node's line is
//! yellow, a destructive confirm has an accent border. A text-only snapshot would pass a screen
//! that lost every colour. The census is one line per distinct style with the number of cells that
//! wear it and the first few characters of them, so a style that moves, appears or disappears is a
//! diff a reviewer can read.
//!
//! # PNGs
//!
//! With `MARION_HOME_DUMP=<dir>` set, every screen is also written there as `<name>.json` — one
//! `[symbol, fg, bg, flags]` per cell — for a rasteriser to turn into dark- and light-theme images.

use marion_tui::home::{
    self, AgentTypeRow, Body, Expanded, FeedRow, FormField, FormView, HarnessRow, HelpView, Hint,
    Input, KeyRow, LoginRow, MessageView, NodeRow, Ready, RecentRow, ResultView, Screen, SetupView,
    StartView, StreamLine, TaskView, Theme, TokenView, WatchView,
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
            "gemini",
            Some("0.9.1"),
            Ready::Attention,
            "sign-in needed",
            "headless · pane",
            Some("run `gemini` once and sign in"),
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

fn start_view() -> StartView {
    StartView {
        harnesses: harnesses(),
        checking: false,
        cursor: 0,
        kinds: vec![
            (s("claude"), s("edits in a worktree")),
            (s("claude-orchestrator"), s("read-only, delegates")),
        ],
        kind: 0,
        models: vec![s("default"), s("opus"), s("sonnet")],
        model: 1,
        options: vec![(s("timeout"), s("20m")), (s("change record"), s("on"))],
        recent: vec![
            RecentRow {
                when: s("14:24"),
                tone: Tone::Live,
                who: s("claude opus"),
                prompt: s("Add rate limiting to /v1/orders"),
                outcome: s("running"),
            },
            RecentRow {
                when: s("13:02"),
                tone: Tone::Done,
                who: s("claude sonnet"),
                prompt: s("Review the pagination PR for off-by-one errors"),
                outcome: s("landed"),
            },
            RecentRow {
                when: s("11:47"),
                tone: Tone::Done,
                who: s("codex default"),
                prompt: s("Port the CSV exporter to streaming writes"),
                outcome: s("landed"),
            },
            RecentRow {
                when: s("10:15"),
                tone: Tone::Failed,
                who: s("gemini default"),
                prompt: s("Summarise open TODOs in src/billing"),
                outcome: s("failed"),
            },
        ],
        echo: vec![
            s("marion"),
            s("run"),
            s("claude"),
            s("--model"),
            s("opus"),
            s("--prompt"),
            s(PROMPT),
        ],
    }
}

/// The forest's rows, with connectors from `tree::Tree` itself so the fixture draws the prefixes
/// the real screen will.
fn rows() -> Vec<NodeRow> {
    // id, parent, harness, kind, tone, elapsed, tokens, doing
    type Spec<'a> = (
        &'a str,
        Option<&'a str>,
        &'a str,
        &'a str,
        Tone,
        &'a str,
        Option<u64>,
        &'a str,
    );
    let spec: [Spec; 6] = [
        (
            "01a093dc-5e1a",
            None,
            "claude",
            "orchestrator",
            Tone::Live,
            "12m04s",
            Some(312_400),
            "waiting on 2 children",
        ),
        (
            "01a093dc-a1f0",
            Some("01a093dc-5e1a"),
            "codex",
            "impl",
            Tone::Live,
            "2m17s",
            Some(88_000),
            "editing src/limits/bucket.rs",
        ),
        (
            "01a093dc-91cd",
            Some("01a093dc-5e1a"),
            "codex",
            "impl",
            Tone::Blocked,
            "3m02s",
            Some(41_000),
            "asks to write .github/workflows/ci.yml",
        ),
        (
            "01a093dc-7b2e",
            Some("01a093dc-5e1a"),
            "codex",
            "impl",
            Tone::Failed,
            "4m50s",
            Some(120_000),
            "exit 101: 3 tests failed",
        ),
        (
            "01a093dc-3c33",
            Some("01a093dc-5e1a"),
            "codex",
            "impl",
            Tone::Done,
            "6m41s",
            Some(197_100),
            "landed marion/t-3c33",
        ),
        (
            "01a093dc-88f2",
            None,
            "claude",
            "review",
            Tone::Done,
            "1h ago",
            Some(54_000),
            "landed marion/t-88f2",
        ),
    ];
    let t = tree::Tree::new(
        spec.iter()
            .map(|(id, parent, ..)| tree::Node {
                id: s(id),
                parent: parent.map(s),
                label: String::new(),
                state: String::new(),
                tone: Tone::Live,
                actions: vec![],
                note: None,
            })
            .collect(),
    );
    spec.iter()
        .enumerate()
        .map(|(i, (id, _, h, kind, tone, el, tok, doing))| NodeRow {
            id: s(id),
            prefix: {
                let (prefix, node) = t.row(i).expect("a row per node");
                assert_eq!(node.id, *id, "the fixture is listed in render order");
                prefix.to_string()
            },
            harness: s(h),
            kind: s(kind),
            short: id[id.len() - 4..].to_string(),
            tone: *tone,
            elapsed: s(el),
            tokens: *tok,
            doing: s(doing),
        })
        .collect()
}

fn landed() -> Expanded {
    Expanded {
        live: false,
        task: Some(task_view("Add token-bucket limiter to /v1/orders")),
        messages: vec![MessageView {
            clock: s("14:27"),
            text: s("operator · 41 bytes · delivered via turn"),
        }],
        stream: stream(9),
        stream_scroll: 0,
        stream_unread: None,
        needs: None,
        tokens: Some(TokenView {
            input: 184_210,
            output: 12_900,
            cached: 151_000,
            rate: vec![
                2_000, 5_000, 9_000, 14_000, 11_000, 8_000, 12_000, 19_000, 23_000, 17_000, 9_000,
                6_000, 11_000, 15_000, 21_000, 26_000, 18_000, 12_000, 9_000, 13_000, 16_000,
                11_000, 7_000, 4_000, 2_000, 1_000,
            ],
            window: s("30s"),
            context_permille: Some(410),
        }),
        result: Some(ResultView {
            summary: String::new(),
            tone: Some(Tone::Done),
            branch: Some(s("marion/t-3c33")),
            added: 84,
            removed: 12,
            files: 3,
            merge: Some(s("git merge --no-ff marion/t-3c33")),
        }),
        workspace: Some(s("~/.local/state/marion/9f3e/worktrees/t-3c33")),
        caps: [
            "steer",
            "interrupt",
            "fork",
            "resume",
            "view",
            "permissions",
        ]
        .iter()
        .enumerate()
        .map(|(i, c)| (s(c), i % 2 == 0))
        .collect(),
    }
}

fn task_view(prompt: &str) -> TaskView {
    TaskView {
        prompt: s(prompt),
        appended: Some(s(
            "When you have finished, call the `report` tool of the marion MCP server exactly once.",
        )),
        acceptance: vec![s("cargo test passes"), s("100 req/min per key, 429 after")],
        verification: vec![s("cargo test -q limits")],
    }
}

/// `n` lines of a child working, oldest first: calls in the default colour, words dim.
fn stream(n: usize) -> Vec<StreamLine> {
    let all = [
        (false, "read_file(src/limits/mod.rs)"),
        (false, "read_file(Cargo.toml)"),
        (true, "I'll add a token bucket keyed by API key."),
        (false, "apply_patch(src/limits/bucket.rs +61)"),
        (false, "apply_patch(src/routes/orders.rs +18 -9)"),
        (false, "command_execution(cargo build -q)"),
        (false, "command_execution(cargo test -q limits)"),
        (true, "142 passed; committing."),
        (false, "command_execution(git commit -am limiter)"),
        (false, "report({\"narrative\":\"limiter in, tests green\"})"),
    ];
    (0..n)
        .map(|i| {
            let (said, text) = all[i % all.len()];
            StreamLine {
                clock: format!("14:{:02}:{:02}", 24 + i / 6, (i * 7) % 60),
                said,
                text: s(text),
            }
        })
        .collect()
}

fn feed() -> Vec<FeedRow> {
    [
        (
            "14:31",
            Tone::Done,
            "3c33",
            "landed marion/t-3c33 · 3 files +84 −12",
        ),
        (
            "14:30",
            Tone::Failed,
            "7b2e",
            "exit 101 · 3 tests failed in limits::bucket",
        ),
        (
            "14:29",
            Tone::Blocked,
            "91cd",
            "asks to write .github/workflows/ci.yml",
        ),
        ("14:29", Tone::Live, "a1f0", "spawned by 5e1a · codex impl"),
        ("14:24", Tone::Live, "5e1a", "started · claude opus"),
    ]
    .iter()
    .map(|(c, t, id, text)| FeedRow {
        clock: s(c),
        tone: *t,
        short: s(id),
        text: s(text),
    })
    .collect()
}

fn watch_view(cursor: usize, expanded: Option<Expanded>) -> WatchView {
    WatchView {
        supervisor: true,
        rows: rows(),
        cursor,
        expanded,
        running: 2,
        attention: 2,
        feed: feed(),
        filter: None,
    }
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
            t("gemini", Ready::Attention, false),
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
        profiles: None,
        form: None,
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

fn help_view() -> HelpView {
    let k = |key: &str, verb: &str, cmd: &str| KeyRow {
        key: s(key),
        verb: s(verb),
        command: s(cmd),
    };
    HelpView {
        sections: vec![
            (
                s("Everywhere"),
                vec![
                    k("tab", "next screen", ""),
                    k("?", "this page", ""),
                    k("q", "quit", ""),
                ],
            ),
            (
                s("Watch"),
                vec![
                    k("enter", "attach", "marion attach <id>"),
                    k("s", "steer", "marion steer <id> <text>"),
                    k("x", "cancel", "marion cancel <id>"),
                    k("c", "copy the merge", "git merge --no-ff <branch>"),
                    k("!", "next attention", "marion list --attention"),
                ],
            ),
            (
                s("Start"),
                vec![
                    k("enter", "run", "marion run <type> --prompt <text>"),
                    k("↑↓", "harness", ""),
                    k("←→", "model", ""),
                ],
            ),
        ],
    }
}

fn hints(tab: &str) -> Vec<Hint> {
    let h = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| Hint::new(*k, *v)).collect();
    match tab {
        "start" => h(&[
            ("enter", "run"),
            ("↑↓", "harness"),
            ("←→", "model"),
            ("^o", "read-only"),
            ("tab", "screens"),
        ]),
        "watch" => h(&[
            ("j/k", "move"),
            ("enter", "attach"),
            ("s", "steer"),
            ("x", "cancel"),
            ("c", "copy merge"),
            ("!", "next attention"),
        ]),
        "setup" => h(&[
            ("j/k", "move"),
            ("enter", "expand"),
            ("r", "re-check"),
            ("n", "new type"),
            ("a", "add key"),
            ("x", "remove key"),
        ]),
        "confirm" => h(&[("y", "yes"), ("any key", "no")]),
        "form" => h(&[
            ("tab", "next field"),
            ("←→", "choose"),
            ("enter", "preview"),
            ("esc", "close"),
        ]),
        _ => h(&[("tab", "back")]),
    }
}

fn prompt(text: &str) -> Input {
    Input::Prompt {
        text: s(text),
        placeholder: s("describe the task, enter runs it"),
        focused: true,
    }
}

fn command(line: &str, note: &str) -> Input {
    Input::Command {
        line: s(line),
        note: s(note),
    }
}

// ------------------------------------------------------------------------------ rendering

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

fn screen<'a>(body: Body<'a>, input: Input, tab: &str) -> Screen<'a> {
    Screen {
        theme: Theme::TRUECOLOR,
        project: PROJECT,
        attention: 2,
        body,
        input,
        hints: hints(tab),
        notice: None,
        frame: 0,
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

#[test]
fn start_typing_a_prompt() {
    let v = start_view();
    insta::assert_snapshot!(at_every_size(&screen(
        Body::Start(&v),
        prompt(PROMPT),
        "start"
    )));
}

/// The cursor on a blocked harness shows its fix line, with the command lifted out in the accent.
#[test]
fn start_blocked_harness_shows_its_fix() {
    let mut v = start_view();
    v.cursor = 4;
    // The choices are the selected harness's: copilot has no read-only flavour, and no history.
    v.kinds = vec![(s("copilot"), s("edits in a worktree"))];
    v.kind = 0;
    v.models = vec![s("default")];
    v.model = 0;
    v.echo = vec![
        s("marion"),
        s("run"),
        s("copilot"),
        s("--prompt"),
        s("<text>"),
    ];
    insta::assert_snapshot!(at_every_size(&screen(Body::Start(&v), prompt(""), "start")));
}

#[test]
fn watch_done_node_with_landed_branch_and_tokens() {
    let v = watch_view(4, Some(landed()));
    let input = command("marion attach 01a093dc-3c33", "same as enter");
    insta::assert_snapshot!(at_every_size(&screen(Body::Watch(&v), input, "watch")));
}

#[test]
fn watch_blocked_node_says_what_it_needs() {
    let e = Expanded {
        live: true,
        task: Some(task_view("Wire the limiter into CI")),
        stream: stream(4),
        needs: Some(s("permission to write .github/workflows/ci.yml")),
        tokens: Some(TokenView {
            input: 38_000,
            output: 3_100,
            cached: 30_000,
            rate: vec![],
            window: s("30s"),
            context_permille: None,
        }),
        ..Default::default()
    };
    let v = watch_view(2, Some(e));
    let input = command("marion attach 01a093dc-91cd", "answer it there");
    insta::assert_snapshot!(at_every_size(&screen(Body::Watch(&v), input, "watch")));
}

#[test]
fn watch_empty_forest() {
    let v = WatchView {
        supervisor: true,
        ..Default::default()
    };
    let input = command("marion run <type> --prompt <text>", "");
    let mut sc = screen(Body::Watch(&v), input, "watch");
    sc.attention = 0;
    insta::assert_snapshot!(report(&sc, 80, 24));
}

#[test]
fn watch_no_supervisor() {
    let v = WatchView::default();
    let input = command("marion run <type> --prompt <text>", "");
    let mut sc = screen(Body::Watch(&v), input, "watch");
    sc.attention = 0;
    insta::assert_snapshot!(report(&sc, 80, 24));
}

/// A running child's live stream, longer than its window and scrolled back: the window, and the
/// line saying what is above and below it.
#[test]
fn watch_running_stream_scrolled_back() {
    let e = Expanded {
        live: true,
        task: Some(task_view(
            "Implement the bucket and wire it into /v1/orders; keep the existing middleware order and add a regression test for burst traffic",
        )),
        stream: stream(30),
        stream_scroll: 6,
        ..Default::default()
    };
    let v = watch_view(1, Some(e));
    let input = command("marion attach 01a093dc-a1f0", "same as enter");
    insta::assert_snapshot!(at_every_size(&screen(Body::Watch(&v), input, "watch")));
}

/// The TASK block by itself: the prompt ellipsised at three rows, marion's appended instruction dim,
/// then the criteria and the checks.
#[test]
fn watch_task_block_shows_what_marion_sent() {
    let e = Expanded {
        live: true,
        task: Some(task_view(
            &"Add a token-bucket limiter to /v1/orders. ".repeat(8),
        )),
        ..Default::default()
    };
    let v = watch_view(1, Some(e));
    let buf = draw(&screen(Body::Watch(&v), command("x", ""), "watch"), 100, 30);
    let text = rows_of(&buf).join("\n");
    let task_rows: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.contains("TASK"))
        .take_while(|l| !l.contains("RAN") && !l.contains("RUNNING"))
        .collect();
    assert_eq!(task_rows.len(), 3 + 1 + 2 + 1, "{text}");
    assert!(task_rows[2].trim_end().ends_with('…'), "{text}");
    assert!(
        task_rows[3].contains("+ marion: When you have finished"),
        "{text}"
    );
    assert!(task_rows[4].contains("✓ cargo test passes"), "{text}");
    assert!(task_rows[6].contains("$ cargo test -q limits"), "{text}");
}

/// **The greying survives the move from the tree screen**: an unmeasured capability is drawn in
/// the tree's own grey, a measured one is not, and both are on screen.
#[test]
fn unmeasured_capabilities_are_greyed_not_hidden() {
    let v = watch_view(4, Some(landed()));
    let buf = draw(&screen(Body::Watch(&v), command("x", ""), "watch"), 160, 50);
    let grey = tree::greyed().fg.unwrap();
    let text = rows_of(&buf).join("\n");
    let (y, row) = text
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("CAN"))
        .unwrap_or_else(|| panic!("no capability row:\n{text}"));
    let x_of = |word: &str| {
        row.find(word)
            .map(|i| row[..i].chars().count() as u16)
            .unwrap()
    };
    assert_ne!(
        buf[(x_of("steer"), y as u16)].fg,
        grey,
        "steer is measured: {row}"
    );
    assert_eq!(
        buf[(x_of("interrupt"), y as u16)].fg,
        grey,
        "interrupt is not: {row}"
    );
}

/// A notice replaces the hints until the next key, and says so in bold.
#[test]
fn a_notice_takes_the_hint_row() {
    let v = watch_view(1, None);
    let mut sc = screen(Body::Watch(&v), command("x", ""), "watch");
    sc.notice = Some(s(
        "queued as m-1; reaches codex a1f0 at its next tool round or turn",
    ));
    let buf = draw(&sc, 100, 30);
    let last = rows_of(&buf).pop().unwrap();
    assert!(
        last.contains("queued as m-1") && !last.contains("enter attach"),
        "{last}"
    );
    assert!(last.ends_with("? shortcuts"), "{last}");
}

/// Following the end: a new line appears at the bottom of the window without any key pressed.
#[test]
fn a_following_stream_shows_the_newest_line() {
    let mut e = Expanded {
        live: true,
        stream: stream(20),
        ..Default::default()
    };
    let draw_text = |e: &Expanded| {
        let v = watch_view(1, Some(e.clone()));
        rows_of(&draw(
            &screen(Body::Watch(&v), command("x", ""), "watch"),
            100,
            30,
        ))
        .join("\n")
    };
    let before = draw_text(&e);
    e.stream.push(StreamLine {
        clock: s("14:59:59"),
        said: false,
        text: s("command_execution(echo newest)"),
    });
    let after = draw_text(&e);
    assert!(
        !before.contains("echo newest") && after.contains("echo newest"),
        "{after}"
    );
    e.stream_scroll = 3;
    assert!(
        !draw_text(&e).contains("echo newest"),
        "scrolled back, the newest is below the window"
    );
}

#[test]
fn watch_compose_open() {
    let v = watch_view(
        1,
        Some(Expanded {
            live: true,
            task: Some(task_view("Implement the bucket")),
            ..Default::default()
        }),
    );
    let input = Input::Compose {
        target: s("steer a1f0"),
        text: s("use a VecDeque, not a Vec, for the window"),
    };
    insta::assert_snapshot!(report(&screen(Body::Watch(&v), input, "watch"), 100, 30));
}

#[test]
fn watch_confirm_open() {
    let v = watch_view(
        1,
        Some(Expanded {
            live: true,
            task: Some(task_view("Implement the bucket")),
            ..Default::default()
        }),
    );
    let input = Input::Confirm {
        question: s("Cancel codex impl a1f0?"),
        command: s("marion cancel 01a093dc-a1f0"),
    };
    insta::assert_snapshot!(report(&screen(Body::Watch(&v), input, "watch"), 100, 30));
}

#[test]
fn setup_expanded_harness() {
    let v = setup_view(false);
    let input = command("gemini", "sign in, then r");
    insta::assert_snapshot!(at_every_size(&screen(Body::Setup(&v), input, "setup")));
}

#[test]
fn setup_doctor_checking() {
    let v = setup_view(true);
    let input = command("marion doctor", "checking…");
    insta::assert_snapshot!(report(&screen(Body::Setup(&v), input, "setup"), 100, 30));
}

#[test]
fn setup_logins_listed() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.cursor = v.harnesses.len() + 2;
    let input = command("marion logout openrouter:work", "x");
    insta::assert_snapshot!(at_every_size(&screen(Body::Setup(&v), input, "setup")));
}

#[test]
fn setup_profiles_placeholder_and_no_keys() {
    let mut v = setup_view(false);
    v.expanded = false;
    v.cursor = v.harnesses.len() - 1;
    v.logins.clear();
    v.logins_note = s("no keys stored · `a` adds one with `marion login <provider>`");
    let input = command("marion doctor", "r");
    insta::assert_snapshot!(report(&screen(Body::Setup(&v), input, "setup"), 100, 30));
}

#[test]
fn setup_form_open() {
    let mut v = setup_view(false);
    v.form = Some(form_view(false));
    let input = command(".marion/agents.toml", "enter previews");
    insta::assert_snapshot!(at_every_size(&screen(Body::Setup(&v), input, "form")));
}

#[test]
fn setup_form_diff_waits_for_y() {
    let mut v = setup_view(false);
    v.form = Some(form_view(true));
    let input = Input::Confirm {
        question: s("Write reviewer to .marion/agents.toml?"),
        command: s("+5 lines"),
    };
    insta::assert_snapshot!(report(&screen(Body::Setup(&v), input, "confirm"), 100, 30));
}

#[test]
fn help_lists_every_key() {
    let v = help_view();
    insta::assert_snapshot!(report(
        &screen(Body::Help(&v), command("marion --help", ""), "help"),
        100,
        30
    ));
}

/// The 256-colour fallback changes the accent and nothing else.
#[test]
fn the_indexed_theme_swaps_only_the_accent() {
    let v = watch_view(4, Some(landed()));
    let input = command("marion attach 01a093dc-3c33", "same as enter");
    let mut sc = screen(Body::Watch(&v), input, "watch");
    let rgb = draw(&sc, 100, 30);
    sc.theme = Theme::INDEXED;
    let idx = draw(&sc, 100, 30);
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

/// Every status on screen carries its glyph: a colour never stands for a state alone.
#[test]
fn every_coloured_status_cell_is_a_glyph_or_its_own_words() {
    let v = watch_view(4, Some(landed()));
    let buf = draw(&screen(Body::Watch(&v), command("x", ""), "watch"), 100, 30);
    let text = rows_of(&buf).join("\n");
    for n in rows() {
        let row = text
            .lines()
            .find(|l| l.contains(&format!("{} {}", n.harness, n.kind)) && l.contains(&n.short));
        let row = row.unwrap_or_else(|| panic!("{} missing:\n{text}", n.short));
        assert!(
            row.contains(n.tone.glyph()),
            "{} has no glyph: {row}",
            n.short
        );
    }
}

/// A forest taller than the screen scrolls to keep the selected node and its expansion in view.
#[test]
fn a_long_forest_keeps_the_selection_on_screen() {
    let mut v = watch_view(0, None);
    let base = v.rows.clone();
    for i in 0..30 {
        let mut r = base[1].clone();
        r.short = format!("{i:04}");
        v.rows.insert(1, r);
    }
    v.cursor = v.rows.len() - 1;
    v.expanded = Some(landed());
    let buf = draw(&screen(Body::Watch(&v), command("x", ""), "watch"), 80, 24);
    let text = rows_of(&buf).join("\n");
    assert!(text.contains("claude review 88f2"), "{text}");
    assert!(text.contains("RESULT"), "{text}");
}
