//! The home screen's decisions, without a terminal: which key does what on which page, and that
//! nothing destructive happens without a `y`.

use super::*;
use marion_core::contract::{AgentId, ExitStatus};
use marion_core::encoding::Duration;
use marion_core::harness::Harness as H;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::result::{ActionKind, ActionLine, ActivityPage, CompletionSummary};
use marion_tui::home::{Footer, Ink};

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
        prompt: "x".into(),
        pane: false,
    };
    assert_eq!(
        headless.argv().unwrap(),
        ["marion", "run", "codex", "--prompt", "x", "--detach"]
    );
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
    assert_eq!(titles(&h), ["agents", "input line", "setup"]);
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
    for k in ['n', 's'] {
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

/// **The footer offers only what the selection takes**: a running agent steers and cancels, an
/// ended one resumes and opens its shell, merge and diff only once a branch has landed, and an
/// empty list offers a new task and Setup.
#[test]
fn the_footer_offers_only_what_the_selection_takes() {
    let mut h = home();
    let running = footer_keys(&h);
    assert!(running.contains(&"x".to_string()), "{running:?}");
    assert!(!running.contains(&"u".to_string()), "{running:?}");
    assert!(!running.contains(&"m".to_string()), "{running:?}");
    assert_eq!(
        running.last().map(String::as_str),
        Some("?"),
        "`? more` last"
    );
    h.key(Key::Char('j'));
    let ended = footer_keys(&h);
    assert!(ended.contains(&"o".to_string()), "{ended:?}");
    assert!(!ended.contains(&"x".to_string()), "{ended:?}");
    assert!(
        !ended.contains(&"m".to_string()),
        "nothing landed yet: {ended:?}"
    );
    h.absorb_detail(AgentId("child".into()), landed("marion/t-1"));
    let landed = footer_keys(&h);
    assert!(
        landed.contains(&"m".to_string()) && landed.contains(&"d".to_string()),
        "{landed:?}"
    );
    h.lost_supervisor();
    assert_eq!(footer_keys(&h), ["n", ",", "?"]);
    h.set_nodes(vec![]);
    assert_eq!(footer_keys(&h), ["n", ",", "?"]);
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

/// **The order holds while a line is open**: a row must not move under a steer being typed for
/// it. A node that arrives meanwhile goes to the end, and the order comes back once the line
/// closes.
#[test]
fn set_nodes_holds_the_order_while_a_line_is_open() {
    use marion_core::node::BlockReason;
    let mut h = home();
    h.set_nodes(vec![node("a", None, NodeState::Running)]);
    h.key(Key::Char('s'));
    let asks = node("b", None, NodeState::Blocked(BlockReason::Permission));
    h.set_nodes(vec![asks.clone(), node("a", None, NodeState::Running)]);
    assert_eq!(row_ids(&frame(&h)), ["a", "b"], "held while typing");
    assert_eq!(
        h.selected().unwrap().agent_id.0,
        "a",
        "the steer's row stays put"
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

/// **A merge from here takes the node out of what is left to land**: its rank and the count drop.
#[test]
fn a_merged_branch_has_nothing_left_to_land() {
    let mut h = home();
    let mut root = node("root", None, NodeState::Exited(ExitStatus::Ok));
    root.changed = Some(9);
    let mut child = node("child", Some("root"), NodeState::Exited(ExitStatus::Ok));
    child.changed = Some(2);
    h.set_nodes(vec![root, child]);
    assert_eq!(frame(&h).counts, "1 to land", "a root has nothing to land");
    h.select("child");
    h.absorb_detail(AgentId("child".into()), landed("marion/t-1"));
    h.key(Key::Char('m'));
    assert_eq!(h.key(Key::Char('y')), Effect::Merge("marion/t-1".into()));
    h.merged("marion/t-1");
    assert_eq!(frame(&h).counts, "");
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

#[test]
fn steer_types_on_the_line_and_enter_sends_it_to_the_node_it_began_on() {
    let mut h = home();
    h.key(Key::Char('s'));
    let Footer::Input { target, .. } = frame(&h).footer else {
        panic!("the input line")
    };
    assert_eq!(target, "steer codex root");
    typed(&mut h, "use a deque");
    h.key(Key::Paste(" please".into()));
    h.key(Key::Char('j'));
    assert!(
        matches!(&h.mode, Mode::Typing { text, .. } if text == "use a deque pleasej"),
        "j is text on the line: {:?}",
        h.mode
    );
    h.key(Key::Backspace);
    assert_eq!(
        h.key(Key::Enter),
        Effect::Steer(AgentId("root".into()), "use a deque please".into())
    );
    // Esc drops it; a blank steer sends nothing.
    h.key(Key::Char('s'));
    typed(&mut h, "never mind");
    assert_eq!(h.key(Key::Esc), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    h.key(Key::Char('s'));
    assert_eq!(h.key(Key::Enter), Effect::None);
}

#[test]
fn ended_nodes_resume_and_running_ones_steer() {
    let mut h = home();
    assert_eq!(h.key(Key::Char('u')), Effect::None, "root is running");
    assert_eq!(h.notice.as_deref(), Some("still running: s steers it"));
    h.key(Key::Char('j'));
    assert_eq!(
        h.key(Key::Char('u')),
        Effect::Resume(AgentId("child".into()))
    );
    assert_eq!(h.key(Key::Char('s')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("ended: u resumes it"));
    assert_eq!(
        h.key(Key::Char('x')),
        Effect::None,
        "an ended node is not cancelled"
    );
    assert_eq!(h.mode, Mode::Normal);
}

#[test]
fn merge_and_diff_need_a_landed_branch() {
    let mut h = home();
    h.key(Key::Char('j'));
    assert_eq!(h.key(Key::Char('d')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("no branch landed"));
    assert_eq!(h.key(Key::Char('m')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("no branch to merge"));
    assert_eq!(h.mode, Mode::Normal);
    h.absorb_detail(AgentId("child".into()), landed("marion/t-1"));
    assert_eq!(h.key(Key::Char('d')), Effect::Diff("marion/t-1".into()));
}

/// **`m` merges an agent's landed branch, and only after a `y` that names the branch and the
/// command.**
#[test]
fn m_merges_a_landed_branch_after_a_y() {
    let mut h = home();
    let id = h.selected().unwrap().agent_id.clone();
    h.absorb_detail(id, landed("marion/t1"));
    assert_eq!(
        h.key(Key::Char('m')),
        Effect::None,
        "no merge on the first key"
    );
    let Footer::Confirm { question, command } = frame(&h).footer else {
        panic!("a confirm");
    };
    assert_eq!(question, "merge marion/t1 into your checkout?");
    assert_eq!(command, "git merge --no-ff -- marion/t1");
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("not merged"));
    h.key(Key::Char('m'));
    assert_eq!(h.key(Key::Char('y')), Effect::Merge("marion/t1".into()));
}

#[test]
fn o_opens_a_shell_in_the_selected_agents_workspace() {
    use marion_core::contract::Workspace;
    let mut h = home();
    assert_eq!(h.key(Key::Char('o')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("no workspace known yet"));
    h.absorb_detail(
        AgentId("root".into()),
        NodeDetail {
            workspace: Some(Workspace::Worktree {
                path: "/w/agents/01a0/worktree".into(),
                branch: "marion/t-3c33".into(),
            }),
            ..Default::default()
        },
    );
    assert_eq!(
        h.key(Key::Char('o')),
        Effect::Shell("/w/agents/01a0/worktree".into())
    );
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

/// **A race row acts on its winner**: Enter goes to it, `m` goes to it and, once its detail says
/// which branch it landed, asks to merge that branch; before a winner, or for keys that act on one
/// agent, it says what does work there.
#[test]
fn a_race_row_goes_to_its_winner_and_merges_it() {
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
    h.key(Key::Char('s'));
    assert!(h.notice.as_deref().unwrap_or("").starts_with("a race"));
    assert_eq!(h.mode, Mode::Normal, "no steer for a header");

    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        seat("s1", 1, Some(SeatVerdict::Failed), 100),
        seat("s2", 2, Some(SeatVerdict::Won), 50),
    ]);
    h.watch.tree.select(RACE);
    h.key(Key::Enter);
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some("s2"));
    h.watch.tree.select(RACE);
    assert_eq!(h.key(Key::Char('m')), Effect::None);
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some("s2"));
    h.absorb_detail(AgentId("s2".into()), landed("marion/s2"));
    assert_eq!(h.mode, Mode::Confirm(Effect::Merge("marion/s2".into())));
    assert_eq!(h.key(Key::Char('y')), Effect::Merge("marion/s2".into()));
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
fn every_effect_has_its_command_and_only_cancel_kill_and_merge_ask_first() {
    let id = AgentId("0199-abc".into());
    let all = [
        Effect::Run {
            agent_type: "codex".into(),
            model: Some("o3".into()),
            prompt: "p q".into(),
            pane: true,
        },
        Effect::Attach(id.clone()),
        Effect::Steer(id.clone(), "-x".into()),
        Effect::Cancel(id.clone()),
        Effect::Kill(id.clone()),
        Effect::Resume(id.clone()),
        Effect::Shell("/w".into()),
        Effect::Diff("b".into()),
        Effect::Merge("marion/t1".into()),
        Effect::Recheck,
        Effect::EditTypes,
    ];
    for e in &all {
        assert!(e.argv().is_some_and(|a| !a.is_empty()), "{e:?}");
        assert_eq!(
            e.destructive(),
            matches!(e, Effect::Cancel(_) | Effect::Kill(_) | Effect::Merge(_)),
            "{e:?}"
        );
    }
    assert_eq!(
        Effect::Merge("marion/t1".into()).argv(),
        Some(
            ["git", "merge", "--no-ff", "--", "marion/t1"]
                .map(String::from)
                .to_vec()
        )
    );
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

/// **Setup's footer is the keys of the section under the cursor**: a harness offers details, the
/// re-check and the agent-type keys; a key row offers add and remove. Enter expands only a
/// harness.
#[test]
fn setup_footer_follows_the_section_under_the_cursor() {
    let mut h = setup_home();
    h.setup.cursor = 0;
    let on_harness = footer_keys(&h);
    assert!(
        ["enter", "r", "e", "n"]
            .iter()
            .all(|k| on_harness.contains(&k.to_string())),
        "{on_harness:?}"
    );
    h.key(Key::Enter);
    assert!(h.setup.expanded, "Enter shows a harness's details");
    h.setup.expanded = false;
    h.setup.cursor = h.harnesses.len();
    assert!(h.selected_login().is_some());
    assert_eq!(footer_keys(&h), ["a", "x", "esc"]);
    h.key(Key::Enter);
    assert!(!h.setup.expanded, "a key has no details");
}

#[test]
fn setup_cursor_runs_on_from_the_harnesses_into_the_stored_keys() {
    let mut h = setup_home();
    for _ in 0..10 {
        h.key(Key::Char('j'));
    }
    assert_eq!(
        h.setup.cursor, 4,
        "two harnesses, two keys, then the row that adds the first profile, and no further"
    );
    assert_eq!(h.selected_login(), None);
    h.key(Key::Char('k'));
    assert_eq!(
        h.selected_login().map(|l| l.id.as_str()),
        Some("openrouter:work")
    );
    h.key(Key::Char('k'));
    assert_eq!(h.selected_login().map(|l| l.id.as_str()), Some("anthropic"));
    h.setup.cursor = 0;
    assert_eq!(h.selected_login(), None, "a harness row is not a key");
}

#[test]
fn removing_a_key_waits_for_y_and_names_the_credential_id() {
    let mut h = setup_home();
    h.setup.cursor = 3;
    assert_eq!(h.key(Key::Char('x')), Effect::None);
    assert_eq!(
        h.mode,
        Mode::Confirm(Effect::Logout("openrouter:work".into()))
    );
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(h.notice.as_deref(), Some("key kept"));
    h.key(Key::Char('x'));
    assert_eq!(
        h.key(Key::Char('y')),
        Effect::Logout("openrouter:work".into())
    );
    let mut h = setup_home();
    assert_eq!(h.key(Key::Char('x')), Effect::None);
    assert_eq!(h.mode, Mode::Normal, "x on a harness row removes nothing");
    assert_eq!(h.notice.as_deref(), Some("select a key or a profile"));
}

fn typing(purpose: Purpose, text: &str) -> Mode {
    Mode::Typing {
        purpose,
        text: text.into(),
    }
}

#[test]
fn adding_a_key_asks_for_the_provider_and_hands_off_to_marion_key_add() {
    let mut h = setup_home();
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
    h.setup.cursor = 3;
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

/// Two harnesses, two keys, then two profiles: rows 4 and 5.
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

#[test]
fn setup_cursor_runs_on_from_the_stored_keys_into_the_profiles() {
    let mut h = profiles_home();
    for _ in 0..10 {
        h.key(Key::Char('j'));
    }
    assert_eq!(h.setup.cursor, 5, "two harnesses, two keys, two profiles");
    assert_eq!(h.selected_profile().map(|p| p.name.as_str()), Some("cx"));
    assert_eq!(h.selected_login(), None, "a profile row is not a key");
    // With none, the cursor still reaches the one row that adds the first.
    let mut h = setup_home();
    for _ in 0..10 {
        h.key(Key::Char('j'));
    }
    assert_eq!(h.setup.cursor, 4);
    assert!(h.on_profiles());
    assert_eq!(h.selected_profile(), None);
}

#[test]
fn u_makes_the_selected_profile_its_harness_default_and_says_so_when_it_already_is() {
    let mut h = profiles_home();
    h.setup.cursor = 5;
    assert_eq!(
        h.key(Key::Char('u')),
        Effect::ProfileUse {
            harness: "codex".into(),
            name: "cx".into()
        }
    );
    h.setup.cursor = 4;
    assert_eq!(h.key(Key::Char('u')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("claude already runs on work"));
    h.setup.cursor = 0;
    assert_eq!(h.key(Key::Char('u')), Effect::None, "u off the profiles");
    assert_eq!(h.notice.as_deref(), Some("select a profile"));
}

#[test]
fn removing_a_profile_waits_for_y_and_n_keeps_it() {
    let mut h = profiles_home();
    h.setup.cursor = 4;
    assert_eq!(h.key(Key::Char('x')), Effect::None);
    assert_eq!(
        h.mode,
        Mode::Confirm(Effect::ProfileRemove {
            harness: "claude".into(),
            name: "work".into(),
        })
    );
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    assert_eq!(h.notice.as_deref(), Some("profile kept"));
    h.key(Key::Char('x'));
    assert_eq!(
        h.key(Key::Char('y')),
        Effect::ProfileRemove {
            harness: "claude".into(),
            name: "work".into(),
        }
    );
    // On the row that adds the first profile there is nothing to remove.
    let mut h = setup_home();
    h.setup.cursor = 4;
    assert_eq!(h.key(Key::Char('x')), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
}

#[test]
fn adding_a_profile_asks_for_harness_and_name_and_hands_off_to_marion_profile_add() {
    let mut h = profiles_home();
    h.setup.cursor = 5;
    assert_eq!(h.key(Key::Char('a')), Effect::None);
    assert_eq!(
        h.mode,
        typing(Purpose::Profile, "codex "),
        "on a profile's row the line starts with its harness"
    );
    typed(&mut h, "personal");
    assert_eq!(
        h.key(Key::Enter),
        Effect::ProfileAdd {
            harness: "codex".into(),
            name: "personal".into()
        }
    );
    // Refused here, before the terminal changes hands: a harness with no carrier, a name that is
    // not one, a name taken, and a line without both words. Each keeps the line open.
    for (text, says) in [
        ("copilot gh", "takes no profiles"),
        ("claude ../x", "not a profile name"),
        ("claude work", "already exists"),
        ("claude", "a harness and a name"),
    ] {
        h.mode = typing(Purpose::Profile, text);
        assert_eq!(h.key(Key::Enter), Effect::None, "{text}");
        assert!(
            h.notice.as_deref().unwrap_or("").contains(says),
            "{text}: {:?}",
            h.notice
        );
        assert_eq!(h.mode, typing(Purpose::Profile, text));
    }
    h.key(Key::Esc);
    // Off the profiles, `a` is still the key line.
    h.setup.cursor = 3;
    h.key(Key::Char('a'));
    assert!(matches!(
        h.mode,
        Mode::Typing {
            purpose: Purpose::Key,
            ..
        }
    ));
    // On the row that adds the first, the line starts empty.
    let mut h = setup_home();
    h.setup.cursor = 4;
    h.key(Key::Char('a'));
    assert_eq!(h.mode, typing(Purpose::Profile, ""));
    assert_eq!(h.key(Key::Esc), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
}

#[test]
fn no_single_key_on_a_profile_is_destructive() {
    for k in every_key() {
        let mut h = profiles_home();
        h.setup.cursor = 4;
        let e = h.key(k.clone());
        assert!(!e.destructive(), "{k:?} returned {e:?} without a confirm");
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
fn the_form_fills_a_draft_and_enter_asks_for_a_preview() {
    let mut h = setup_home();
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    let Mode::Form(f) = &h.mode else {
        panic!("n opens the form, got {:?}", h.mode)
    };
    assert_eq!(
        f.draft.harness, "claude",
        "the first harness a file can name"
    );
    typed(&mut h, "reviewer");
    h.key(Key::Tab);
    h.key(Key::Right);
    h.key(Key::Tab);
    typed(&mut h, "o3");
    h.key(Key::Backspace);
    h.key(Key::Tab);
    typed(&mut h, "Reviews diffs");
    h.key(Key::Tab);
    h.key(Key::Right);
    let Effect::PreviewType(d) = h.key(Key::Enter) else {
        panic!("enter previews")
    };
    assert_eq!(d.name, "reviewer");
    assert_eq!(d.harness, "copilot", "→ moved to the next harness");
    assert_eq!(d.model, "o");
    assert_eq!(d.description, "Reviews diffs");
    assert_eq!(d.tools, types_form::Tools::Read);
    assert!(
        matches!(h.mode, Mode::Form(_)),
        "the form stays open while it previews"
    );
    h.key(Key::BackTab);
    let Mode::Form(f) = &h.mode else { panic!() };
    assert_eq!(f.field, 3, "shift-tab goes back a field");
}

#[test]
fn an_incomplete_form_previews_nothing_and_says_what_is_missing() {
    let mut h = setup_home();
    h.key(Key::Char('n'));
    assert_eq!(h.key(Key::Enter), Effect::None);
    let Mode::Form(f) = &h.mode else { panic!() };
    assert!(f.error.as_deref().unwrap_or("").contains("name"), "{f:?}");
    assert_eq!(h.key(Key::Esc), Effect::None);
    assert_eq!(h.mode, Mode::Normal, "esc closes it");
}

#[test]
fn a_preview_waits_for_y_and_anything_else_goes_back_to_the_form() {
    let mut h = setup_home();
    h.key(Key::Char('n'));
    typed(&mut h, "reviewer");
    let Effect::PreviewType(d) = ({
        h.key(Key::Tab);
        h.key(Key::Tab);
        h.key(Key::Tab);
        typed(&mut h, "Reviews");
        h.key(Key::Enter)
    }) else {
        panic!()
    };
    h.show_preview("NEW".into(), vec![('+', "x".into())]);
    assert_eq!(
        h.mode,
        Mode::Confirm(Effect::WriteTypes {
            text: "NEW".into(),
            draft: d.clone()
        })
    );
    assert_eq!(h.setup.preview.len(), 1);
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    let Mode::Form(f) = &h.mode else {
        panic!("back to the form, got {:?}", h.mode)
    };
    assert_eq!(f.draft, d, "with the draft as it was");
    assert!(h.setup.preview.is_empty());
    h.form_refused("agent type \"reviewer\" would shadow".into());
    let Mode::Form(f) = &h.mode else { panic!() };
    assert!(f.error.as_deref().unwrap().contains("shadow"));
    h.show_preview("NEW".into(), vec![]);
    assert_eq!(
        h.key(Key::Char('y')),
        Effect::WriteTypes {
            text: "NEW".into(),
            draft: d
        }
    );
    assert!(
        Effect::WriteTypes {
            text: String::new(),
            draft: Default::default()
        }
        .destructive()
    );
}

#[test]
fn no_key_on_a_stored_key_or_in_the_form_is_destructive() {
    for k in every_key() {
        let mut h = setup_home();
        h.setup.cursor = 3;
        let e = h.key(k.clone());
        assert!(!e.destructive(), "{k:?} on a key's row returned {e:?}");
        let mut h = setup_home();
        h.key(Key::Char('n'));
        let e = h.key(k.clone());
        assert!(!e.destructive(), "{k:?} in the form returned {e:?}");
    }
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

/// A profile's row says what its probe said, and shows the login command only while it is logged
/// out or the probe could not tell — the command for the operator to run, never marion.
#[test]
fn a_logged_out_profile_shows_its_login_command_and_a_logged_in_one_does_not() {
    use crate::profiles::LoginState;
    let mut h = profiles_home();
    h.setup.profiles[0].login = Some(LoginState::LoggedIn);
    h.setup.profiles[1].login = Some(LoginState::LoggedOut);
    let f = frame(&h);
    let rows = &f.setup.profiles;
    assert_eq!(
        (rows[0].note.as_str(), rows[0].login.as_deref()),
        ("logged in", None)
    );
    assert_eq!(rows[1].note, "logged out");
    assert_eq!(
        rows[1].login.as_deref(),
        Some("CODEX_HOME='/p/cx' codex login")
    );
    assert!(rows[0].default && !rows[1].default);
    h.setup.profiles[1].login = Some(LoginState::Unknown("timed out".into()));
    assert!(
        frame(&h).setup.profiles[1].login.is_some(),
        "an unknown login offers the command too"
    );
    h.setup.profiles[1].login = None;
    assert_eq!(frame(&h).setup.profiles[1].note, "checking…");
    assert!(h.animating(), "a probe still out keeps the spinner turning");
    h.setup.cursor = 5;
    assert!(footer_has(&h, "u", "use"));
}

#[test]
fn a_new_listing_selects_the_profile_just_added_and_keeps_the_cursor_on_a_real_row() {
    let mut h = profiles_home();
    let harnesses = h.setup.profile_harnesses.clone();
    let mut rows = h.setup.profiles.clone();
    rows.push(profile("claude", "personal", false));
    h.set_profiles(rows, harnesses.clone(), Some("personal"));
    assert_eq!(
        h.selected_profile().map(|p| p.name.as_str()),
        Some("personal"),
        "the profile just added is selected, so its login command shows"
    );
    // The last profile removed: the cursor comes back to the row that adds one.
    h.set_profiles(Vec::new(), harnesses, None);
    assert_eq!(h.setup.cursor, 4);
    assert!(h.on_profiles() && h.selected_profile().is_none());
}

/// **Setup's last row turns notifications on or off**: past the profiles, Enter runs the `marion
/// notify` that flips it, and the footer says which way it goes.
#[test]
fn setup_notifications_row_flips_them_with_enter() {
    let mut h = setup_home();
    h.setup.notify = Some(false);
    h.setup.notify_shown_by = "osascript".into();
    for _ in 0..10 {
        h.key(Key::Char('j'));
    }
    assert!(h.on_notify(), "the cursor stops on the last row");
    assert!(!h.on_profiles(), "which is not a profile row");
    assert!(footer_has(&h, "enter", "turn on"));
    assert_eq!(h.key(Key::Enter), Effect::Notify(true));
    assert_eq!(
        Effect::Notify(true).argv().unwrap(),
        ["marion", "notify", "on"]
    );
    h.setup.notify = None;
    assert!(!h.on_notify(), "no row until the setting has been read");
}
