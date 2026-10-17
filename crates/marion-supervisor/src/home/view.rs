//! A [`Home`] as `marion_tui::home`'s views: every word the screen shows is decided here, from the
//! wire types only the supervisor can read, and `marion-tui` only draws it.

use super::{Effect, Field, Home, Mode, Purpose};
use crate::tree::{attention_of, short_id};
use marion_core::contract::{AgentId, ExitStatus};
use marion_core::proto::NodeSummary;
use marion_core::proto::result::{ActionKind, NodeDetail};
use marion_tui::home::help::KeyRow;
use marion_tui::home::text::{elapsed, shell_line, tokens};
use marion_tui::home::{
    AgentRow, AgentTypeRow, AgentsView, Body, Footer, FormField, FormView, HarnessRow, HelpView,
    Hint, Ink, LineKind, LoginRow, Page, Preview, ProfileRow, Ready, SetupView, StreamLine,
};
use marion_tui::tree::Tone;

/// Where the project keeps things, for Setup's Project line.
#[derive(Debug, Clone, Default)]
pub struct Places {
    /// The project as the operator recognises it (`~/code/acme-api`).
    pub project: String,
    pub state: String,
}

/// Every view a frame needs, owned, so the caller can borrow them into a
/// [`marion_tui::home::Screen`].
pub struct Frame {
    pub title: String,
    pub counts: String,
    pub agents: AgentsView,
    pub setup: SetupView,
    pub help: HelpView,
    pub footer: Footer,
}

impl Frame {
    pub fn body(&self, page: Page) -> Body<'_> {
        match page {
            Page::Agents => Body::Agents(&self.agents),
            Page::Setup => Body::Setup(&self.setup),
            Page::Help => Body::Help(&self.help),
        }
    }
}

/// One frame of `home`, now.
pub fn frame(home: &Home, places: &Places) -> Frame {
    frame_at(home, places, std::time::SystemTime::now())
}

/// One frame of `home` as of `now`, which run times are worded against.
pub fn frame_at(home: &Home, places: &Places, now: std::time::SystemTime) -> Frame {
    let (title, counts) = match home.page {
        Page::Agents => (places.project.clone(), counts(home)),
        Page::Setup => (
            "setup".to_string(),
            marion_tui::home::setup::readiness(
                &home.harnesses.iter().map(harness_row).collect::<Vec<_>>(),
                home.checking,
            ),
        ),
        Page::Help => ("keys".to_string(), String::new()),
    };
    Frame {
        title,
        counts,
        agents: agents(home, now),
        setup: setup(home, places),
        help: help(home),
        footer: footer(home),
    }
}

/// The status line's counts: what runs, what needs the operator, what is ready to land. A count of
/// zero is left out rather than shown.
fn counts(home: &Home) -> String {
    let w = &home.watch;
    let land = w.nodes.iter().filter(|n| w.lands(n)).count();
    [
        (crate::tree::running(&w.nodes), "running"),
        (crate::tree::attention_count(&w.nodes), "need you"),
        (land, "to land"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect::<Vec<_>>()
    .join("  ")
}

/// `~/…` for a path under `$HOME`, which is how an operator reads their own paths.
pub fn tilde(path: &str) -> String {
    under_home(path, std::env::var("HOME").ok().as_deref()).unwrap_or_else(|| path.to_string())
}

/// `path` as `~/…` when it is `home` or under it, else `None`.
fn under_home(path: &str, home: Option<&str>) -> Option<String> {
    let home = home.filter(|h| !h.is_empty())?.trim_end_matches('/');
    let rest = path.strip_prefix(home)?;
    (rest.is_empty() || rest.starts_with('/')).then(|| format!("~{rest}"))
}

/// The project as the top bar names it: `~/code/acme · main` under `$HOME`, and just the
/// directory's name elsewhere (`repo · main`) — a long absolute path says nothing the name does
/// not. The branch is left off when there is none to name.
pub fn project_label(path: &str, home: Option<&str>, branch: Option<&str>) -> String {
    let place = under_home(path, home).unwrap_or_else(|| {
        std::path::Path::new(path)
            .file_name()
            .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
    });
    match branch.filter(|b| !b.is_empty() && *b != "HEAD") {
        Some(b) => format!("{place} · {b}"),
        None => place,
    }
}

// ---------------------------------------------------------------------------------- agents

fn agents(home: &Home, now: std::time::SystemTime) -> AgentsView {
    let w = &home.watch;
    let mut rows = Vec::new();
    for i in 0..w.tree.rows() {
        let Some((prefix, tnode)) = w.tree.row(i) else {
            break;
        };
        let row = |title: String, who: &str, tokens: Option<u64>| AgentRow {
            id: tnode.id.clone(),
            prefix: prefix.to_string(),
            tone: tnode.tone,
            title,
            who: who.to_string(),
            time: String::new(),
            tokens,
        };
        if let Some(race) = crate::tree::race_of_row(&tnode.id) {
            let s = race_summary(race, &w.nodes);
            rows.push(row(s.label(), "race", s.tokens));
            continue;
        }
        if let Some(run) = crate::tree::workflow_of_row(&tnode.id) {
            let s = workflow_summary(run, &w.nodes, now);
            rows.push(AgentRow {
                time: s.elapsed_secs.map(run_time).unwrap_or_default(),
                ..row(workflow_title(&s), "workflow", s.tokens)
            });
            continue;
        }
        let Some(n) = w.nodes.iter().find(|n| n.agent_id.0 == tnode.id) else {
            continue;
        };
        let detail = detail_of(home, n);
        let mut title = title_of(home, n);
        if let Some(note) = crate::tree::review_note(n)
            .or_else(|| crate::tree::race_note(n))
            .or_else(|| crate::tree::workflow_note(n))
        {
            title = format!("{title} · {note}");
        }
        rows.push(AgentRow {
            tone: tone_of(n, tnode.tone),
            time: node_time(n, now),
            ..row(
                title,
                &who_of(n),
                n.tokens
                    .or_else(|| detail.and_then(|d| d.usage).map(|u| u.total())),
            )
        });
    }
    let cursor = w.tree.cursor().min(rows.len().saturating_sub(1));
    AgentsView {
        rows,
        cursor,
        preview: preview(home, now),
        full: w.full && home.selected().is_some(),
    }
}

/// The row's glyph: the tree's own tone, except that an orphan — whose tone is the unmeasured grey
/// — needs the operator, and says so with the needs-you glyph.
fn tone_of(n: &NodeSummary, tone: Tone) -> Tone {
    if tone == Tone::Unknown && attention_of(n).is_some() {
        Tone::Blocked
    } else {
        tone
    }
}

fn detail_of<'a>(home: &'a Home, n: &NodeSummary) -> Option<&'a NodeDetail> {
    match &home.watch.detail {
        Some((id, d)) if *id == n.agent_id => Some(d),
        _ => None,
    }
}

/// What the row and the preview call a node: its task's first line once read, else its name,
/// else its agent type.
fn title_of(home: &Home, n: &NodeSummary) -> String {
    home.watch
        .titles
        .get(&n.agent_id.0)
        .cloned()
        .or_else(|| n.name.clone())
        .unwrap_or_else(|| n.agent_type.clone())
}

/// `codex`, or `codex groq:llama-3.3-70b` for an endpoint node: the harness, and the model where
/// marion knows which one it asked for.
fn who_of(n: &NodeSummary) -> String {
    match &n.endpoint {
        Some(e) => format!("{} {}", n.harness.cli_name(), e.label()),
        None => n.harness.cli_name().to_string(),
    }
}

/// How long a node has run, or ran: from its start to its end, or to now while it runs. Blank
/// where no start was recorded.
fn node_time(n: &NodeSummary, now: std::time::SystemTime) -> String {
    let Some(started) = n.started_at else {
        return String::new();
    };
    let until = if n.state.is_exited() {
        match n.ended_at {
            Some(e) => e.0,
            None => return String::new(),
        }
    } else {
        now
    };
    until
        .duration_since(started.0)
        .map(|d| run_time(d.as_secs()))
        .unwrap_or_default()
}

/// A run time in a five-column cell, in its two largest units: `45s`, `12m`, `3h04`, `2d03`.
pub fn run_time(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h{:02}", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d{:02}", secs / 86_400, secs % 86_400 / 3600),
    }
}

fn race_summary(race: &str, nodes: &[NodeSummary]) -> crate::tree::RaceSummary {
    crate::tree::RaceSummary::of(&marion_core::race::RaceId(race.to_string()), nodes)
}

fn workflow_summary(
    run: &str,
    nodes: &[NodeSummary],
    now: std::time::SystemTime,
) -> crate::tree::WorkflowSummary {
    crate::tree::WorkflowSummary::of(
        &marion_core::workflow::WorkflowId(run.to_string()),
        nodes,
        now,
    )
}

/// A workflow header's title: the run and the step it is on (`workflow ship · 3/4 gate r2`), or
/// how it closed. Its tokens and time are the row's columns, so they are not repeated here.
fn workflow_title(s: &crate::tree::WorkflowSummary) -> String {
    let at = match (s.closed, &s.current) {
        (Some(outcome), _) => outcome.word().to_string(),
        (None, Some((step, round, id))) => {
            let mut at = format!("{}/{} {id}", u16::from(*step) + 1, s.steps);
            if *round > 0 {
                at.push_str(&format!(" r{}", u16::from(*round) + 1));
            }
            at
        }
        (None, None) => "starting".into(),
    };
    format!("workflow {} · {at}", s.name)
}

/// What a node waits on the operator for, in a few words: the supervisor's own words where it
/// gave some, else what its state says. `None` for a node that is not waiting on anyone.
fn needs(n: &NodeSummary) -> Option<String> {
    use marion_core::node::{BlockReason, NodeState, ReapState};
    let what = match (n.state, n.reap_state) {
        (NodeState::Exited(_), _) => return None,
        (_, ReapState::Orphaned) => "orphaned: marion has no record of it ending",
        (NodeState::Blocked(BlockReason::Permission), _) => "wants permission",
        (NodeState::Blocked(BlockReason::Elicitation), _) => "asks a question",
        (NodeState::Blocked(BlockReason::Descendants), _) => "waits on its agents",
        (NodeState::Blocked(BlockReason::BootDialog), _) => "a dialog is open in its terminal",
        _ => return None,
    };
    Some(match &n.attention {
        Some(said) => marion_core::text::visible(said).into_owned(),
        None => what.to_string(),
    })
}

/// The selected row, previewed.
fn preview(home: &Home, now: std::time::SystemTime) -> Option<Preview> {
    let w = &home.watch;
    let row = w.tree.selected()?;
    if let Some(race) = crate::tree::race_of_row(&row.id) {
        let s = race_summary(race, &w.nodes);
        let outcome = match &s.decided {
            Some(Some(winner)) => format!("{winner} won; every branch is kept"),
            Some(None) => "no agent won; every branch is kept".into(),
            None => format!("{} of {} agents finished", s.ended, s.seats),
        };
        return Some(Preview {
            who: format!("race {}", short_id(race)),
            title: s.label(),
            facts: vec![vec![(outcome, Ink::Plain)]],
            quiet: Some("enter goes to the winner".into()),
            ..Preview::default()
        });
    }
    if let Some(run) = crate::tree::workflow_of_row(&row.id) {
        let s = workflow_summary(run, &w.nodes, now);
        let mut facts = Vec::new();
        if let Some(b) = s.budget {
            let spent = s.tokens.unwrap_or(0);
            let ink = if spent >= b { Ink::Warn } else { Ink::Plain };
            facts.push(vec![(
                format!("budget {} of {}", tokens(spent), tokens(b)),
                ink,
            )]);
        }
        return Some(Preview {
            who: format!("workflow {}", short_id(run)),
            title: workflow_title(&s),
            facts,
            quiet: Some("enter goes to the step it is on".into()),
            ..Preview::default()
        });
    }
    let n = home.selected()?;
    let detail = detail_of(home, n);
    let mut facts = Vec::new();
    if let Some(d) = detail {
        facts.extend(ended_facts(n, d));
    }
    if let Some(note) = crate::tree::review_note(n) {
        facts.push(vec![(note, Ink::Plain)]);
    }
    if let Some(e) = &n.endpoint {
        let route = e
            .route
            .as_deref()
            .map_or(String::new(), |r| format!(" · {r}"));
        facts.push(vec![(format!("{}{route}", e.label()), Ink::Dim)]);
    }
    if let Some((line, over)) = subtree_line(home, n, now) {
        facts.push(vec![(line, if over { Ink::Warn } else { Ink::Dim })]);
    }
    Some(Preview {
        who: format!("{} {}", n.harness.cli_name(), short_id(&n.agent_id.0)),
        title: title_of(home, n),
        needs: needs(n),
        facts,
        stream: if detail.is_some() {
            stream(home)
        } else {
            Vec::new()
        },
        scroll: w.scroll,
        quiet: match detail {
            None => Some("reading".into()),
            Some(d) => d.stream.as_ref().and_then(|p| p.unread.clone()),
        },
    })
}

/// How an ended node ended, and what it left: its status and run time with its tokens, the first
/// line of its own account, and its branch with what the branch changes.
fn ended_facts(n: &NodeSummary, d: &NodeDetail) -> Vec<Vec<(String, Ink)>> {
    let Some(c) = &d.completion else {
        return d
            .failure
            .iter()
            .map(|f| vec![(f.clone(), Ink::Bad)])
            .collect();
    };
    let ink = match c.status {
        ExitStatus::Ok => Ink::Plain,
        ExitStatus::Cancelled | ExitStatus::Unreported => Ink::Dim,
        _ => Ink::Bad,
    };
    let ran = match (n.started_at, n.ended_at) {
        (Some(s), Some(e)) => {
            e.0.duration_since(s.0)
                .map(|d| format!(" in {}", elapsed(d.as_secs())))
                .unwrap_or_default()
        }
        _ => String::new(),
    };
    let mut first = vec![(format!("{}{ran}", c.status.word()), ink)];
    if let Some(u) = d.usage {
        first.push((
            format!("  {} in  {} out", tokens(u.input), tokens(u.output)),
            Ink::Dim,
        ));
    }
    let mut facts = vec![first];
    if let Some(line) = c.narrative.as_deref().and_then(|t| t.lines().next()) {
        facts.push(vec![(
            marion_core::text::visible(line).into_owned(),
            Ink::Plain,
        )]);
    }
    if let Some(b) = &c.branch {
        let mut l = vec![(b.clone(), Ink::Bold)];
        if let Some(diff) = c.diff {
            l.push((format!("  +{}", diff.added), Ink::Good));
            l.push((format!(" -{}", diff.removed), Ink::Bad));
            let s = if diff.files == 1 { "" } else { "s" };
            l.push((format!("  {} file{s}", diff.files), Ink::Dim));
        }
        facts.push(l);
    }
    facts
}

/// The selected node's output, as the preview draws it.
fn stream(home: &Home) -> Vec<StreamLine> {
    home.watch
        .stream
        .iter()
        .map(|l| StreamLine {
            clock: clock(&l.at, true),
            kind: match l.kind {
                ActionKind::Call => LineKind::Call,
                ActionKind::Files => LineKind::Files,
                ActionKind::Ended if crate::activity::succeeded(l) => LineKind::Done,
                ActionKind::Ended => LineKind::Failed,
                ActionKind::Said => LineKind::Said,
            },
            text: l.text.clone(),
        })
        .collect()
}

/// **A parent's subtree, in one line**: its tokens, files, agents and how many run, and its tree
/// budget where it has one — whether it is past the budget's warn line beside it. `None` for a
/// leaf without a budget, whose figures are its own row's.
fn subtree_line(
    home: &Home,
    n: &NodeSummary,
    now: std::time::SystemTime,
) -> Option<(String, bool)> {
    let t = home.watch.rollup.get(&n.agent_id)?;
    let budget = crate::rollup::budget_of(n, t);
    if !home.watch.rollup.has_children(&n.agent_id) && budget.is_none() {
        return None;
    }
    let mut parts = vec![if t.claimed > 0 {
        format!("{} tokens", tokens(t.tokens))
    } else {
        "tokens not reported".to_string()
    }];
    let s = if t.changed == 1 { "" } else { "s" };
    parts.push(format!("{} file{s}", t.changed));
    if let Some(wall) = t.wall_to(now) {
        parts.push(elapsed(wall.as_secs()));
    }
    parts.push(format!("{} agents, {} running", t.nodes, t.live));
    let over = budget.is_some_and(|(_, _, over)| over);
    if let Some((spent, limit, _)) = budget {
        parts.push(format!("budget {} of {}", tokens(spent), tokens(limit)));
    }
    Some((parts.join("  "), over))
}

/// `hh:mm[:ss]` out of a recorded RFC3339 timestamp, in the operator's local time.
fn clock(at: &str, with_seconds: bool) -> String {
    let offset = epoch_secs(at).map_or(0, local_offset);
    clock_in(at, offset, with_seconds)
}

/// `hh:mm[:ss]` of `at` shifted by `offset` seconds east of UTC; `at` itself when it is not a
/// timestamp marion recorded.
pub fn clock_in(at: &str, offset: i64, with_seconds: bool) -> String {
    match epoch_secs(at) {
        Some(secs) => clock_of(secs, offset, with_seconds),
        None => at.to_string(),
    }
}

/// `hh:mm[:ss]` of `secs` since the epoch, `offset` seconds east of UTC.
fn clock_of(secs: i64, offset: i64, with_seconds: bool) -> String {
    let day = (secs + offset).rem_euclid(86_400);
    let (h, m, s) = (day / 3600, day % 3600 / 60, day % 60);
    if with_seconds {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}")
    }
}

/// Seconds since the epoch of a recorded RFC3339 timestamp.
fn epoch_secs(at: &str) -> Option<i64> {
    let t: marion_core::encoding::SystemTime =
        serde_json::from_value(serde_json::Value::String(at.to_string())).ok()?;
    let d = t.0.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(d.as_secs()).ok()
}

/// The local zone's offset east of UTC, in seconds, at `epoch`: `localtime_r`'s `tm_gmtoff`,
/// which is the one place the operator's zone (and its daylight saving) is already decided.
fn local_offset(epoch: i64) -> i64 {
    // `struct tm` as macOS and glibc/musl both lay it out: nine ints, then `tm_gmtoff` and
    // `tm_zone`.
    #[repr(C)]
    struct Tm {
        ints: [std::ffi::c_int; 9],
        gmtoff: std::ffi::c_long,
        zone: *const std::ffi::c_char,
    }
    unsafe extern "C" {
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let mut tm = Tm {
        ints: [0; 9],
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: both pointers are live for the call; `localtime_r` writes only into `tm`. `time_t`
    // is a 64-bit signed integer on every target marion builds for.
    let ok = unsafe { !localtime_r(&epoch, &mut tm).is_null() };
    // `c_long` is `i64` on the 64-bit targets marion ships and narrower elsewhere.
    #[allow(clippy::useless_conversion)]
    let offset = if ok { i64::from(tm.gmtoff) } else { 0 };
    offset
}

// ---------------------------------------------------------------------------------- Setup

fn harness_row(h: &super::Harness) -> HarnessRow {
    HarnessRow {
        name: h.name.clone(),
        version: h.version.clone(),
        ready: h.ready,
        note: h.note.clone(),
        surfaces: h.surfaces.clone(),
        fix: h.fix.clone(),
        detail: h.detail.clone(),
    }
}

fn setup(home: &Home, places: &Places) -> SetupView {
    let ready_of = |harness: &str| {
        home.harnesses
            .iter()
            .find(|h| h.name == harness)
            .map_or(Ready::Absent, |h| h.ready)
    };
    SetupView {
        harnesses: home.harnesses.iter().map(harness_row).collect(),
        checking: home.checking,
        cursor: home.setup.cursor,
        expanded: home.setup.expanded,
        project: vec![
            ("checkout".into(), places.project.clone()),
            ("state".into(), short_place(&places.state)),
            ("agents".into(), crate::run::AGENT_TYPES_FILE.into()),
        ],
        agent_types: home
            .types
            .iter()
            .map(|t| AgentTypeRow {
                name: t.name.clone(),
                ready: ready_of(&t.harness),
                custom: t.custom,
            })
            .collect(),
        agents_file: crate::run::AGENT_TYPES_FILE.into(),
        validation: home.setup.validation.clone(),
        logins: home
            .setup
            .logins
            .iter()
            .map(|l| LoginRow {
                provider: l.provider.clone(),
                id: l.id.clone(),
                note: l.note.clone(),
            })
            .collect(),
        logins_note: logins_note(home),
        profiles: home.setup.profiles.iter().map(profile_row).collect(),
        profiles_note: profiles_note(home),
        notify: home
            .setup
            .notify
            .map(|on| (on, home.setup.notify_shown_by.clone())),
        form: form_view(home),
    }
}

/// The widest a path is shown on Setup: its start and its end, the middle elided.
const PLACE_W: usize = 36;

/// Every absolute path in `text` `~`-shortened and, past [`PLACE_W`], middle-elided: a state
/// directory under `/private/tmp/…` is recognisable by its ends.
pub fn short_place(text: &str) -> String {
    text.split(' ')
        .map(|w| {
            if w.starts_with('/') || w.starts_with('~') {
                marion_tui::home::text::clip_middle(&tilde(w), PLACE_W)
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The line under the stored keys: why they could not be listed, else where they are kept and
/// what the keys do. Subscription logins are never marion's: they stay with each harness's CLI.
fn logins_note(home: &Home) -> String {
    if let Some(e) = &home.setup.logins_error {
        return format!("keys could not be listed: {e}");
    }
    let Some(store) = &home.setup.store else {
        return "listing keys…".into();
    };
    let store = &short_place(store);
    if home.setup.logins.is_empty() {
        format!(
            "no API keys stored ({store}) · `a` adds one with `marion key add <provider>`; \
             subscription logins stay with each harness's own CLI"
        )
    } else {
        format!("{store} · `a` adds a key · `x` removes the selected one")
    }
}

/// One profile as Setup draws it: its login in the probe's words, and — while it is logged out —
/// the command that logs in to it, for the operator to run.
fn profile_row(p: &super::StoredProfile) -> ProfileRow {
    use crate::profiles::LoginState;
    let (ready, note) = match &p.login {
        None => (Ready::Checking, "checking…".to_string()),
        Some(LoginState::LoggedIn) => (Ready::Ready, "logged in".to_string()),
        Some(LoginState::LoggedOut) => (Ready::Attention, "logged out".to_string()),
        Some(LoginState::Unknown(why)) => (Ready::Attention, format!("login unknown: {why}")),
    };
    ProfileRow {
        harness: p.harness.clone(),
        name: p.name.clone(),
        default: p.default,
        login: (ready == Ready::Attention).then(|| p.login_command.clone()),
        ready,
        note,
        limit: p.limit.clone(),
    }
}

/// The line under the profiles: why they could not be listed, else what the keys do. marion
/// never logs in for anyone: `add` prints the command, and the row shows it.
fn profiles_note(home: &Home) -> String {
    if let Some(e) = &home.setup.profiles_error {
        return format!("logins could not be listed: {e}");
    }
    if !home.setup.profiles_listed {
        return "listing logins…".into();
    }
    if home.setup.profiles.is_empty() {
        "each harness uses its own login · `a` runs `marion profile add`, then you run the login it prints"
            .into()
    } else {
        "`a` adds one · `u` makes the selected the default · `x` removes it, keeping its \
         directory · marion never logs in for you"
            .into()
    }
}

/// The agent-type form, while it is open or its preview waits for a `y`.
fn form_view(home: &Home) -> Option<FormView> {
    let (form, preview) = match &home.mode {
        Mode::Form(f) => (f.clone(), Vec::new()),
        Mode::Confirm(Effect::WriteTypes { draft, .. }) => (
            super::Form {
                draft: draft.clone(),
                field: Field::ALL.len() - 1,
                error: None,
            },
            home.setup.preview.clone(),
        ),
        _ => return None,
    };
    let d = &form.draft;
    let fields = Field::ALL
        .iter()
        .map(|f| {
            let (value, hint) = match f {
                Field::Name => (d.name.clone(), "letters, digits, - and _"),
                Field::Harness => (d.harness.clone(), ""),
                Field::Model => (d.model.clone(), "the harness's own default"),
                Field::Description => (d.description.clone(), "one line: what it is for"),
                Field::Tools => (d.tools.word().to_string(), ""),
                Field::Provider => (d.provider.clone(), "optional: an endpoint provider id"),
            };
            FormField {
                label: f.label().to_string(),
                value,
                choice: f.is_choice(),
                hint: hint.to_string(),
            }
        })
        .collect();
    Some(FormView {
        title: "Agent type".into(),
        fields,
        field: form.field,
        error: form.error,
        file: crate::run::AGENT_TYPES_FILE.into(),
        preview,
    })
}

// ---------------------------------------------------------------------------------- keys

/// One key: what to press, what it does, the command it stands for (empty when none).
pub type KeyRow3 = (&'static str, &'static str, &'static str);

/// **Every key, by page, with the command it stands for**: the `?` page reads this, and
/// [`super::Home::key`] is what it describes — the state machine's tests press each one, and the
/// footer offers no key this table does not list.
pub const KEYS: &[(&str, &[KeyRow3])] = &[
    (
        "agents",
        &[
            ("j/k", "move", ""),
            ("enter", "attach (pane); ^] d back", "marion attach <id>"),
            ("enter", "full output (headless)", "marion ls <id>"),
            ("n", "new task", "marion run <type> --prompt <text>"),
            ("s", "steer a running agent", "marion steer <id> <text>"),
            ("x", "cancel, asks; again kills", "marion cancel <id>"),
            ("u", "resume an ended agent", "marion resume <id>"),
            ("o", "shell in its workspace", ""),
            ("d", "what its branch changed", "git log -p HEAD..<branch>"),
            (
                "m",
                "merge its branch, asks",
                "git merge --no-ff -- <branch>",
            ),
            ("pgup/dn", "scroll output; end follows", ""),
            (",", "setup", ""),
            ("?", "this page", ""),
            ("q", "quit", ""),
        ],
    ),
    (
        "input line",
        &[
            ("tab", "next agent type", ""),
            ("↑↓", "the type's models", "--model <m>"),
            ("enter", "run, send or add", ""),
            ("esc", "close it", ""),
        ],
    ),
    (
        "setup",
        &[
            ("enter", "a harness's detail and fix", ""),
            ("r", "re-check", "marion doctor"),
            ("e", "edit agent types", "$EDITOR .marion/agents.toml"),
            ("n", "new agent type, previewed", ".marion/agents.toml"),
            ("a", "add a key, or a login", "marion key add <provider>"),
            ("u", "use a login", "marion profile use <harness> <name>"),
            ("x", "remove key or login, asks", "marion key rm <id>"),
            ("esc", "back", ""),
        ],
    ),
];

/// The key table, the page help was opened from first.
fn help(home: &Home) -> HelpView {
    let first = match home.help_from {
        Page::Setup => "setup",
        _ => "agents",
    };
    let mut order: Vec<&(&str, &[KeyRow3])> = KEYS.iter().collect();
    order.sort_by_key(|(title, _)| *title != first);
    HelpView {
        scroll: home.help_scroll,
        sections: order
            .into_iter()
            .map(|(title, keys)| {
                (
                    title.to_string(),
                    keys.iter()
                        .map(|(k, v, c)| KeyRow {
                            key: k.to_string(),
                            verb: v.to_string(),
                            command: c.to_string(),
                        })
                        .collect(),
                )
            })
            .collect(),
    }
}

/// The footer: the line being typed, the confirm, or the keys the selection takes — each offered
/// only where it does something, so a key the footer shows is never refused.
fn footer(home: &Home) -> Footer {
    let h = |pairs: &[(&str, &str)]| -> Vec<Hint> {
        pairs.iter().map(|(k, v)| Hint::new(*k, *v)).collect()
    };
    match &home.mode {
        Mode::Typing { purpose, text } => {
            let (target, note, keys) = match purpose {
                Purpose::New => {
                    let target = match home.new_type() {
                        Some(t) => format!("new {}", t.name),
                        None => "new".to_string(),
                    };
                    let note = home
                        .new_blocked()
                        .or_else(|| home.new_model())
                        .unwrap_or_default();
                    (
                        target,
                        note,
                        h(&[("tab", "type"), ("↑↓", "model"), ("enter", "run")]),
                    )
                }
                Purpose::Steer(id) => (
                    format!("steer {}", who_short(home, id)),
                    String::new(),
                    h(&[("enter", "send"), ("esc", "cancel")]),
                ),
                Purpose::Key => (
                    "key add".to_string(),
                    String::new(),
                    h(&[("enter", "add"), ("esc", "cancel")]),
                ),
                Purpose::Profile => (
                    "profile add".to_string(),
                    String::new(),
                    h(&[("enter", "add"), ("esc", "cancel")]),
                ),
            };
            Footer::Input {
                target,
                note,
                text: text.clone(),
                keys,
            }
        }
        Mode::Confirm(effect) => confirm(home, effect),
        Mode::Form(_) => Footer::Keys(h(&[
            ("tab", "next field"),
            ("←→", "choose"),
            ("enter", "preview"),
            ("esc", "close"),
        ])),
        Mode::Normal => Footer::Keys(match home.page {
            Page::Agents => agent_keys(home),
            Page::Setup => setup_keys(home),
            Page::Help => h(&[("esc", "back"), ("j/k", "scroll")]),
        }),
    }
}

/// `codex 91cd`: a node as the input and confirm lines name it.
fn who_short(home: &Home, id: &AgentId) -> String {
    home.watch
        .nodes
        .iter()
        .find(|n| n.agent_id == *id)
        .map_or_else(
            || short_id(&id.0).to_string(),
            |n| format!("{} {}", n.harness.cli_name(), short_id(&n.agent_id.0)),
        )
}

fn can(n: &NodeSummary, action: &str) -> bool {
    crate::tree::actions_for(n)
        .iter()
        .any(|a| a.name == action && a.available)
}

/// The agents page's keys for the selection.
fn agent_keys(home: &Home) -> Vec<Hint> {
    let mut k: Vec<(&str, &str)> = Vec::new();
    let more = ("?", "more");
    if home.watch.nodes.is_empty() {
        return [("n", "new"), (",", "setup"), more]
            .iter()
            .map(|(a, b)| Hint::new(*a, *b))
            .collect();
    }
    if home.selected_race().is_some() {
        k.extend([("enter", "winner"), ("m", "merge"), ("n", "new")]);
    } else if home.selected_workflow().is_some() {
        k.extend([("enter", "step"), ("n", "new")]);
    } else if let Some(n) = home.selected() {
        let landed = home.branch().is_some();
        if home.watch.full {
            k.push(("esc", "back"));
        } else {
            k.push(("enter", if n.pane { "attach" } else { "output" }));
            k.push(("n", "new"));
        }
        if n.state.is_exited() {
            if landed {
                k.extend([("m", "merge"), ("d", "diff")]);
            }
            if can(n, "resume") {
                k.push(("u", "resume"));
            }
            k.push(("o", "shell"));
        } else {
            if can(n, "steer") {
                k.push(("s", "steer"));
            }
            k.push(("x", "cancel"));
        }
    }
    k.push(more);
    k.into_iter().map(|(a, b)| Hint::new(a, b)).collect()
}

/// Setup's keys for the row under the cursor, and nothing that does something else there.
fn setup_keys(home: &Home) -> Vec<Hint> {
    let pairs: &[(&str, &str)] = if home.on_notify() {
        if home.setup.notify == Some(true) {
            &[("enter", "turn off"), ("esc", "back")]
        } else {
            &[("enter", "turn on"), ("esc", "back")]
        }
    } else if home.on_profiles() {
        &[
            ("a", "add login"),
            ("u", "use"),
            ("x", "remove"),
            ("esc", "back"),
        ]
    } else if home.selected_login().is_some() {
        &[("a", "add key"), ("x", "remove key"), ("esc", "back")]
    } else {
        &[
            ("enter", "details"),
            ("r", "re-check"),
            ("e", "edit types"),
            ("n", "new type"),
            ("a", "add key"),
            ("esc", "back"),
        ]
    };
    pairs.iter().map(|(a, b)| Hint::new(*a, *b)).collect()
}

/// The confirm line for `effect`: the question in a few words, and the command it runs.
fn confirm(home: &Home, effect: &Effect) -> Footer {
    let line = |e: &Effect| {
        e.argv()
            .map(|a| shell_line(&a, usize::MAX))
            .unwrap_or_default()
    };
    let (question, command) = match effect {
        Effect::WriteTypes { draft, .. } => {
            let added = home.setup.preview.iter().filter(|(t, _)| *t == '+').count();
            let removed = home.setup.preview.iter().filter(|(t, _)| *t == '-').count();
            (
                format!("write {} to {}?", draft.name, crate::run::AGENT_TYPES_FILE),
                format!("+{added} -{removed} lines"),
            )
        }
        Effect::Logout(id) => (format!("remove the key {id}?"), line(effect)),
        Effect::Merge(branch) => (format!("merge {branch} into your checkout?"), line(effect)),
        Effect::ProfileRemove { name, .. } => (format!("remove the profile {name}?"), line(effect)),
        Effect::Kill(id) => (
            format!("cancel pending; kill {} now?", who_short(home, id)),
            line(effect),
        ),
        Effect::Cancel(id) => {
            let below = live_below(&home.watch.nodes, id);
            let who = who_short(home, id);
            let q = if below > 0 {
                format!("cancel {who} and the {below} below it?")
            } else {
                format!("cancel {who}?")
            };
            (q, line(effect))
        }
        other => ("go ahead?".to_string(), line(other)),
    };
    Footer::Confirm { question, command }
}

/// How many live nodes are below `id` — what a cancel of `id` ends with it.
fn live_below(nodes: &[NodeSummary], id: &AgentId) -> usize {
    let mut frontier = vec![id.clone()];
    let mut count = 0;
    while let Some(parent) = frontier.pop() {
        for n in nodes
            .iter()
            .filter(|n| n.parent_id.as_ref() == Some(&parent))
        {
            frontier.push(n.agent_id.clone());
            if !n.state.is_exited() {
                count += 1;
            }
        }
    }
    count
}
