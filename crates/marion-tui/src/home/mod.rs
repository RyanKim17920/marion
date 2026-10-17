//! The home screen bare `marion` opens: a terminal agent manager. The agents in a list on the
//! left, the selected one previewed on the right, one status line above and one footer line below.
//! Setup (`,`) and help (`?`) take the body's place; nothing else is a screen of its own.
//!
//! # Layout, top to bottom
//!
//! The status line (`marion  <project>`, then the counts on the right), the body, and the footer.
//! The footer is the one place an operator types: it shows the keys the selection takes, and turns
//! into the input line (a new task, a steer) or the confirm line (anything destructive) while one
//! is open. The confirm line shows the command the action is, so the screen still teaches the CLI.
//!
//! # What this module is not
//!
//! Like [`crate::tree`], it decides nothing. Every view here is plain, already-worded data — a state
//! [`crate::tree::Tone`], a title, token counts — projected by the supervisor, which is the only
//! crate that can read a `NodeState` or a doctor row. What this module owns is the look: one accent
//! ([`theme`]), status as glyph plus ANSI-16 colour, dim for context, and column arithmetic
//! ([`text`]) that loses the least important words first. It never shows a price: marion reports
//! tokens, and a dollar figure would be a guess about the operator's plan.

pub mod agents;
pub mod help;
pub mod keys;
pub mod setup;
pub mod text;
pub mod theme;
pub mod widgets;

pub use agents::{AgentRow, AgentsView, Ink, LineKind, Preview, StreamLine};
pub use help::{HelpView, KeyRow};
pub use setup::{AgentTypeRow, FormField, FormView, HarnessRow, LoginRow, ProfileRow, SetupView};
pub use theme::{Ready, Theme};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use widgets::{FooterLine, StatusLine};

/// Columns between the screen edge and a page's content, where Setup puts its selection caret.
pub const GUTTER: u16 = 2;

/// What the body shows: the agents, Setup, or every key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Agents,
    Setup,
    Help,
}

/// One `key verb` pair on the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub key: String,
    pub verb: String,
}

impl Hint {
    pub fn new(key: impl Into<String>, verb: impl Into<String>) -> Self {
        Hint {
            key: key.into(),
            verb: verb.into(),
        }
    }
}

/// What the footer row holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Footer {
    /// The keys the selection takes, `? more` kept last when the row is too narrow for them all.
    Keys(Vec<Hint>),
    /// A line being typed: `target` names what it is for (`new claude-orchestrator`, `steer codex
    /// 91cd`), `note` is a dim word after the target (the model, or why the type cannot run), and
    /// `keys` are the line's own keys on the right.
    Input {
        target: String,
        note: String,
        text: String,
        keys: Vec<Hint>,
    },
    /// Something destructive waiting for `y`: the question and the command it would run.
    Confirm { question: String, command: String },
}

/// The body.
#[derive(Debug, Clone, Copy)]
pub enum Body<'a> {
    Agents(&'a AgentsView),
    Setup(&'a SetupView),
    Help(&'a HelpView),
}

/// The whole screen, one frame of it.
#[derive(Debug, Clone)]
pub struct Screen<'a> {
    pub theme: Theme,
    /// What the status line names after `marion`: the project, or the page (`setup`).
    pub title: &'a str,
    /// The status line's right side, already worded (`2 running  2 need you  1 to land`).
    pub counts: &'a str,
    pub body: Body<'a>,
    pub footer: Footer,
    /// Shown on the footer in place of its keys: what the last key did, or why it did nothing.
    pub notice: Option<String>,
    /// Animation tick: spinners advance one frame per tick.
    pub frame: usize,
}

/// The rectangles a [`Screen`] is drawn into, for the tests and for a caller placing a cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub status: Rect,
    pub body: Rect,
    pub footer: Rect,
}

impl Layout {
    /// The status line, the body, the footer. The footer is claimed first, so a window one row
    /// tall still says what a key will do.
    pub fn of(area: Rect) -> Layout {
        let footer_h = area.height.min(1);
        let status_h = area.height.saturating_sub(footer_h).min(1);
        let footer = Rect::new(area.x, area.bottom() - footer_h, area.width, footer_h);
        let status = Rect::new(area.x, area.y, area.width, status_h);
        let body = Rect::new(
            area.x,
            area.y + status_h,
            area.width,
            area.height - status_h - footer_h,
        );
        Layout {
            status,
            body,
            footer,
        }
    }
}

impl Widget for &Screen<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let layout = Layout::of(area);
        StatusLine {
            title: self.title,
            counts: self.counts,
        }
        .render(layout.status, buf);
        match self.body {
            Body::Agents(v) => agents::render(v, self.theme, self.frame, layout.body, buf),
            Body::Setup(v) => setup::render(v, self.theme, self.frame, layout.body, buf),
            Body::Help(v) => help::render(v, self.theme, layout.body, buf),
        }
        FooterLine {
            footer: &self.footer,
            notice: self.notice.as_deref(),
            theme: self.theme,
        }
        .render(layout.footer, buf);
        self.theme.decolour(area, buf);
    }
}
