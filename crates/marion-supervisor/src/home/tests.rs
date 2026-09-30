//! The home screen's decisions, without a terminal: which key does what on which tab, and that
//! nothing destructive happens without a `y`.

use super::*;
use marion_core::contract::{AgentId, ExitStatus};
use marion_core::encoding::Duration;
use marion_core::harness::Harness as H;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::result::{ActionKind, ActionLine, ActivityPage, CompletionSummary};

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
    let mut h = Home::new(Tab::Start);
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

#[test]
fn start_types_the_prompt_and_enter_runs_the_chosen_type_and_model() {
    let mut h = home();
    typed(&mut h, "add a limiter");
    assert_eq!(h.start.prompt, "add a limiter");
    // Implementers first: the plain name is the default choice.
    assert_eq!(h.kinds()[0].name, "claude");
    h.key(Key::Right);
    let run = h.key(Key::Enter);
    assert_eq!(
        run,
        Effect::Run {
            agent_type: "claude".into(),
            model: Some("opus".into()),
            prompt: "add a limiter".into(),
            pane: false,
        }
    );
    assert!(
        h.start.prompt.is_empty(),
        "the box clears once the run is sent"
    );
}

#[test]
fn ctrl_o_picks_the_read_only_flavour_and_default_model_sends_no_model() {
    let mut h = home();
    h.key(Key::Ctrl('o'));
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

#[test]
fn ctrl_p_runs_in_a_pane_so_enter_can_attach() {
    let mut h = home();
    h.key(Key::Ctrl('p'));
    typed(&mut h, "x");
    let Effect::Run { pane, .. } = h.key(Key::Enter) else {
        panic!("a run")
    };
    assert!(pane);
}

#[test]
fn a_blank_prompt_or_an_unready_harness_runs_nothing_and_says_why() {
    let mut h = home();
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert!(h.notice.as_deref().unwrap().contains("type what"));
    h.key(Key::Down);
    typed(&mut h, "x");
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert!(
        h.notice.as_deref().unwrap().contains("not ready"),
        "{:?}",
        h.notice
    );
    assert_eq!(h.start.prompt, "x", "a refused run keeps what was typed");
}

#[test]
fn tab_cycles_the_screens_both_ways() {
    let mut h = home();
    h.key(Key::Tab);
    assert_eq!(h.tab, Tab::Watch);
    h.key(Key::Tab);
    h.key(Key::Tab);
    assert_eq!(h.tab, Tab::Help);
    h.key(Key::Tab);
    assert_eq!(h.tab, Tab::Start);
    h.key(Key::BackTab);
    assert_eq!(h.tab, Tab::Help);
}

#[test]
fn cancel_waits_for_y_and_anything_else_keeps_the_node() {
    let mut h = home();
    h.tab = Tab::Watch;
    assert_eq!(
        h.key(Key::Char('x')),
        Effect::None,
        "no kill on the first key"
    );
    assert!(matches!(h.mode, Mode::Confirm(_)));
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
    h.key(Key::Char('x'));
    assert_eq!(
        h.key(Key::Char('y')),
        Effect::Cancel(AgentId("root".into()))
    );
}

/// **A second `x` on a node whose cancel is still in its grace asks to kill it** — the
/// escalation for a harness ignoring its abort, `marion cancel --force` — and still only after a
/// `y`.
#[test]
fn x_on_a_node_being_cancelled_asks_to_kill_it() {
    let mut h = home();
    h.tab = Tab::Watch;
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

/// **No destructive effect is reachable without a `y`**: every key on every tab, from a fresh
/// state, is pressed once, and none returns a destructive effect.
#[test]
fn no_single_key_on_any_tab_is_destructive() {
    let keys = [
        Key::Enter,
        Key::Esc,
        Key::Tab,
        Key::Backspace,
        Key::Up,
        Key::Down,
        Key::Left,
        Key::Right,
        Key::PageUp,
        Key::PageDown,
        Key::Ctrl('o'),
        Key::Paste("p".into()),
    ]
    .into_iter()
    .chain((b'!'..=b'~').map(|b| Key::Char(b as char)))
    .collect::<Vec<_>>();
    for tab in Tab::ALL {
        for k in &keys {
            let mut h = home();
            h.tab = tab;
            let e = h.key(k.clone());
            assert!(
                !e.destructive(),
                "{k:?} on {tab:?} returned {e:?} without a confirm"
            );
        }
    }
}

#[test]
fn steer_composes_in_the_box_and_enter_sends_it_to_the_node_it_began_on() {
    let mut h = home();
    h.tab = Tab::Watch;
    h.key(Key::Char('s'));
    typed(&mut h, "use a deque");
    h.key(Key::Paste(" please".into()));
    h.key(Key::Char('j'));
    assert!(
        matches!(&h.mode, Mode::Compose { text, .. } if text == "use a dequej please" || text.ends_with('j'))
    );
    h.key(Key::Backspace);
    assert_eq!(
        h.key(Key::Enter),
        Effect::Steer(AgentId("root".into()), "use a deque please".into())
    );
    // Esc drops it.
    h.key(Key::Char('s'));
    typed(&mut h, "never mind");
    assert_eq!(h.key(Key::Esc), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
}

#[test]
fn ended_nodes_resume_and_running_ones_steer() {
    let mut h = home();
    h.tab = Tab::Watch;
    assert_eq!(h.key(Key::Char('u')), Effect::None, "root is running");
    h.key(Key::Char('j'));
    assert_eq!(
        h.key(Key::Char('u')),
        Effect::Resume(AgentId("child".into()))
    );
    assert_eq!(h.key(Key::Char('s')), Effect::None);
    assert!(h.notice.as_deref().unwrap().contains("brings it back"));
    assert_eq!(
        h.key(Key::Char('x')),
        Effect::None,
        "an ended node is not cancelled"
    );
}

#[test]
fn copy_merge_and_diff_need_a_landed_branch() {
    let mut h = home();
    h.tab = Tab::Watch;
    h.key(Key::Char('j'));
    assert_eq!(h.key(Key::Char('c')), Effect::None);
    h.absorb_detail(
        AgentId("child".into()),
        NodeDetail {
            completion: Some(CompletionSummary {
                status: ExitStatus::Ok,
                narrative: None,
                branch: Some("marion/t-1".into()),
                commit: None,
                changed_paths: 1,
                exit: String::new(),
                diff: None,
            }),
            ..Default::default()
        },
    );
    assert_eq!(
        h.key(Key::Char('c')),
        Effect::Copy("git merge --no-ff marion/t-1".into())
    );
    assert_eq!(h.key(Key::Char('d')), Effect::Diff("marion/t-1".into()));
}

#[test]
fn bang_jumps_to_the_node_that_needs_you() {
    let mut h = home();
    h.set_nodes(vec![
        node("a", None, NodeState::Running),
        node("b", None, NodeState::Exited(ExitStatus::Failed)),
    ]);
    h.tab = Tab::Watch;
    h.key(Key::Char('!'));
    assert_eq!(h.selected().unwrap().agent_id.0, "b");
}

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

/// **A call's end is drawn as its outcome, and the row still names the call.** Paged in two, a
/// command and its end fold to one line each; the row's one line is the command, not `✓ 3.0s`,
/// and the end is drawn as a success.
#[test]
fn an_ended_call_shows_its_outcome_and_the_row_keeps_the_call() {
    let mut h = home();
    h.set_nodes(vec![node("root", None, NodeState::Running)]);
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
    let f = view::frame(&h, &view::Places::default());
    let texts: Vec<(&str, marion_tui::home::LineKind)> = f
        .watch
        .expanded
        .as_ref()
        .unwrap()
        .stream
        .iter()
        .map(|l| (l.text.as_str(), l.kind))
        .collect();
    assert_eq!(
        texts,
        [
            ("$ make test", marion_tui::home::LineKind::Call),
            ("✓ 3.0s", marion_tui::home::LineKind::Done),
        ]
    );
    assert_eq!(f.watch.rows[0].doing, "$ make test");
}

#[test]
fn stream_scrolls_back_and_follows_again() {
    let mut h = home();
    h.tab = Tab::Watch;
    h.watch.stream = (0..30)
        .map(|i| ActionLine {
            at: "t".into(),
            kind: ActionKind::Call,
            text: i.to_string(),
            id: None,
        })
        .collect();
    h.key(Key::PageUp);
    assert_eq!(h.watch.scroll, PAGE);
    h.key(Key::Char('J'));
    assert_eq!(h.watch.scroll, PAGE - 1);
    h.key(Key::PageDown);
    assert_eq!(h.watch.scroll, 0, "back at the end: following");
    h.key(Key::Char('j'));
    assert_eq!(h.watch.scroll, 0);
}

/// **Enter opens a headless agent's stream, and attaches only to a pane agent.** Start runs
/// agents headless, so Enter on Watch used to attach to a node with no terminal and be refused;
/// now it toggles the selected agent's stream full-height, Esc goes back, and the hint says which.
#[test]
fn enter_streams_a_headless_agent_and_attaches_only_to_a_pane_one() {
    let mut h = home();
    h.tab = Tab::Watch;
    assert_eq!(
        h.watch_default(),
        Effect::None,
        "no attach on a headless agent"
    );
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert!(h.watch.full_stream, "Enter opens its stream");
    let f = view::frame(&h, &view::Places::default());
    assert!(f.watch.full_stream);
    assert!(f.hints.iter().any(|x| x.key == "esc"), "{:?}", f.hints);
    assert_eq!(h.key(Key::Esc), Effect::None);
    assert!(!h.watch.full_stream, "Esc goes back to the forest");
    let f = view::frame(&h, &view::Places::default());
    assert!(
        f.hints
            .iter()
            .any(|x| x.key == "enter" && x.verb == "stream"),
        "{:?}",
        f.hints
    );

    let mut paned = node("root", None, NodeState::Running);
    paned.pane = true;
    h.set_nodes(vec![paned]);
    assert_eq!(h.key(Key::Enter), Effect::Attach(AgentId("root".into())));
    let f = view::frame(&h, &view::Places::default());
    assert!(
        f.hints
            .iter()
            .any(|x| x.key == "enter" && x.verb == "attach"),
        "{:?}",
        f.hints
    );
}

/// **A blocked agent says what the operator can do about it here**: a pane agent's dialog is
/// answered by attaching, a headless one has no terminal to attach to and is pointed at the keys
/// that act on it, and one waiting on its own agents points at them.
#[test]
fn a_blocked_agent_says_what_the_operator_can_do_about_it() {
    use marion_core::node::BlockReason;
    let needs = |n: NodeSummary| {
        let mut h = home();
        h.tab = Tab::Watch;
        h.set_nodes(vec![n]);
        view::frame(&h, &view::Places::default())
            .watch
            .expanded
            .and_then(|e| e.needs)
            .unwrap_or_default()
    };
    let headless = needs(node(
        "root",
        None,
        NodeState::Blocked(BlockReason::Permission),
    ));
    assert!(headless.contains("s steers it, x cancels it"), "{headless}");
    assert!(!headless.contains("attach"), "{headless}");
    let mut paned = node("root", None, NodeState::Blocked(BlockReason::BootDialog));
    paned.pane = true;
    assert!(needs(paned).contains("enter attaches"));
    let waiting = needs(node(
        "root",
        None,
        NodeState::Blocked(BlockReason::Descendants),
    ));
    assert!(waiting.contains("its own agents"), "{waiting}");
}

/// **Help opens from wherever the operator is and goes back there.** `?` is help on an empty
/// Start prompt and a question mark in a typed one; F1 opens it from any tab; Esc, `?` or F1 return
/// to the tab it was opened from, not to Start.
#[test]
fn help_opens_from_any_tab_and_returns_to_it() {
    let mut h = home();
    h.key(Key::Char('?'));
    assert_eq!(h.tab, Tab::Help, "`?` on an empty prompt");
    h.key(Key::Esc);
    assert_eq!(h.tab, Tab::Start);
    typed(&mut h, "why?");
    assert_eq!(h.tab, Tab::Start);
    assert_eq!(
        h.start.prompt, "why?",
        "a question mark in a prompt is text"
    );
    h.key(Key::F1);
    assert_eq!(h.tab, Tab::Help, "F1 even mid-prompt");
    h.key(Key::F1);
    assert_eq!(h.tab, Tab::Start);
    assert_eq!(h.start.prompt, "why?", "the prompt survives");
    for from in [Tab::Watch, Tab::Setup] {
        h.tab = from;
        h.key(Key::Char('?'));
        assert_eq!(h.tab, Tab::Help);
        h.key(Key::Esc);
        assert_eq!(h.tab, from, "back where it was opened");
    }
}

/// Help drawn as the operator sees it: the real key table (`view::KEYS`) through `marion_tui`, at
/// the size a terminal opens at.
fn help_screen(h: &Home, w: u16, rows: u16) -> String {
    use ratatui::widgets::Widget as _;
    let f = view::frame(h, &view::Places::default());
    let screen = marion_tui::home::Screen {
        theme: marion_tui::home::theme::Theme::from_colorterm(None),
        project: "~/code/acme-api",
        attention: 0,
        body: f.body(Tab::Help),
        input: f.input.clone(),
        hints: f.hints.clone(),
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

/// **Help shows every section of the real key table at 80x24** — two columns, the commands left
/// for a wider screen — and the section for the tab it was opened from comes first; where it does
/// not fit it scrolls with j/k. Mutation: draw one column, or drop the first section. It fails.
#[test]
fn help_shows_every_section_of_the_real_key_table_at_80x24() {
    let mut h = home();
    h.tab = Tab::Watch;
    h.key(Key::Char('?'));
    let screen = help_screen(&h, 80, 24);
    for (title, _) in view::KEYS {
        assert!(
            screen.contains(&title.to_uppercase()),
            "{title} is missing at 80x24:\n{screen}"
        );
    }
    let watch = screen.find("WATCH").unwrap();
    let start = screen.find("START").unwrap();
    assert!(
        watch < start,
        "opened from Watch, Watch comes first:\n{screen}"
    );
    insta::assert_snapshot!(screen);
    // A screen too short for the page scrolls, and says so.
    let short = help_screen(&h, 80, 14);
    assert!(short.contains("j/k scroll"), "{short}");
    h.key(Key::Char('j'));
    assert_eq!(h.help_scroll, 1);
    h.key(Key::Char('k'));
    h.key(Key::Char('k'));
    assert_eq!(h.help_scroll, 0);
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

/// **`m` merges an agent's landed branch, and only after a `y` that names the branch and the
/// command**; `c` still copies it. An agent that landed nothing says so.
#[test]
fn m_merges_a_landed_branch_after_a_y() {
    let mut h = home();
    h.tab = Tab::Watch;
    assert_eq!(h.key(Key::Char('m')), Effect::None);
    assert!(h.notice.as_deref().unwrap_or("").contains("no branch"));
    let id = h.selected().unwrap().agent_id.clone();
    h.absorb_detail(
        id,
        NodeDetail {
            completion: Some(CompletionSummary {
                status: ExitStatus::Ok,
                narrative: None,
                branch: Some("marion/t1".into()),
                commit: None,
                changed_paths: 1,
                exit: String::new(),
                diff: None,
            }),
            ..Default::default()
        },
    );
    assert_eq!(
        h.key(Key::Char('m')),
        Effect::None,
        "no merge on the first key"
    );
    let f = view::frame(&h, &view::Places::default());
    let marion_tui::home::Input::Confirm { question, command } = f.input else {
        panic!("a confirm");
    };
    assert!(question.contains("marion/t1"), "{question}");
    assert_eq!(command, "git merge --no-ff marion/t1");
    assert_eq!(h.key(Key::Char('n')), Effect::None);
    assert_eq!(h.notice.as_deref(), Some("not merged"));
    h.key(Key::Char('m'));
    assert_eq!(h.key(Key::Char('y')), Effect::Merge("marion/t1".into()));
    assert!(matches!(h.key(Key::Char('c')), Effect::Copy(_)));
}

/// **Setup's hints are the keys of the section under the cursor**: a harness offers details, the
/// re-check and the agent-type keys (with `e`); a key row offers add and remove; a login row add,
/// use and remove. Enter expands only a harness, and on the other rows says what does work.
#[test]
fn setup_hints_follow_the_section_under_the_cursor() {
    let mut h = setup_home();
    let keys = |h: &Home| {
        view::frame(h, &view::Places::default())
            .hints
            .into_iter()
            .map(|x| x.key)
            .collect::<Vec<_>>()
    };
    h.setup.cursor = 0;
    let on_harness = keys(&h);
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
    let on_key = keys(&h);
    assert_eq!(on_key, ["j/k", "a", "x"]);
    h.key(Key::Enter);
    assert!(!h.setup.expanded, "a key has no details");
    assert!(h.notice.as_deref().unwrap_or("").contains("a adds a key"));
}

/// **Watch's hint row keeps `!` where a narrow screen cannot drop it**, second; and with nothing
/// to act on it offers only the key that leads to Start.
#[test]
fn watch_hints_put_needs_you_second_and_an_empty_forest_offers_only_start() {
    let mut h = home();
    h.tab = Tab::Watch;
    let keys = |h: &Home| {
        view::frame(h, &view::Places::default())
            .hints
            .into_iter()
            .map(|x| x.key)
            .collect::<Vec<_>>()
    };
    assert_eq!(keys(&h)[1], "!");
    h.lost_supervisor();
    assert_eq!(keys(&h), ["tab"]);
    h.set_nodes(vec![]);
    assert_eq!(keys(&h), ["tab"]);
}

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
        Effect::Copy("git merge b".into()),
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
    let merge = Effect::Merge("marion/t1".into());
    assert_eq!(
        merge.argv(),
        Some(
            ["git", "merge", "--no-ff", "marion/t1"]
                .map(String::from)
                .to_vec()
        )
    );
    assert!(
        merge.destructive(),
        "a merge changes the checkout: after a y"
    );
    assert_eq!(Effect::None.argv(), None);
    assert_eq!(Effect::Quit.argv(), None);
}

// ------------------------------------------------------------------------ the view's words

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

fn detail_with_workspace(w: marion_core::contract::Workspace) -> NodeDetail {
    NodeDetail {
        workspace: Some(w),
        ..Default::default()
    }
}

#[test]
fn a_workspace_reads_as_its_branch_not_its_state_path() {
    use marion_core::contract::Workspace;
    let mut h = home();
    h.set_nodes(vec![node("root", None, NodeState::Running)]);
    h.absorb_detail(
        AgentId("root".into()),
        detail_with_workspace(Workspace::Worktree {
            path: "/private/tmp/state/ea39/agents/01a0/worktree".into(),
            branch: "marion/t-3c33".into(),
        }),
    );
    let f = view::frame(&h, &view::Places::default());
    let ws = f.watch.expanded.unwrap().workspace.unwrap();
    assert_eq!(ws, "worktree marion/t-3c33");

    h.absorb_detail(
        AgentId("root".into()),
        detail_with_workspace(Workspace::SharedCwd {
            path: "/private/tmp/run/repo".into(),
        }),
    );
    let f = view::frame(&h, &view::Places::default());
    let ws = f.watch.expanded.unwrap().workspace.unwrap();
    assert_eq!(ws, "the checkout repo");
}

#[test]
fn can_names_only_the_few_things_the_home_keys_do() {
    let mut h = home();
    h.set_nodes(vec![node("root", None, NodeState::Running)]);
    let f = view::frame(&h, &view::Places::default());
    let caps = f.watch.expanded.unwrap().caps;
    let names: Vec<&str> = caps.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        (1..=3).contains(&names.len()),
        "a few words, not every capability field: {names:?}"
    );
    assert!(
        names
            .iter()
            .all(|n| ["steer", "resume", "interrupt"].contains(n)),
        "{names:?}"
    );
}

/// The clock the loop may wake for: only while something on the tab moves with time. An idle
/// forest, an ended one, or a tab with nothing animated makes the screen sleep until an event.
#[test]
fn only_a_moving_screen_asks_for_a_clock() {
    let mut h = home();
    h.tab = Tab::Watch;
    h.set_nodes(vec![]);
    assert!(!h.animating(), "an empty forest sleeps");
    h.set_nodes(vec![node("a", None, NodeState::Exited(ExitStatus::Ok))]);
    assert!(!h.animating(), "an ended forest sleeps");
    h.set_nodes(vec![node("a", None, NodeState::Running)]);
    assert!(h.animating(), "a running node's elapsed time ticks");
    h.tab = Tab::Help;
    assert!(!h.animating(), "not while another tab hides it");
    h.tab = Tab::Setup;
    assert!(!h.animating());
    h.checking = true;
    assert!(h.animating(), "a harness still being checked spins");
    h.tab = Tab::Start;
    assert!(h.animating());
}

/// A row carries its elapsed time and its token total from the summary — for every node, not
/// only the selected one whose detail was read.
#[test]
fn every_row_shows_its_elapsed_time_and_tokens() {
    use marion_core::encoding::SystemTime;
    let t0 = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
    let mut running = node("a", None, NodeState::Running);
    running.started_at = Some(SystemTime(t0));
    running.tokens = Some(88_000);
    let mut unstarted = node("b", None, NodeState::Running);
    unstarted.started_at = None;
    let mut h = home();
    h.set_nodes(vec![running, unstarted]);
    let now = t0 + std::time::Duration::from_secs(137);
    let f = view::frame_at(&h, &view::Places::default(), now);
    let a = &f.watch.rows[0];
    assert_eq!((a.elapsed.as_str(), a.tokens), ("2m17s", Some(88_000)));
    let b = &f.watch.rows[1];
    assert_eq!(
        (b.elapsed.as_str(), b.tokens),
        ("", None),
        "unknown is blank, never 0"
    );

    let mut ended = node("c", None, NodeState::Exited(ExitStatus::Ok));
    ended.started_at = Some(SystemTime(t0));
    ended.ended_at = Some(SystemTime(t0 + std::time::Duration::from_secs(60)));
    h.set_nodes(vec![ended]);
    let later = t0 + std::time::Duration::from_secs(60 + 3840);
    let f = view::frame_at(&h, &view::Places::default(), later);
    assert_eq!(f.watch.rows[0].elapsed, "1h ago", "fits the 7-column time");
}

#[test]
fn the_expanded_node_draws_per_turn_tokens_and_the_diff_it_landed() {
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
            turns: vec![3, 12],
            completion: Some(CompletionSummary {
                status: ExitStatus::Ok,
                narrative: None,
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
    let e = view::frame(&h, &view::Places::default())
        .watch
        .expanded
        .unwrap();
    let t = e.tokens.unwrap();
    assert_eq!((t.rate, t.window.as_str()), (vec![3, 12], "turn"));
    let r = e.result.unwrap();
    assert_eq!((r.added, r.removed, r.files), (84, 12, 3));
}

/// The activity feed is what changed in the forest while the screen watched it, newest first:
/// a node appearing, needing the operator, ending. The snapshot a screen opens on is where it
/// starts, not a flood of "started" lines.
#[test]
fn the_feed_records_what_changed_while_the_screen_watched() {
    let mut h = home();
    h.set_nodes(vec![node("root", None, NodeState::Running)]);
    assert!(h.watch.feed.is_empty(), "the opening snapshot is not news");
    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        node("kid", Some("root"), NodeState::Running),
    ]);
    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        node("kid", Some("root"), NodeState::Exited(ExitStatus::Failed)),
    ]);
    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        node("kid", Some("root"), NodeState::Exited(ExitStatus::Failed)),
    ]);
    let texts: Vec<(&str, &str)> = h
        .watch
        .feed
        .iter()
        .map(|f| (f.short.as_str(), f.text.as_str()))
        .collect();
    assert_eq!(
        texts,
        [("kid", "failed"), ("kid", "spawned by root · codex")]
    );
    let f = view::frame(&h, &view::Places::default());
    assert_eq!(f.watch.feed.len(), 2);
    assert_eq!(f.watch.feed[0].tone, marion_tui::tree::Tone::Failed);
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
        Key::Paste("p".into()),
    ]
    .into_iter()
    .chain((b'!'..=b'~').map(|b| Key::Char(b as char)))
    .collect()
}

fn setup_home() -> Home {
    let mut h = home();
    h.tab = Tab::Setup;
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
    assert!(h.notice.is_some());
}

#[test]
fn adding_a_key_asks_for_the_provider_and_hands_off_to_marion_key_add() {
    let mut h = setup_home();
    assert_eq!(h.key(Key::Char('a')), Effect::None);
    assert_eq!(
        h.mode,
        Mode::Login {
            text: String::new()
        }
    );
    typed(&mut h, "openrouter:personal");
    assert_eq!(
        h.key(Key::Enter),
        Effect::Login("openrouter:personal".into())
    );
    // A provider marion does not know is refused here, before the terminal changes hands.
    h.key(Key::Char('a'));
    typed(&mut h, "nope");
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert!(
        h.notice
            .as_deref()
            .unwrap_or("")
            .contains("marion key list")
    );
    // On a key's row, the box starts with that key's provider.
    h.setup.cursor = 3;
    h.key(Key::Char('a'));
    assert_eq!(
        h.mode,
        Mode::Login {
            text: "openrouter".into()
        }
    );
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
    assert!(h.notice.is_some());
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
        Mode::Profile {
            text: "codex ".into()
        },
        "on a profile's row the box starts with its harness"
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
    // not one, a name taken, and a box without both words.
    for (text, says) in [
        ("copilot gh", "takes no profiles"),
        ("claude ../x", "not a profile name"),
        ("claude work", "already exists"),
        ("claude", "a harness and a name"),
    ] {
        h.key(Key::Char('a'));
        h.mode = Mode::Profile { text: text.into() };
        assert_eq!(h.key(Key::Enter), Effect::None, "{text}");
        assert!(
            h.notice.as_deref().unwrap_or("").contains(says),
            "{text}: {:?}",
            h.notice
        );
    }
    // Off the profiles, `a` is still the key box.
    h.setup.cursor = 3;
    h.key(Key::Char('a'));
    assert!(matches!(h.mode, Mode::Login { .. }));
    // On the row that adds the first, the box starts empty.
    let mut h = setup_home();
    h.setup.cursor = 4;
    h.key(Key::Char('a'));
    assert_eq!(
        h.mode,
        Mode::Profile {
            text: String::new()
        }
    );
    assert_eq!(h.key(Key::Esc), Effect::None);
    assert_eq!(h.mode, Mode::Normal);
}

#[test]
fn no_single_key_on_a_profile_is_destructive() {
    let keys = (b'!'..=b'~').map(|b| Key::Char(b as char)).chain([
        Key::Enter,
        Key::Esc,
        Key::Backspace,
        Key::Up,
        Key::Down,
    ]);
    for k in keys {
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
/// dispatcher over a scratch profile store, does what the pane says — and `add` only **prints**
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

#[test]
fn recent_runs_say_when_they_started() {
    let mut started = node(
        "0199aaaa-1111-7000-8000-000000000001",
        None,
        NodeState::Running,
    );
    started.started_at = Some(marion_core::encoding::SystemTime(
        std::time::SystemTime::now(),
    ));
    let unknown = node(
        "0199aaaa-2222-7000-8000-000000000002",
        None,
        NodeState::Running,
    );
    let mut h = home();
    h.set_nodes(vec![started, unknown]);
    let f = view::frame(&h, &view::Places::default());
    let whens: Vec<&str> = f.start.recent.iter().map(|r| r.when.as_str()).collect();
    assert_eq!(whens[0], "2222", "no start: the short id");
    assert!(
        whens[1].len() == 5 && whens[1].as_bytes()[2] == b':',
        "a start: hh:mm, {whens:?}"
    );
}

/// A profile's row says what its probe said, and shows the login command only while it is logged
/// out or the probe could not tell — the command for the operator to run, never marion.
#[test]
fn a_logged_out_profile_shows_its_login_command_and_a_logged_in_one_does_not() {
    use crate::profiles::LoginState;
    let mut h = profiles_home();
    h.setup.profiles[0].login = Some(LoginState::LoggedIn);
    h.setup.profiles[1].login = Some(LoginState::LoggedOut);
    let f = view::frame(&h, &view::Places::default());
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
    let f = view::frame(&h, &view::Places::default());
    assert!(
        f.setup.profiles[1].login.is_some(),
        "an unknown login offers the command too"
    );
    h.setup.profiles[1].login = None;
    let f = view::frame(&h, &view::Places::default());
    assert_eq!(f.setup.profiles[1].note, "checking…");
    assert!(h.animating(), "a probe still out keeps the spinner turning");
    // On a profile the box echoes `use`; on the add row, `add`.
    h.setup.cursor = 5;
    let f = view::frame(&h, &view::Places::default());
    assert!(
        matches!(&f.input, marion_tui::home::Input::Command { line, .. } if line == "marion profile use codex cx"),
        "{:?}",
        f.input
    );
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

/// **An endpoint node says which model it runs on**: `provider:model` on its row, and with the route
/// in its expansion; a node on its harness's own configuration shows neither.
#[test]
fn an_endpoint_node_shows_its_provider_and_model_on_its_row_and_its_route_when_expanded() {
    let mut h = home();
    let mut root = node("root", None, NodeState::Running);
    root.endpoint = Some(marion_core::proto::NodeEndpoint {
        provider: "groq".into(),
        model: Some("llama-3.3-70b".into()),
        route: Some("translated".into()),
    });
    h.set_nodes(vec![root, node("child", Some("root"), NodeState::Running)]);
    let f = view::frame(&h, &view::Places::default());
    assert!(
        f.watch.rows[0].kind.ends_with("groq:llama-3.3-70b"),
        "{:?}",
        f.watch.rows[0]
    );
    assert!(!f.watch.rows[1].kind.contains(':'), "{:?}", f.watch.rows[1]);
    let expanded = f.watch.expanded.unwrap();
    assert_eq!(
        expanded.endpoint.as_deref(),
        Some("groq:llama-3.3-70b · translated")
    );
}

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

/// **A race's seats sit under one header row on Watch**, placed where their parent put them. The
/// header sums the seats — their count, how many finished, their tokens so far — and names the
/// winner once decided; each seat's row says its seat and verdict. The header is no node, so the
/// cursor on it selects nothing to attach to.
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
    let f = view::frame(&h, &view::Places::default());
    let ids: Vec<&str> = f.watch.rows.iter().map(|r| r.id.as_str()).collect();
    let header = "race:019f0000-1a2b-7000-8000-00000000000a";
    assert_eq!(ids, ["root", header, "s1", "s2"]);
    let race = &f.watch.rows[1];
    assert_eq!(race.harness, "race");
    assert!(
        race.kind.contains("2 agents · 1 of 2 done"),
        "{}",
        race.kind
    );
    assert_eq!(race.tokens, Some(150), "the seats' tokens, summed");
    assert!(
        f.watch.rows[2].kind.ends_with("· agent 1"),
        "{}",
        f.watch.rows[2].kind
    );

    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        seat("s1", 1, Some(SeatVerdict::Failed), 100),
        seat("s2", 2, Some(SeatVerdict::Won), 50),
    ]);
    let f = view::frame(&h, &view::Places::default());
    assert!(
        f.watch.rows[1].kind.contains("#2 codex won"),
        "{}",
        f.watch.rows[1].kind
    );
    assert!(
        f.watch.rows[3].kind.ends_with("agent 2 ★ won"),
        "{}",
        f.watch.rows[3].kind
    );
    assert!(
        f.watch.rows[2].kind.ends_with("agent 1 failed"),
        "{}",
        f.watch.rows[2].kind
    );

    // Down from root lands on the header, which is no node.
    h.tab = Tab::Watch;
    h.watch.tree.select(header);
    assert!(h.selected().is_none());
    assert_eq!(h.watch_default(), Effect::None);
}

/// **A parent's row carries its subtree, against its tree budget where it has one, and the
/// header the forest's total** — the leaf's row carries nothing, its subtree being itself.
#[test]
fn a_parent_row_shows_its_subtree_against_its_budget_and_the_header_the_forest() {
    let mut h = home();
    h.tab = Tab::Watch;
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
    let f = view::frame(&h, &view::Places::default());
    assert_eq!(f.watch.total.as_deref(), Some("Σ1.4k"));
    let rows = &f.watch.rows;
    assert_eq!(
        rows[0].subtree,
        Some(("Σ1.4k/1.5k".to_string(), true)),
        "past 80% of its tree budget"
    );
    assert_eq!(rows[1].subtree, None);
    let (line, over) = f.watch.expanded.unwrap().subtree.unwrap();
    assert!(over);
    assert!(
        line.starts_with("Σ1.4k tokens · 5 Σ files")
            && line.contains("2 nodes (1 running)")
            && line.ends_with("budget 1.4k / 1.5k"),
        "{line}"
    );
}

/// **A race row acts on its winner**: Enter goes to it, `m` goes to it and, once its detail says
/// which branch it landed, asks to merge that branch; before a winner, or for keys that act on one
/// agent, it says what does work there.
#[test]
fn a_race_row_goes_to_its_winner_and_merges_it() {
    use marion_core::race::SeatVerdict;
    let mut h = home();
    h.tab = Tab::Watch;
    let header = "race:019f0000-1a2b-7000-8000-00000000000a";
    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        seat("s1", 1, None, 100),
        NodeSummary {
            state: NodeState::Running,
            ..seat("s2", 2, None, 50)
        },
    ]);
    h.watch.tree.select(header);
    assert_eq!(h.key(Key::Enter), Effect::None);
    assert!(
        h.notice
            .as_deref()
            .unwrap_or("")
            .contains("no agent has won")
    );
    h.key(Key::Char('s'));
    assert!(h.notice.as_deref().unwrap_or("").contains("a race row"));

    h.set_nodes(vec![
        node("root", None, NodeState::Running),
        seat("s1", 1, Some(SeatVerdict::Failed), 100),
        seat("s2", 2, Some(SeatVerdict::Won), 50),
    ]);
    h.watch.tree.select(header);
    h.key(Key::Enter);
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some("s2"));
    h.watch.tree.select(header);
    assert_eq!(h.key(Key::Char('m')), Effect::None);
    assert_eq!(h.selected().map(|n| n.agent_id.0.as_str()), Some("s2"));
    h.absorb_detail(
        AgentId("s2".into()),
        NodeDetail {
            completion: Some(CompletionSummary {
                status: ExitStatus::Ok,
                narrative: None,
                branch: Some("marion/s2".into()),
                commit: None,
                changed_paths: 1,
                exit: String::new(),
                diff: None,
            }),
            ..Default::default()
        },
    );
    assert_eq!(h.mode, Mode::Confirm(Effect::Merge("marion/s2".into())));
    assert_eq!(h.key(Key::Char('y')), Effect::Merge("marion/s2".into()));
}

/// **Setup's last row turns notifications on or off**: past the profiles, Enter runs the `marion
/// notify` that flips it, the box echoes that command, and the hints say which way it goes.
#[test]
fn setup_notifications_row_flips_them_with_enter() {
    let mut h = home();
    h.tab = Tab::Setup;
    h.setup.notify = Some(false);
    h.setup.notify_shown_by = "osascript".into();
    for _ in 0..10 {
        h.key(Key::Char('j'));
    }
    assert!(h.on_notify(), "the cursor stops on the last row");
    assert!(!h.on_profiles(), "which is not a profile row");
    let f = view::frame(&h, &view::Places::default());
    assert!(
        f.hints
            .iter()
            .any(|x| x.key == "enter" && x.verb == "turn on"),
        "{:?}",
        f.hints
    );
    let marion_tui::home::Input::Command { line, .. } = &f.input else {
        panic!("the box echoes a command");
    };
    assert_eq!(line, "marion notify on");
    assert_eq!(h.key(Key::Enter), Effect::Notify(true));
    h.setup.notify = None;
    assert!(!h.on_notify(), "no row until the setting has been read");
}
