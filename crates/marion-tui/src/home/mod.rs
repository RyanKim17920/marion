//! The home screen bare `marion` opens: **Start** (run a task), **Watch** (the forest, the
//! selected node expanded in place), **Setup** (what `marion doctor` found, agent types, logins)
//! and **?** (every key).
//!
//! # Layout, top to bottom
//!
//! A [`widgets::TopBar`] — `◆ marion  <project>` and the tabs — then the tab's body, then a rounded
//! [`widgets::InputBox`] and a [`widgets::HintRow`]. The box is the one place an operator types:
//! the prompt on Start, a steer message on Watch, a `y` to confirm something destructive. When
//! nothing is being typed it shows **the CLI command the selected action is**, so every key on this
//! screen teaches the command that does the same thing without it.
//!
//! # What this module is not
//!
//! Like [`crate::tree`], it decides nothing. Every view here is plain, already-rendered data — a
//! state word and a [`crate::tree::Tone`], a readiness and its note, token counts — projected by
//! the supervisor, which is the only crate that can read a `NodeState` or a doctor row. What this
//! module owns is the look: one accent ([`theme`]), status as glyph plus ANSI-16 colour, dim for
//! context, and column arithmetic ([`text`]) that loses the least important words first. It never
//! shows a price: marion reports tokens, and a dollar figure would be a guess about the operator's
//! plan.

pub mod help;
pub mod keys;
pub mod setup;
pub mod start;
pub mod text;
pub mod theme;
pub mod watch;
pub mod widgets;

pub use help::{HelpView, KeyRow};
pub use setup::{AgentTypeRow, FormField, FormView, LoginRow, ProfileRow, SetupView};
pub use start::{HarnessRow, RecentRow, StartView};
pub use theme::{Ready, Theme};
pub use watch::{
    Expanded, FeedRow, MessageView, NodeRow, ResultView, StreamLine, TaskView, TokenView, WatchView,
};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use widgets::{HintRow, InputBox, TopBar};

/// The four tabs, in the order the top bar shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Start,
    Watch,
    Setup,
    Help,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Start, Tab::Watch, Tab::Setup, Tab::Help];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Start => "Start",
            Tab::Watch => "Watch",
            Tab::Setup => "Setup",
            Tab::Help => "?",
        }
    }
}

/// One `key verb` pair on the hint row.
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

/// What the bottom box holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Start's task prompt. `placeholder` shows, dim, while `text` is empty.
    Prompt {
        text: String,
        placeholder: String,
        focused: bool,
    },
    /// The command the selected action is, and a short dim note on the right.
    Command { line: String, note: String },
    /// A message being written for a node: `target` names it (`steer 3c33`).
    Compose { target: String, text: String },
    /// A destructive action waiting for `y`: the question and the command it would run.
    Confirm { question: String, command: String },
}

/// The tab's body.
#[derive(Debug, Clone, Copy)]
pub enum Body<'a> {
    Start(&'a StartView),
    Watch(&'a WatchView),
    Setup(&'a SetupView),
    Help(&'a HelpView),
}

impl Body<'_> {
    pub fn tab(&self) -> Tab {
        match self {
            Body::Start(_) => Tab::Start,
            Body::Watch(_) => Tab::Watch,
            Body::Setup(_) => Tab::Setup,
            Body::Help(_) => Tab::Help,
        }
    }
}

/// The whole screen, one frame of it.
#[derive(Debug, Clone)]
pub struct Screen<'a> {
    pub theme: Theme,
    /// The project, as the operator would recognise it (`~/code/acme-api`).
    pub project: &'a str,
    /// Nodes that need the operator, forest-wide: the top bar's `◐N` beside Watch.
    pub attention: usize,
    pub body: Body<'a>,
    pub input: Input,
    pub hints: Vec<Hint>,
    /// Shown on the hint row in place of the hints: see [`widgets::HintRow::notice`].
    pub notice: Option<String>,
    /// Animation tick: spinners advance one frame per tick.
    pub frame: usize,
}

/// Columns between the screen edge and a body's content: the selection caret lives here.
pub const GUTTER: u16 = 2;

/// The rectangles a [`Screen`] is drawn into, for a caller that needs them (a mouse click, a
/// cursor position) and for the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub top: Rect,
    pub body: Rect,
    pub input: Rect,
    pub hints: Rect,
}

impl Layout {
    /// Top bar and a blank row, the body, a blank row, the box, the hint row. The bottom rows are
    /// claimed first, so a short window costs body rows rather than the box that says what a key
    /// will do. The body spans the full width; screens indent their content by [`GUTTER`] and
    /// put the selection caret in it.
    pub fn of(area: Rect, input: &Input) -> Layout {
        let hints_h = area.height.min(1);
        let box_h = InputBox::height(input, area.width).min(area.height - hints_h);
        let top_h = TopBar::HEIGHT.min(area.height - hints_h - box_h);
        let hints = Rect::new(area.x, area.bottom() - hints_h, area.width, hints_h);
        let input_r = Rect::new(
            area.x + 1.min(area.width),
            hints.y - box_h,
            area.width.saturating_sub(2),
            box_h,
        );
        let body_y = area.y + top_h + 1;
        let body_h = input_r.y.saturating_sub(body_y + 1);
        let body = Rect::new(area.x, body_y, area.width, body_h);
        Layout {
            top: Rect::new(area.x, area.y, area.width, top_h),
            body,
            input: input_r,
            hints,
        }
    }
}

impl Widget for &Screen<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let layout = Layout::of(area, &self.input);
        TopBar {
            project: self.project,
            tab: self.body.tab(),
            attention: self.attention,
            theme: self.theme,
        }
        .render(layout.top, buf);
        match self.body {
            Body::Start(v) => start::render(v, self.theme, self.frame, layout.body, buf),
            Body::Watch(v) => watch::render(v, self.theme, self.frame, layout.body, buf),
            Body::Setup(v) => setup::render(v, self.theme, self.frame, layout.body, buf),
            Body::Help(v) => help::render(v, self.theme, layout.body, buf),
        }
        InputBox {
            input: &self.input,
            theme: self.theme,
        }
        .render(layout.input, buf);
        HintRow {
            hints: &self.hints,
            notice: self.notice.as_deref(),
        }
        .render(layout.hints, buf);
    }
}
