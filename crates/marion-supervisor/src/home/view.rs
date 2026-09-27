//! A [`Home`] as `marion_tui::home`'s views: every word the screen shows is decided here, from the
//! wire types only the supervisor can read, and `marion-tui` only draws it.

use super::{AgentType, Effect, Home, Mode};
use crate::tree::{attention_of, short_id};
use marion_core::contract::{ExitStatus, Workspace};
use marion_core::proto::NodeSummary;
use marion_core::proto::result::{ActionKind, ActionLine, NodeDetail};
use marion_tui::home::help::KeyRow;
use marion_tui::home::text::shell_line;
use marion_tui::home::{
    AgentTypeRow, Body, Expanded, FeedRow, HarnessRow, HelpView, Hint, Input, MessageView, NodeRow,
    Ready, RecentRow, ResultView, SetupView, StartView, StreamLine, Tab, TaskView, TokenView,
    WatchView,
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
            when: short_id(&n.agent_id.0).to_string(),
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
        NodeState::Exited(ExitStatus::Ok) => "done".into(),
        NodeState::Exited(s) => format!("{s:?}").to_lowercase(),
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
            kind: n.name.clone().unwrap_or_else(|| kind_of(n)),
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

/// The row's time: how long a live node has been running. Blank where no start was recorded.
fn row_time(n: &NodeSummary, now: std::time::SystemTime) -> String {
    let Some(started) = n.started_at else {
        return String::new();
    };
    if n.state.is_exited() {
        return String::new();
    }
    now.duration_since(started.0)
        .map(|d| elapsed_word(d.as_secs()))
        .unwrap_or_default()
}

/// A duration as few characters as it needs: `45s`, `2m17s`, `12m04s`, `1h05m`, `2d03h`. The
/// unit below the largest is zero-padded so a ticking column does not jitter.
pub fn elapsed_word(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m{s:02}s"),
        (0, _, _) => format!("{h}h{m:02}m"),
        _ => format!("{d}d{h:02}h"),
    }
}

/// What the row says after the harness: the agent type with the harness's own name taken off —
/// nothing for a plain type (`codex` on codex), `orchestrator` for `claude-orchestrator`, the whole
/// type for a custom one.
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
    if let Some(last) = stream.and_then(|s| s.last()) {
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
            ..Default::default()
        };
    };
    Expanded {
        live,
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
                said: l.kind == ActionKind::Said,
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
            let status = format!("{:?}", c.status).to_lowercase();
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
                merge: c.branch.as_ref().map(|b| format!("git merge --no-ff {b}")),
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
            ("repo".into(), places.project.clone()),
            ("state".into(), places.state.clone()),
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
        logins: None,
    }
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
            ("!", "next that needs you", "marion list --attention"),
            ("J/K", "scroll its stream", ""),
        ],
    ),
    (
        "Setup",
        &[
            ("enter", "expand", ""),
            ("r", "re-check", "marion doctor"),
            ("e", "edit agent types", "$EDITOR .marion/agents.toml"),
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
        (_, Tab::Setup) => h(&[
            ("j/k", "move"),
            ("enter", "expand"),
            ("r", "re-check"),
            ("e", "edit types"),
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
        Mode::Confirm(effect) => {
            let who = home
                .watch
                .nodes
                .iter()
                .find(|n| matches!(effect, Effect::Cancel(id) if *id == n.agent_id))
                .map_or_else(String::new, |n| {
                    format!(" {} {}", n.agent_type, short_id(&n.agent_id.0))
                });
            Input::Confirm {
                question: format!("Cancel{who}?"),
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
            Tab::Setup => Input::Command {
                line: line(&Effect::Recheck),
                note: "r".into(),
            },
            Tab::Help => Input::Command {
                line: "marion --help".into(),
                note: String::new(),
            },
        },
    }
}
