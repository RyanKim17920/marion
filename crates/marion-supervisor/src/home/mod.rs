//! **The home screen**: what bare `marion` opens on a terminal, and what `marion ls` opens on its
//! Watch tab.
//!
//! # Three layers, and only the last one does I/O
//!
//! * This module is the **state machine**: [`Home`] holds what the operator is looking at, and
//!   [`Home::key`] turns one key into the next state and at most one [`Effect`]. It reads no file,
//!   dials no socket and draws nothing, so every decision here — which key does what on which tab,
//!   that nothing destructive happens without a `y` — is a plain unit test.
//! * [`view`] projects a [`Home`] onto `marion_tui::home`'s views, which only draw.
//! * [`session`] runs the loop: it paints, reads the keyboard, keeps the forest current from
//!   `tree/subscribe`, and carries each [`Effect`] out **through a command or an RPC marion already
//!   has** — a run is `marion run … --detach`, a steer is `node/steer`, a cancel is `node/kill`, an
//!   attach is [`crate::attach::run`] after leaving the screen, exactly as the old tree screen did.
//!   There is no second spawn path here.
//!
//! # Every effect is a command
//!
//! [`Effect::argv`] is the command line the effect stands for, and the bottom box shows it before a
//! key is pressed: the home screen teaches the CLI rather than hiding it. `marion`'s own tests parse
//! each of those lines back through the binary's argument parser and compare, so an effect and its
//! echo cannot drift apart.

pub mod session;
pub mod types_form;
pub mod view;
pub mod wake;

use marion_core::contract::AgentId;
use marion_core::proto::NodeSummary;
use marion_core::proto::result::{ActionLine, NodeDetail};
use marion_tui::home::keys::Key;
use marion_tui::home::{Ready, Tab};
use marion_tui::tree::Tree;
use std::path::PathBuf;

/// What a key asks the session to do. [`Effect::None`] is by far the most common answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    /// `marion run <type> [--model <m>] --prompt <text> --detach`, or `--pane` in place of
    /// `--detach` for a root in a terminal marion owns (which returns at once too, and is the kind
    /// Enter can attach to).
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
    /// Put this text on the operator's clipboard (OSC 52).
    Copy(String),
    /// Leave the screen, run `git merge --no-ff <branch>` in the project, come back. It changes the
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
    /// The command line this effect is, for the box to show and for the operator to learn. `None`
    /// for [`Effect::None`] and [`Effect::Quit`], which are not commands, and for the form's
    /// preview and write, which the box and the diff describe instead.
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
            Effect::Diff(branch) => v(&["git", "log", "-p", "--stat", &format!("HEAD..{branch}")]),
            Effect::Copy(text) => vec![text.clone()],
            Effect::Merge(branch) => v(&["git", "merge", "--no-ff", branch]),
            Effect::Recheck => v(&["marion", "doctor"]),
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
    /// Writing a steer for a node.
    Compose {
        target: AgentId,
        text: String,
    },
    /// A destructive effect waiting for `y`.
    Confirm(Effect),
    /// Typing the credential id (`<provider>[:<label>]`) to hand to `marion login`.
    Login {
        text: String,
    },
    /// Typing `<harness> <name>` to hand to `marion profile add`.
    Profile {
        text: String,
    },
    /// Filling in the agent-type form.
    Form(Form),
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

/// Start's choices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Start {
    /// Into [`Home::start_harnesses`].
    pub cursor: usize,
    /// Into [`Home::kinds`] for the selected harness.
    pub kind: usize,
    /// Into [`Home::models`] for the selected harness; 0 is `default`.
    pub model: usize,
    pub prompt: String,
    /// Run in a terminal marion owns (`--pane`) rather than headless: attachable with Enter, but
    /// with no action stream to show, since a terminal emits bytes rather than frames.
    pub pane: bool,
}

/// Watch's state: the forest, the selection, and the selected node's detail and stream.
#[derive(Debug, Clone)]
pub struct Watch {
    /// A supervisor is serving this project.
    pub supervisor: bool,
    pub nodes: Vec<NodeSummary>,
    pub tree: Tree,
    /// The selected node's last `node/get` detail.
    pub detail: Option<(AgentId, NodeDetail)>,
    /// The selected node's action stream so far, and where the next page starts.
    pub stream: Vec<ActionLine>,
    pub next: Option<u64>,
    /// Lines up from the newest the stream window is scrolled; 0 follows.
    pub scroll: usize,
    /// What changed in the forest while the screen watched it, newest first, at most
    /// [`FEED_MAX`].
    pub feed: Vec<FeedItem>,
    /// Every node's subtree totals, rebuilt with the forest ([`crate::rollup`]).
    pub rollup: crate::rollup::Rollup,
    /// The selected agent's stream fills the screen: Enter on a headless agent, Esc back.
    pub full_stream: bool,
    /// `m` or `c` pressed on a race row: done to its winner once the winner's detail (and branch)
    /// arrives.
    pub pending: Option<(AgentId, char)>,
}

/// One change in the forest, as the feed shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    pub at: std::time::SystemTime,
    pub tone: marion_tui::tree::Tone,
    pub short: String,
    pub text: String,
}

/// The most feed lines kept: more than any screen shows.
pub const FEED_MAX: usize = 50;

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
            feed: Vec::new(),
            rollup: crate::rollup::Rollup::default(),
            full_stream: false,
            pending: None,
        }
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
}

/// The whole home screen's state.
#[derive(Debug, Clone)]
pub struct Home {
    pub tab: Tab,
    pub mode: Mode,
    pub start: Start,
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
    /// The tab help was opened from, and goes back to.
    pub help_from: Tab,
    /// Rows help is scrolled down, where it does not fit the screen.
    pub help_scroll: usize,
}

/// How many stream lines a Page Up / Page Down moves.
pub const PAGE: usize = 8;

impl Home {
    pub fn new(tab: Tab) -> Self {
        Home {
            tab,
            mode: Mode::Normal,
            start: Start::default(),
            watch: Watch::default(),
            setup: Setup::default(),
            harnesses: Vec::new(),
            checking: false,
            types: Vec::new(),
            recent_models: Vec::new(),
            notice: None,
            notice_sticky: false,
            help_from: tab,
            help_scroll: 0,
        }
    }

    /// Open help, remembering the tab to go back to; from help itself, go back.
    fn toggle_help(&mut self) {
        if self.tab == Tab::Help {
            self.tab = self.help_from;
        } else {
            self.help_from = self.tab;
            self.help_scroll = 0;
            self.tab = Tab::Help;
        }
    }

    // ------------------------------------------------------------------ derived facts

    /// The harnesses Start offers: those that have at least one agent type, in doctor's order.
    pub fn start_harnesses(&self) -> Vec<&Harness> {
        self.harnesses
            .iter()
            .filter(|h| self.types.iter().any(|t| t.harness == h.name))
            .collect()
    }

    fn start_harness(&self) -> Option<&Harness> {
        self.start_harnesses().get(self.start.cursor).copied()
    }

    /// The agent types of the harness under Start's cursor: implementers first, then read-only.
    pub fn kinds(&self) -> Vec<&AgentType> {
        let Some(h) = self.start_harness() else {
            return Vec::new();
        };
        let mut ts: Vec<&AgentType> = self.types.iter().filter(|t| t.harness == h.name).collect();
        ts.sort_by_key(|t| (!t.writes, t.custom));
        ts
    }

    /// `default`, then the models the selected harness was last run on.
    pub fn models(&self) -> Vec<String> {
        let mut out = vec!["default".to_string()];
        if let Some(h) = self.start_harness()
            && let Some((_, ms)) = self.recent_models.iter().find(|(n, _)| *n == h.name)
        {
            out.extend(ms.iter().cloned());
        }
        out
    }

    /// The run Start's choices add up to, whether or not a prompt has been typed.
    pub fn pending_run(&self) -> Option<Effect> {
        let kind = self.kinds().get(self.start.kind)?.name.clone();
        let model = self
            .models()
            .get(self.start.model)
            .filter(|m| *m != "default")
            .cloned();
        Some(Effect::Run {
            agent_type: kind,
            model,
            prompt: self.start.prompt.clone(),
            pane: self.start.pane,
        })
    }

    /// The node under Watch's cursor.
    pub fn selected(&self) -> Option<&NodeSummary> {
        let id = &self.watch.tree.selected()?.id;
        self.watch.nodes.iter().find(|n| n.agent_id.0 == *id)
    }

    /// The race whose header row is under Watch's cursor.
    pub fn selected_race(&self) -> Option<String> {
        crate::tree::race_of_row(&self.watch.tree.selected()?.id).map(str::to_string)
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

    /// A key on a race's header row, which has no agent of its own: Enter goes to the winner, and
    /// `m`/`c` act on the winner's branch once its detail is read. Anything else says what does.
    fn race_key(&mut self, race: &str, key: &Key) -> Effect {
        let winner = self.race_winner(race);
        match (key, winner) {
            (Key::Enter, Some(w)) => {
                self.select(&w.0);
            }
            (Key::Char(c @ ('m' | 'c')), Some(w)) => {
                self.watch.pending = Some((w.clone(), *c));
                self.select(&w.0);
            }
            (Key::Enter | Key::Char('m' | 'c'), None) => {
                self.notice = Some("no agent has won this race yet; its agents are below".into())
            }
            _ => {
                self.notice = Some(
                    "a race row: enter goes to its winner, m merges it; select an agent below to \
                     steer or cancel it"
                        .into(),
                )
            }
        }
        Effect::None
    }

    fn selected_detail(&self) -> Option<&NodeDetail> {
        let sel = self.selected()?;
        match &self.watch.detail {
            Some((id, d)) if *id == sel.agent_id => Some(d),
            _ => None,
        }
    }

    /// Whether anything the current tab shows moves with time — a running node's elapsed time and
    /// spinner on Watch, a harness still being checked on Start or Setup — and so needs a clock.
    /// `false` is the idle screen, which sleeps until something happens.
    pub fn animating(&self) -> bool {
        match self.tab {
            Tab::Watch => self.watch.nodes.iter().any(|n| !n.state.is_exited()),
            Tab::Start => self.checking,
            Tab::Setup => self.checking || self.setup.profiles.iter().any(|p| p.login.is_none()),
            Tab::Help => false,
        }
    }

    /// What Enter would do on Watch: the effect the box echoes before it is pressed. Only a pane
    /// agent has a terminal to attach to; Enter on a headless one opens its stream instead, which
    /// is no command.
    pub fn watch_default(&self) -> Effect {
        match self.selected() {
            Some(n) if n.pane => Effect::Attach(n.agent_id.clone()),
            _ => Effect::None,
        }
    }

    // ------------------------------------------------------------------ the forest

    /// A new snapshot of the forest, keeping the selection where the node survives.
    pub fn set_nodes(&mut self, nodes: Vec<NodeSummary>) {
        if self.watch.supervisor {
            let now = std::time::SystemTime::now();
            for item in feed_items(&self.watch.nodes, &nodes) {
                self.watch.feed.insert(0, FeedItem { at: now, ..item });
            }
            self.watch.feed.truncate(FEED_MAX);
        }
        let keep = self.watch.tree.selected().map(|n| n.id.clone());
        self.watch.tree = crate::tree::build(&nodes, keep.as_deref());
        self.watch.rollup = crate::rollup::Rollup::build(&nodes);
        self.watch.nodes = nodes;
        self.watch.supervisor = true;
    }

    /// The supervisor went away (or was never there).
    pub fn lost_supervisor(&mut self) {
        self.watch = Watch::default();
    }

    /// Select `id` on Watch, if the forest has it.
    pub fn select(&mut self, id: &str) -> bool {
        let found = self.watch.tree.select(id);
        if found {
            self.selection_moved();
        }
        found
    }

    /// A `node/get` answer for `id`: its detail, and the page of its stream appended.
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
        let branch = detail.completion.as_ref().and_then(|c| c.branch.clone());
        let pending = self.watch.pending.take();
        match (pending, branch) {
            (Some((p, 'm')), Some(b)) if p == id => self.mode = Mode::Confirm(Effect::Merge(b)),
            (Some((p, _)), Some(b)) if p == id => {
                self.notice = Some(marion_core::contract::merge_command(&b))
            }
            (Some((p, _)), None) if p == id => {
                self.notice = Some("the winner landed no branch to merge".into())
            }
            (other, _) => self.watch.pending = other,
        }
        self.watch.detail = Some((id, detail));
    }

    fn selection_moved(&mut self) {
        self.watch.scroll = 0;
    }

    // ------------------------------------------------------------------ keys

    /// One key: the next state, and what (if anything) the session should do.
    pub fn key(&mut self, key: Key) -> Effect {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Confirm(effect) => {
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
                        self.notice = Some("not written; still editing".into());
                    }
                    Effect::Logout(_) => self.notice = Some("key kept".into()),
                    Effect::Merge(_) => self.notice = Some("not merged".into()),
                    Effect::ProfileRemove { .. } => self.notice = Some("profile kept".into()),
                    _ => self.notice = Some("not cancelled".into()),
                }
                Effect::None
            }
            Mode::Login { mut text } => match key {
                Key::Enter => self.login(text),
                Key::Esc | Key::Ctrl('c') => Effect::None,
                Key::Backspace => {
                    text.pop();
                    self.mode = Mode::Login { text };
                    Effect::None
                }
                Key::Char(c) => {
                    push_bounded(&mut text, &c.to_string());
                    self.mode = Mode::Login { text };
                    Effect::None
                }
                Key::Paste(p) => {
                    push_bounded(&mut text, p.trim());
                    self.mode = Mode::Login { text };
                    Effect::None
                }
                _ => {
                    self.mode = Mode::Login { text };
                    Effect::None
                }
            },
            Mode::Profile { mut text } => match key {
                Key::Enter => self.profile_add(text),
                Key::Esc | Key::Ctrl('c') => Effect::None,
                Key::Backspace => {
                    text.pop();
                    self.mode = Mode::Profile { text };
                    Effect::None
                }
                Key::Char(c) => {
                    push_bounded(&mut text, &c.to_string());
                    self.mode = Mode::Profile { text };
                    Effect::None
                }
                Key::Paste(p) => {
                    push_bounded(&mut text, p.trim());
                    self.mode = Mode::Profile { text };
                    Effect::None
                }
                _ => {
                    self.mode = Mode::Profile { text };
                    Effect::None
                }
            },
            Mode::Form(form) => self.form_key(form, key),
            Mode::Compose { target, mut text } => match key {
                Key::Enter if !text.trim().is_empty() => Effect::Steer(target, text),
                Key::Enter | Key::Esc | Key::Ctrl('c') => Effect::None,
                Key::Backspace => {
                    text.pop();
                    self.mode = Mode::Compose { target, text };
                    Effect::None
                }
                Key::Char(c) => {
                    push_bounded(&mut text, &c.to_string());
                    self.mode = Mode::Compose { target, text };
                    Effect::None
                }
                Key::Paste(p) => {
                    push_bounded(&mut text, &p);
                    self.mode = Mode::Compose { target, text };
                    Effect::None
                }
                _ => {
                    self.mode = Mode::Compose { target, text };
                    Effect::None
                }
            },
            Mode::Normal => self.normal(key),
        }
    }

    fn normal(&mut self, key: Key) -> Effect {
        // Keys that mean the same on every tab.
        match key {
            Key::Ctrl('c') => return Effect::Quit,
            Key::Tab => {
                self.tab = next_tab(self.tab, 1);
                return Effect::None;
            }
            Key::BackTab => {
                self.tab = next_tab(self.tab, -1);
                return Effect::None;
            }
            Key::F1 => {
                self.toggle_help();
                return Effect::None;
            }
            _ => {}
        }
        match self.tab {
            Tab::Start => self.start_key(key),
            Tab::Watch => self.watch_key(key),
            Tab::Setup => self.setup_key(key),
            Tab::Help => match key {
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

    /// Start: the box is always typing, so letters are text; the choices move on arrows and `^o`.
    fn start_key(&mut self, key: Key) -> Effect {
        let s = &mut self.start;
        match key {
            // `?` is a question mark in a prompt, and help on an empty one.
            Key::Char('?') if s.prompt.is_empty() => self.toggle_help(),
            Key::Char(c) => push_bounded(&mut s.prompt, &c.to_string()),
            Key::Paste(p) => push_bounded(&mut s.prompt, &p),
            Key::Backspace => {
                s.prompt.pop();
            }
            Key::Esc => s.prompt.clear(),
            Key::Up => {
                s.cursor = s.cursor.saturating_sub(1);
                s.kind = 0;
                s.model = 0;
            }
            Key::Down => {
                let n = self.start_harnesses().len();
                let s = &mut self.start;
                s.cursor = (s.cursor + 1).min(n.saturating_sub(1));
                s.kind = 0;
                s.model = 0;
            }
            Key::Left => s.model = s.model.saturating_sub(1),
            Key::Right => {
                let n = self.models().len();
                self.start.model = (self.start.model + 1).min(n.saturating_sub(1));
            }
            Key::Ctrl('p') => self.start.pane = !self.start.pane,
            Key::Ctrl('o') => {
                let n = self.kinds().len().max(1);
                self.start.kind = (self.start.kind + 1) % n;
            }
            Key::Enter => return self.start_run(),
            _ => {}
        }
        Effect::None
    }

    fn start_run(&mut self) -> Effect {
        if self.start.prompt.trim().is_empty() {
            self.notice = Some("type what the agent should do, then enter".into());
            return Effect::None;
        }
        match self.start_harness() {
            Some(h) if matches!(h.ready, Ready::Ready) => {}
            Some(h) if matches!(h.ready, Ready::Checking) => {
                self.notice = Some(format!("{} is still being checked", h.name));
                return Effect::None;
            }
            Some(h) => {
                self.notice = Some(format!("{} is not ready: {}", h.name, h.note));
                return Effect::None;
            }
            None => {
                self.notice = Some("no harness to run on".into());
                return Effect::None;
            }
        }
        let run = self.pending_run().unwrap_or(Effect::None);
        if run != Effect::None {
            self.start.prompt.clear();
        }
        run
    }

    fn watch_key(&mut self, key: Key) -> Effect {
        if let Some(race) = self.selected_race()
            && !matches!(
                key,
                Key::Char('q' | '?' | 'j' | 'k' | '!' | 'J' | 'K')
                    | Key::Up
                    | Key::Down
                    | Key::PageUp
                    | Key::PageDown
            )
        {
            return self.race_key(&race, &key);
        }
        match key {
            Key::Char('q') => return Effect::Quit,
            Key::Char('?') => self.toggle_help(),
            Key::Char('j') | Key::Down => {
                self.watch.tree.move_by(1);
                self.selection_moved();
            }
            Key::Char('k') | Key::Up => {
                self.watch.tree.move_by(-1);
                self.selection_moved();
            }
            Key::Char('K') | Key::PageUp => {
                let step = if key == Key::PageUp { PAGE } else { 1 };
                let max = self.watch.stream.len();
                self.watch.scroll = (self.watch.scroll + step).min(max);
            }
            Key::Char('J') | Key::PageDown => {
                let step = if key == Key::PageDown { PAGE } else { 1 };
                self.watch.scroll = self.watch.scroll.saturating_sub(step);
            }
            Key::Char('!') => {
                let nodes = self.watch.nodes.clone();
                let hit = self.watch.tree.cycle_to(|id| {
                    nodes
                        .iter()
                        .any(|n| n.agent_id.0 == id && crate::tree::attention_of(n).is_some())
                });
                self.notice = (!hit).then(|| "nothing needs you".to_string());
                self.selection_moved();
            }
            Key::Enter => match self.selected() {
                Some(n) if n.pane => return Effect::Attach(n.agent_id.clone()),
                Some(_) => {
                    self.watch.full_stream = !self.watch.full_stream;
                    self.watch.scroll = 0;
                }
                None => self.notice = Some("nothing selected".into()),
            },
            Key::Esc if self.watch.full_stream => self.watch.full_stream = false,
            Key::Char('s') => match self.selected() {
                Some(n) if !n.state.is_exited() => {
                    self.mode = Mode::Compose {
                        target: n.agent_id.clone(),
                        text: String::new(),
                    }
                }
                Some(_) => {
                    self.notice =
                        Some("it has ended; u brings it back from its recorded session".into())
                }
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
                Some(_) => self.notice = Some("it has already ended".into()),
                None => self.notice = Some("nothing selected".into()),
            },
            Key::Char('u') => match self.selected() {
                Some(n) if n.state.is_exited() => return Effect::Resume(n.agent_id.clone()),
                Some(_) => self.notice = Some("it is still running; steer it with s".into()),
                None => self.notice = Some("nothing selected".into()),
            },
            Key::Char('o') => match self.workspace() {
                Some(dir) => return Effect::Shell(dir),
                None => self.notice = Some("no workspace known for it yet".into()),
            },
            Key::Char('d') => match self.branch() {
                Some(b) => return Effect::Diff(b),
                None => self.notice = Some("it has landed no branch".into()),
            },
            Key::Char('c') => match self.branch() {
                Some(b) => return Effect::Copy(marion_core::contract::merge_command(&b)),
                None => self.notice = Some("it has landed no branch to merge".into()),
            },
            Key::Char('m') => match self.branch() {
                Some(b) => self.mode = Mode::Confirm(Effect::Merge(b)),
                None => self.notice = Some("it has landed no branch to merge".into()),
            },
            _ => {}
        }
        Effect::None
    }

    fn workspace(&self) -> Option<PathBuf> {
        use marion_core::contract::Workspace;
        Some(match self.selected_detail()?.workspace.as_ref()? {
            Workspace::Worktree { path, .. } | Workspace::SharedCwd { path } => path.clone(),
        })
    }

    fn branch(&self) -> Option<String> {
        self.selected_detail()?.completion.as_ref()?.branch.clone()
    }

    fn setup_key(&mut self, key: Key) -> Effect {
        match key {
            Key::Char('q') => return Effect::Quit,
            Key::Char('?') => self.toggle_help(),
            Key::Char('j') | Key::Down => {
                // The profiles, or the one row that adds the first while there are none.
                let n = self.profiles_start() + self.setup.profiles.len().max(1);
                self.setup.cursor = (self.setup.cursor + 1).min(n.saturating_sub(1));
            }
            Key::Char('k') | Key::Up => self.setup.cursor = self.setup.cursor.saturating_sub(1),
            // Details are a harness's; a key or a login row has nothing more to show.
            Key::Enter if self.setup.cursor < self.harnesses.len() => {
                self.setup.expanded = !self.setup.expanded
            }
            Key::Enter => {
                self.notice = Some(if self.on_profiles() {
                    "a adds a login · u uses the selected one · x removes it".into()
                } else {
                    "a adds a key · x removes the selected one".into()
                })
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
                self.mode = Mode::Profile { text };
            }
            Key::Char('a') => {
                let text = self
                    .selected_login()
                    .map(|l| l.provider.clone())
                    .unwrap_or_default();
                self.mode = Mode::Login { text };
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
                None => self.notice = Some("select a profile to use it".into()),
            },
            Key::Char('x') => match (self.selected_login(), self.selected_profile()) {
                (Some(l), _) => self.mode = Mode::Confirm(Effect::Logout(l.id.clone())),
                (_, Some(p)) => {
                    self.mode = Mode::Confirm(Effect::ProfileRemove {
                        harness: p.harness.clone(),
                        name: p.name.clone(),
                    })
                }
                _ => self.notice = Some("select a stored key or a profile to remove it".into()),
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
        self.setup.cursor >= self.profiles_start()
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

    /// Enter in the login box: `marion login <text>` when its provider is one marion knows.
    fn login(&mut self, text: String) -> Effect {
        let text = text.trim().to_string();
        let provider = text.split(':').next().unwrap_or("");
        if text.is_empty() {
            self.notice = Some("type a provider id, then enter".into());
            return Effect::None;
        }
        if !self.setup.providers.iter().any(|p| p == provider) {
            self.notice = Some(format!(
                "no provider named `{provider}`; `marion key list` shows them"
            ));
            return Effect::None;
        }
        Effect::Login(text)
    }

    /// Enter in the profile box: `marion profile add <harness> <name>` when the harness takes
    /// profiles and the name is one — refused here, before the terminal changes hands, otherwise.
    fn profile_add(&mut self, text: String) -> Effect {
        let words: Vec<&str> = text.split_whitespace().collect();
        let [harness, name] = words.as_slice() else {
            self.notice = Some("type a harness and a name for the profile, then enter".into());
            return Effect::None;
        };
        if !self.setup.profile_harnesses.iter().any(|h| h == harness) {
            self.notice = Some(format!(
                "`{harness}` takes no profiles; these do: {}",
                self.setup.profile_harnesses.join(", ")
            ));
            return Effect::None;
        }
        if !marion_core::agent_type::is_valid_name(name) {
            self.notice = Some(format!(
                "`{name}` is not a profile name: letters, digits, - and _"
            ));
            return Effect::None;
        }
        if self.setup.profiles.iter().any(|p| p.name == *name) {
            self.notice = Some(format!("a profile named `{name}` already exists"));
            return Effect::None;
        }
        Effect::ProfileAdd {
            harness: harness.to_string(),
            name: name.to_string(),
        }
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

/// What moved between two snapshots, oldest first: a node that appeared, and a node that came
/// to need the operator or ended. Other moves (spawning to running, a turn starting) are the row's
/// to show, not news.
fn feed_items(old: &[NodeSummary], new: &[NodeSummary]) -> Vec<FeedItem> {
    use crate::tree::{attention_of, row, short_id};
    use marion_core::node::NodeState;
    let mut out = Vec::new();
    for n in new {
        let item = |text: String| FeedItem {
            at: std::time::UNIX_EPOCH,
            tone: row(n).tone,
            short: short_id(&n.agent_id.0).to_string(),
            text,
        };
        match old.iter().find(|o| o.agent_id == n.agent_id) {
            None => out.push(item(match &n.parent_id {
                Some(p) => format!("spawned by {} · {}", short_id(&p.0), n.agent_type),
                None => format!("started · {}", n.agent_type),
            })),
            Some(o) if o.state != n.state => {
                let text = match n.state {
                    NodeState::Exited(s) => Some(s.word().to_string()),
                    NodeState::Blocked(_) => attention_of(n),
                    _ => None,
                };
                out.extend(text.map(item));
            }
            Some(_) => {}
        }
    }
    out
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

/// The largest prompt or steer the box takes: `node/steer`'s own bound.
const TEXT_LIMIT: usize = marion_core::proto::params::MAX_STEER_BYTES;

fn push_bounded(text: &mut String, more: &str) {
    for c in more.chars() {
        if text.len() + c.len_utf8() > TEXT_LIMIT {
            break;
        }
        text.push(c);
    }
}

fn next_tab(tab: Tab, step: isize) -> Tab {
    let i = Tab::ALL.iter().position(|t| *t == tab).unwrap_or(0) as isize;
    let n = Tab::ALL.len() as isize;
    Tab::ALL[(i + step).rem_euclid(n) as usize]
}

#[cfg(test)]
mod tests;
