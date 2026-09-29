//! A [`Home`] as `marion_tui::home`'s views: every word the screen shows is decided here, from the
//! wire types only the supervisor can read, and `marion-tui` only draws it.

use super::{AgentType, Effect, Field, Home, Mode};
use crate::tree::{attention_of, short_id};
use marion_core::contract::{AgentId, ExitStatus, Workspace};
use marion_core::proto::NodeSummary;
use marion_core::proto::result::{ActionKind, ActionLine, NodeDetail};
use marion_tui::home::help::KeyRow;
use marion_tui::home::text::shell_line;
use marion_tui::home::{
    AgentTypeRow, Body, Expanded, FeedRow, FormField, FormView, HarnessRow, HelpView, Hint, Input,
    LineKind, LoginRow, MessageView, NodeRow, ProfileRow, Ready, RecentRow, ResultView, SetupView,
    StartView, StreamLine, Tab, TaskView, TokenView, WatchView,
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
    pub start: StartView,
    pub watch: WatchView,
    pub setup: SetupView,
    pub help: HelpView,
    pub input: Input,
    pub hints: Vec<Hint>,
    pub attention: usize,
}

impl Frame {
    pub fn body(&self, tab: Tab) -> Body<'_> {
        match tab {
            Tab::Start => Body::Start(&self.start),
            Tab::Watch => Body::Watch(&self.watch),
            Tab::Setup => Body::Setup(&self.setup),
            Tab::Help => Body::Help(&self.help),
        }
    }
}

/// One frame of `home`, now.
pub fn frame(home: &Home, places: &Places) -> Frame {
    frame_at(home, places, std::time::SystemTime::now())
}

/// One frame of `home` as of `now`, which elapsed times are worded against.
pub fn frame_at(home: &Home, places: &Places, now: std::time::SystemTime) -> Frame {
    Frame {
        start: start(home),
        watch: watch(home, now),
        setup: setup(home, places),
        help: help(),
        input: input(home),
        hints: hints(home),
        attention: crate::tree::attention_count(&home.watch.nodes),
    }
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

// ---------------------------------------------------------------------------------- Start

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

fn grant(t: &AgentType) -> &'static str {
    if t.writes {
        "edits in a worktree"
    } else {
        "read-only, delegates"
    }
}

fn start(home: &Home) -> StartView {
    let echo = match home.pending_run() {
        Some(Effect::Run {
            agent_type,
            model,
            prompt,
            pane,
        }) => Effect::Run {
            agent_type,
            model,
            pane,
            prompt: if prompt.trim().is_empty() {
                "<text>".into()
            } else {
                prompt
            },
        }
        .argv()
        .unwrap_or_default(),
        _ => Vec::new(),
    };
    StartView {
        harnesses: home
            .start_harnesses()
            .into_iter()
            .map(harness_row)
            .collect(),
        checking: home.checking,
        cursor: home.start.cursor,
        kinds: home
            .kinds()
            .into_iter()
            .map(|t| (t.name.clone(), grant(t).to_string()))
            .collect(),
        kind: home.start.kind,
        models: home.models(),
        model: home.start.model,
        options: vec![(
            "surface".into(),
            if home.start.pane { "pane" } else { "headless" }.into(),
        )],
        recent: recent(&home.watch.nodes),
        echo,
    }
}

/// The roots this supervisor holds, most recent first.
fn recent(nodes: &[NodeSummary]) -> Vec<RecentRow> {
    nodes
        .iter()
        .rev()
        .filter(|n| n.parent_id.is_none())
        .map(|n| RecentRow {
            // When it started, in local time; its short id where no start was recorded.
            when: n
                .started_at
                .map_or_else(|| short_id(&n.agent_id.0).to_string(), |t| local_clock(t.0)),
            tone: crate::tree::row(n).tone,
            who: n.agent_type.clone(),
            prompt: n.name.clone().unwrap_or_default(),
            outcome: outcome_word(n),
        })
        .collect()
}

fn outcome_word(n: &NodeSummary) -> String {
    use marion_core::node::NodeState;
    match n.state {
        NodeState::Exited(s) => s.word().into(),
        NodeState::Blocked(_) => "blocked".into(),
        _ => "running".into(),
    }
}

// ---------------------------------------------------------------------------------- Watch

fn watch(home: &Home, now: std::time::SystemTime) -> WatchView {
    let w = &home.watch;
    let mut rows = Vec::new();
    let mut cursor = 0;
    for i in 0..w.tree.rows() {
        let Some((prefix, tnode)) = w.tree.row(i) else {
            break;
        };
        if let Some(race) = crate::tree::race_of_row(&tnode.id) {
            if i == w.tree.cursor() {
                cursor = rows.len();
            }
            rows.push(race_row(prefix, tnode, race, &w.nodes));
            continue;
        }
        let Some(n) = w.nodes.iter().find(|n| n.agent_id.0 == tnode.id) else {
            continue;
        };
        let selected = i == w.tree.cursor();
        if selected {
            cursor = rows.len();
        }
        let detail = match &w.detail {
            Some((id, d)) if *id == n.agent_id => Some(d),
            _ => None,
        };
        rows.push(NodeRow {
            id: n.agent_id.0.clone(),
            prefix: prefix.to_string(),
            harness: n.harness.cli_name().to_string(),
            // An endpoint node's `provider:model` rides the kind, which is what gives way first on
            // a narrow window.
            kind: {
                let kind = n
                    .name
                    .clone()
                    .or_else(|| crate::tree::review_note(n))
                    .or_else(|| {
                        crate::tree::race_note(n).map(|seat| format!("{} · {seat}", kind_of(n)))
                    })
                    .unwrap_or_else(|| kind_of(n));
                match &n.endpoint {
                    Some(e) => format!("{kind} {}", e.label()).trim().to_string(),
                    None => kind,
                }
            },
            short: short_id(&n.agent_id.0).to_string(),
            tone: tnode.tone,
            elapsed: row_time(n, now),
            tokens: n
                .tokens
                .or_else(|| detail.and_then(|d| d.usage).map(|u| u.total())),
            doing: doing(n, detail, selected.then_some(&w.stream[..])),
        });
    }
    WatchView {
        supervisor: w.supervisor,
        rows,
        cursor,
        expanded: home.selected().map(|n| expanded(home, n)),
        running: crate::tree::running(&w.nodes),
        attention: crate::tree::attention_count(&w.nodes),
        attention_note: w
            .nodes
            .iter()
            .filter(|n| attention_of(n).is_some())
            .find_map(|n| n.attention.clone()),
        feed: w
            .feed
            .iter()
            .map(|f| FeedRow {
                clock: local_clock(f.at),
                tone: f.tone,
                short: f.short.clone(),
                text: f.text.clone(),
            })
            .collect(),
        filter: None,
    }
}

/// The row's time: how long a live node has been running, or how long ago an ended one ended.
/// Blank where no start (or end) was recorded.
fn row_time(n: &NodeSummary, now: std::time::SystemTime) -> String {
    let Some(started) = n.started_at else {
        return String::new();
    };
    if n.state.is_exited() {
        // How long ago it ended, where the end was recorded.
        return n
            .ended_at
            .and_then(|e| now.duration_since(e.0).ok())
            .map(|d| ago(d.as_secs()))
            .unwrap_or_default();
    }
    now.duration_since(started.0)
        .map(|d| marion_tui::home::text::elapsed(d.as_secs()))
        .unwrap_or_default()
}

/// How long ago, in its largest unit only (`12m ago`, `1h ago`): an ended node's time needs no
/// seconds, and it fits the row's time column.
fn ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// What the row says after the harness: the agent type with the harness's own name taken off —
/// nothing for a plain type (`codex` on codex), `orchestrator` for `claude-orchestrator`, the whole
/// type for a custom one.
/// **A race's header row**, summed from its seats: no node stands behind it, so it has no harness,
/// no elapsed time of its own and nothing to attach to. Its tokens are the seats' so far, live.
fn race_row(
    prefix: &str,
    tnode: &marion_tui::tree::Node,
    race: &str,
    nodes: &[NodeSummary],
) -> NodeRow {
    let summary = crate::tree::RaceSummary::of(&marion_core::race::RaceId(race.to_string()), nodes);
    NodeRow {
        id: tnode.id.clone(),
        prefix: prefix.to_string(),
        harness: "race".into(),
        kind: summary.label(),
        short: short_id(race).to_string(),
        tone: tnode.tone,
        elapsed: String::new(),
        tokens: summary.tokens,
        doing: match &summary.decided {
            Some(Some(winner)) => format!("{winner} won; every seat's branch is kept"),
            Some(None) => "no seat won; every seat's branch is kept".into(),
            None => format!("{} of {} seats finished", summary.ended, summary.seats),
        },
    }
}

fn kind_of(n: &NodeSummary) -> String {
    let harness = n.harness.cli_name().to_string();
    let t = n.agent_type.as_str();
    let head = t.split('-').next().unwrap_or(t);
    if !harness.starts_with(head) {
        return t.to_string();
    }
    t.strip_prefix(head)
        .map(|rest| rest.trim_start_matches('-').to_string())
        .unwrap_or_default()
}

/// The row's one line: what needs the operator, else what it landed, else the last thing it did,
/// else its state.
fn doing(n: &NodeSummary, detail: Option<&NodeDetail>, stream: Option<&[ActionLine]>) -> String {
    if let Some(why) = attention_of(n) {
        return why;
    }
    if let Some(b) = detail
        .and_then(|d| d.completion.as_ref())
        .and_then(|c| c.branch.as_ref())
    {
        return format!("landed {b}");
    }
    // The last thing it started, not an end: `✓ 3.2s` alone says nothing about what ran.
    if let Some(last) = stream.and_then(|s| s.iter().rev().find(|l| l.kind != ActionKind::Ended)) {
        return last.text.clone();
    }
    crate::tree::row(n).state
}

/// `hh:mm[:ss]` out of a recorded RFC3339 timestamp, in the operator's local time.
fn clock(at: &str, with_seconds: bool) -> String {
    let offset = epoch_secs(at).map_or(0, local_offset);
    clock_in(at, offset, with_seconds)
}

/// `hh:mm` of a moment, in local time.
fn local_clock(at: std::time::SystemTime) -> String {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    clock_of(secs, local_offset(secs), false)
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

/// The capabilities Watch's keys stand on, in the order the keys are offered (`s`, `u`), each
/// with whether marion measured it for this node: the full list is `marion doctor`'s to print.
const HOME_CAPS: &[&str] = &["steer", "resume", "interrupt"];

fn home_caps(n: &NodeSummary) -> Vec<(String, bool)> {
    let actions = crate::tree::actions_for(n);
    HOME_CAPS
        .iter()
        .filter_map(|want| actions.iter().find(|a| a.name == *want))
        .map(|a| (a.name.clone(), a.available))
        .collect()
}

/// `provider:model · route` for an endpoint node's expansion.
fn endpoint_of(n: &NodeSummary) -> Option<String> {
    n.endpoint.as_ref().map(|e| match &e.route {
        Some(r) => format!("{} · {r}", e.label()),
        None => e.label(),
    })
}

fn expanded(home: &Home, n: &NodeSummary) -> Expanded {
    let live = !n.state.is_exited();
    let needs = matches!(n.state, marion_core::node::NodeState::Blocked(_))
        .then(|| attention_of(n))
        .flatten();
    let caps = home_caps(n);
    let Some(d) = (match &home.watch.detail {
        Some((id, d)) if *id == n.agent_id => Some(d),
        _ => None,
    }) else {
        return Expanded {
            live,
            needs,
            stream_unread: Some("reading…".into()),
            caps,
            endpoint: endpoint_of(n),
            ..Default::default()
        };
    };
    Expanded {
        live,
        endpoint: endpoint_of(n),
        task: d.task.as_ref().map(|t| TaskView {
            prompt: t.prompt.clone(),
            // marion's own report instruction, named rather than quoted: the sentence is the same
            // on every node and pushed the operator's criteria off a short screen.
            appended: t
                .appended
                .as_ref()
                .map(|_| "report when finished".to_string()),
            acceptance: t.acceptance.clone(),
            verification: t.verification.clone(),
        }),
        messages: d
            .messages
            .iter()
            .map(|m| MessageView {
                clock: clock(&m.at, false),
                text: format!("{} · {} bytes · {}", m.from, m.len, m.outcome),
            })
            .collect(),
        stream: home
            .watch
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
            .collect(),
        stream_scroll: home.watch.scroll,
        stream_unread: d.stream.as_ref().and_then(|p| p.unread.clone()),
        needs,
        tokens: d.usage.map(|u| TokenView {
            input: u.input,
            output: u.output,
            cached: u.cache_read,
            // Per turn where the harness reports usage per turn; empty draws no sparkline.
            rate: d.turns.clone(),
            window: "turn".into(),
            context_permille: None,
        }),
        result: d.completion.as_ref().map(|c| {
            let tone = match c.status {
                ExitStatus::Ok => Tone::Done,
                ExitStatus::Cancelled | ExitStatus::Unreported => Tone::Unknown,
                _ => Tone::Failed,
            };
            let status = c.status.word().to_string();
            let summary = match &c.narrative {
                Some(n) => format!("{status}: {}", n.lines().next().unwrap_or("")),
                None => status,
            };
            ResultView {
                summary,
                tone: Some(tone),
                branch: c.branch.clone(),
                added: c.diff.map_or(0, |d| d.added),
                removed: c.diff.map_or(0, |d| d.removed),
                files: c.diff.map_or_else(
                    || u32::try_from(c.changed_paths).unwrap_or(u32::MAX),
                    |d| d.files,
                ),
                merge: c
                    .branch
                    .as_deref()
                    .map(marion_core::contract::merge_command),
            }
        }),
        caps,
        // Its branch, not the long state path its worktree sits at: `o` opens a shell there.
        workspace: d.workspace.as_ref().map(|w| match w {
            Workspace::Worktree { branch, .. } => format!("worktree {branch}"),
            Workspace::SharedCwd { path } => format!(
                "the checkout {}",
                project_label(
                    &path.to_string_lossy(),
                    std::env::var("HOME").ok().as_deref(),
                    None
                )
            ),
        }),
    }
}

// ---------------------------------------------------------------------------------- Setup

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
            "no API keys stored ({store}) · `a` adds one with `marion login <provider>`; \
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
        return format!("profiles could not be listed: {e}");
    }
    if !home.setup.profiles_listed {
        return "listing profiles…".into();
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

/// Every key, by screen, with the command it stands for: the `?` page reads this, and
/// [`super::Home::key`] is what it describes — the state machine's tests press each one.
/// One key: what to press, what it does, the command it stands for (empty when none).
pub type KeyRow3 = (&'static str, &'static str, &'static str);

pub const KEYS: &[(&str, &[KeyRow3])] = &[
    (
        "Everywhere",
        &[
            ("tab", "next screen", ""),
            ("^c", "quit", ""),
            ("q", "quit, off Start", ""),
        ],
    ),
    (
        "Start",
        &[
            ("enter", "run", "marion run <type> --prompt <text> --detach"),
            ("↑↓", "harness", ""),
            ("←→", "model", ""),
            ("^o", "read-only flavour", ""),
            ("^p", "headless or pane", "--pane"),
            ("esc", "clear the prompt", ""),
        ],
    ),
    (
        "Watch",
        &[
            ("j/k", "move", ""),
            ("enter", "attach", "marion attach <id>"),
            ("s", "steer", "marion steer <id> <text>"),
            ("x", "cancel, asks first", "marion cancel <id>"),
            ("u", "resume an ended node", "marion resume <id>"),
            ("o", "shell in its workspace", ""),
            ("d", "what its branch changed", "git log -p HEAD..<branch>"),
            ("c", "copy the merge", "git merge --no-ff <branch>"),
            ("!", "next that needs you", "marion ls --attention"),
            ("J/K", "scroll its stream", ""),
        ],
    ),
    (
        "Setup",
        &[
            ("enter", "expand", ""),
            ("r", "re-check", "marion doctor"),
            ("e", "edit agent types", "$EDITOR .marion/agents.toml"),
            ("n", "new agent type, previewed", ".marion/agents.toml"),
            ("a", "add a provider key", "marion login <provider>"),
            ("x", "remove a key, asks first", "marion logout <id>"),
            (
                "a",
                "on Profiles: add one",
                "marion profile add <harness> <name>",
            ),
            ("u", "use a profile", "marion profile use <harness> <name>"),
            (
                "x",
                "on Profiles: remove one, asks first",
                "marion profile remove <name>",
            ),
        ],
    ),
];

fn help() -> HelpView {
    HelpView {
        sections: KEYS
            .iter()
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

fn hints(home: &Home) -> Vec<Hint> {
    let h = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| Hint::new(*k, *v)).collect();
    match (&home.mode, home.tab) {
        (Mode::Compose { .. }, _) => h(&[("enter", "send"), ("esc", "cancel")]),
        (Mode::Confirm(_), _) => h(&[("y", "yes"), ("any key", "no")]),
        (Mode::Login { .. }, _) => h(&[("enter", "log in"), ("esc", "cancel")]),
        (Mode::Profile { .. }, _) => h(&[("enter", "add"), ("esc", "cancel")]),
        (Mode::Form(_), _) => h(&[
            ("tab", "next field"),
            ("←→", "choose"),
            ("enter", "preview"),
            ("esc", "close"),
        ]),
        (_, Tab::Start) => h(&[
            ("enter", "run"),
            ("↑↓", "harness"),
            ("←→", "model"),
            ("^o", "read-only"),
            ("tab", "screens"),
        ]),
        (_, Tab::Watch) => h(&[
            ("j/k", "move"),
            ("enter", "attach"),
            ("s", "steer"),
            ("x", "cancel"),
            ("u", "resume"),
            ("c", "copy merge"),
            ("!", "needs you"),
        ]),
        (_, Tab::Setup) if home.on_profiles() => h(&[
            ("j/k", "move"),
            ("a", "add profile"),
            ("u", "use"),
            ("x", "remove"),
            ("r", "re-check"),
        ]),
        (_, Tab::Setup) => h(&[
            ("j/k", "move"),
            ("enter", "expand"),
            ("r", "re-check"),
            ("n", "new type"),
            ("a", "add key"),
            ("x", "remove key"),
        ]),
        (_, Tab::Help) => h(&[("tab", "next screen"), ("esc", "back")]),
    }
}

fn input(home: &Home) -> Input {
    let line = |e: &Effect| {
        e.argv()
            .map(|a| shell_line(&a, usize::MAX))
            .unwrap_or_default()
    };
    match &home.mode {
        Mode::Compose { target, text } => Input::Compose {
            target: format!("steer {}", short_id(&target.0)),
            text: text.clone(),
        },
        Mode::Login { text } => Input::Compose {
            target: "marion login".into(),
            text: text.clone(),
        },
        Mode::Profile { text } => Input::Compose {
            target: "marion profile add".into(),
            text: text.clone(),
        },
        Mode::Form(_) => Input::Command {
            line: crate::run::AGENT_TYPES_FILE.into(),
            note: "enter previews".into(),
        },
        Mode::Confirm(Effect::WriteTypes { draft, .. }) => {
            let added = home.setup.preview.iter().filter(|(t, _)| *t == '+').count();
            let removed = home.setup.preview.iter().filter(|(t, _)| *t == '-').count();
            Input::Confirm {
                question: format!("Write {} to {}?", draft.name, crate::run::AGENT_TYPES_FILE),
                command: format!("+{added} −{removed} lines"),
            }
        }
        Mode::Confirm(effect @ Effect::Logout(id)) => Input::Confirm {
            question: format!("Remove the key {id}?"),
            command: line(effect),
        },
        Mode::Confirm(effect @ Effect::ProfileRemove(name)) => Input::Confirm {
            question: format!("Remove the profile {name}?"),
            command: line(effect),
        },
        Mode::Confirm(effect) => {
            let target = match effect {
                Effect::Cancel(id) | Effect::Kill(id) => Some(id),
                _ => None,
            };
            let who = home
                .watch
                .nodes
                .iter()
                .find(|n| target == Some(&n.agent_id))
                .map_or_else(String::new, |n| {
                    format!(" {} {}", n.agent_type, short_id(&n.agent_id.0))
                });
            let below = target.map_or(0, |id| live_below(&home.watch.nodes, id));
            let question = match effect {
                Effect::Kill(_) => format!("Kill{who} now? Its cancel is still waiting on it"),
                _ if below > 0 => format!("Cancel{who} and the {below} below it?"),
                _ => format!("Cancel{who}?"),
            };
            Input::Confirm {
                question,
                command: line(effect),
            }
        }
        Mode::Normal => match home.tab {
            Tab::Start => Input::Prompt {
                text: home.start.prompt.clone(),
                placeholder: "describe the task; enter runs it".into(),
                focused: true,
            },
            Tab::Watch => Input::Command {
                line: line(&home.watch_default()),
                note: if home.selected().is_some() {
                    "enter".into()
                } else {
                    String::new()
                },
            },
            Tab::Setup => match (home.selected_login(), home.selected_profile()) {
                (Some(l), _) => Input::Command {
                    line: line(&Effect::Logout(l.id.clone())),
                    note: "x".into(),
                },
                (_, Some(p)) => Input::Command {
                    line: line(&Effect::ProfileUse {
                        harness: p.harness.clone(),
                        name: p.name.clone(),
                    }),
                    note: "u".into(),
                },
                _ if home.on_profiles() => Input::Command {
                    line: "marion profile add <harness> <name>".into(),
                    note: "a".into(),
                },
                _ => Input::Command {
                    line: line(&Effect::Recheck),
                    note: "r".into(),
                },
            },
            Tab::Help => Input::Command {
                line: "marion --help".into(),
                note: String::new(),
            },
        },
    }
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
