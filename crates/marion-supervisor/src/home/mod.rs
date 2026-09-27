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
    /// `node/kill`. Destructive: only ever returned after a confirming `y`.
    Cancel(AgentId),
    /// Leave the screen, run `marion resume <id>`, come back.
    Resume(AgentId),
    /// Leave the screen, open the operator's `$SHELL` in this directory, come back.
    Shell(PathBuf),
    /// Leave the screen, show what the branch changed, come back.
    Diff(String),
    /// Put this text on the operator's clipboard (OSC 52).
    Copy(String),
    /// Probe the harnesses again (`marion doctor`).
    Recheck,
    /// Leave the screen, open the project's agents file in `$EDITOR`, come back and validate it.
    EditTypes,
}

impl Effect {
    /// The command line this effect is, for the box to show and for the operator to learn. `None`
    /// for [`Effect::None`] and [`Effect::Quit`], which are not commands.
    pub fn argv(&self) -> Option<Vec<String>> {
        let v = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        Some(match self {
            Effect::None | Effect::Quit => return None,
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
            Effect::Resume(id) => v(&["marion", "resume", &id.0]),
            Effect::Shell(dir) => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
                v(&["cd", &dir.to_string_lossy(), "&&", &shell])
            }
            Effect::Diff(branch) => v(&["git", "log", "-p", "--stat", &format!("HEAD..{branch}")]),
            Effect::Copy(text) => vec![text.clone()],
            Effect::Recheck => v(&["marion", "doctor"]),
            Effect::EditTypes => {
                let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
                v(&[&editor, crate::run::AGENT_TYPES_FILE])
            }
        })
    }

    /// Whether this effect ends or discards something, and so waits for a `y`.
    pub fn destructive(&self) -> bool {
        matches!(self, Effect::Cancel(_))
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
        }
    }
}

/// Setup's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setup {
    pub cursor: usize,
    pub expanded: bool,
    /// The last validation of the agents file: passed, and its one line.
    pub validation: Option<(bool, String)>,
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
            Tab::Start | Tab::Setup => self.checking,
            Tab::Help => false,
        }
    }

    /// What Enter would do on Watch: the effect the box echoes before it is pressed.
    pub fn watch_default(&self) -> Effect {
        match self.selected() {
            Some(n) => Effect::Attach(n.agent_id.clone()),
            None => Effect::None,
        }
    }

    // ------------------------------------------------------------------ the forest

    /// A new snapshot of the forest, keeping the selection where the node survives.
    pub fn set_nodes(&mut self, nodes: Vec<NodeSummary>) {
        let keep = self.watch.tree.selected().map(|n| n.id.clone());
        self.watch.tree = crate::tree::build(&nodes, keep.as_deref());
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
                self.watch.stream.extend(page.lines);
                self.watch.next = Some(page.next);
            }
            if let Some(why) = page.unread {
                detail.stream = Some(marion_core::proto::result::ActivityPage {
                    unread: Some(why),
                    ..Default::default()
                });
            }
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
                    return effect;
                }
                self.notice = Some("not cancelled".into());
                Effect::None
            }
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
            _ => {}
        }
        match self.tab {
            Tab::Start => self.start_key(key),
            Tab::Watch => self.watch_key(key),
            Tab::Setup => self.setup_key(key),
            Tab::Help => match key {
                Key::Char('q') => Effect::Quit,
                Key::Esc | Key::Char('?') => {
                    self.tab = Tab::Start;
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
        match key {
            Key::Char('q') => return Effect::Quit,
            Key::Char('?') => self.tab = Tab::Help,
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
            Key::Enter => return self.watch_default(),
            Key::Char('s') => match self.selected() {
                Some(n) if !n.state.is_exited() => {
                    self.mode = Mode::Compose {
                        target: n.agent_id.clone(),
                        text: String::new(),
                    }
                }
                Some(_) => self.notice = Some("it has ended; resume it with u".into()),
                None => self.notice = Some("nothing selected".into()),
            },
            Key::Char('x') => match self.selected() {
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
                Some(b) => return Effect::Copy(format!("git merge --no-ff {b}")),
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
            Key::Char('?') => self.tab = Tab::Help,
            Key::Char('j') | Key::Down => {
                let n = self.harnesses.len();
                self.setup.cursor = (self.setup.cursor + 1).min(n.saturating_sub(1));
            }
            Key::Char('k') | Key::Up => self.setup.cursor = self.setup.cursor.saturating_sub(1),
            Key::Enter => self.setup.expanded = !self.setup.expanded,
            Key::Char('r') => return Effect::Recheck,
            Key::Char('e') => return Effect::EditTypes,
            _ => {}
        }
        Effect::None
    }
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
