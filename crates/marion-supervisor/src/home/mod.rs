//! **The home screen**: what bare `marion` and `marion ls` open on a terminal — the agents in a
//! list, the selected one previewed beside them, Setup on `,` and every key on `?`.
//!
//! # Three layers, and only the last one does I/O
//!
//! * This module is the **state machine**: [`Home`] holds what the operator is looking at, and
//!   [`Home::key`] turns one key into the next state and at most one [`Effect`]. It reads no file,
//!   dials no socket and draws nothing, so every decision here — which key does what, that nothing
//!   destructive happens without a `y` — is a plain unit test.
//! * [`view`] projects a [`Home`] onto `marion_tui::home`'s views, which only draw.
//! * [`session`] runs the loop: it paints, reads the keyboard, keeps the forest current from
//!   `tree/subscribe`, and carries each [`Effect`] out **through a command or an RPC marion already
//!   has** — a run is `marion run …`, a cancel is `node/cancel`, an attach is
//!   [`crate::attach::run`] after leaving the screen, and the `:` line runs the `marion` command
//!   typed on it. There is no second spawn path here.
//!
//! # Every effect is a command
//!
//! [`Effect::argv`] is the command line the effect stands for, and the confirm line and `?` show
//! it: the home screen teaches the CLI rather than hiding it. `marion`'s own tests parse each of
//! those lines back through the binary's argument parser and compare, so an effect and its echo
//! cannot drift apart.
//!
//! [`Home`]: crate::home::Home
//! [`Home::key`]: crate::home::Home::key
//! [`Effect`]: crate::home::Effect
//! [`view`]: crate::home::view
//! [`session`]: crate::home::session
//! [`Effect::argv`]: crate::home::Effect::argv

pub mod options;
pub mod session;
pub mod view;
pub mod wake;

use marion_core::contract::AgentId;
use marion_core::proto::NodeSummary;
use marion_core::proto::result::{ActionLine, NodeDetail};
use marion_tui::home::keys::Key;
use marion_tui::home::{Page, Ready};
use marion_tui::tree::Tree;
use std::collections::HashMap;

/// What a key asks the session to do. [`Effect::None`] is by far the most common answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    /// `marion run <type> [--model <m>] [options] --prompt <text> --detach`, or `--pane` in place
    /// of `--detach` when the type's harness has a pane surface and the options do not say
    /// otherwise: either way it returns once the root exists, and a pane root is one `enter` can
    /// attach to. `options` are the words typed before the task, for `marion run` to parse.
    Run {
        agent_type: String,
        model: Option<String>,
        options: Vec<String>,
        prompt: String,
        pane: bool,
    },
    /// Leave the screen, run [`crate::attach::run`], come back.
    Attach(AgentId),
    /// `node/cancel`: the node and everything below it, each turn ended the way its harness allows.
    /// Destructive: only ever returned after a confirming `y`.
    Cancel(AgentId),
    /// `node/kill` of a node already being cancelled — the cancel's escalation, for a harness that
    /// is ignoring its abort. Destructive: after a `y`.
    Kill(AgentId),
    /// Leave the screen, run `marion <words>` in the project, wait for `enter`, come back: the `:`
    /// line, which reaches every command `marion --help` lists.
    Command(Vec<String>),
    /// Probe the harnesses again (`marion doctor`).
    Recheck,
    /// Leave the screen, open the project's agents file in `$EDITOR`, come back and validate it.
    EditTypes,
    /// Leave the screen, run `marion login <credential id>` (which reads the key itself, echo off),
    /// come back and list the keys again. Only ever an operator's keypress: nothing else starts it.
    Login(String),
    /// Leave the screen, run `marion logout <credential id>`, come back. Destructive: after a `y`.
    Logout(String),
    /// Leave the screen, run `marion profile add <harness> <name>` — which makes the profile's
    /// directory and **prints** the command that logs in to it, never running it — come back and
    /// list the profiles again.
    ProfileAdd {
        harness: String,
        name: String,
    },
    /// `marion profile use <harness> <name>`: the harness's nodes run on it from now on.
    ProfileUse {
        harness: String,
        name: String,
    },
    /// `marion profile rm <harness> <name>`: forgotten, its directory kept. Destructive: after a
    /// `y`.
    ProfileRemove {
        harness: String,
        name: String,
    },
    /// `marion notify on|off`: the operator's notify.toml, and this project's running supervisor at
    /// once. The row's Enter, turning it to the other state.
    Notify(bool),
    /// `marion trust allow --repo` (`true`) or `deny --repo`: whether nodes on the operator's own
    /// login load this repository's own harness config. The repository row's Enter; trusting
    /// widens what loads, so it waits for a `y`.
    RepoTrust(bool),
}

impl Effect {
    /// The command line this effect is, for the confirm line and `?` to show and for the operator
    /// to learn. `None` for [`Effect::None`] and [`Effect::Quit`], which are not commands.
    pub fn argv(&self) -> Option<Vec<String>> {
        let v = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        Some(match self {
            Effect::None | Effect::Quit => return None,
            Effect::Login(id) => v(&["marion", "key", "add", id]),
            Effect::Logout(id) => v(&["marion", "key", "rm", id]),
            Effect::ProfileAdd { harness, name } => v(&["marion", "profile", "add", harness, name]),
            Effect::ProfileUse { harness, name } => v(&["marion", "profile", "use", harness, name]),
            Effect::ProfileRemove { harness, name } => {
                v(&["marion", "profile", "rm", harness, name])
            }
            Effect::Run {
                agent_type,
                model,
                options,
                prompt,
                pane,
            } => {
                let mut a = v(&["marion", "run", agent_type]);
                if let Some(m) = model {
                    a.extend(v(&["--model", m]));
                }
                // After the picked model, so a `--model` typed on the line is the one that counts.
                a.extend(options.iter().cloned());
                a.extend(v(&["--prompt", prompt]));
                a.push(if *pane { "--pane" } else { "--detach" }.to_string());
                a
            }
            Effect::Attach(id) => v(&["marion", "attach", &id.0]),
            Effect::Command(words) => std::iter::once("marion".to_string())
                .chain(words.iter().cloned())
                .collect(),
            Effect::Cancel(id) => v(&["marion", "cancel", &id.0]),
            Effect::Kill(id) => v(&["marion", "cancel", &id.0, "--force"]),
            Effect::Recheck => v(&["marion", "doctor"]),
            Effect::Notify(on) => v(&["marion", "notify", if *on { "on" } else { "off" }]),
            Effect::RepoTrust(on) => v(&[
                "marion",
                "trust",
                if *on { "allow" } else { "deny" },
                "--repo",
            ]),
            Effect::EditTypes => {
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
                v(&[&editor, crate::run::AGENT_TYPES_FILE])
            }
        })
    }

    /// Whether this effect ends or discards something, or widens what loads on the operator's
    /// login, and so waits for a `y`. A `:` line asks when the commands table says its command
    /// does ([`crate::verbs::Spec::asks`]).
    pub fn destructive(&self) -> bool {
        match self {
            Effect::Command(words) => words
                .first()
                .and_then(|name| crate::verbs::spec(name))
                .is_some_and(|spec| spec.asks_first(words)),
            _ => matches!(
                self,
                Effect::Cancel(_)
                    | Effect::Kill(_)
                    | Effect::Logout(_)
                    | Effect::ProfileRemove { .. }
                    | Effect::RepoTrust(true)
            ),
        }
    }
}

/// One harness as the home screen shows it, from `marion doctor`'s rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Harness {
    /// The harness as a type names it (`claude`, `codex`, `acp opencode`).
    pub name: String,
    pub version: Option<String>,
    pub ready: Ready,
    pub note: String,
    pub surfaces: String,
    /// Doctor measured a pane surface for it: a root of its types runs in a terminal marion owns.
    pub pane: bool,
    pub fix: Option<String>,
    pub detail: Vec<(String, String)>,
}

/// An agent type this project can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentType {
    pub name: String,
    /// Its harness, as [`Harness::name`] spells it.
    pub harness: String,
    /// It edits files (an implementer); otherwise it is read-only and delegates.
    pub writes: bool,
    /// Declared in the project's agents file rather than built in.
    pub custom: bool,
}

/// What the operator is doing with the keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing on the footer's input line, for `purpose`.
    Typing {
        purpose: Purpose,
        text: String,
    },
    /// A destructive effect waiting for `y`.
    Confirm(Effect),
}

/// What the input line is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// A new task, for [`Home::new_type`] on [`Home::new_model`].
    New,
    /// A `marion` command, on the selected agent where it takes one ([`Home::command_line`]).
    Command,
    /// The credential id (`<provider>[:<label>]`) to hand to `marion key add`.
    Key,
    /// A login's name, to hand to `marion profile add <harness> <name>`.
    Profile(String),
    /// A search of the list, which filters it as it is typed. `from` is the row selected when it
    /// opened, where `esc` goes back to.
    Search { from: Option<String> },
}

/// The `n` line read: the options for `marion run`, how it runs where they say (`Some(true)` in a
/// pane, `Some(false)` headless), and the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLine {
    pub options: Vec<String>,
    pub pane: Option<bool>,
    pub task: String,
}

/// One row of Setup the cursor can rest on, in the order Setup shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupItem {
    /// Into [`Home::harnesses`].
    Harness(usize),
    /// Into [`Setup::profiles`]: a login, under its harness.
    Profile(usize),
    /// Into [`Setup::logins`].
    Key(usize),
    /// The row in the keys' place while none is stored.
    NoKeys,
    /// The project's agents file.
    Types,
    /// Desktop notifications, once read.
    Notify,
    /// This repository's own harness config and the operator's answer, where there is either.
    Repo,
}

/// One stored provider key, by credential id — the only thing about a key the home screen holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLogin {
    pub provider: String,
    pub id: String,
    /// Why the store could not say, when it could not.
    pub note: Option<String>,
}

/// One profile, as `marion profile list` finds it — never a credential, only where one lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredProfile {
    /// The harness as the operator types it (`claude`).
    pub harness: String,
    pub name: String,
    /// The harness's default profile.
    pub default: bool,
    /// What the harness's own read-only probe said; `None` until it has answered.
    pub login: Option<crate::profiles::LoginState>,
    /// The command that logs in to it, from its row's carrier: shown, never run.
    pub login_command: String,
    /// The last usage-limit reading a child's stream left for it.
    pub limit: Option<String>,
}

/// The `n` line's choices, kept between one task and the next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewTask {
    /// Into [`Home::types`].
    pub kind: usize,
    /// Into [`Home::models`] for the chosen type; 0 is its own default.
    pub model: usize,
}

/// The agents page's state: the forest, the selection, and the selected node's detail and output.
#[derive(Debug, Clone)]
pub struct Watch {
    /// A supervisor is serving this project.
    pub supervisor: bool,
    /// The forest, in the order the list shows it ([`crate::tree::by_urgency`]).
    pub nodes: Vec<NodeSummary>,
    pub tree: Tree,
    /// The selected node's last `node/get` detail.
    pub detail: Option<(AgentId, NodeDetail)>,
    /// The selected node's output so far, and where the next page starts.
    pub stream: Vec<ActionLine>,
    pub next: Option<u64>,
    /// Lines up from the newest the output is scrolled; 0 follows.
    pub scroll: usize,
    /// Every node's subtree totals, rebuilt with the forest ([`crate::rollup`]).
    pub rollup: crate::rollup::Rollup,
    /// The selected agent's output fills the body: `enter` on a headless agent, `esc` back.
    pub full: bool,
    /// Each node's task, first line, once a `node/get` has read it: the row's title.
    pub titles: HashMap<String, String>,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            supervisor: false,
            nodes: Vec::new(),
            tree: Tree::new(Vec::new()),
            detail: None,
            stream: Vec::new(),
            next: None,
            scroll: 0,
            rollup: crate::rollup::Rollup::default(),
            full: false,
            titles: HashMap::new(),
        }
    }
}

/// Setup's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setup {
    /// Into [`Home::setup_items`].
    pub cursor: usize,
    /// The selected harness shows its detail.
    pub expanded: bool,
    /// The last validation of the agents file: passed, and its one line.
    pub validation: Option<(bool, String)>,
    /// The stored provider keys, as `marion login --list` finds them.
    pub logins: Vec<StoredLogin>,
    /// The provider ids that take a key: what `a` accepts before handing off.
    pub providers: Vec<String>,
    /// Where keys are kept (the store's own one line), once listed.
    pub store: Option<String>,
    /// Why the keys could not be listed, when they could not.
    pub logins_error: Option<String>,
    /// The profiles, by harness then name, as `marion profile list` finds them.
    pub profiles: Vec<StoredProfile>,
    /// The harnesses a profile can be added for: those whose row names a carrier.
    pub profile_harnesses: Vec<String>,
    /// The profiles have been listed at least once.
    pub profiles_listed: bool,
    /// Why the profiles could not be listed, when they could not.
    pub profiles_error: Option<String>,
    /// Whether desktop notifications are on, as `notify.toml` (and `MARION_NOTIFY`) say; `None`
    /// until read.
    pub notify: Option<bool>,
    /// How a notice is shown here (`marion notify status`'s "shown by"), once read.
    pub notify_shown_by: String,
    /// This repository's own harness config and the operator's answer about it, once read;
    /// `None` where it holds none and has no answer.
    pub repo: Option<RepoTrust>,
}

/// This repository's own harness configuration, as Setup's repository row shows it
/// ([`crate::trust::repo_standing`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTrust {
    /// `Some(true)` trusted, `Some(false)` declined, `None` never answered.
    pub answer: Option<bool>,
    /// The harness config it holds of its own (`.mcp.json`, …).
    pub files: Vec<String>,
}

impl RepoTrust {
    /// It holds harness config the operator has never answered about: a needs-you item. A
    /// declined repository was answered, and is not asked about again.
    pub fn needs_you(&self) -> bool {
        self.answer.is_none() && !self.files.is_empty()
    }
}

/// The whole home screen's state.
#[derive(Debug, Clone)]
pub struct Home {
    pub page: Page,
    pub mode: Mode,
    pub new: NewTask,
    pub watch: Watch,
    pub setup: Setup,
    /// Every harness doctor knows, as probed so far.
    pub harnesses: Vec<Harness>,
    /// The doctor probe is still running.
    pub checking: bool,
    pub types: Vec<AgentType>,
    /// Models each harness was last run on, most recent first.
    pub recent_models: Vec<(String, Vec<String>)>,
    /// One line for the operator: the last effect's answer, or why a key did nothing.
    pub notice: Option<String>,
    /// The notice is an error that stays until Esc, rather than until the next key: a failed run's
    /// reason is read after the keys that follow it, not before.
    pub notice_sticky: bool,
    /// The page help was opened from, and goes back to.
    pub help_from: Page,
    /// Rows help is scrolled down, where it does not fit the screen.
    pub help_scroll: usize,
}

/// How many output lines a Page Up / Page Down moves.
pub const PAGE: usize = 8;

impl Home {
    pub fn new() -> Self {
        Home {
            page: Page::Agents,
            mode: Mode::Normal,
            new: NewTask::default(),
            watch: Watch::default(),
            setup: Setup::default(),
            harnesses: Vec::new(),
            checking: false,
            types: Vec::new(),
            recent_models: Vec::new(),
            notice: None,
            notice_sticky: false,
            help_from: Page::Agents,
            help_scroll: 0,
        }
    }

    /// Open help, remembering the page to go back to; from help itself, go back.
    fn toggle_help(&mut self) {
        if self.page == Page::Help {
            self.page = self.help_from;
        } else {
            self.help_from = self.page;
            self.help_scroll = 0;
            self.page = Page::Help;
        }
    }

    // ------------------------------------------------------------------ derived facts

    /// The harness `t` runs on, as doctor found it.
    pub fn harness_of(&self, t: &AgentType) -> Option<&Harness> {
        self.harnesses.iter().find(|h| h.name == t.harness)
    }

    /// The type the `n` line runs.
    pub fn new_type(&self) -> Option<&AgentType> {
        self.types.get(self.new.kind)
    }

    /// The models the `n` line cycles for its type: `default` (the type's own), then the models
    /// the type's harness was last run on.
    pub fn models(&self) -> Vec<String> {
        let mut out = vec!["default".to_string()];
        if let Some(t) = self.new_type()
            && let Some((_, ms)) = self.recent_models.iter().find(|(n, _)| *n == t.harness)
        {
            out.extend(ms.iter().cloned());
        }
        out
    }

    /// The model the `n` line runs on, `None` for the type's own default.
    pub fn new_model(&self) -> Option<String> {
        self.models()
            .get(self.new.model)
            .filter(|m| *m != "default")
            .cloned()
    }

    /// The `n` line opens on a type that can run: the one last used if it still can, else the
    /// first whose harness is ready. Where none is, the choice is left for the line to explain.
    fn ready_type(&mut self) {
        if self.new_blocked().is_none() {
            return;
        }
        let ready = |t: &AgentType| self.harness_of(t).is_some_and(|h| h.ready == Ready::Ready);
        if let Some(i) = self.types.iter().position(ready) {
            self.new = NewTask { kind: i, model: 0 };
        }
    }

    /// Why the `n` line's type cannot run now, in a few words, or `None` when it can.
    pub fn new_blocked(&self) -> Option<String> {
        let t = self.new_type()?;
        match self.harness_of(t) {
            Some(h) if h.ready == Ready::Ready => None,
            Some(h) if h.ready == Ready::Checking => Some("checking".into()),
            Some(h) => Some(match &h.fix {
                Some(fix) => format!("{}: {}", h.note, fix.replace('`', "")),
                None => h.note.clone(),
            }),
            None => Some(format!("{} is not installed", t.harness)),
        }
    }

    /// The node under the cursor.
    pub fn selected(&self) -> Option<&NodeSummary> {
        let id = &self.watch.tree.selected()?.id;
        self.watch.nodes.iter().find(|n| n.agent_id.0 == *id)
    }

    /// The race whose header row is under the cursor.
    pub fn selected_race(&self) -> Option<String> {
        crate::tree::race_of_row(&self.watch.tree.selected()?.id).map(str::to_string)
    }

    /// The workflow run whose header row is under the cursor.
    pub fn selected_workflow(&self) -> Option<String> {
        crate::tree::workflow_of_row(&self.watch.tree.selected()?.id).map(str::to_string)
    }

    /// A race's winning agent, once one has won.
    fn race_winner(&self, race: &str) -> Option<AgentId> {
        self.watch
            .nodes
            .iter()
            .find(|n| {
                n.race.as_ref().is_some_and(|b| {
                    b.race_id.0 == race && b.verdict == Some(marion_core::race::SeatVerdict::Won)
                })
            })
            .map(|n| n.agent_id.clone())
    }

    /// A key on a race's header row, which has no agent of its own: `enter` goes to the winner.
    /// Anything else says what does.
    fn race_key(&mut self, race: &str, key: &Key) -> Effect {
        match (key, self.race_winner(race)) {
            (Key::Enter, Some(w)) => {
                self.select(&w.0);
            }
            (Key::Enter, None) => self.notice = Some("no winner yet".into()),
            _ => self.notice = Some("a race: enter goes to its winner".into()),
        }
        Effect::None
    }

    pub fn selected_detail(&self) -> Option<&NodeDetail> {
        let sel = self.selected()?;
        match &self.watch.detail {
            Some((id, d)) if *id == sel.agent_id => Some(d),
            _ => None,
        }
    }

    /// Whether anything the page shows moves with time — a running node's time and spinner, a
    /// harness still being checked — and so needs a clock. `false` is the idle screen, which sleeps
    /// until something happens.
    pub fn animating(&self) -> bool {
        match self.page {
            Page::Agents => self.watch.nodes.iter().any(|n| !n.state.is_exited()),
            Page::Setup => self.checking || self.setup.profiles.iter().any(|p| p.login.is_none()),
            Page::Help => false,
        }
    }

    // ------------------------------------------------------------------ the forest

    /// A new snapshot of the forest, keeping the selection where the node survives. The list is
    /// re-ordered by urgency, except while a line is open: a row must not move under a steer being
    /// typed for it, so the order is held and new nodes go to the end until the line closes.
    pub fn set_nodes(&mut self, nodes: Vec<NodeSummary>) {
        let keep = self.watch.tree.selected().map(|n| n.id.clone());
        let nodes = if matches!(self.mode, Mode::Normal) {
            crate::tree::by_urgency(&nodes, &crate::tree::to_land)
        } else {
            let at = |n: &NodeSummary| {
                self.watch
                    .nodes
                    .iter()
                    .position(|o| o.agent_id == n.agent_id)
                    .unwrap_or(usize::MAX)
            };
            let mut held = nodes;
            held.sort_by_key(at);
            held
        };
        self.watch.tree = crate::tree::build(&nodes, keep.as_deref());
        self.watch.rollup = crate::rollup::Rollup::build(&nodes);
        self.watch.nodes = nodes;
        self.watch.supervisor = true;
        self.refollow_search();
    }

    /// The list or a title changed under a search: the cursor onto a match again, so the row it
    /// highlights is the one the preview shows.
    fn refollow_search(&mut self) {
        if let Some(q) = self.search_query().map(str::to_string) {
            self.follow_search(&q);
        }
    }

    /// The order again, now that nothing holds it.
    fn reorder(&mut self) {
        if self.watch.supervisor {
            let nodes = std::mem::take(&mut self.watch.nodes);
            self.set_nodes(nodes);
        }
    }

    /// The supervisor went away (or was never there). The titles are kept: a task's first line
    /// does not change, and the same nodes come back with the supervisor.
    pub fn lost_supervisor(&mut self) {
        let titles = std::mem::take(&mut self.watch.titles);
        self.watch = Watch {
            titles,
            ..Watch::default()
        };
    }

    /// Select `id`, if the forest has it.
    pub fn select(&mut self, id: &str) -> bool {
        let found = self.watch.tree.select(id);
        if found {
            self.selection_moved();
        }
        found
    }

    /// The nodes whose title has not been read: what the session asks `node/get` for.
    pub fn untitled(&self) -> Vec<AgentId> {
        self.watch
            .nodes
            .iter()
            .filter(|n| !self.watch.titles.contains_key(&n.agent_id.0))
            .map(|n| n.agent_id.clone())
            .collect()
    }

    /// A node's title, read: its task's first line, or its agent type where it has no task (a
    /// root, whose prompt went to its harness rather than into a contract). Returns `false`, and
    /// records nothing, for a node with no task yet that is still spawning: its contract may not be
    /// written yet, so the answer is not final and is worth asking again — nor for a node no
    /// longer in the forest, whose type is not known to stand in.
    pub fn set_title(&mut self, id: &AgentId, task: Option<&str>) -> bool {
        let line = task
            .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
            .map(|l| marion_core::text::visible(l).into_owned());
        let node = self.watch.nodes.iter().find(|n| n.agent_id == *id);
        let title = match (line, node) {
            (Some(l), _) => l,
            (None, Some(n)) if n.state == marion_core::node::NodeState::Spawning => return false,
            (None, Some(n)) => n.agent_type.clone(),
            (None, None) => return false,
        };
        self.watch.titles.insert(id.0.clone(), title);
        self.refollow_search();
        true
    }

    /// A `node/get` answer for `id`: its detail, and the page of its output appended.
    pub fn absorb_detail(&mut self, id: AgentId, mut detail: NodeDetail) {
        let same = matches!(&self.watch.detail, Some((d, _)) if *d == id);
        if !same {
            self.watch.stream.clear();
            self.watch.next = None;
            self.watch.scroll = 0;
        }
        if let Some(page) = detail.stream.take() {
            if Some(page.from) == self.watch.next || self.watch.next.is_none() {
                crate::activity::fold(&mut self.watch.stream, page.lines);
                self.watch.next = Some(page.next);
            }
            if let Some(why) = page.unread {
                detail.stream = Some(marion_core::proto::result::ActivityPage {
                    unread: Some(why),
                    ..Default::default()
                });
            }
        }
        if !self.watch.titles.contains_key(&id.0) {
            let task = detail.task.as_ref().map(|t| t.prompt.clone());
            let _ = self.set_title(&id, task.as_deref());
        }
        self.watch.detail = Some((id, detail));
    }

    fn selection_moved(&mut self) {
        self.watch.scroll = 0;
    }

    // ------------------------------------------------------------------ keys

    /// One key: the next state, and what (if anything) the session should do.
    pub fn key(&mut self, key: Key) -> Effect {
        let was_normal = matches!(self.mode, Mode::Normal);
        let effect = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Confirm(effect) => self.confirm_key(effect, key),
            Mode::Typing { purpose, text } => self.typing_key(purpose, text, key),
            Mode::Normal => self.normal(key),
        };
        // A line that just closed releases the order it held.
        if !was_normal && matches!(self.mode, Mode::Normal) {
            self.reorder();
        }
        effect
    }

    fn confirm_key(&mut self, effect: Effect, key: Key) -> Effect {
        if key == Key::Char('y') {
            return effect;
        }
        match effect {
            Effect::Logout(_) => self.notice = Some("key kept".into()),
            Effect::ProfileRemove { .. } => self.notice = Some("login kept".into()),
            Effect::RepoTrust(_) => self.notice = Some("not trusted".into()),
            Effect::Command(_) => self.notice = Some("not run".into()),
            _ => self.notice = Some("not cancelled".into()),
        }
        Effect::None
    }

    fn typing_key(&mut self, purpose: Purpose, mut text: String, key: Key) -> Effect {
        if let Purpose::Search { from } = &purpose {
            match key {
                Key::Esc | Key::Ctrl('c') => {
                    if let Some(id) = from {
                        self.watch.tree.select(id);
                    }
                    self.selection_moved();
                    return Effect::None;
                }
                Key::Down | Key::Up => {
                    let matches = self.search_matches(&text);
                    let at = self.watch.tree.cursor();
                    // The next match below or the one before above, wrapping either way.
                    let to = if key == Key::Down {
                        matches.iter().find(|&&i| i > at).or(matches.first())
                    } else {
                        matches.iter().rev().find(|&&i| i < at).or(matches.last())
                    };
                    if let Some(&row) = to {
                        self.select_row(row);
                    }
                    self.mode = Mode::Typing { purpose, text };
                    return Effect::None;
                }
                _ => {}
            }
        }
        match key {
            Key::Esc | Key::Ctrl('c') => return Effect::None,
            Key::Enter => return self.submit(purpose, text),
            Key::Backspace => {
                text.pop();
            }
            Key::Char(c) => push_bounded(&mut text, &c.to_string()),
            Key::Paste(p) => push_bounded(&mut text, &p),
            Key::Tab | Key::BackTab if purpose == Purpose::New => {
                let n = self.types.len().max(1);
                let by = if key == Key::Tab { 1 } else { n - 1 };
                self.new.kind = (self.new.kind + by) % n;
                self.new.model = 0;
            }
            Key::Tab if purpose == Purpose::Command => self.complete(&mut text),
            Key::Down if purpose == Purpose::New => {
                let n = self.models().len();
                self.new.model = (self.new.model + 1) % n;
            }
            Key::Up if purpose == Purpose::New => {
                let n = self.models().len();
                self.new.model = (self.new.model + n - 1) % n;
            }
            _ => {}
        }
        if matches!(purpose, Purpose::Search { .. }) {
            self.follow_search(&text);
        }
        self.mode = Mode::Typing { purpose, text };
        Effect::None
    }

    /// The cursor on a match of `query` as it, or the list, changes: kept where it is on one, else
    /// moved to the next match below, wrapping. With none it stays put, and the list shows nothing.
    fn follow_search(&mut self, query: &str) {
        let matches = self.search_matches(query);
        let at = self.watch.tree.cursor();
        if matches.binary_search(&at).is_err()
            && let Some(&row) = matches.iter().find(|&&i| i > at).or(matches.first())
        {
            self.select_row(row);
        }
    }

    /// The search being typed, once it is more than blanks.
    fn search_query(&self) -> Option<&str> {
        match &self.mode {
            Mode::Typing {
                purpose: Purpose::Search { .. },
                text,
            } if !text.trim().is_empty() => Some(text),
            _ => None,
        }
    }

    /// Put the cursor on list row `row`.
    fn select_row(&mut self, row: usize) {
        if let Some(id) = self.watch.tree.row(row).map(|(_, n)| n.id.clone()) {
            self.watch.tree.select(&id);
            self.selection_moved();
        }
    }

    /// **The list rows `query` matches, by index, in list order**: one pass over the list, each
    /// row's node found in a map built once, so a keystroke costs one walk of the forest. A row
    /// matches where its title, agent type, name or short id holds the query, in any case; a
    /// race's or a workflow's header row matches on its own label.
    pub fn search_matches(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        let by_id: HashMap<&str, &NodeSummary> = self
            .watch
            .nodes
            .iter()
            .map(|n| (n.agent_id.0.as_str(), n))
            .collect();
        let tree = &self.watch.tree;
        (0..tree.rows())
            .filter(|&i| {
                tree.row(i)
                    .is_some_and(|(_, n)| row_matches(&self.watch.titles, &by_id, &n.id, &q))
            })
            .collect()
    }

    /// The list's rows as they are shown, by row index, each with whether it is drawn dim. Every
    /// row, until a search is typed; then the rows that match it, and dim, the rows above them in
    /// the tree, so a match is seen where it sits.
    pub fn shown_rows(&self) -> Vec<(usize, bool)> {
        let tree = &self.watch.tree;
        let Some(query) = self.search_query() else {
            return (0..tree.rows()).map(|i| (i, false)).collect();
        };
        let matches = self.search_matches(query);
        let row_of: HashMap<&str, usize> = (0..tree.rows())
            .filter_map(|i| tree.row(i).map(|(_, n)| (n.id.as_str(), i)))
            .collect();
        let mut keep = vec![false; tree.rows()];
        for &m in &matches {
            let mut at = Some(m);
            // Bounded by the row count, so a parent cycle cannot hold the frame.
            for _ in 0..tree.rows() {
                let Some(i) = at else { break };
                if std::mem::replace(&mut keep[i], true) {
                    break;
                }
                at = tree
                    .row(i)
                    .and_then(|(_, n)| n.parent.as_deref())
                    .and_then(|p| row_of.get(p).copied());
            }
        }
        (0..tree.rows())
            .filter(|&i| keep[i])
            .map(|i| (i, matches.binary_search(&i).is_err()))
            .collect()
    }

    /// **The `n` line's options and its task.** `--detach` and `--pane` choose how it runs and are
    /// taken out (the run adds the one chosen); `--worktree` and `--verify` run it in a worktree,
    /// which is headless (`marion run` refuses `--pane` with them); `None` leaves it to the type.
    /// `--prompt` is refused: the task is the text after the options.
    pub fn new_options(&self, text: &str) -> Result<NewLine, String> {
        let (mut words, task) = options::split(text)?;
        if words
            .iter()
            .any(|w| w == "--prompt" || w.starts_with("--prompt="))
        {
            return Err("the task is the text after the options; leave out --prompt".into());
        }
        // The screen states its own project to every run; a second one would contradict it.
        if let Some(flag) = crate::run::PLACE_FLAGS.iter().find(|f| {
            words
                .iter()
                .any(|w| w == *f || w.starts_with(&format!("{f}=")))
        }) {
            return Err(format!(
                "{flag}: the screen runs its tasks in its own project"
            ));
        }
        let mut take = |flag: &str| {
            let had = words.iter().any(|w| w == flag);
            words.retain(|w| w != flag);
            had
        };
        let (detach, pane) = (take("--detach"), take("--pane"));
        let surface = match (detach, pane) {
            (true, true) => return Err("--pane or --detach, not both".into()),
            (true, false) => Some(false),
            (false, true) => Some(true),
            (false, false) => words
                .iter()
                .any(|w| w == "--worktree" || w == "--verify" || w.starts_with("--verify="))
                .then_some(false),
        };
        Ok(NewLine {
            options: words,
            pane: surface,
            task,
        })
    }

    /// Enter on the input line. A line that cannot be acted on stays open with the reason, so the
    /// text is not lost to a typo.
    fn submit(&mut self, purpose: Purpose, text: String) -> Effect {
        let refused = |home: &mut Home, why: String, purpose: Purpose, text: String| {
            home.notice = Some(why);
            home.mode = Mode::Typing { purpose, text };
            Effect::None
        };
        match purpose {
            Purpose::Search { .. } => {
                let at = self.watch.tree.cursor();
                if self.search_matches(&text).binary_search(&at).is_err() {
                    return refused(self, "no match".into(), purpose, text);
                }
                self.selection_moved();
                Effect::None
            }
            Purpose::Command => match self.command_line(&text) {
                Ok(words) => {
                    let effect = Effect::Command(words);
                    if effect.destructive() {
                        self.mode = Mode::Confirm(effect);
                        return Effect::None;
                    }
                    effect
                }
                Err(why) => refused(self, why, purpose, text),
            },
            Purpose::New => {
                let line = match self.new_options(&text) {
                    Ok(line) => line,
                    Err(why) => return refused(self, why, purpose, text),
                };
                if line.task.is_empty() {
                    return refused(self, "type the task".into(), purpose, text);
                }
                let Some(t) = self.new_type().cloned() else {
                    return refused(self, "no agent type to run".into(), purpose, text);
                };
                if let Some(why) = self.new_blocked() {
                    return refused(self, format!("{}: {why}", t.name), purpose, text);
                }
                Effect::Run {
                    pane: line
                        .pane
                        .unwrap_or_else(|| self.harness_of(&t).is_some_and(|h| h.pane)),
                    agent_type: t.name,
                    model: self.new_model(),
                    options: line.options,
                    prompt: line.task,
                }
            }
            Purpose::Key => match self.login(&text) {
                Ok(e) => e,
                Err(why) => refused(self, why, purpose, text),
            },
            Purpose::Profile(harness) => match self.profile_add(&harness, &text) {
                Ok(e) => e,
                Err(why) => refused(self, why, Purpose::Profile(harness), text),
            },
        }
    }

    fn normal(&mut self, key: Key) -> Effect {
        if key == Key::Ctrl('c') {
            return Effect::Quit;
        }
        match self.page {
            Page::Agents => self.agents_key(key),
            Page::Setup => self.setup_key(key),
            Page::Help => match key {
                Key::Char('q') => Effect::Quit,
                Key::Esc | Key::Char('?') => {
                    self.toggle_help();
                    Effect::None
                }
                Key::Char('j') | Key::Down => {
                    self.help_scroll = (self.help_scroll + 1).min(view::help_lines(self));
                    Effect::None
                }
                Key::Char('k') | Key::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                    Effect::None
                }
                _ => Effect::None,
            },
        }
    }

    fn agents_key(&mut self, key: Key) -> Effect {
        let anywhere = matches!(
            key,
            Key::Char('q' | '?' | ',' | 'n' | 'j' | 'k' | '/' | ':')
                | Key::Up
                | Key::Down
                | Key::PageUp
                | Key::PageDown
                | Key::End
        );
        if !anywhere && let Some(race) = self.selected_race() {
            return self.race_key(&race, &key);
        }
        match key {
            Key::Char('q') => return Effect::Quit,
            Key::Char('?') => self.toggle_help(),
            Key::Char(',') => self.page = Page::Setup,
            Key::Char('j') | Key::Down => {
                self.watch.tree.move_by(1);
                self.selection_moved();
            }
            Key::Char('k') | Key::Up => {
                self.watch.tree.move_by(-1);
                self.selection_moved();
            }
            Key::PageUp => {
                let max = self.watch.stream.len();
                self.watch.scroll = (self.watch.scroll + PAGE).min(max);
            }
            Key::PageDown => self.watch.scroll = self.watch.scroll.saturating_sub(PAGE),
            Key::End => self.watch.scroll = 0,
            Key::Char('n') => {
                self.ready_type();
                self.mode = Mode::Typing {
                    purpose: Purpose::New,
                    text: String::new(),
                }
            }
            Key::Char('/') if !self.watch.tree.is_empty() => {
                self.mode = Mode::Typing {
                    purpose: Purpose::Search {
                        from: self.watch.tree.selected().map(|n| n.id.clone()),
                    },
                    text: String::new(),
                }
            }
            Key::Char(':') => {
                self.mode = Mode::Typing {
                    purpose: Purpose::Command,
                    text: String::new(),
                }
            }
            Key::Enter => return self.enter(),
            Key::Esc if self.watch.full => self.watch.full = false,
            Key::Char('x') => match self.selected() {
                // A second `x` on a node whose cancel is still waiting out its grace kills it now.
                Some(n) if !n.state.is_exited() && n.cancel.is_some() => {
                    self.mode = Mode::Confirm(Effect::Kill(n.agent_id.clone()))
                }
                Some(n) if !n.state.is_exited() => {
                    self.mode = Mode::Confirm(Effect::Cancel(n.agent_id.clone()))
                }
                Some(_) => self.notice = Some("already ended".into()),
                None => self.notice = Some("nothing selected".into()),
            },
            _ => {}
        }
        Effect::None
    }

    /// `enter`: a workflow header goes to the step it is on; a pane agent is attached; a headless
    /// one has its output fill the body, or stop filling it.
    fn enter(&mut self) -> Effect {
        if let Some(wf) = self.selected_workflow() {
            let run = marion_core::workflow::WorkflowId(wf);
            let now = std::time::SystemTime::now();
            match crate::tree::WorkflowSummary::of(&run, &self.watch.nodes, now).current_node {
                Some(id) => {
                    self.select(&id);
                }
                None => self.notice = Some("no step has started".into()),
            }
            return Effect::None;
        }
        match self.selected() {
            Some(n) if n.pane => return Effect::Attach(n.agent_id.clone()),
            Some(_) => {
                self.watch.full = !self.watch.full;
                self.watch.scroll = 0;
            }
            None => self.notice = Some("nothing selected".into()),
        }
        Effect::None
    }

    /// **The `:` line read**: `marion`'s words for the command typed, with the selected agent's id
    /// put first after the command where the command acts on one agent and the words do not name
    /// one already (`:steer use a deque` is `marion steer <id> use a deque`; `:steer 3f2a hi` is
    /// `marion steer 3f2a hi`). Refused, with why, for a word that is no command or a command that
    /// is [`OFF_LINE`].
    pub fn command_line(&self, text: &str) -> Result<Vec<String>, String> {
        let mut words = options::words(text)?;
        let Some(name) = words.first() else {
            return Err("type a command; tab completes it".into());
        };
        if let Some((_, why)) = OFF_LINE.iter().find(|(off, _)| off == name) {
            return Err(why.to_string());
        }
        let Some(spec) = crate::verbs::spec(name) else {
            return Err(format!("no command named `{name}`; tab completes one"));
        };
        if spec.on_agent
            && let Some((id, false)) = self.command_agent(&words)
        {
            words.insert(1, id.0);
        }
        Ok(words)
    }

    /// **The agent the `:` line's words act on**, and whether they name it themselves: the agent
    /// the first word after the command names — by the rule every command reads an `<id>` by
    /// ([`crate::node_label::resolve_node`]: its whole id, its short id, or a start of its id,
    /// even one several agents share, which the command then refuses) — else the selected agent.
    pub fn command_agent(&self, words: &[String]) -> Option<(AgentId, bool)> {
        let nodes = &self.watch.nodes;
        let named = words.get(1).filter(|w| !w.is_empty()).and_then(|w| {
            match crate::tree::resolve_target(w, nodes) {
                Ok(id) if nodes.iter().any(|n| n.agent_id == id) => Some(id),
                // It names no agent here: the command's own words.
                Ok(_) => None,
                // A start several agents share: named, for the command to refuse as ambiguous.
                Err(_) => Some(AgentId(w.clone())),
            }
        });
        match named {
            Some(id) => Some((id, true)),
            None => self.selected().map(|n| (n.agent_id.clone(), false)),
        }
    }

    /// `tab` on the `:` line: the command word typed so far completed — to the command where one
    /// starts with it, else as far as every one that does agrees. After the command, nothing.
    fn complete(&mut self, text: &mut String) {
        let word = text.trim_start();
        if word.contains(char::is_whitespace) {
            return;
        }
        let found = completions(word);
        match found.as_slice() {
            [] => self.notice = Some(format!("no command starts `{word}`")),
            [one] => *text = format!("{} ", one.name),
            [first, rest @ ..] => {
                // Command words are ASCII: a byte count is a char boundary.
                let common = rest.iter().fold(first.name, |common, s| {
                    let agree = common
                        .bytes()
                        .zip(s.name.bytes())
                        .take_while(|(a, b)| a == b)
                        .count();
                    &common[..agree]
                });
                *text = common.to_string();
            }
        }
    }

    fn setup_key(&mut self, key: Key) -> Effect {
        let item = self.selected_item();
        match key {
            Key::Char('q') => return Effect::Quit,
            Key::Char('?') => self.toggle_help(),
            Key::Esc | Key::Char(',') => self.page = Page::Agents,
            Key::Char('j') | Key::Down => self.move_setup(1),
            Key::Char('k') | Key::Up => self.move_setup(-1),
            Key::Char('r') => return Effect::Recheck,
            Key::Enter => match item {
                Some(SetupItem::Harness(_)) => self.setup.expanded = !self.setup.expanded,
                Some(SetupItem::Profile(i)) => {
                    let p = &self.setup.profiles[i];
                    if p.default {
                        self.notice = Some(format!("{} already runs on {}", p.harness, p.name));
                    } else {
                        return Effect::ProfileUse {
                            harness: p.harness.clone(),
                            name: p.name.clone(),
                        };
                    }
                }
                Some(SetupItem::Types) => return Effect::EditTypes,
                Some(SetupItem::Notify) => return Effect::Notify(self.setup.notify != Some(true)),
                Some(SetupItem::Repo) => match self.repo_effect() {
                    deny @ Effect::RepoTrust(false) => return deny,
                    allow => self.mode = Mode::Confirm(allow),
                },
                Some(SetupItem::Key(_) | SetupItem::NoKeys) | None => {}
            },
            Key::Char('a') => match item {
                Some(SetupItem::Harness(i)) => {
                    let harness = self.harnesses[i].name.clone();
                    self.add_login(harness);
                }
                Some(SetupItem::Profile(i)) => {
                    let harness = self.setup.profiles[i].harness.clone();
                    self.add_login(harness);
                }
                Some(SetupItem::Key(i)) => {
                    self.mode = Mode::Typing {
                        purpose: Purpose::Key,
                        text: self.setup.logins[i].provider.clone(),
                    }
                }
                Some(SetupItem::NoKeys) => {
                    self.mode = Mode::Typing {
                        purpose: Purpose::Key,
                        text: String::new(),
                    }
                }
                Some(SetupItem::Types | SetupItem::Notify | SetupItem::Repo) | None => {}
            },
            Key::Backspace | Key::Delete => match item {
                Some(SetupItem::Key(i)) => {
                    let id = self.setup.logins[i].id.clone();
                    self.mode = Mode::Confirm(Effect::Logout(id));
                }
                Some(SetupItem::Profile(i)) => {
                    let p = &self.setup.profiles[i];
                    self.mode = Mode::Confirm(Effect::ProfileRemove {
                        harness: p.harness.clone(),
                        name: p.name.clone(),
                    });
                }
                _ => {}
            },
            _ => {}
        }
        Effect::None
    }

    /// `a` on a harness or one of its logins: the input line for a new login's name, where the
    /// harness takes logins at all.
    fn add_login(&mut self, harness: String) {
        if self.setup.profile_harnesses.contains(&harness) {
            self.mode = Mode::Typing {
                purpose: Purpose::Profile(harness),
                text: String::new(),
            };
        } else {
            self.notice = Some(format!("{harness} takes no logins of marion's"));
        }
    }

    fn move_setup(&mut self, by: isize) {
        let last = self.setup_items().len().saturating_sub(1);
        let at = self.setup.cursor.min(last) as isize + by;
        self.setup.cursor = at.clamp(0, last as isize) as usize;
        self.setup.expanded = false;
    }

    /// **Setup's rows the cursor can rest on, in the order Setup shows them**: each installed
    /// harness with its logins under it, then any login whose harness doctor does not list, the
    /// stored keys (or the one row that adds the first), the agents file, notifications once read,
    /// and the repository's trust where there is something to show. A harness that is not
    /// installed is folded into one line and is not a row, unless it has logins.
    pub fn setup_items(&self) -> Vec<SetupItem> {
        let mut out = Vec::new();
        let profiles = &self.setup.profiles;
        let logins_of = |name: &str| -> Vec<SetupItem> {
            profiles
                .iter()
                .enumerate()
                .filter(|(_, p)| p.harness == name)
                .map(|(i, _)| SetupItem::Profile(i))
                .collect()
        };
        for (i, h) in self.harnesses.iter().enumerate() {
            let logins = logins_of(&h.name);
            if h.ready != Ready::Absent || !logins.is_empty() {
                out.push(SetupItem::Harness(i));
                out.extend(logins);
            }
        }
        out.extend(
            self.setup
                .profiles
                .iter()
                .enumerate()
                .filter(|(_, p)| !self.harnesses.iter().any(|h| h.name == p.harness))
                .map(|(i, _)| SetupItem::Profile(i)),
        );
        if self.setup.logins.is_empty() {
            out.push(SetupItem::NoKeys);
        } else {
            out.extend((0..self.setup.logins.len()).map(SetupItem::Key));
        }
        out.push(SetupItem::Types);
        if self.setup.notify.is_some() {
            out.push(SetupItem::Notify);
        }
        if self.setup.repo.is_some() {
            out.push(SetupItem::Repo);
        }
        out
    }

    /// The row under Setup's cursor.
    pub fn selected_item(&self) -> Option<SetupItem> {
        let items = self.setup_items();
        items
            .get(self.setup.cursor.min(items.len().saturating_sub(1)))
            .copied()
    }

    /// The repository row's Enter: stop trusting a trusted repository, trust any other.
    pub fn repo_effect(&self) -> Effect {
        Effect::RepoTrust(self.setup.repo.as_ref().and_then(|r| r.answer) != Some(true))
    }

    /// This repository's own harness config waits for the operator's answer.
    pub fn repo_needs_you(&self) -> bool {
        self.setup.repo.as_ref().is_some_and(RepoTrust::needs_you)
    }

    /// A new listing of the profiles. The cursor moves onto `select` where the listing has it — a
    /// login just added, whose row shows the command that signs it in — and otherwise stays on the
    /// row it was on, or the nearest one that still exists.
    pub fn set_profiles(
        &mut self,
        rows: Vec<StoredProfile>,
        harnesses: Vec<String>,
        select: Option<&str>,
    ) {
        let was = self.selected_item();
        // A login is followed by its harness and name: the new listing may number it differently.
        let was_login = match was {
            Some(SetupItem::Profile(i)) => self
                .setup
                .profiles
                .get(i)
                .map(|p| (p.harness.clone(), p.name.clone())),
            _ => None,
        };
        let s = &mut self.setup;
        s.profiles = rows;
        s.profile_harnesses = harnesses;
        s.profiles_listed = true;
        s.profiles_error = None;
        let items = self.setup_items();
        let profiles = &self.setup.profiles;
        let chosen = select
            .and_then(|n| profiles.iter().position(|p| p.name == n))
            .or_else(|| {
                let (h, n) = was_login.as_ref()?;
                profiles
                    .iter()
                    .position(|p| &p.harness == h && &p.name == n)
            })
            .map(SetupItem::Profile)
            .or(was.filter(|w| !matches!(w, SetupItem::Profile(_))));
        if let Some(i) = chosen.and_then(|c| items.iter().position(|it| *it == c)) {
            self.setup.cursor = i;
        }
        self.setup.cursor = self.setup.cursor.min(items.len().saturating_sub(1));
    }

    /// Enter on the key line: `marion key add <text>` when its provider is one marion knows.
    fn login(&self, text: &str) -> Result<Effect, String> {
        let text = text.trim();
        let provider = text.split(':').next().unwrap_or("");
        if text.is_empty() {
            return Err("type a provider id".into());
        }
        if !self.setup.providers.iter().any(|p| p == provider) {
            return Err(format!(
                "no provider named `{provider}`; marion key list shows them"
            ));
        }
        Ok(Effect::Login(text.to_string()))
    }

    /// Enter on the login line: `marion profile add <harness> <name>` when the name is one and is
    /// free — refused here, before the terminal changes hands, otherwise.
    fn profile_add(&self, harness: &str, text: &str) -> Result<Effect, String> {
        let name = text.trim();
        if name.is_empty() {
            return Err("type a name for the login".into());
        }
        if !marion_core::agent_type::is_valid_name(name) {
            return Err(format!("`{name}` {NOT_A_LOGIN_NAME}"));
        }
        if self.setup.profiles.iter().any(|p| p.name == name) {
            return Err(format!("a login named `{name}` already exists"));
        }
        Ok(Effect::ProfileAdd {
            harness: harness.to_string(),
            name: name.to_string(),
        })
    }
}

impl Default for Home {
    fn default() -> Self {
        Home::new()
    }
}

/// Why a name typed for a new login is refused, after the name.
pub const NOT_A_LOGIN_NAME: &str = "is not a login name: letters, digits, - and _";

/// The commands the `:` line will not run, each with why: what is no operator's to run from a
/// screen.
pub const OFF_LINE: &[(&str, &str)] = &[(
    "mcp",
    "marion mcp serves an MCP client over stdio; the client's own config runs it",
)];

/// The commands the `:` line completes `prefix` to: every command `marion --help` lists that
/// starts with it, in that order, less [`OFF_LINE`].
pub fn completions(prefix: &str) -> Vec<&'static crate::verbs::Spec> {
    crate::verbs::SPECS
        .iter()
        .filter(|s| s.summary.is_some() && s.name.starts_with(prefix))
        .filter(|s| !OFF_LINE.iter().any(|(off, _)| *off == s.name))
        .collect()
}

/// Whether the list row `id` matches `query` (lower case), as [`Home::search_matches`] reads it.
fn row_matches(
    titles: &HashMap<String, String>,
    nodes: &HashMap<&str, &NodeSummary>,
    id: &str,
    query: &str,
) -> bool {
    let has = |s: &str| s.to_lowercase().contains(query);
    if let Some(n) = nodes.get(id) {
        titles.get(id).is_some_and(|t| has(t))
            || has(&n.agent_type)
            || n.name.as_deref().is_some_and(has)
            || has(crate::tree::short_id(id))
    } else {
        crate::tree::race_of_row(id)
            .or_else(|| crate::tree::workflow_of_row(id))
            .is_some_and(has)
    }
}

/// **Why a `marion` command failed, in one line**: its own `marion: …` or `marion <verb>: …` refusal where it
/// printed one, which names the cause, rather than the last line, which is the usage pointer
/// (`` `marion run --help` shows how to use it ``); else the last line it printed.
pub fn failure_line(stdout: &str, stderr: &str) -> String {
    let lines = || stdout.lines().chain(stderr.lines()).map(str::trim);
    lines()
        .find(|l| {
            l.starts_with("marion: ")
                || l.strip_prefix("marion ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .is_some_and(|verb| verb.ends_with(':'))
        })
        .or_else(|| lines().rfind(|l| !l.is_empty()))
        .unwrap_or("")
        .to_string()
}

/// The largest task or steer the input line takes: `node/steer`'s own bound.
const TEXT_LIMIT: usize = marion_core::proto::params::MAX_STEER_BYTES;

fn push_bounded(text: &mut String, more: &str) {
    for c in more.chars() {
        if text.len() + c.len_utf8() > TEXT_LIMIT {
            break;
        }
        text.push(c);
    }
}

#[cfg(test)]
mod tests;
