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
        tokens: None,
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
    assert!(h.notice.as_deref().unwrap().contains("resume"));
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

#[test]
fn stream_scrolls_back_and_follows_again() {
    let mut h = home();
    h.tab = Tab::Watch;
    h.watch.stream = (0..30)
        .map(|i| ActionLine {
            at: "t".into(),
            kind: ActionKind::Call,
            text: i.to_string(),
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

#[test]
fn every_effect_has_its_command_and_only_cancel_is_destructive() {
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
        Effect::Resume(id.clone()),
        Effect::Shell("/w".into()),
        Effect::Diff("b".into()),
        Effect::Copy("git merge b".into()),
        Effect::Recheck,
        Effect::EditTypes,
    ];
    for e in &all {
        assert!(e.argv().is_some_and(|a| !a.is_empty()), "{e:?}");
        assert_eq!(e.destructive(), matches!(e, Effect::Cancel(_)), "{e:?}");
    }
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

#[test]
fn elapsed_time_is_worded_short_and_widens_only_as_it_grows() {
    use view::elapsed_word;
    assert_eq!(elapsed_word(0), "0s");
    assert_eq!(elapsed_word(45), "45s");
    assert_eq!(elapsed_word(137), "2m17s");
    assert_eq!(elapsed_word(724), "12m04s");
    assert_eq!(elapsed_word(3900), "1h05m");
    assert_eq!(elapsed_word(2 * 86_400 + 3 * 3600), "2d03h");
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
