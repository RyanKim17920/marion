//! The home screen's decisions, without a terminal: which key does what on which page, and that
//! nothing destructive happens without a `y`.

use super::*;
use marion_core::contract::{AgentId, ExitStatus};
use marion_core::encoding::Duration;
use marion_core::harness::Harness as H;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::result::{ActionKind, ActionLine, ActivityPage, CompletionSummary};
use marion_tui::home::{Footer, Ink, ProfileRow, SetupLine};

fn node(id: &str, parent: Option<&str>, state: NodeState) -> NodeSummary {
    NodeSummary {
        widened: vec![],
        budget: None,
        changed: None,
        review_of: None,
        review: None,
        agent_id: AgentId(id.into()),
        parent_id: parent.map(|p| AgentId(p.into())),
        name: None,
        agent_type: "codex".into(),
        harness: H::Codex,
        harness_version: Some("0.146.0".into()),
        depth: 0,
        state,
        reap_state: ReapState::Live,
        timeout: Duration::from_secs(900),
        pane: false,
        started_at: None,
        ended_at: None,
        tokens: None,
        attention: None,
        endpoint: None,
        race: None,
        cancel: None,
        workflow: None,
    }
}

fn harness(name: &str, ready: Ready) -> Harness {
    Harness {
        name: name.into(),
        version: Some("1.0.0".into()),
        ready,
        note: if ready == Ready::Ready {
            "ready".into()
        } else {
            "too old".into()
        },
        surfaces: "headless".into(),
        pane: false,
        fix: None,
        detail: vec![],
    }
}

fn ty(name: &str, harness: &str, writes: bool) -> AgentType {
    AgentType {
        name: name.into(),
        harness: harness.into(),
        writes,
        custom: false,
    }
}

fn home() -> Home {
    let mut h = Home::new();
    h.harnesses = vec![
        harness("claude", Ready::Ready),
        harness("copilot", Ready::Broken),
    ];
    h.types = vec![
        ty("claude-orchestrator", "claude", false),
        ty("claude", "claude", true),
        ty("copilot", "copilot", true),
    ];
    h.recent_models = vec![("claude".into(), vec!["opus".into(), "sonnet".into()])];
    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        node("child", Some("root"), NodeState::Exited(ExitStatus::Ok)),
    ]);
    h
}

fn typed(h: &mut Home, text: &str) {
    for c in text.chars() {
        assert_eq!(h.key(Key::Char(c)), Effect::None);
    }
}

fn frame(h: &Home) -> view::Frame {
    view::frame(h, &view::Places::default())
}

/// The footer's keys, or nothing when it is a line being typed or a confirm.
fn footer_keys(h: &Home) -> Vec<String> {
    match frame(h).footer {
        Footer::Keys(k) => k.into_iter().map(|x| x.key).collect(),
        Footer::Input { keys, .. } => keys.into_iter().map(|x| x.key).collect(),
        Footer::Confirm { .. } => Vec::new(),
    }
}

fn footer_has(h: &Home, key: &str, verb: &str) -> bool {
    match frame(h).footer {
        Footer::Keys(k) | Footer::Input { keys: k, .. } => {
            k.iter().any(|x| x.key == key && x.verb == verb)
        }
        Footer::Confirm { .. } => false,
    }
}

fn row_ids(f: &view::Frame) -> Vec<String> {
    f.agents.rows.iter().map(|r| r.id.clone()).collect()
}

fn row<'a>(f: &'a view::Frame, id: &str) -> &'a marion_tui::home::AgentRow {
    f.agents
        .rows
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("no row {id}: {:?}", row_ids(f)))
}

/// The selected row's preview facts, each line's spans joined.
fn facts(h: &Home) -> Vec<(String, Vec<Ink>)> {
    frame(h)
        .agents
        .preview
        .expect("a preview")
        .facts
        .into_iter()
        .map(|l| {
            let text = l.iter().map(|(t, _)| t.as_str()).collect::<String>();
            (text, l.into_iter().map(|(_, i)| i).collect())
        })
        .collect()
}

fn landed(branch: &str) -> NodeDetail {
    NodeDetail {
        completion: Some(CompletionSummary {
            status: ExitStatus::Ok,
            narrative: None,
            branch: Some(branch.into()),
            commit: None,
            changed_paths: 1,
            exit: String::new(),
            diff: None,
        }),
        ..Default::default()
    }
}

// ------------------------------------------------------------------ the `n` line

/// **`n` opens the input line on the type last used; tab cycles the types and ↑↓ the type's
/// models**, and Enter runs exactly what the line shows. The choices stay for the next task.
#[test]
fn n_line_runs_the_chosen_type_on_the_chosen_model() {
    let mut h = home();
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    typed(&mut h, "add a limiter");
    h.key(Key::Tab);
    assert_eq!(h.new_type().map(|t| t.name.as_str()), Some("claude"));
    h.key(Key::Down);
    assert_eq!(h.new_model().as_deref(), Some("opus"));
    let Footer::Input {
        target, note, text, ..
    } = frame(&h).footer
    else {
        panic!("the input line")
    };
    assert_eq!(
        (target.as_str(), note.as_str(), text.as_str()),
        ("new claude", "opus", "add a limiter")
    );
    assert_eq!(
        h.key(Key::Enter),
        Effect::Run {
            agent_type: "claude".into(),
            model: Some("opus".into()),
            options: vec![],
            prompt: "add a limiter".into(),
            pane: false,
        }
    );
    assert_eq!(h.mode, Mode::Normal, "the line closes once the run is sent");
    h.key(Key::Char('n'));
    assert_eq!(
        (h.new_type().map(|t| t.name.clone()), h.new_model()),
        (Some("claude".into()), Some("opus".into())),
        "the next task starts on the same choices"
    );
}

/// **`n` opens on a type that can run**: the one last used while it still can, else the first
/// whose harness is ready, rather than the first in the list.
#[test]
fn n_opens_on_a_type_that_can_run() {
    let mut h = home();
    h.harnesses[0].ready = Ready::Broken;
    h.harnesses[1].ready = Ready::Ready;
    h.key(Key::Char('n'));
    assert_eq!(h.new_type().map(|t| t.name.as_str()), Some("copilot"));
    h.key(Key::Esc);
    h.harnesses[0].ready = Ready::Ready;
    h.key(Key::Char('n'));
    assert_eq!(
        h.new_type().map(|t| t.name.as_str()),
        Some("copilot"),
        "the last type, still ready, is kept"
    );
    h.key(Key::Esc);
    for x in &mut h.harnesses {
        x.ready = Ready::Broken;
    }
    h.key(Key::Char('n'));
    assert_eq!(h.new.kind, 2, "none ready: the line says why instead");
}

/// The type's own default model sends no `--model`; shift-tab wraps backwards; another type
/// starts again on its own default.
#[test]
fn the_default_model_sends_no_model_and_a_new_type_resets_it() {
    let mut h = home();
    h.key(Key::Char('n'));
    h.key(Key::Up);
    assert_eq!(
        h.new_model().as_deref(),
        Some("sonnet"),
        "↑ wraps to the last"
    );
    h.key(Key::BackTab);
    assert_eq!(h.new_type().map(|t| t.name.as_str()), Some("copilot"));
    assert_eq!(h.new.model, 0, "a new type is on its own default");
    h.key(Key::Tab);
    typed(&mut h, "plan it");
    assert_eq!(
        h.key(Key::Enter),
        Effect::Run {
            agent_type: "claude-orchestrator".into(),
            model: None,
            options: vec![],
            prompt: "plan it".into(),
            pane: false,
        }
    );
}

/// **The type's harness decides pane or headless**: one with a measured pane surface runs in a
/// pane `enter` can attach to, the rest detached. There is no key for it.
#[test]
fn the_types_harness_decides_pane_or_headless() {
    let mut h = home();
    h.harnesses[0].pane = true;
    h.key(Key::Char('n'));
    typed(&mut h, "x");
    let run = h.key(Key::Enter);
    let Effect::Run { pane, .. } = &run else {
        panic!("a run, got {run:?}")
    };
    assert!(pane);
    assert_eq!(
        run.argv().unwrap().last().map(String::as_str),
        Some("--pane")
    );
    let headless = Effect::Run {
        agent_type: "codex".into(),
        model: None,
        options: vec![],
        prompt: "x".into(),
        pane: false,
    };
    assert_eq!(
        headless.argv().unwrap(),
        ["marion", "run", "codex", "--prompt", "x", "--detach"]
    );
}

/// **Options go before the task, as on the CLI**: `--detach` runs a pane-capable type headless,
/// which is the screen's way to `marion run --detach`; other options reach `marion run` as typed,
/// after the picked model so a typed `--model` wins; `--worktree`/`--verify` run headless.
#[test]
fn options_typed_before_the_task_reach_marion_run_and_detach_runs_headless() {
    let mut h = home();
    h.harnesses[0].pane = true;
    h.key(Key::Char('n'));
    typed(&mut h, "--detach --timeout 1200 add a limiter");
    let Footer::Input { note, .. } = frame(&h).footer else {
        panic!("the input line")
    };
    assert_eq!(
        note, "headless",
        "the line says how it will run once an option chooses"
    );
    let run = h.key(Key::Enter);
    assert_eq!(
        run,
        Effect::Run {
            agent_type: "claude-orchestrator".into(),
            model: None,
            options: vec!["--timeout".into(), "1200".into()],
            prompt: "add a limiter".into(),
            pane: false,
        }
    );
    assert_eq!(
        run.argv().unwrap(),
        [
            "marion",
            "run",
            "claude-orchestrator",
            "--timeout",
            "1200",
            "--prompt",
            "add a limiter",
            "--detach"
        ]
    );
    for (line, pane) in [
        ("--verify 'cargo test' fix it", false),
        ("--worktree fix it", false),
        ("--pane fix it", true),
        ("fix it", true),
    ] {
        h.key(Key::Char('n'));
        typed(&mut h, line);
        let Effect::Run { pane: got, .. } = h.key(Key::Enter) else {
            panic!("{line}: a run")
        };
        assert_eq!(got, pane, "{line}");
    }
    // What the line cannot split, or that would lose the task, is refused with the line kept.
    for (line, says) in [
        ("--prompt x fix it", "leave out --prompt"),
        ("--pane --detach fix it", "not both"),
        ("--timeout", "needs a value"),
        ("--detach", "type the task"),
        ("--verify \"open fix it", "not closed"),
    ] {
        h.key(Key::Char('n'));
        typed(&mut h, line);
        assert_eq!(h.key(Key::Enter), Effect::None, "{line}");
        assert!(
            h.notice.as_deref().unwrap_or("").contains(says),
            "{line}: {:?}",
            h.notice
        );
        assert_eq!(h.mode, typing(Purpose::New, line));
        h.key(Key::Esc);
    }
}

/// **A line that cannot run stays open with the reason**: a blank task, and a type whose harness
/// is not ready, keep what was typed rather than losing it to the refusal.
#[test]
fn the_n_line_refuses_a_blank_task_or_an_unready_type_and_keeps_the_text() {
    let mut h = home();
    h.key(Key::Char('n'));
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("type the task"));
    assert!(matches!(h.mode, Mode::Typing { .. }), "still open");
    h.key(Key::Tab);
    h.key(Key::Tab);
    typed(&mut h, "x");
    let Footer::Input { note, .. } = frame(&h).footer else {
        panic!("the input line")
    };
    assert_eq!(note, "too old", "the line says why before enter does");
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("copilot: too old"));
    assert_eq!(
        h.mode,
        Mode::Typing {
            purpose: Purpose::New,
            text: "x".into()
        },
        "a refused run keeps what was typed"
    );
    h.types.clear();
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("no agent type to run"));
}

// ------------------------------------------------------------------ pages

#[test]
fn comma_opens_setup_and_esc_or_comma_come_back() {
    let mut h = home();
    assert_eq!(h.page, Page::Agents, "bare marion opens on the agents");
    h.key(Key::Char(','));
    assert_eq!(h.page, Page::Setup);
    h.key(Key::Esc);
    assert_eq!(h.page, Page::Agents);
    h.key(Key::Char(','));
    h.key(Key::Char(','));
    assert_eq!(h.page, Page::Agents);
}

/// **Help opens from wherever the operator is and goes back there**, and a `?` typed into a line
/// is text.
#[test]
fn help_opens_from_any_page_and_returns_to_it() {
    let mut h = home();
    for from in [Page::Agents, Page::Setup] {
        h.page = from;
        h.key(Key::Char('?'));
        assert_eq!(h.page, Page::Help);
        h.key(Key::Esc);
        assert_eq!(h.page, from, "back where it was opened");
    }
    h.page = Page::Agents;
    h.key(Key::Char('?'));
    h.key(Key::Char('?'));
    assert_eq!(h.page, Page::Agents, "`?` closes it too");
    h.key(Key::Char('n'));
    typed(&mut h, "why?");
    assert_eq!(h.page, Page::Agents);
    assert!(matches!(&h.mode, Mode::Typing { text, .. } if text == "why?"));
}

/// Help drawn as the operator sees it: the real key table (`view::KEYS`) through `marion_tui`, at
/// the size a terminal opens at.
fn help_screen(h: &Home, w: u16, rows: u16) -> String {
    use ratatui::widgets::Widget as _;
    let f = frame(h);
    let screen = marion_tui::home::Screen {
        theme: marion_tui::home::theme::Theme::from_colorterm(None),
        title: &f.title,
        counts: &f.counts,
        body: f.body(Page::Help),
        footer: f.footer.clone(),
        notice: None,
        frame: 0,
    };
    let area = ratatui::layout::Rect::new(0, 0, w, rows);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    (&screen).render(area, &mut buf);
    (0..rows)
        .map(|y| {
            (0..w)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// **Help shows every key of the real key table at 80x24**, none clipped, and the section for the
/// page it was opened from comes first; where it does not fit it scrolls with j/k.
#[test]
fn help_shows_every_section_of_the_real_key_table_at_80x24() {
    let mut h = home();
    h.key(Key::Char('?'));
    let screen = help_screen(&h, 80, 24);
    for (title, keys) in view::KEYS {
        for (key, _, _) in *keys {
            assert!(
                screen.contains(&format!("{key}  ")),
                "{title}: `{key}` is missing or clipped at 80x24:\n{screen}"
            );
        }
    }
    insta::assert_snapshot!(screen);
    let titles = |h: &Home| {
        frame(h)
            .help
            .sections
            .into_iter()
            .map(|(t, _)| t)
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(&h), ["agents", "commands", "input line", "setup"]);
    h.key(Key::Esc);
    h.key(Key::Char(','));
    h.key(Key::Char('?'));
    assert_eq!(
        titles(&h)[0],
        "setup",
        "opened from Setup, Setup comes first"
    );
    // A screen too short for the page scrolls, and says so.
    let short = help_screen(&h, 80, 14);
    assert!(short.contains("j/k scroll"), "{short}");
    h.key(Key::Char('j'));
    assert_eq!(h.help_scroll, 1);
    h.key(Key::Char('k'));
    h.key(Key::Char('k'));
    assert_eq!(h.help_scroll, 0);
}

/// The clock the loop may wake for: only while something on the page moves with time. An idle
/// forest, an ended one, or a page with nothing animated makes the screen sleep until an event.
#[test]
fn only_a_moving_screen_asks_for_a_clock() {
    let mut h = home();
    h.set_nodes(vec![]);
    assert!(!h.animating(), "an empty forest sleeps");
    h.set_nodes(vec![node("a", None, NodeState::Exited(ExitStatus::Ok))]);
    assert!(!h.animating(), "an ended forest sleeps");
    h.set_nodes(vec![node("a", None, NodeState::Running)]);
    assert!(h.animating(), "a running node's time ticks");
    h.page = Page::Help;
    assert!(!h.animating(), "not while another page hides it");
    h.page = Page::Setup;
    assert!(!h.animating());
    h.checking = true;
    assert!(h.animating(), "a harness still being checked spins");
}

// ------------------------------------------------------------------ the footer

/// **The footer offers no key the key table does not list**, in every state it can be in: `?`
/// teaches the whole of what the footer shows a part of.
#[test]
fn every_footer_key_is_in_the_key_table() {
    let table: Vec<&str> = view::KEYS
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|(k, _, _)| *k))
        .collect();
    let mut states: Vec<Home> = Vec::new();
    let mut h = home();
    states.push(h.clone());
    h.key(Key::Char('j'));
    states.push(h.clone());
    h.absorb_detail(AgentId("child".into()), landed("marion/t-1"));
    states.push(h.clone());
    let mut empty = home();
    empty.set_nodes(vec![]);
    states.push(empty);
    for k in ['n', ':'] {
        let mut h = home();
        h.key(Key::Char(k));
        states.push(h);
    }
    let mut full = home();
    full.key(Key::Enter);
    states.push(full);
    let mut help = home();
    help.key(Key::Char('?'));
    states.push(help);
    let mut s = profiles_home();
    for cursor in 0..=6 {
        s.setup.cursor = cursor;
        states.push(s.clone());
    }
    s.key(Key::Char('a'));
    states.push(s);
    for h in &states {
        for k in footer_keys(h) {
            assert!(
                table.contains(&k.as_str()),
                "`{k}` is on the footer ({:?}, {:?}) but not in the key table",
                h.page,
                h.mode
            );
        }
    }
}

/// **The footer offers the page's eight keys, each only where it does something**: `x` for a
/// running agent and not an ended one, `j/k` and `enter` only over a list, and `?` last, the one
/// a narrow row keeps.
#[test]
fn the_footer_offers_only_what_the_selection_takes() {
    let mut h = home();
    assert_eq!(
        footer_keys(&h),
        ["j/k", "enter", "n", "x", ":", ",", "q", "?"]
    );
    h.key(Key::Char('j'));
    assert_eq!(
        footer_keys(&h),
        ["j/k", "enter", "n", ":", ",", "q", "?"],
        "an ended agent is not cancelled"
    );
    h.lost_supervisor();
    assert_eq!(footer_keys(&h), ["n", ":", ",", "q", "?"]);
    h.set_nodes(vec![]);
    assert_eq!(footer_keys(&h), ["n", ":", ",", "q", "?"]);
}

// ------------------------------------------------------------------ the list

/// **The list leads with what needs the operator**, and the counts say how many of each.
#[test]
fn the_list_leads_with_the_node_that_needs_you() {
    use marion_core::node::BlockReason;
    let mut h = home();
    h.set_nodes(vec![
        node("a", None, NodeState::Running),
        node("b", None, NodeState::Blocked(BlockReason::Permission)),
    ]);
    let f = frame(&h);
    assert_eq!(row_ids(&f), ["b", "a"]);
    assert_eq!(f.counts, "2 running  1 need you");
    assert_eq!(h.selected().unwrap().agent_id.0, "b");
}

/// **The order holds while a line is open**: a row must not move under a command being typed
/// for it. A node that arrives meanwhile goes to the end, and the order comes back once the line
/// closes.
#[test]
fn set_nodes_holds_the_order_while_a_line_is_open() {
    use marion_core::node::BlockReason;
    let mut h = home();
    h.set_nodes(vec![node("a", None, NodeState::Running)]);
    h.key(Key::Char(':'));
    let asks = node("b", None, NodeState::Blocked(BlockReason::Permission));
    h.set_nodes(vec![asks.clone(), node("a", None, NodeState::Running)]);
    assert_eq!(row_ids(&frame(&h)), ["a", "b"], "held while typing");
    assert_eq!(
        h.selected().unwrap().agent_id.0,
        "a",
        "the command's row stays put"
    );
    h.key(Key::Esc);
    assert_eq!(
        row_ids(&frame(&h)),
        ["b", "a"],
        "released when the line closes"
    );
    assert_eq!(
        h.selected().unwrap().agent_id.0,
        "a",
        "the selection follows its node"
    );
}

/// **A row's title is its task's first line**, read once per node; a root, whose prompt went to its
/// harness rather than into a contract, is called by its type. The titles outlive a lost
/// supervisor: the same nodes come back with it.
#[test]
fn a_rows_title_is_its_tasks_first_line_or_its_type() {
    let mut h = home();
    assert_eq!(h.untitled().len(), 2);
    h.absorb_detail(
        AgentId("child".into()),
        NodeDetail {
            task: Some(marion_core::proto::result::TaskSent {
                prompt: "\n  fix the limiter\x07\n\nand more".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    assert!(h.set_title(&AgentId("root".into()), None));
    assert!(h.untitled().is_empty());
    let f = frame(&h);
    let title = &row(&f, "child").title;
    assert!(
        title.starts_with("fix the limiter"),
        "the first line: {title}"
    );
    assert!(
        !title.contains('\x07'),
        "a control is never drawn raw: {title:?}"
    );
    assert_eq!(row(&f, "root").title, "codex");
    h.lost_supervisor();
    assert_eq!(h.watch.titles.len(), 2);
}

/// **A node still spawning has no title yet, not its type for good**: its contract may not be
/// written, so the answer is not recorded and the session asks again.
#[test]
fn a_spawning_nodes_missing_task_is_asked_again() {
    let mut h = home();
    h.set_nodes(vec![node("kid", None, NodeState::Spawning)]);
    assert!(!h.set_title(&AgentId("kid".into()), None), "not final");
    assert_eq!(h.untitled().len(), 1, "still untitled, so asked again");
    assert_eq!(
        row(&frame(&h), "kid").title,
        "codex",
        "drawn by its type meanwhile"
    );
    assert!(h.set_title(&AgentId("kid".into()), Some("read the limiter")));
    assert_eq!(row(&frame(&h), "kid").title, "read the limiter");
}

/// **A title for a node the forest no longer has records nothing**: its type is not known to stand
/// in, and an empty title would be kept for good.
#[test]
fn a_title_for_a_node_that_is_gone_records_nothing() {
    let mut h = home();
    assert!(!h.set_title(&AgentId("gone".into()), None));
    assert!(!h.watch.titles.contains_key("gone"));
    assert!(
        h.set_title(&AgentId("gone".into()), Some("its task")),
        "a task is its own title, known or not"
    );
}

/// A row carries its run time and its token total from the summary, for every node, not only
/// the selected one whose detail was read; unknown is blank, never 0.
#[test]
fn every_row_shows_its_run_time_and_tokens() {
    use marion_core::encoding::SystemTime;
    let t0 = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
    let mut running = node("a", None, NodeState::Running);
    running.started_at = Some(SystemTime(t0));
    running.tokens = Some(88_000);
    let unstarted = node("b", None, NodeState::Running);
    let mut h = home();
    h.set_nodes(vec![running, unstarted]);
    let now = t0 + std::time::Duration::from_secs(137);
    let f = view::frame_at(&h, &view::Places::default(), now);
    let a = row(&f, "a");
    assert_eq!((a.time.as_str(), a.tokens), ("2m", Some(88_000)));
    let b = row(&f, "b");
    assert_eq!((b.time.as_str(), b.tokens), ("", None));

    let mut ended = node("c", None, NodeState::Exited(ExitStatus::Ok));
    ended.started_at = Some(SystemTime(t0));
    ended.ended_at = Some(SystemTime(t0 + std::time::Duration::from_secs(3_840)));
    h.set_nodes(vec![ended]);
    let f = view::frame_at(
        &h,
        &view::Places::default(),
        now + std::time::Duration::from_secs(9_999),
    );
    assert_eq!(
        row(&f, "c").time,
        "1h04",
        "an ended node's time is how long it ran"
    );
}

#[test]
fn run_times_fit_five_columns_in_their_two_largest_units() {
    use view::run_time;
    assert_eq!(run_time(45), "45s");
    assert_eq!(run_time(12 * 60 + 59), "12m");
    assert_eq!(run_time(3 * 3600 + 4 * 60), "3h04");
    assert_eq!(run_time(2 * 86_400 + 3 * 3600), "2d03");
    for s in [0, 59, 3_599, 86_399, 99 * 86_400] {
        assert!(run_time(s).chars().count() <= 5, "{s}");
    }
}

// ------------------------------------------------------------------ the selection's keys

#[test]
fn cancel_waits_for_y_and_anything_else_keeps_the_node() {
    let mut h = home();
    assert_eq!(
        h.key(Key::Char('x')),
        Effect::None,
        "no cancel on the first key"
    );
    assert!(matches!(h.mode, Mode::Confirm(_)));
    let Footer::Confirm { question, command } = frame(&h).footer else {
        panic!("a confirm")
    };
    assert_eq!(question, "cancel codex root?");
    assert_eq!(command, "marion cancel root");
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(h.notice.as_deref(), Some("not cancelled"));
    h.key(Key::Char('x'));
    assert_eq!(
        h.key(Key::Char('y')),
        Effect::Cancel(AgentId("root".into()))
    );
}

/// The cancel line counts what it ends below the node.
#[test]
fn the_cancel_line_counts_the_live_agents_below() {
    let mut h = home();
    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        node("a", Some("root"), NodeState::Running),
        node("b", Some("a"), NodeState::Running),
        node("c", Some("root"), NodeState::Exited(ExitStatus::Ok)),
    ]);
    h.select("root");
    h.key(Key::Char('x'));
    let Footer::Confirm { question, .. } = frame(&h).footer else {
        panic!("a confirm")
    };
    assert_eq!(question, "cancel codex root and the 2 below it?");
}

/// **A second `x` on a node whose cancel is still in its grace asks to kill it** — the
/// escalation for a harness ignoring its abort, `marion cancel --force` — and still only after a
/// `y`.
#[test]
fn x_on_a_node_being_cancelled_asks_to_kill_it() {
    let mut h = home();
    let mut cancelling = node("root", None, NodeState::Running);
    cancelling.cancel = Some(marion_core::proto::NodeCancel {
        by: marion_core::journal::CancelBy::Operator,
        forced: false,
        had_abort: true,
    });
    h.set_nodes(vec![cancelling]);
    assert_eq!(h.key(Key::Char('x')), Effect::None);
    assert_eq!(h.key(Key::Char('y')), Effect::Kill(AgentId("root".into())));
    assert_eq!(
        Effect::Kill(AgentId("root".into())).argv(),
        Some(vec![
            "marion".into(),
            "cancel".into(),
            "root".into(),
            "--force".into()
        ])
    );
}

/// **No destructive effect is reachable without a `y`**: every key on every page, from a fresh
/// state, is pressed once, and none returns a destructive effect.
#[test]
fn no_single_key_on_any_page_is_destructive() {
    for page in [Page::Agents, Page::Setup, Page::Help] {
        for k in every_key() {
            let mut h = home();
            h.page = page;
            let e = h.key(k.clone());
            assert!(
                !e.destructive(),
                "{k:?} on {page:?} returned {e:?} without a confirm"
            );
        }
    }
}

/// An ended agent has nothing to cancel, and `x` says so rather than asking.
#[test]
fn x_on_an_ended_agent_says_it_has_ended() {
    let mut h = home();
    h.key(Key::Char('j'));
    assert_eq!(h.key(Key::Char('x')), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(h.notice.as_deref(), Some("already ended"));
}

// ------------------------------------------------------------------ the `:` line

/// The `:` line's words once typed and entered.
fn colon(h: &mut Home, text: &str) -> Effect {
    h.mode = Mode::Normal;
    h.key(Key::Char(':'));
    h.key(Key::Paste(text.into()));
    h.key(Key::Enter)
}

/// **`:` runs any command `marion --help` lists, on the selected agent**: a command that acts on
/// one agent gets the selected agent's id first, one that does not runs as typed, and the words
/// split as a shell splits them. `j` is text on the line.
#[test]
fn colon_runs_any_command_on_the_selected_agent() {
    let mut h = home();
    h.key(Key::Char(':'));
    let Footer::Input { target, .. } = frame(&h).footer else {
        panic!("the input line")
    };
    assert_eq!(target, "marion");
    typed(&mut h, "steer use a deque j");
    assert!(footer_has(&h, "enter", "run"));
    let words = |w: &[&str]| Effect::Command(w.iter().map(|s| s.to_string()).collect());
    assert_eq!(
        h.key(Key::Enter),
        words(&["steer", "root", "use", "a", "deque", "j"])
    );
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(
        colon(&mut h, r#"steer "use a deque""#),
        words(&["steer", "root", "use a deque"])
    );
    assert_eq!(colon(&mut h, "doctor"), words(&["doctor"]));
    assert_eq!(
        colon(&mut h, "cancel --force"),
        words(&["cancel", "root", "--force"])
    );
    assert_eq!(
        words(&["diff", "root"]).argv(),
        Some(vec!["marion".into(), "diff".into(), "root".into()]),
        "the line it runs is marion and the words"
    );
    h.set_nodes(vec![]);
    assert_eq!(
        colon(&mut h, "ls"),
        words(&["ls"]),
        "with nothing selected, as typed"
    );
}

/// **A line that names no command, or one the screen does not run, stays open with why**, so the
/// text is not lost.
#[test]
fn colon_refuses_what_is_no_command_and_keeps_the_text() {
    let mut h = home();
    for (text, why) in [
        (
            "bogus thing",
            "no command named `bogus`; tab completes one".to_string(),
        ),
        ("  ", "type a command; tab completes it".to_string()),
        ("mcp", OFF_LINE[0].1.to_string()),
        (r#"steer "open"#, "a \" is not closed".to_string()),
    ] {
        assert_eq!(colon(&mut h, text), Effect::None, "{text}");
        assert_eq!(h.notice.as_deref(), Some(why.as_str()), "{text}");
        assert_eq!(
            h.mode,
            Mode::Typing {
                purpose: Purpose::Command,
                text: text.into()
            }
        );
    }
}

/// **`tab` completes the command word**: to the command where only one starts with it, else as
/// far as all that do agree; `mcp`, which the line refuses, is never offered. After the command,
/// `tab` does nothing.
#[test]
fn tab_completes_the_command_word() {
    let mut h = home();
    let tab = |h: &mut Home, typed: &str| -> String {
        h.mode = Mode::Normal;
        h.key(Key::Char(':'));
        h.key(Key::Paste(typed.into()));
        h.key(Key::Tab);
        match &h.mode {
            Mode::Typing { text, .. } => text.clone(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(tab(&mut h, "st"), "steer ");
    assert_eq!(tab(&mut h, "m"), "merge ", "mcp is not a completion");
    assert_eq!(tab(&mut h, "s"), "s", "search, shell and steer agree on s");
    assert_eq!(tab(&mut h, "re"), "resume ");
    assert_eq!(tab(&mut h, "steer x"), "steer x");
    assert_eq!(tab(&mut h, "z"), "z");
    assert_eq!(h.notice.as_deref(), Some("no command starts `z`"));
    assert_eq!(completions("").len(), crate::verbs::SPECS.len() - 3);
}

/// **The `:` line's note teaches it**: the commands the word typed could be, then — once it is a
/// command — the agent it acts on, or what it does.
#[test]
fn the_colon_lines_note_names_the_commands_then_the_agent() {
    let mut h = home();
    let note = |h: &Home| match frame(h).footer {
        Footer::Input { note, .. } => note,
        other => panic!("{other:?}"),
    };
    h.key(Key::Char(':'));
    typed(&mut h, "s");
    assert_eq!(note(&h), "search shell steer");
    typed(&mut h, "teer");
    assert_eq!(note(&h), "send a running agent a message");
    typed(&mut h, " ");
    assert_eq!(note(&h), "on codex root");
    h.mode = Mode::Normal;
    h.key(Key::Char(':'));
    typed(&mut h, "doctor ");
    assert_eq!(note(&h), "check each harness is installed and ready");
    h.mode = Mode::Normal;
    h.key(Key::Char(':'));
    typed(&mut h, "q");
    assert_eq!(note(&h), "no such command");
}

/// **Enter shows a headless agent's output full, and attaches only to a pane agent**; Esc goes
/// back, and the footer says which Enter does.
#[test]
fn enter_fills_the_body_with_a_headless_agent_and_attaches_a_pane_one() {
    let mut h = home();
    assert!(footer_has(&h, "enter", "output"));
    assert_eq!(
        h.key(Key::Enter),
        Effect::None,
        "no attach on a headless agent"
    );
    assert!(h.watch.full);
    assert!(frame(&h).agents.full);
    assert!(footer_has(&h, "esc", "back"));
    assert_eq!(h.key(Key::Esc), Effect::None);
    assert!(!h.watch.full, "Esc goes back to the list");

    let mut paned = node("root", None, NodeState::Running);
    paned.pane = true;
    h.set_nodes(vec![paned]);
    assert!(footer_has(&h, "enter", "attach"));
    assert_eq!(h.key(Key::Enter), Effect::Attach(AgentId("root".into())));
    h.set_nodes(vec![]);
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("nothing selected"));
}

/// **A blocked agent's preview says what it waits on the operator for**: the supervisor's own
/// words where it gave some, else what its state says; a running agent waits on nobody.
#[test]
fn a_blocked_agents_preview_says_what_it_needs() {
    use marion_core::node::BlockReason;
    let needs = |n: NodeSummary| {
        let mut h = home();
        h.set_nodes(vec![n]);
        frame(&h).agents.preview.and_then(|p| p.needs)
    };
    let blocked = |r| node("root", None, NodeState::Blocked(r));
    assert_eq!(
        needs(blocked(BlockReason::Permission)).as_deref(),
        Some("wants permission")
    );
    assert_eq!(
        needs(blocked(BlockReason::BootDialog)).as_deref(),
        Some("a dialog is open in its terminal")
    );
    assert_eq!(
        needs(blocked(BlockReason::Descendants)).as_deref(),
        Some("waits on its agents")
    );
    let said = NodeSummary {
        attention: Some("approve `rm -rf build`?".into()),
        ..blocked(BlockReason::Permission)
    };
    assert_eq!(needs(said).as_deref(), Some("approve `rm -rf build`?"));
    assert_eq!(needs(node("root", None, NodeState::Running)), None);
}

// ------------------------------------------------------------------ the preview

#[test]
fn stream_pages_append_only_where_the_last_one_ended() {
    let mut h = home();
    let line = |t: &str| ActionLine {
        at: "t".into(),
        kind: ActionKind::Call,
        text: t.into(),
        id: None,
    };
    let page = |from, next, t: &str| NodeDetail {
        stream: Some(ActivityPage {
            from,
            next,
            lines: vec![line(t)],
            unread: None,
        }),
        ..Default::default()
    };
    let id = AgentId("root".into());
    h.absorb_detail(id.clone(), page(0, 10, "a"));
    h.absorb_detail(id.clone(), page(10, 20, "b"));
    h.absorb_detail(id.clone(), page(10, 20, "stale"));
    let texts: Vec<_> = h.watch.stream.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["a", "b"]);
    // Another node starts over.
    h.absorb_detail(AgentId("child".into()), page(0, 5, "c"));
    assert_eq!(h.watch.stream.len(), 1);
}

// ------------------------------------------------------------------ search

fn titled() -> Home {
    let mut h = home();
    h.set_title(&AgentId("root".into()), Some("add rate limiting"));
    h.set_title(&AgentId("child".into()), Some("port invoices"));
    h
}

fn search_note(h: &Home) -> String {
    match frame(h).footer {
        Footer::Input { target, note, .. } => {
            assert_eq!(target, "search");
            note
        }
        f => panic!("the search line, got {f:?}"),
    }
}

/// **`/` filters the list as it is typed**, keeping the rows above a match dim so it is seen
/// where it sits; the cursor goes to the match, and enter keeps it there with the list whole again.
#[test]
fn slash_filters_the_list_and_enter_goes_to_the_match() {
    let mut h = titled();
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some("root"));
    h.key(Key::Char('/'));
    assert_eq!(search_note(&h), "", "nothing typed, nothing counted");
    assert_eq!(h.shown_rows(), [(0, false), (1, false)]);
    typed(&mut h, "INVO");
    assert_eq!(
        h.shown_rows(),
        [(0, true), (1, false)],
        "case does not matter"
    );
    assert_eq!(search_note(&h), "1 match");
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some("child"));
    let f = frame(&h);
    assert_eq!(
        f.agents.rows.iter().map(|r| r.dim).collect::<Vec<_>>(),
        [true, false]
    );
    assert_eq!(f.agents.cursor, 1);
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some("child"));
    assert_eq!(h.shown_rows().len(), 2, "the list is whole again");
}

/// No match shows nothing, says so, and keeps the line open on enter; esc goes back to the row
/// selected when the search opened.
#[test]
fn a_search_with_no_match_keeps_the_line_and_esc_goes_back() {
    let mut h = titled();
    h.key(Key::Char('/'));
    typed(&mut h, "invo");
    typed(&mut h, "zzz");
    assert!(h.shown_rows().is_empty());
    assert_eq!(search_note(&h), "no match");
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("no match"));
    assert!(matches!(
        h.mode,
        Mode::Typing {
            purpose: Purpose::Search { .. },
            ..
        }
    ));
    h.key(Key::Esc);
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(
        h.selected().map(|n| n.agent_id.0.as_str()),
        Some("root"),
        "back where it was"
    );
}

/// **The highlight and the preview stay on one row while the list changes under a search**: a
/// forest without the selected match moves the cursor to a match that is left, a title read
/// meanwhile can make the selected row stop matching, and a search that hides every row previews
/// nothing rather than a row the list does not show.
#[test]
fn a_search_keeps_the_highlight_and_the_preview_on_one_row() {
    let forest = || {
        vec![
            node("root", None, NodeState::Running),
            node("child", Some("root"), NodeState::Exited(ExitStatus::Ok)),
            node("other", None, NodeState::Running),
        ]
    };
    let mut h = titled();
    h.set_nodes(forest());
    h.set_title(&AgentId("other".into()), Some("more invoices"));
    h.key(Key::Char('/'));
    typed(&mut h, "invo");
    let previewed = |h: &Home| {
        let f = frame(h);
        let lit = f.agents.rows.get(f.agents.cursor).map(|r| r.id.clone());
        (lit, f.agents.preview.map(|p| p.title))
    };
    let selected = h.selected().map(|n| n.agent_id.0.clone()).unwrap();
    let (lit, shown) = previewed(&h);
    assert_eq!(lit.as_deref(), Some(selected.as_str()));
    assert_eq!(shown.as_ref(), h.watch.titles.get(&selected));
    // The selected match leaves the forest: the cursor goes to the match that is left.
    let left = if selected == "child" {
        "other"
    } else {
        "child"
    };
    h.set_nodes(
        forest()
            .into_iter()
            .filter(|n| n.agent_id.0 != selected)
            .collect(),
    );
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some(left));
    assert_eq!(previewed(&h).0.as_deref(), Some(left));
    // Its title is read again and no longer matches: nothing is left, so nothing is previewed.
    h.watch.titles.remove(left);
    h.set_title(&AgentId(left.into()), Some("something else"));
    assert!(h.shown_rows().is_empty());
    assert_eq!(frame(&h).agents.preview, None);
}

/// ↑↓ step through the matches, wrapping; a match is on its title, agent type or short id.
#[test]
fn arrows_step_through_the_matches_and_wrap() {
    let mut h = titled();
    h.key(Key::Char('/'));
    typed(&mut h, "codex");
    assert_eq!(search_note(&h), "2 matches", "both are of type codex");
    let at = |h: &Home| h.selected().map(|n| n.agent_id.0.clone()).unwrap();
    assert_eq!(at(&h), "root");
    h.key(Key::Down);
    assert_eq!(at(&h), "child");
    h.key(Key::Down);
    assert_eq!(at(&h), "root", "wraps");
    h.key(Key::Up);
    assert_eq!(at(&h), "child", "and back");
    h.key(Key::Esc);
    h.key(Key::Char('/'));
    typed(&mut h, "chi");
    assert_eq!(at(&h), "child", "the short id of `child` is itself");
}

/// `/` on an empty list opens nothing; over a list it opens the search line.
#[test]
fn slash_needs_a_list() {
    let mut h = home();
    h.set_nodes(vec![]);
    h.key(Key::Char('/'));
    assert_eq!(h.mode, Mode::Normal);
    let mut h = titled();
    h.key(Key::Char('/'));
    assert!(matches!(
        h.mode,
        Mode::Typing {
            purpose: Purpose::Search { .. },
            ..
        }
    ));
}

/// **What a model said is drawn, never obeyed**: a control in an output line reaches the preview
/// as its escape, so it cannot clear the screen or move the cursor.
#[test]
fn an_output_line_reaches_the_preview_with_its_controls_spelled() {
    let mut h = home();
    h.set_nodes(vec![node("root", None, NodeState::Running)]);
    h.absorb_detail(
        AgentId("root".into()),
        NodeDetail {
            stream: Some(ActivityPage {
                from: 0,
                next: 1,
                lines: vec![ActionLine {
                    at: "t".into(),
                    kind: ActionKind::Said,
                    text: "done\x1b[2J\x07".into(),
                    id: None,
                }],
                unread: None,
            }),
            ..Default::default()
        },
    );
    let stream = frame(&h).agents.preview.expect("a preview").stream;
    let text = &stream.last().expect("the line").text;
    assert!(text.starts_with("done"), "{text:?}");
    assert!(!text.contains(['\x1b', '\x07']), "{text:?}");
}

/// **A call's end is drawn as its outcome.** Paged in two, a command and its end fold to one line
/// each, the end drawn as a success; the preview shows the output only once the detail is read.
#[test]
fn an_ended_call_shows_its_outcome() {
    use marion_tui::home::LineKind;
    let mut h = home();
    h.set_nodes(vec![node("root", None, NodeState::Running)]);
    assert_eq!(
        frame(&h).agents.preview.unwrap().quiet.as_deref(),
        Some("reading")
    );
    let line = |at: &str, kind, text: &str| ActionLine {
        at: at.into(),
        kind,
        text: text.into(),
        id: Some("item_4".into()),
    };
    let page = |from, next, lines| NodeDetail {
        stream: Some(ActivityPage {
            from,
            next,
            lines,
            unread: None,
        }),
        ..Default::default()
    };
    let id = AgentId("root".into());
    let started = line("2026-09-28T04:38:25.000Z", ActionKind::Call, "$ make test");
    h.absorb_detail(id.clone(), page(0, 10, vec![started.clone()]));
    h.absorb_detail(
        id,
        page(
            10,
            20,
            vec![
                started,
                line("2026-09-28T04:38:28.000Z", ActionKind::Ended, "✓"),
            ],
        ),
    );
    let p = frame(&h).agents.preview.unwrap();
    let texts: Vec<(&str, LineKind)> = p.stream.iter().map(|l| (l.text.as_str(), l.kind)).collect();
    assert_eq!(
        texts,
        [("$ make test", LineKind::Call), ("✓ 3.0s", LineKind::Done)]
    );
    assert_eq!(p.quiet, None);
}

#[test]
fn the_output_scrolls_back_and_follows_again() {
    let mut h = home();
    h.watch.stream = (0..20)
        .map(|i| ActionLine {
            at: "t".into(),
            kind: ActionKind::Call,
            text: i.to_string(),
            id: None,
        })
        .collect();
    h.key(Key::PageUp);
    assert_eq!(h.watch.scroll, PAGE);
    h.key(Key::PageUp);
    h.key(Key::PageUp);
    assert_eq!(h.watch.scroll, 20, "no further back than the first line");
    h.key(Key::PageDown);
    assert_eq!(h.watch.scroll, 20 - PAGE);
    h.key(Key::End);
    assert_eq!(h.watch.scroll, 0, "back at the end: following");
    h.key(Key::PageUp);
    h.key(Key::Char('j'));
    assert_eq!(h.watch.scroll, 0, "another selection starts at its end");
}

/// An ended agent's preview says how it ended and what it left: its tokens in and out, and its
/// branch with what the branch changes.
#[test]
fn an_ended_agents_preview_shows_its_tokens_and_the_diff_it_landed() {
    use marion_core::proto::result::DiffStat;
    let mut h = home();
    h.set_nodes(vec![node("a", None, NodeState::Exited(ExitStatus::Ok))]);
    h.absorb_detail(
        AgentId("a".into()),
        NodeDetail {
            usage: Some(marion_core::contract::TokenUsage {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: None,
            }),
            completion: Some(CompletionSummary {
                status: ExitStatus::Ok,
                narrative: Some("added the limiter\nand its tests".into()),
                branch: Some("marion/t-1".into()),
                commit: None,
                changed_paths: 3,
                exit: String::new(),
                diff: Some(DiffStat {
                    added: 84,
                    removed: 12,
                    files: 3,
                }),
            }),
            ..Default::default()
        },
    );
    let lines: Vec<String> = facts(&h).into_iter().map(|(t, _)| t).collect();
    assert!(lines[0].ends_with("  10 in  5 out"), "{lines:?}");
    assert_eq!(lines[1], "added the limiter", "the account's first line");
    assert_eq!(lines[2], "marion/t-1  +84 -12  3 files");
}

/// **A node a trusted repository's config was held back from says so in its preview**, dim, and
/// one with nothing held back says nothing about it.
#[test]
fn a_preview_names_the_repository_config_held_back() {
    let mut h = home();
    h.set_nodes(vec![node("a", None, NodeState::Running)]);
    h.absorb_detail(AgentId("a".into()), NodeDetail::default());
    assert!(
        !facts(&h).iter().any(|(t, _)| t.contains("held back")),
        "{:?}",
        facts(&h)
    );
    h.absorb_detail(
        AgentId("a".into()),
        NodeDetail {
            repo_config_held: vec![".mcp.json".into()],
            ..Default::default()
        },
    );
    assert!(
        facts(&h).contains(&(
            "repository config held back: .mcp.json differs from yours".into(),
            vec![Ink::Dim]
        )),
        "{:?}",
        facts(&h)
    );
}

/// **An endpoint node says which model it runs on**: `provider:model` on its row, and with the
/// route in its preview; a node on its harness's own configuration shows neither.
#[test]
fn an_endpoint_node_shows_its_provider_and_model() {
    let mut h = home();
    let mut root = node("root", None, NodeState::Running);
    root.endpoint = Some(marion_core::proto::NodeEndpoint {
        provider: "groq".into(),
        model: Some("llama-3.3-70b".into()),
        route: Some("translated".into()),
    });
    h.set_nodes(vec![root, node("child", Some("root"), NodeState::Running)]);
    h.select("root");
    let f = frame(&h);
    assert_eq!(row(&f, "root").who, "codex groq:llama-3.3-70b");
    assert_eq!(row(&f, "child").who, "codex");
    assert!(
        facts(&h)
            .iter()
            .any(|(t, _)| t == "groq:llama-3.3-70b · translated"),
        "{:?}",
        facts(&h)
    );
}

/// **A parent's preview carries its subtree, against its tree budget where it has one**, warned
/// past the budget's line; a leaf's carries nothing, its subtree being itself.
#[test]
fn a_parents_preview_shows_its_subtree_against_its_budget() {
    let mut h = home();
    let mut root = node("root", None, NodeState::Running);
    root.tokens = Some(1_000);
    root.changed = Some(2);
    root.budget = Some(marion_core::budget::Budget {
        tree_tokens: Some(1_500),
        ..Default::default()
    });
    let mut child = node("child", Some("root"), NodeState::Exited(ExitStatus::Ok));
    child.tokens = Some(400);
    child.changed = Some(3);
    h.set_nodes(vec![root, child]);
    h.select("root");
    let subtree = facts(&h)
        .into_iter()
        .find(|(t, _)| t.contains("agents,"))
        .expect("a subtree line");
    assert!(
        subtree.0.starts_with("1.4k tokens  5 files")
            && subtree.0.contains("2 agents, 1 running")
            && subtree.0.ends_with("budget 1.4k of 1.5k"),
        "{}",
        subtree.0
    );
    assert_eq!(subtree.1, [Ink::Warn], "past its tree budget's line");
    h.select("child");
    assert!(!facts(&h).iter().any(|(t, _)| t.contains("agents,")));
}

// ------------------------------------------------------------------ races and workflows

fn seat(
    id: &str,
    n: u8,
    verdict: Option<marion_core::race::SeatVerdict>,
    tokens: u64,
) -> NodeSummary {
    NodeSummary {
        race: Some(marion_core::proto::model::RaceBadge {
            race_id: marion_core::race::RaceId("019f0000-1a2b-7000-8000-00000000000a".into()),
            seat: n,
            verdict,
        }),
        tokens: Some(tokens),
        ..node(id, Some("root"), NodeState::Exited(ExitStatus::Ok))
    }
}

const RACE: &str = "race:019f0000-1a2b-7000-8000-00000000000a";

/// **A race's seats sit under one header row**, placed where their parent put them. The header
/// sums the seats — their count, how many finished, their tokens — and names the winner once
/// decided; each seat's row says its seat and verdict. The header is no node.
#[test]
fn a_race_is_one_header_row_over_its_seats() {
    use marion_core::race::SeatVerdict;
    let mut h = home();
    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        seat("s1", 1, None, 100),
        NodeSummary {
            state: NodeState::Running,
            ..seat("s2", 2, None, 50)
        },
    ]);
    let f = frame(&h);
    let ids = row_ids(&f);
    assert_eq!(&ids[..2], ["root", RACE]);
    assert_eq!(ids.len(), 4);
    let race = row(&f, RACE);
    assert_eq!(race.who, "race");
    assert!(
        race.title.ends_with("2 agents · 1 of 2 done"),
        "{}",
        race.title
    );
    assert_eq!(race.tokens, Some(150), "the seats' tokens, summed");
    assert!(
        row(&f, "s1").title.ends_with("· agent 1"),
        "{}",
        row(&f, "s1").title
    );

    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        seat("s1", 1, Some(SeatVerdict::Failed), 100),
        seat("s2", 2, Some(SeatVerdict::Won), 50),
    ]);
    let f = frame(&h);
    assert!(
        row(&f, RACE).title.ends_with("won"),
        "{}",
        row(&f, RACE).title
    );
    assert!(row(&f, "s2").title.ends_with("agent 2 ★ won"));
    assert!(row(&f, "s1").title.ends_with("agent 1 failed"));

    h.watch.tree.select(RACE);
    assert!(h.selected().is_none());
    assert!(footer_has(&h, "enter", "winner"));
}

/// **A race row goes to its winner on Enter**; before a winner, or for a key that acts on one
/// agent, it says what does work there.
#[test]
fn a_race_row_goes_to_its_winner() {
    use marion_core::race::SeatVerdict;
    let mut h = home();
    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        seat("s1", 1, None, 100),
        NodeSummary {
            state: NodeState::Running,
            ..seat("s2", 2, None, 50)
        },
    ]);
    h.watch.tree.select(RACE);
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("no winner yet"));
    h.key(Key::Char('x'));
    assert!(h.notice.as_deref().unwrap_or("").starts_with("a race"));
    assert_eq!(h.mode, Mode::Normal, "no cancel for a header");

    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        seat("s1", 1, Some(SeatVerdict::Failed), 100),
        seat("s2", 2, Some(SeatVerdict::Won), 50),
    ]);
    h.watch.tree.select(RACE);
    h.key(Key::Enter);
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some("s2"));
}

const WF: &str = "019f0000-1a2b-7000-8000-00000000000b";

/// A node that ran step `step` (round `round`) of the run `ship`, four steps, 2M budget.
fn step(
    id: &str,
    step: u8,
    round: u8,
    step_id: &str,
    state: NodeState,
    tokens: u64,
) -> NodeSummary {
    NodeSummary {
        workflow: Some(marion_core::proto::model::WorkflowBadge {
            wf_id: marion_core::workflow::WorkflowId(WF.into()),
            step,
            round,
            verdict: None,
            closed: None,
            name: "ship".into(),
            steps: 4,
            step_id: step_id.into(),
            budget_tokens: Some(2_000_000),
        }),
        tokens: Some(tokens),
        ..node(id, None, state)
    }
}

/// **A workflow run is one header row over its steps**: the header says the run and the step it
/// is on of how many (and the review round), its tokens and its clock in the row's columns; each
/// step node says its step. Enter on the header goes to the step running now.
#[test]
fn a_workflow_is_one_header_row_over_its_steps() {
    let twelve_minutes_ago = marion_core::encoding::SystemTime(
        std::time::SystemTime::now() - std::time::Duration::from_secs(12 * 60 + 5),
    );
    let mut h = home();
    h.set_nodes(vec![
        NodeSummary {
            started_at: Some(twelve_minutes_ago),
            ..step(
                "plan",
                0,
                0,
                "plan",
                NodeState::Exited(ExitStatus::Ok),
                400_000,
            )
        },
        step("gate1", 2, 0, "gate", NodeState::Exited(ExitStatus::Ok), 0),
        step("gate2", 2, 1, "gate", NodeState::Running, 1_000_000),
    ]);
    let f = frame(&h);
    let header = format!("workflow:{WF}");
    assert_eq!(row_ids(&f)[0], header);
    assert_eq!(row_ids(&f).len(), 4);
    let head = row(&f, &header);
    assert_eq!(head.who, "workflow");
    assert_eq!(head.title, "workflow ship · 3/4 gate r2");
    assert_eq!((head.time.as_str(), head.tokens), ("12m", Some(1_400_000)));
    assert!(row(&f, "plan").title.ends_with("step plan"));
    assert!(row(&f, "gate2").title.ends_with("step gate r2"));
    h.watch.tree.select(&header);
    assert!(
        facts(&h).iter().any(|(t, _)| t == "budget 1.4M of 2M"),
        "{:?}",
        facts(&h)
    );

    assert!(h.selected().is_none(), "the header is no node");
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(
        h.selected().map(|n| n.agent_id.0.as_str()),
        Some("gate2"),
        "Enter on the header goes to the step running now"
    );
}

// ------------------------------------------------------------------ effects and their words

#[test]
fn every_effect_has_its_command_and_only_cancel_and_kill_ask_first() {
    let id = AgentId("0199-abc".into());
    let all = [
        Effect::Run {
            agent_type: "codex".into(),
            model: Some("o3".into()),
            options: vec![],
            prompt: "p q".into(),
            pane: true,
        },
        Effect::Attach(id.clone()),
        Effect::Command(vec!["merge".into(), id.0.clone()]),
        Effect::Cancel(id.clone()),
        Effect::Kill(id.clone()),
        Effect::Recheck,
        Effect::EditTypes,
    ];
    for e in &all {
        assert!(e.argv().is_some_and(|a| !a.is_empty()), "{e:?}");
        assert_eq!(
            e.destructive(),
            matches!(e, Effect::Cancel(_) | Effect::Kill(_)),
            "{e:?}"
        );
    }
    assert_eq!(Effect::None.argv(), None);
    assert_eq!(Effect::Quit.argv(), None);
}

/// **A failed run's notice names the cause**: marion's own `marion <verb>: …` line, not the usage
/// pointer printed after it; with no such line, the last thing printed.
#[test]
fn a_failed_runs_notice_is_its_marion_verb_line() {
    let stderr = "marion run: no agent type named `codx`; `marion run --help` lists them\n\
                  \n\
                  `marion run --help` shows how to use it\n";
    assert_eq!(
        failure_line("", stderr),
        "marion run: no agent type named `codx`; `marion run --help` lists them"
    );
    assert_eq!(
        failure_line("", "marion: unknown agent type \"codx\"; known: claude\n"),
        "marion: unknown agent type \"codx\"; known: claude"
    );
    assert_eq!(failure_line("", "boom\nlast words\n"), "last words");
    assert_eq!(failure_line("", ""), "");
    assert_eq!(
        failure_line("marion is starting\n", "oops\n"),
        "oops",
        "prose that begins with marion is not a refusal"
    );
}

#[test]
fn the_project_reads_as_home_relative_or_its_name_and_branch() {
    use view::project_label;
    let home = Some("/Users/r");
    assert_eq!(
        project_label("/Users/r/code/acme", home, Some("main")),
        "~/code/acme · main"
    );
    assert_eq!(
        project_label("/Users/r/code/acme", home, None),
        "~/code/acme"
    );
    // Outside $HOME an absolute path says nothing the name does not: the name.
    assert_eq!(
        project_label("/private/tmp/mn-501/run-7/repo", home, Some("t-1")),
        "repo · t-1"
    );
    // A sibling of $HOME that merely shares its prefix is not under it.
    assert_eq!(project_label("/Users/rx/acme", home, None), "acme");
    assert_eq!(project_label("/Users/r", home, None), "~");
    // A detached head names no branch worth showing.
    assert_eq!(project_label("/Users/r/a", home, Some("HEAD")), "~/a");
}

#[test]
fn clocks_are_shown_in_the_operators_local_time() {
    use view::clock_in;
    let at = "2026-09-27T21:02:27.123Z";
    assert_eq!(clock_in(at, 0, true), "21:02:27");
    assert_eq!(clock_in(at, 2 * 3600, true), "23:02:27");
    assert_eq!(
        clock_in(at, 5 * 3600 + 1800, false),
        "02:32",
        "across midnight"
    );
    assert_eq!(clock_in(at, -9 * 3600, false), "12:02");
    assert_eq!(clock_in("not a time", 3600, true), "not a time");
}

// ------------------------------------------------------------------ Setup: logins and the form

fn every_key() -> Vec<Key> {
    [
        Key::Enter,
        Key::Esc,
        Key::Tab,
        Key::BackTab,
        Key::Backspace,
        Key::Delete,
        Key::Up,
        Key::Down,
        Key::Left,
        Key::Right,
        Key::PageUp,
        Key::PageDown,
        Key::End,
        Key::Paste("p".into()),
    ]
    .into_iter()
    .chain((b'!'..=b'~').map(|b| Key::Char(b as char)))
    .collect()
}

/// Setup, reached the way the operator does: `,` from the agents.
fn setup_home() -> Home {
    let mut h = home();
    h.key(Key::Char(','));
    assert_eq!(h.page, Page::Setup);
    h.setup.providers = vec!["anthropic".into(), "openrouter".into()];
    h.setup.logins = vec![
        StoredLogin {
            provider: "anthropic".into(),
            id: "anthropic".into(),
            note: None,
        },
        StoredLogin {
            provider: "openrouter".into(),
            id: "openrouter:work".into(),
            note: None,
        },
    ];
    h
}

fn typing(purpose: Purpose, text: &str) -> Mode {
    Mode::Typing {
        purpose,
        text: text.into(),
    }
}

/// Two of [`setup_home`]'s rows: the second harness, and the second key.
const COPILOT: usize = 1;
const OPENROUTER: usize = 3;

/// The row `item` is on, by moving the cursor there the way the operator does.
fn select(h: &mut Home, item: SetupItem) {
    h.setup.cursor = 0;
    for _ in 0..h.setup_items().len() {
        if h.selected_item() == Some(item) {
            return;
        }
        h.key(Key::Char('j'));
    }
    panic!("{item:?} is not a row: {:?}", h.setup_items());
}

/// **Setup is one list in one order**: each harness with its logins under it, a login whose
/// harness doctor does not list, the keys, the agents file, and notifications once read. The
/// cursor walks it with j/k and stops at both ends.
#[test]
fn setup_rows_run_harnesses_logins_keys_files_notify() {
    let mut h = profiles_home();
    h.setup.notify = Some(false);
    assert_eq!(
        h.setup_items(),
        [
            SetupItem::Harness(0),
            SetupItem::Profile(0),
            SetupItem::Harness(1),
            SetupItem::Profile(1),
            SetupItem::Key(0),
            SetupItem::Key(1),
            SetupItem::Types,
            SetupItem::Notify,
        ],
        "claude's login under claude; codex is not listed, so its login follows the harnesses"
    );
    for _ in 0..20 {
        h.key(Key::Char('j'));
    }
    assert_eq!(
        h.selected_item(),
        Some(SetupItem::Notify),
        "stops at the end"
    );
    for _ in 0..20 {
        h.key(Key::Char('k'));
    }
    assert_eq!(
        h.selected_item(),
        Some(SetupItem::Harness(0)),
        "and the top"
    );
    h.setup.notify = None;
    assert!(
        !h.setup_items().contains(&SetupItem::Notify),
        "no row until read"
    );
    h.setup.logins.clear();
    assert!(
        h.setup_items().contains(&SetupItem::NoKeys),
        "with no key stored, one row adds the first"
    );
}

/// A harness that is not installed is folded into one line and is no row — unless a login of its
/// own still needs a home.
#[test]
fn an_absent_harness_is_no_row_unless_it_has_a_login() {
    let mut h = setup_home();
    h.harnesses.push(harness("goose", Ready::Absent));
    assert!(!h.setup_items().contains(&SetupItem::Harness(2)));
    let f = frame(&h);
    assert!(
        f.setup
            .lines
            .contains(&SetupLine::Absent(vec!["goose".into()])),
        "{:?}",
        f.setup.lines
    );
    h.setup.profiles = vec![profile("goose", "g", false)];
    assert_eq!(
        h.setup_items()[..4],
        [
            SetupItem::Harness(0),
            SetupItem::Harness(1),
            SetupItem::Harness(2),
            SetupItem::Profile(0)
        ]
    );
}

/// The view's groups are headed once each, in order, and the cursor's line is the selected row's.
#[test]
fn setup_lines_head_each_group_once_and_point_at_the_selected_row() {
    let mut h = profiles_home();
    h.setup.notify = Some(true);
    select(&mut h, SetupItem::Key(1));
    let v = frame(&h).setup;
    let groups: Vec<&str> = v
        .lines
        .iter()
        .filter_map(|l| match l {
            SetupLine::Group(g) => Some(g.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(groups, ["harnesses", "api keys", "files", "notify"]);
    assert!(
        matches!(&v.lines[v.cursor.unwrap()], SetupLine::Key(k) if k.id == "openrouter:work"),
        "{:?}",
        v.lines
    );
    let orphan = v.lines.iter().find_map(|l| match l {
        SetupLine::Profile(p) if p.name.contains("cx") => Some(p.name.clone()),
        _ => None,
    });
    assert_eq!(
        orphan.as_deref(),
        Some("codex cx"),
        "a login away from its harness names it"
    );
}

/// **"N of M ready" is said once**: on the status line, and not again on the harnesses' header.
#[test]
fn setup_says_how_many_are_ready_once() {
    let h = setup_home();
    let f = frame(&h);
    assert_eq!(f.title, "setup");
    assert_eq!(f.counts, "1 of 2 ready");
    assert!(
        f.setup
            .lines
            .contains(&SetupLine::Group("harnesses".into())),
        "the header is the bare group name"
    );
}

/// **Setup's footer is the keys the selected row takes**, with re-check and back on every row.
#[test]
fn setup_footer_follows_the_row_under_the_cursor() {
    let mut h = profiles_home();
    h.setup.notify = Some(false);
    let cases: [(SetupItem, &[&str]); 7] = [
        (SetupItem::Harness(0), &["enter", "a", "r", "esc"]),
        (SetupItem::Harness(1), &["enter", "r", "esc"]),
        (SetupItem::Profile(0), &["a", "del", "r", "esc"]),
        (SetupItem::Profile(1), &["enter", "a", "del", "r", "esc"]),
        (SetupItem::Key(0), &["a", "del", "r", "esc"]),
        (SetupItem::Types, &["enter", "r", "esc"]),
        (SetupItem::Notify, &["enter", "r", "esc"]),
    ];
    for (item, keys) in cases {
        select(&mut h, item);
        assert_eq!(footer_keys(&h), keys, "{item:?}");
    }
    assert!(footer_has(&h, "enter", "turn on"));
    h.setup.notify = Some(true);
    assert!(footer_has(&h, "enter", "turn off"));
    h.setup.logins.clear();
    select(&mut h, SetupItem::NoKeys);
    assert_eq!(footer_keys(&h), ["a", "r", "esc"]);
}

/// Enter does the row's one thing: a harness's details, the login made the default, the agents
/// file in `$EDITOR`, notifications flipped. A key has nothing to open.
#[test]
fn enter_does_the_selected_rows_one_thing() {
    let mut h = profiles_home();
    h.setup.notify = Some(false);
    select(&mut h, SetupItem::Harness(0));
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert!(h.setup.expanded, "a harness shows its details");
    h.key(Key::Char('j'));
    assert!(!h.setup.expanded, "moving off closes them");
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("claude already runs on work"));
    select(&mut h, SetupItem::Profile(1));
    assert_eq!(
        h.key(Key::Enter),
        Effect::ProfileUse {
            harness: "codex".into(),
            name: "cx".into()
        }
    );
    select(&mut h, SetupItem::Key(0));
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert!(!h.setup.expanded, "a key has no details");
    select(&mut h, SetupItem::Types);
    assert_eq!(h.key(Key::Enter), Effect::EditTypes);
    select(&mut h, SetupItem::Notify);
    assert_eq!(h.key(Key::Enter), Effect::Notify(true));
    assert_eq!(
        Effect::Notify(true).argv().unwrap(),
        ["marion", "notify", "on"]
    );
}

/// **Setup's repository row answers `marion trust … --repo`**: last, under its own group; Enter on
/// an unanswered repository asks for a `y` before `allow --repo`, and `n` keeps it untrusted;
/// Enter on a trusted one runs `deny --repo` at once.
#[test]
fn setup_repository_row_trusts_after_a_y_and_stops_trusting_at_once() {
    let mut h = profiles_home();
    h.setup.notify = Some(true);
    h.setup.repo = Some(crate::home::RepoTrust {
        answer: None,
        files: vec![".mcp.json".into(), ".codex/config.toml".into()],
    });
    assert_eq!(h.setup_items().last(), Some(&SetupItem::Repo));
    for _ in 0..20 {
        h.key(Key::Char('j'));
    }
    assert_eq!(
        h.selected_item(),
        Some(SetupItem::Repo),
        "the cursor stops on the last row"
    );
    let v = frame(&h).setup;
    assert!(
        matches!(
            &v.lines[v.cursor.unwrap()],
            SetupLine::Repo { answer: None, files } if files == ".mcp.json, .codex/config.toml"
        ),
        "{:?}",
        v.lines
    );
    assert!(v.lines.contains(&SetupLine::Group("repository".into())));
    assert_eq!(footer_keys(&h), ["enter", "r", "esc"]);
    assert!(footer_has(&h, "enter", "trust"));
    let e = h.key(Key::Enter);
    assert!(!e.destructive(), "Enter alone trusts nothing: {e:?}");
    assert_eq!(h.mode, Mode::Confirm(Effect::RepoTrust(true)));
    let Footer::Confirm { question, command } = frame(&h).footer else {
        panic!("a confirm asks before trusting");
    };
    assert!(
        question.contains(".mcp.json, .codex/config.toml"),
        "the question names what would load: {question}"
    );
    assert_eq!(command, "marion trust allow --repo");
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("not trusted"));
    h.key(Key::Enter);
    assert_eq!(h.key(Key::Char('y')), Effect::RepoTrust(true));
    h.setup.repo.as_mut().unwrap().answer = Some(true);
    assert!(footer_has(&h, "enter", "stop trusting"));
    assert_eq!(h.key(Key::Enter), Effect::RepoTrust(false));
    assert_eq!(
        Effect::RepoTrust(false).argv().unwrap(),
        ["marion", "trust", "deny", "--repo"]
    );
    h.setup.repo = None;
    assert!(
        !h.setup_items().contains(&SetupItem::Repo),
        "no row where there is nothing to show"
    );
}

/// **An unanswered repository needs the operator**: the agents page's status line says so and
/// where it is answered, with no key of its own. A declined one was answered and says nothing.
#[test]
fn an_unanswered_repository_is_named_on_the_status_line() {
    let mut h = home();
    h.setup.repo = Some(crate::home::RepoTrust {
        answer: None,
        files: vec![".mcp.json".into(), ".claude/settings.json".into()],
    });
    assert!(h.repo_needs_you());
    let counts = frame(&h).counts;
    assert!(counts.ends_with("  repository trust in ,"), "{counts}");
    h.setup.repo.as_mut().unwrap().answer = Some(false);
    assert!(!h.repo_needs_you());
    let counts = frame(&h).counts;
    assert!(!counts.contains("repository"), "{counts}");
}

/// **`del` removes, after a `y`**: a key by its credential id, a login by harness and name. Both
/// the Mac's delete (Backspace) and forward delete are `del`; on any other row it does nothing.
#[test]
fn del_removes_a_key_or_a_login_after_y() {
    let mut h = profiles_home();
    select(&mut h, SetupItem::Key(1));
    assert_eq!(h.key(Key::Backspace), Effect::None);
    assert_eq!(
        h.mode,
        Mode::Confirm(Effect::Logout("openrouter:work".into()))
    );
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(h.notice.as_deref(), Some("key kept"));
    h.key(Key::Delete);
    assert_eq!(
        h.key(Key::Char('y')),
        Effect::Logout("openrouter:work".into())
    );
    select(&mut h, SetupItem::Profile(0));
    h.key(Key::Delete);
    let remove = Effect::ProfileRemove {
        harness: "claude".into(),
        name: "work".into(),
    };
    assert_eq!(h.mode, Mode::Confirm(remove.clone()));
    let Footer::Confirm { question, .. } = frame(&h).footer else {
        panic!("a confirm line")
    };
    assert_eq!(question, "remove the login claude work?");
    assert_eq!(h.key(Key::Char('x')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("login kept"));
    h.key(Key::Backspace);
    assert_eq!(h.key(Key::Char('y')), remove);
    for item in [SetupItem::Harness(0), SetupItem::Types] {
        select(&mut h, item);
        assert_eq!(h.key(Key::Delete), Effect::None);
        assert_eq!(h.mode, Mode::Normal, "{item:?} has nothing to remove");
    }
}

#[test]
fn adding_a_key_asks_for_the_provider_and_hands_off_to_marion_key_add() {
    let mut h = setup_home();
    h.setup.logins.clear();
    select(&mut h, SetupItem::NoKeys);
    assert_eq!(h.key(Key::Char('a')), Effect::None);
    assert_eq!(h.mode, typing(Purpose::Key, ""));
    typed(&mut h, "openrouter:personal");
    assert_eq!(
        h.key(Key::Enter),
        Effect::Login("openrouter:personal".into())
    );
    // A provider marion does not know is refused here, before the terminal changes hands, and
    // the line stays open with what was typed.
    h.key(Key::Char('a'));
    typed(&mut h, "nope");
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert!(
        h.notice
            .as_deref()
            .unwrap_or("")
            .contains("marion key list")
    );
    assert_eq!(h.mode, typing(Purpose::Key, "nope"));
    h.key(Key::Esc);
    // On a key's row, the line starts with that key's provider.
    let mut h = setup_home();
    h.setup.cursor = OPENROUTER;
    h.key(Key::Char('a'));
    assert_eq!(h.mode, typing(Purpose::Key, "openrouter"));
    assert_eq!(h.key(Key::Esc), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
}

#[test]
fn key_add_and_rm_echo_the_commands_they_run_and_only_rm_asks_first() {
    assert_eq!(
        Effect::Login("openrouter:work".into()).argv().unwrap(),
        ["marion", "key", "add", "openrouter:work"]
    );
    assert_eq!(
        Effect::Logout("openrouter:work".into()).argv().unwrap(),
        ["marion", "key", "rm", "openrouter:work"]
    );
    assert!(!Effect::Login("x".into()).destructive());
    assert!(Effect::Logout("x".into()).destructive());
}

fn profile(harness: &str, name: &str, default: bool) -> StoredProfile {
    StoredProfile {
        harness: harness.into(),
        name: name.into(),
        default,
        login: None,
        login_command: format!("CODEX_HOME='/p/{name}' {harness} login"),
        limit: None,
    }
}

/// Two harnesses with a login under claude, a codex login whose harness is not listed, two keys.
fn profiles_home() -> Home {
    let mut h = setup_home();
    h.setup.profile_harnesses = vec!["claude".into(), "codex".into()];
    h.setup.profiles = vec![
        profile("claude", "work", true),
        profile("codex", "cx", false),
    ];
    h.setup.profiles_listed = true;
    h
}

/// **`a` on a harness or its login asks only for a name**: the harness is the row's, so the line
/// is `profile add <harness> ›` and Enter hands off to `marion profile add`.
#[test]
fn a_on_a_harness_or_its_login_asks_for_the_new_logins_name() {
    let mut h = profiles_home();
    select(&mut h, SetupItem::Profile(1));
    assert_eq!(h.key(Key::Char('a')), Effect::None);
    assert_eq!(h.mode, typing(Purpose::Profile("codex".into()), ""));
    typed(&mut h, "personal");
    assert_eq!(
        h.key(Key::Enter),
        Effect::ProfileAdd {
            harness: "codex".into(),
            name: "personal".into()
        }
    );
    select(&mut h, SetupItem::Harness(0));
    h.key(Key::Char('a'));
    assert_eq!(h.mode, typing(Purpose::Profile("claude".into()), ""));
    let Footer::Input { target, .. } = frame(&h).footer else {
        panic!("the input line")
    };
    assert_eq!(target, "profile add claude");
    // Refused here, before the terminal changes hands: a name that is not one, a name taken, an
    // empty line. Each keeps the line open.
    for (text, says) in [
        ("../x", "not a login name"),
        ("work", "already exists"),
        ("  ", "type a name"),
    ] {
        h.mode = typing(Purpose::Profile("claude".into()), text);
        assert_eq!(h.key(Key::Enter), Effect::None, "{text}");
        assert!(
            h.notice.as_deref().unwrap_or("").contains(says),
            "{text}: {:?}",
            h.notice
        );
        assert_eq!(h.mode, typing(Purpose::Profile("claude".into()), text));
    }
    h.key(Key::Esc);
    // A harness that takes no logins of marion's says so and opens nothing.
    select(&mut h, SetupItem::Harness(COPILOT));
    assert_eq!(h.key(Key::Char('a')), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(
        h.notice.as_deref(),
        Some("copilot takes no logins of marion's")
    );
}

#[test]
fn no_single_key_on_any_setup_row_is_destructive() {
    let mut rows = profiles_home();
    rows.setup.notify = Some(true);
    for item in rows.setup_items() {
        for k in every_key() {
            let mut h = profiles_home();
            h.setup.notify = Some(true);
            select(&mut h, item);
            let e = h.key(k.clone());
            assert!(!e.destructive(), "{k:?} on {item:?} returned {e:?}");
        }
    }
}

#[test]
fn profile_verbs_echo_the_commands_they_run_and_only_remove_asks_first() {
    let add = Effect::ProfileAdd {
        harness: "claude".into(),
        name: "work".into(),
    };
    let use_ = Effect::ProfileUse {
        harness: "claude".into(),
        name: "work".into(),
    };
    let remove = Effect::ProfileRemove {
        harness: "claude".into(),
        name: "work".into(),
    };
    assert_eq!(
        add.argv().unwrap(),
        ["marion", "profile", "add", "claude", "work"]
    );
    assert_eq!(
        use_.argv().unwrap(),
        ["marion", "profile", "use", "claude", "work"]
    );
    assert_eq!(
        remove.argv().unwrap(),
        ["marion", "profile", "rm", "claude", "work"]
    );
    assert!(!add.destructive() && !use_.destructive());
    assert!(remove.destructive());
}

/// The echoed commands are the verbs `marion profile` runs: each one, handed to the verb's own
/// dispatcher over a scratch profile store, does what the screen says — and `add` only **prints**
/// the login command, which nothing runs.
#[test]
fn the_profile_echoes_run_through_marion_profiles_own_verbs() {
    let s = marion_testsupport::scratch("home-profile-echo");
    let paths = crate::profiles::ProfilePaths {
        config: s.join("config/marion/profiles.toml"),
        data: s.join("data/marion/profiles"),
        state: s.join("state/profiles"),
    };
    let run = |e: &Effect| {
        let argv = e.argv().unwrap();
        assert_eq!(argv[..2], ["marion", "profile"]);
        let mut out = Vec::new();
        crate::profile_cli::run(&argv[2..], &paths, None, &mut out).expect("the verb accepts it");
        String::from_utf8(out).unwrap()
    };
    let said = run(&Effect::ProfileAdd {
        harness: "claude".into(),
        name: "work".into(),
    });
    assert!(said.contains("marion never runs this"), "{said}");
    assert!(said.contains("claude auth login"), "{said}");
    run(&Effect::ProfileUse {
        harness: "claude".into(),
        name: "work".into(),
    });
    let file = crate::profiles::ProfilesFile::load(&paths.config).unwrap();
    assert_eq!(file.default["claude-code"], "work");
    run(&Effect::ProfileRemove {
        harness: "claude".into(),
        name: "work".into(),
    });
    let file = crate::profiles::ProfilesFile::load(&paths.config).unwrap();
    assert!(file.profile.is_empty() && file.default.is_empty());
    assert!(
        paths.dir_for(H::ClaudeCode, "work").is_dir(),
        "remove keeps the directory; only `--purge` deletes it"
    );
}

#[test]
fn setup_shows_long_paths_by_their_ends() {
    let short = view::short_place(
        "file /private/tmp/mn-501/marion-home-e2e-run-83954-t4/config/marion/keys.json",
    );
    assert!(short.starts_with("file /private/"), "{short}");
    assert!(short.ends_with("marion/keys.json"), "{short}");
    assert!(short.chars().count() <= "file ".len() + 36, "{short}");
    let keychain = "macOS Keychain (service \"marion\")";
    assert_eq!(view::short_place(keychain), keychain);
}

/// A login's row says what its probe said, and shows the login command only while it is logged
/// out or the probe could not tell — the command for the operator to run, never marion.
#[test]
fn a_logged_out_login_shows_its_login_command_and_a_logged_in_one_does_not() {
    use crate::profiles::LoginState;
    let mut h = profiles_home();
    h.setup.profiles[0].login = Some(LoginState::LoggedIn);
    h.setup.profiles[1].login = Some(LoginState::LoggedOut);
    let rows = |h: &Home| -> Vec<ProfileRow> {
        frame(h)
            .setup
            .lines
            .into_iter()
            .filter_map(|l| match l {
                SetupLine::Profile(p) => Some(p),
                _ => None,
            })
            .collect()
    };
    let r = rows(&h);
    assert_eq!(
        (r[0].note.as_str(), r[0].login.as_deref()),
        ("logged in", None)
    );
    assert_eq!(r[1].note, "logged out");
    assert_eq!(
        r[1].login.as_deref(),
        Some("CODEX_HOME='/p/cx' codex login")
    );
    assert!(r[0].default && !r[1].default);
    h.setup.profiles[1].login = Some(LoginState::Unknown("timed out".into()));
    assert!(
        rows(&h)[1].login.is_some(),
        "an unknown login offers the command too"
    );
    h.setup.profiles[1].login = None;
    assert_eq!(rows(&h)[1].note, "checking…");
    assert!(h.animating(), "a probe still out keeps the spinner turning");
}

#[test]
fn a_new_listing_selects_the_login_just_added_and_keeps_the_cursor_on_a_real_row() {
    let mut h = profiles_home();
    let harnesses = h.setup.profile_harnesses.clone();
    let mut rows = h.setup.profiles.clone();
    rows.insert(0, profile("claude", "personal", false));
    h.set_profiles(rows.clone(), harnesses.clone(), Some("personal"));
    assert_eq!(
        h.selected_item(),
        Some(SetupItem::Profile(0)),
        "the login just added is selected, so its login command shows"
    );
    // A re-listing that numbers the logins differently keeps the cursor on the same login.
    select(&mut h, SetupItem::Profile(2));
    rows.swap(0, 2);
    h.set_profiles(rows, harnesses.clone(), None);
    assert_eq!(h.selected_item(), Some(SetupItem::Profile(0)));
    assert_eq!(h.setup.profiles[0].name, "cx");
    // Every login removed: the cursor stays on a row that exists.
    h.set_profiles(Vec::new(), harnesses, None);
    assert!(h.selected_item().is_some());
    assert!(h.setup.cursor < h.setup_items().len());
}
