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
//!   has** — a run is `marion run …`, a steer is `node/steer`, a cancel is `node/cancel`, an attach
//!   is [`crate::attach::run`] after leaving the screen. There is no second spawn path here.
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

pub mod session;
pub mod types_form;
pub mod view;
pub mod wake;

use marion_core::contract::AgentId;
use marion_core::proto::NodeSummary;
use marion_core::proto::result::{ActionLine, NodeDetail};
use marion_tui::home::keys::Key;
use marion_tui::home::{Page, Ready};
use marion_tui::tree::Tree;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// What a key asks the session to do. [`Effect::None`] is by far the most common answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    /// `marion run <type> [--model <m>] --prompt <text> --detach`, or `--pane` in place of
    /// `--detach` when the type's harness has a pane surface: either way it returns once the root
    /// exists, and a pane root is one `enter` can attach to.
    Run {
        agent_type: String,
        model: Option<String>,
        prompt: String,
        pane: bool,
    },
    /// Leave the screen, run [`crate::attach::run`], come back.
    Attach(AgentId),
    /// `node/steer`, as the operator.
    Steer(AgentId, String),
    /// `node/cancel`: the node and everything below it, each turn ended the way its harness allows.
    /// Destructive: only ever returned after a confirming `y`.
    Cancel(AgentId),
    /// `node/kill` of a node already being cancelled — the cancel's escalation, for a harness that
    /// is ignoring its abort. Destructive: after a `y`.
    Kill(AgentId),
    /// Leave the screen, run `marion resume <id>`, come back.
    Resume(AgentId),
    /// Leave the screen, open the operator's `$SHELL` in this directory, come back.
    Shell(PathBuf),
    /// Leave the screen, show what the branch changed, come back.
    Diff(String),
    /// Leave the screen, run `git merge --no-ff -- <branch>` in the project, come back. It changes the
    /// operator's checkout: only ever after a `y`.
    Merge(String),
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
    /// Work out what the form's draft does to the agents file: the session reads the file, applies
    /// the draft, holds the result to the spawn path's loader, and shows the diff for a `y`.
    PreviewType(types_form::Draft),
    /// Write the previewed file. Destructive (it replaces the operator's file): after a `y`.
    WriteTypes {
        text: String,
        draft: types_form::Draft,
    },
}

impl Effect {
    /// The command line this effect is, for the confirm line and `?` to show and for the operator
    /// to learn. `None` for [`Effect::None`] and [`Effect::Quit`], which are not commands, and for
    /// the form's preview and write, which the diff describes instead.
    pub fn argv(&self) -> Option<Vec<String>> {
        let v = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        Some(match self {
            Effect::None | Effect::Quit | Effect::PreviewType(_) | Effect::WriteTypes { .. } => {
                return None;
            }
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
                prompt,
                pane,
            } => {
                let mut a = v(&["marion", "run", agent_type]);
                if let Some(m) = model {
                    a.extend(v(&["--model", m]));
                }
                a.extend(v(&["--prompt", prompt]));
                a.push(if *pane { "--pane" } else { "--detach" }.to_string());
                a
            }
            Effect::Attach(id) => v(&["marion", "attach", &id.0]),
            // `--` so a message may begin with a dash and still be read as the message.
            Effect::Steer(id, text) => v(&["marion", "steer", &id.0, "--", text]),
            Effect::Cancel(id) => v(&["marion", "cancel", &id.0]),
            Effect::Kill(id) => v(&["marion", "cancel", &id.0, "--force"]),
            Effect::Resume(id) => v(&["marion", "resume", &id.0]),
            Effect::Shell(dir) => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
                v(&["cd", &dir.to_string_lossy(), "&&", &shell])
            }
            // As `branch_log` runs it: raw, so no diff driver a branch names runs over its content.
            Effect::Diff(branch) => v(&[
                "git",
                "log",
                "-p",
                "--stat",
                "--no-textconv",
                "--no-ext-diff",
                &format!("HEAD..{branch}"),
            ]),
            // `--` so a branch name cannot be read as a flag.
            Effect::Merge(branch) => v(&["git", "merge", "--no-ff", "--", branch]),
            Effect::Recheck => v(&["marion", "doctor"]),
            Effect::Notify(on) => v(&["marion", "notify", if *on { "on" } else { "off" }]),
            Effect::EditTypes => {
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
                v(&[&editor, crate::run::AGENT_TYPES_FILE])
            }
        })
    }

    /// Whether this effect ends or discards something, and so waits for a `y`.
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Effect::Cancel(_)
                | Effect::Kill(_)
                | Effect::Merge(_)
                | Effect::Logout(_)
                | Effect::ProfileRemove { .. }
                | Effect::WriteTypes { .. }
        )
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
    /// Filling in the agent-type form.
    Form(Form),
}

/// What the input line is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// A new task, for [`Home::new_type`] on [`Home::new_model`].
    New,
    /// A steer for this node.
    Steer(AgentId),
    /// The credential id (`<provider>[:<label>]`) to hand to `marion key add`.
    Key,
    /// `<harness> <name>`, to hand to `marion profile add`.
    Profile,
}

/// The agent-type form: the draft, the field being edited, and why the last preview was refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form {
    pub draft: types_form::Draft,
    pub field: usize,
    pub error: Option<String>,
}

/// One field of the form, in the order it shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Harness,
    Model,
    Description,
    Tools,
    Provider,
}

impl Field {
    pub const ALL: [Field; 6] = [
        Field::Name,
        Field::Harness,
        Field::Model,
        Field::Description,
        Field::Tools,
        Field::Provider,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "name",
            Field::Harness => "harness",
            Field::Model => "model",
            Field::Description => "description",
            Field::Tools => "tools",
            Field::Provider => "provider",
        }
    }

    /// Picked with ←→ rather than typed.
    pub fn is_choice(self) -> bool {
        matches!(self, Field::Harness | Field::Tools)
    }
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
    /// `m` pressed on a race row: the winner's branch is merged once its detail arrives.
    pub pending: Option<AgentId>,
    /// Each node's task, first line, once a `node/get` has read it: the row's title.
    pub titles: HashMap<String, String>,
    /// Each node's landed branch, once a `node/get` has read it.
    pub branches: HashMap<String, String>,
    /// Branches merged from this screen: their nodes have nothing left to land.
    pub merged: HashSet<String>,
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
            pending: None,
            titles: HashMap::new(),
            branches: HashMap::new(),
            merged: HashSet::new(),
        }
    }
}

impl Watch {
    /// Whether `n` still has work to land: it has, by [`crate::tree::to_land`], and its branch has
    /// not been merged from here.
    pub fn lands(&self, n: &NodeSummary) -> bool {
        crate::tree::to_land(n)
            && !self
                .branches
                .get(&n.agent_id.0)
                .is_some_and(|b| self.merged.contains(b))
    }
}

/// Setup's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setup {
    /// Into the harnesses, then on into [`Setup::logins`].
    pub cursor: usize,
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
    /// The previewed change to the agents file, while it waits for a `y`.
    pub preview: Vec<types_form::DiffLine>,
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

    /// A key on a race's header row, which has no agent of its own: `enter` goes to the winner,
    /// and `m` merges the winner's branch once its detail is read. Anything else says what does.
    fn race_key(&mut self, race: &str, key: &Key) -> Effect {
        let winner = self.race_winner(race);
        match (key, winner) {
            (Key::Enter, Some(w)) => {
                self.select(&w.0);
            }
            (Key::Char('m'), Some(w)) => {
                self.watch.pending = Some(w.clone());
                self.select(&w.0);
            }
            (Key::Enter | Key::Char('m'), None) => self.notice = Some("no winner yet".into()),
            _ => self.notice = Some("a race: enter goes to its winner, m merges it".into()),
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
            crate::tree::by_urgency(&nodes, &|n| self.watch.lands(n))
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
    /// written yet, so the answer is not final and is worth asking again.
    pub fn set_title(&mut self, id: &AgentId, task: Option<&str>) -> bool {
        let line = task
            .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
            .map(|l| marion_core::text::visible(l).into_owned());
        let node = self.watch.nodes.iter().find(|n| n.agent_id == *id);
        let title = match (line, node) {
            (Some(l), _) => l,
            (None, Some(n)) if n.state == marion_core::node::NodeState::Spawning => return false,
            (None, n) => n.map_or_else(String::new, |n| n.agent_type.clone()),
        };
        self.watch.titles.insert(id.0.clone(), title);
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
        let branch = detail.completion.as_ref().and_then(|c| c.branch.clone());
        if let Some(b) = &branch {
            self.watch.branches.insert(id.0.clone(), b.clone());
        }
        match (self.watch.pending.take(), branch) {
            (Some(p), Some(b)) if p == id => self.mode = Mode::Confirm(Effect::Merge(b)),
            (Some(p), None) if p == id => self.notice = Some("the winner landed no branch".into()),
            (other, _) => self.watch.pending = other,
        }
        self.watch.detail = Some((id, detail));
    }

    /// A merge of `branch` went through: its node has nothing left to land.
    pub fn merged(&mut self, branch: &str) {
        self.watch.merged.insert(branch.to_string());
        self.reorder();
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
            Mode::Form(form) => self.form_key(form, key),
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
            self.setup.preview.clear();
            return effect;
        }
        match effect {
            Effect::WriteTypes { draft, .. } => {
                self.setup.preview.clear();
                self.mode = Mode::Form(Form {
                    draft,
                    field: 0,
                    error: None,
                });
                self.notice = Some("not written".into());
            }
            Effect::Logout(_) => self.notice = Some("key kept".into()),
            Effect::Merge(_) => self.notice = Some("not merged".into()),
            Effect::ProfileRemove { .. } => self.notice = Some("profile kept".into()),
            _ => self.notice = Some("not cancelled".into()),
        }
        Effect::None
    }

    fn typing_key(&mut self, purpose: Purpose, mut text: String, key: Key) -> Effect {
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
        self.mode = Mode::Typing { purpose, text };
        Effect::None
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
            Purpose::Steer(target) if !text.trim().is_empty() => Effect::Steer(target, text),
            Purpose::Steer(_) => Effect::None,
            Purpose::New => {
                if text.trim().is_empty() {
                    return refused(self, "type the task".into(), purpose, text);
                }
                let Some(t) = self.new_type().cloned() else {
                    return refused(self, "no agent type to run".into(), purpose, text);
                };
                if let Some(why) = self.new_blocked() {
                    return refused(self, format!("{}: {why}", t.name), purpose, text);
                }
                Effect::Run {
                    pane: self.harness_of(&t).is_some_and(|h| h.pane),
                    agent_type: t.name,
                    model: self.new_model(),
                    prompt: text,
                }
            }
            Purpose::Key => match self.login(&text) {
                Ok(e) => e,
                Err(why) => refused(self, why, purpose, text),
            },
            Purpose::Profile => match self.profile_add(&text) {
                Ok(e) => e,
                Err(why) => refused(self, why, purpose, text),
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
                    let lines = view::KEYS.iter().map(|(_, k)| k.len() + 1).sum::<usize>();
                    self.help_scroll = (self.help_scroll + 1).min(lines);
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
            Key::Char('q' | '?' | ',' | 'n' | 'j' | 'k')
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
                self.mode = Mode::Typing {
                    purpose: Purpose::New,
                    text: String::new(),
                }
            }
            Key::Enter => return self.enter(),
            Key::Esc if self.watch.full => self.watch.full = false,
            Key::Char('s') => match self.selected() {
                Some(n) if !n.state.is_exited() => {
                    self.mode = Mode::Typing {
                        purpose: Purpose::Steer(n.agent_id.clone()),
                        text: String::new(),
                    }
                }
                Some(_) => self.notice = Some("ended: u resumes it".into()),
                None => self.notice = Some("nothing selected".into()),
            },
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
            Key::Char('u') => match self.selected() {
                Some(n) if n.state.is_exited() => return Effect::Resume(n.agent_id.clone()),
                Some(_) => self.notice = Some("still running: s steers it".into()),
                None => self.notice = Some("nothing selected".into()),
            },
            Key::Char('o') => match self.workspace() {
                Some(dir) => return Effect::Shell(dir),
                None => self.notice = Some("no workspace known yet".into()),
            },
            Key::Char('d') => match self.branch() {
                Some(b) => return Effect::Diff(b),
                None => self.notice = Some("no branch landed".into()),
            },
            Key::Char('m') => match self.branch() {
                Some(b) => self.mode = Mode::Confirm(Effect::Merge(b)),
                None => self.notice = Some("no branch to merge".into()),
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

    fn workspace(&self) -> Option<PathBuf> {
        use marion_core::contract::Workspace;
        Some(match self.selected_detail()?.workspace.as_ref()? {
            Workspace::Worktree { path, .. } | Workspace::SharedCwd { path } => path.clone(),
        })
    }

    /// The selected node's landed branch, once its detail has been read.
    pub fn branch(&self) -> Option<String> {
        self.selected_detail()?.completion.as_ref()?.branch.clone()
    }

    fn setup_key(&mut self, key: Key) -> Effect {
        match key {
            Key::Char('q') => return Effect::Quit,
            Key::Char('?') => self.toggle_help(),
            Key::Esc | Key::Char(',') => self.page = Page::Agents,
            Key::Char('j') | Key::Down => {
                // The profiles (or the one row that adds the first), then the notifications row
                // once it has been read.
                let last = self.notify_row() - usize::from(self.setup.notify.is_none());
                self.setup.cursor = (self.setup.cursor + 1).min(last);
            }
            Key::Char('k') | Key::Up => self.setup.cursor = self.setup.cursor.saturating_sub(1),
            // Details are a harness's; a key or a login row has nothing more to show.
            Key::Enter if self.setup.cursor < self.harnesses.len() => {
                self.setup.expanded = !self.setup.expanded
            }
            Key::Enter if self.on_notify() => {
                return Effect::Notify(self.setup.notify != Some(true));
            }
            Key::Char('r') => return Effect::Recheck,
            Key::Char('e') => return Effect::EditTypes,
            Key::Char('n') => {
                self.mode = Mode::Form(Form {
                    draft: types_form::Draft {
                        harness: self.harness_choices().first().cloned().unwrap_or_default(),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            }
            Key::Char('a') if self.on_profiles() => {
                let text = self
                    .selected_profile()
                    .map(|p| format!("{} ", p.harness))
                    .unwrap_or_default();
                self.mode = Mode::Typing {
                    purpose: Purpose::Profile,
                    text,
                };
            }
            Key::Char('a') => {
                let text = self
                    .selected_login()
                    .map(|l| l.provider.clone())
                    .unwrap_or_default();
                self.mode = Mode::Typing {
                    purpose: Purpose::Key,
                    text,
                };
            }
            Key::Char('u') => match self.selected_profile() {
                Some(p) if p.default => {
                    self.notice = Some(format!("{} already runs on {}", p.harness, p.name))
                }
                Some(p) => {
                    return Effect::ProfileUse {
                        harness: p.harness.clone(),
                        name: p.name.clone(),
                    };
                }
                None => self.notice = Some("select a profile".into()),
            },
            Key::Char('x') => match (self.selected_login(), self.selected_profile()) {
                (Some(l), _) => self.mode = Mode::Confirm(Effect::Logout(l.id.clone())),
                (_, Some(p)) => {
                    self.mode = Mode::Confirm(Effect::ProfileRemove {
                        harness: p.harness.clone(),
                        name: p.name.clone(),
                    })
                }
                _ => self.notice = Some("select a key or a profile".into()),
            },
            _ => {}
        }
        Effect::None
    }

    /// The stored key under Setup's cursor, when the cursor is past the harnesses.
    pub fn selected_login(&self) -> Option<&StoredLogin> {
        let i = self.setup.cursor.checked_sub(self.harnesses.len())?;
        self.setup.logins.get(i)
    }

    /// Where Setup's profile rows begin: after the harnesses and the stored keys.
    fn profiles_start(&self) -> usize {
        self.harnesses.len() + self.setup.logins.len()
    }

    /// Setup's cursor is on the profiles: one of them, or the row that adds the first.
    pub fn on_profiles(&self) -> bool {
        self.setup.cursor >= self.profiles_start() && !self.on_notify()
    }

    /// Setup's last row: desktop notifications, after the profiles (or the row that adds the
    /// first).
    fn notify_row(&self) -> usize {
        self.profiles_start() + self.setup.profiles.len().max(1)
    }

    /// Setup's cursor is on the notifications row, which is there once it has been read.
    pub fn on_notify(&self) -> bool {
        self.setup.notify.is_some() && self.setup.cursor == self.notify_row()
    }

    /// The profile under Setup's cursor.
    pub fn selected_profile(&self) -> Option<&StoredProfile> {
        let i = self.setup.cursor.checked_sub(self.profiles_start())?;
        self.setup.profiles.get(i)
    }

    /// A new listing of the profiles. The cursor moves onto `select` where the listing has it — a
    /// profile just added, whose row shows the login it needs — and otherwise stays on a row that
    /// still exists.
    pub fn set_profiles(
        &mut self,
        rows: Vec<StoredProfile>,
        harnesses: Vec<String>,
        select: Option<&str>,
    ) {
        let s = &mut self.setup;
        s.profiles = rows;
        s.profile_harnesses = harnesses;
        s.profiles_listed = true;
        s.profiles_error = None;
        let start = self.profiles_start();
        let last = start + self.setup.profiles.len().max(1) - 1;
        let chosen = select.and_then(|n| self.setup.profiles.iter().position(|p| p.name == n));
        self.setup.cursor = match chosen {
            Some(i) => start + i,
            None => self.setup.cursor.min(last),
        };
    }

    /// The harnesses an agents file can name, in doctor's order: the rows whose name is a harness
    /// spelling the file's loader accepts.
    pub fn harness_choices(&self) -> Vec<String> {
        self.harnesses
            .iter()
            .filter(|h| h.name.parse::<marion_core::harness::Harness>().is_ok())
            .map(|h| h.name.clone())
            .collect()
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

    /// Enter on the profile line: `marion profile add <harness> <name>` when the harness takes
    /// profiles and the name is one — refused here, before the terminal changes hands, otherwise.
    fn profile_add(&self, text: &str) -> Result<Effect, String> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let [harness, name] = words.as_slice() else {
            return Err("type a harness and a name for the profile".into());
        };
        if !self.setup.profile_harnesses.iter().any(|h| h == harness) {
            return Err(format!(
                "`{harness}` takes no profiles; these do: {}",
                self.setup.profile_harnesses.join(", ")
            ));
        }
        if !marion_core::agent_type::is_valid_name(name) {
            return Err(format!(
                "`{name}` is not a profile name: letters, digits, - and _"
            ));
        }
        if self.setup.profiles.iter().any(|p| p.name == *name) {
            return Err(format!("a profile named `{name}` already exists"));
        }
        Ok(Effect::ProfileAdd {
            harness: harness.to_string(),
            name: name.to_string(),
        })
    }

    fn form_key(&mut self, mut form: Form, key: Key) -> Effect {
        let field = Field::ALL[form.field.min(Field::ALL.len() - 1)];
        let step = |form: &mut Form, by: isize| {
            let n = Field::ALL.len() as isize;
            form.field = (form.field as isize + by).rem_euclid(n) as usize;
        };
        match key {
            Key::Esc | Key::Ctrl('c') => return Effect::None,
            Key::Tab | Key::Down => step(&mut form, 1),
            Key::BackTab | Key::Up => step(&mut form, -1),
            Key::Left | Key::Right if field.is_choice() => {
                let by = if key == Key::Right { 1 } else { -1 };
                match field {
                    Field::Harness => {
                        let choices = self.harness_choices();
                        form.draft.harness = cycle(&choices, &form.draft.harness, by);
                    }
                    _ => {
                        let at = types_form::Tools::ALL
                            .iter()
                            .position(|t| *t == form.draft.tools)
                            .unwrap_or(0) as isize;
                        let n = types_form::Tools::ALL.len() as isize;
                        form.draft.tools = types_form::Tools::ALL[(at + by).rem_euclid(n) as usize];
                    }
                }
            }
            Key::Enter => {
                let d = &form.draft;
                let missing: Vec<&str> = [
                    ("name", &d.name),
                    ("harness", &d.harness),
                    ("description", &d.description),
                ]
                .into_iter()
                .filter(|(_, v)| v.trim().is_empty())
                .map(|(n, _)| n)
                .collect();
                if !missing.is_empty() {
                    form.error = Some(format!("needs a {}", missing.join(", a ")));
                    self.mode = Mode::Form(form);
                    return Effect::None;
                }
                form.error = None;
                let draft = form.draft.clone();
                self.mode = Mode::Form(form);
                return Effect::PreviewType(draft);
            }
            Key::Backspace => {
                if let Some(t) = text_of(&mut form.draft, field) {
                    t.pop();
                }
            }
            Key::Char(c) => {
                if let Some(t) = text_of(&mut form.draft, field) {
                    push_bounded(t, &c.to_string());
                }
            }
            Key::Paste(p) => {
                if let Some(t) = text_of(&mut form.draft, field) {
                    push_bounded(t, &p.replace(['\n', '\r'], " "));
                }
            }
            _ => {}
        }
        self.mode = Mode::Form(form);
        Effect::None
    }

    /// The session's answer to [`Effect::PreviewType`]: the file it would write and the change,
    /// which now waits for a `y`.
    pub fn show_preview(&mut self, text: String, diff: Vec<types_form::DiffLine>) {
        let draft = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Form(f) => f.draft,
            other => {
                self.mode = other;
                return;
            }
        };
        self.setup.preview = diff;
        self.mode = Mode::Confirm(Effect::WriteTypes { text, draft });
    }

    /// The session's answer to [`Effect::PreviewType`] when the result would not load: the form
    /// stays open and says why, in the loader's words.
    pub fn form_refused(&mut self, why: String) {
        if let Mode::Form(f) = &mut self.mode {
            f.error = Some(why);
        }
    }
}

impl Default for Home {
    fn default() -> Self {
        Home::new()
    }
}

/// The text field `field` edits in `d`, or `None` for a field picked with ←→.
fn text_of(d: &mut types_form::Draft, field: Field) -> Option<&mut String> {
    match field {
        Field::Name => Some(&mut d.name),
        Field::Model => Some(&mut d.model),
        Field::Description => Some(&mut d.description),
        Field::Provider => Some(&mut d.provider),
        Field::Harness | Field::Tools => None,
    }
}

/// The choice `by` steps from `current` in `choices`, wrapping; the first when `current` is not one.
fn cycle(choices: &[String], current: &str, by: isize) -> String {
    if choices.is_empty() {
        return current.to_string();
    }
    let n = choices.len() as isize;
    let at = choices.iter().position(|c| c == current);
    let next = match at {
        Some(i) => (i as isize + by).rem_euclid(n),
        None => 0,
    };
    choices[next as usize].clone()
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
