//! §5.6's *"tree pane, content pane"*, and M5 clause 3's greying.
//!
//! # What this module is, and the one thing it deliberately is not
//!
//! §9's M5 asks for *"`marion doctor` reporting their differing capabilities **and the UI greying
//! out what they cannot do**"*. Two halves, and only the second is here. The first is
//! `marion_supervisor::doctor`, and **this module has no capability table of its own** — it does
//! not know what a harness is, it cannot look one up, and it is given no dependency
//! that would let it — see this crate's manifest. A [`Node`] arrives with its [`Action`]s already decided, each carrying the
//! `available` bit somebody else computed, and all this file does is choose two styles.
//!
//! That is the structural answer to the failure the increment brief names: *"a UI that greys from
//! its own copy will drift from doctor's"*. It cannot have a copy. The place the decision is
//! actually made is `marion_supervisor::doctor::capabilities_at`, which doctor's own rows are built
//! from, and `the_trees_greying_is_doctors_own_answer_at_every_key` is what holds the two together.
//!
//! # Ordering, and why a node is never dropped
//!
//! [`Tree::new`] is a depth-first walk in input order, and its post-condition is that **every input
//! node appears exactly once**. That is not tidiness. `tree/subscribe` hands back a flat
//! `Vec<NodeSummary>` whose `parent_id` may name a node the same snapshot does not contain — §4.2's
//! compaction, or a parent the caller is not authorized to see — and a walk that only descended
//! from `parent == None` would silently omit the whole subtree beneath it. An operator would see a
//! tree missing a running agent and have no way to tell that from the agent not existing.
//!
//! So an unreachable node is a **root**: rendered at depth 0 with the connector that says its
//! parent is elsewhere, rather than not rendered. A cycle — which the journal's immutable
//! `parent_id` (§7.5) should make impossible and which this module refuses to trust anyway — is
//! broken by the visited set and its members are appended, again rather than dropped.
//!
//! # Two styles, and no third
//!
//! Available is [`Modifier::BOLD`]; unavailable is [`greyed`]'s one explicit mid grey — greyed and
//! not struck, because a strikethrough says *cannot* and §3.3's answer is *unmeasured*. The
//! label text is **identical** either way, which is deliberate: if the greyed form also changed the
//! characters, a snapshot would still catch a mis-greyed action while the *style* path — the part
//! an operator actually reads at a glance — went untested. The only signal is the style, so a test
//! that asserts styles is asserting the whole mechanism.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

/// One thing an operator could ask a node to do, and whether this node's harness can.
///
/// `name` is a `String` rather than a `&'static str` so that this crate is not the place §3.3's
/// ten field names are written down a second time. They come from
/// `marion_harness::Capabilities::granted`'s vocabulary, through the supervisor, and a copy here
/// would be the second table the module doc says does not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub name: String,
    /// False means **greyed**: §3.3's *degrade visibly*. It says "marion has not measured this on
    /// this node's surfaces", never "the harness cannot" — the distinction `marion doctor`'s notes
    /// carry in prose and ten bools cannot.
    pub available: bool,
}

impl Action {
    pub fn new(name: impl Into<String>, available: bool) -> Self {
        Self {
            name: name.into(),
            available,
        }
    }
}

/// One node, as a tree row.
///
/// Deliberately stringly-typed. `marion-tui` does not depend on `marion_core::proto` and must not:
/// the render path is `ratatui` and a grid, and a widget that knew `NodeState`'s variants would be
/// a second place the wire vocabulary is interpreted. The supervisor projects; this draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    /// §7.5's immutable parent, or `None` for a root. A parent naming a node not in the same
    /// snapshot is treated as `None` — see the module doc.
    pub parent: Option<String>,
    /// What to show. The node's name where it has one; its agent type and a short id where it
    /// does not. Never the whole id: a UUID is wider than the tree column.
    pub label: String,
    /// The node's state, already rendered. One short word.
    pub state: String,
    /// What kind of state that word is, decided by whoever rendered it. See [`Tone`].
    pub tone: Tone,
    pub actions: Vec<Action>,
    /// One caveat about the key the actions were decided at, appended to the strip — *"harness
    /// version unknown"*, for a node whose version marion never read. A `String` for the same
    /// reason [`Action::name`] is one: the words are the supervisor's.
    pub note: Option<String>,
}

/// The kind of state a row is in, for the glyph and colour it is drawn with.
///
/// Five kinds and no more, and **decided by the caller** with the state word: this crate cannot
/// read a state word any more than it can read a capability name (see the module doc), so the
/// supervisor says *which* of these a state is and this only says how each looks. The glyph is
/// the load-bearing half — an operator on a monochrome terminal, or one who cannot tell red from
/// green, scans the column for it — and the colour is the same fact again where there is colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The process is there and doing something, or about to: spawning, ready, running, idle.
    Live,
    /// Waiting on somebody — a permission, an answer, its own descendants.
    Blocked,
    /// Finished, and well.
    Done,
    /// Finished badly: failed, timed out, killed.
    Failed,
    /// marion does not claim to know: orphaned, reaped, cancelled, unreported.
    Unknown,
}

impl Tone {
    /// One column wide, and distinct per tone: see [`Tone`] for why the glyph is not decoration.
    pub fn glyph(self) -> &'static str {
        match self {
            Tone::Live => "●",
            Tone::Blocked => "◐",
            Tone::Done => "✓",
            Tone::Failed => "✗",
            Tone::Unknown => "?",
        }
    }

    /// The colour, where the terminal has one. [`Tone::Done`] is deliberately the default
    /// foreground: a forest of finished nodes should recede so the live and the failed stand out.
    /// [`Tone::Unknown`] is [`greyed`]'s grey, because the two mean the same thing — marion has
    /// not measured this — and one grey is a vocabulary where two would be a puzzle.
    pub fn style(self) -> Style {
        match self {
            Tone::Live => Style::default().fg(Color::Green),
            Tone::Blocked => Style::default().fg(Color::Yellow),
            Tone::Done => Style::default(),
            Tone::Failed => Style::default().fg(Color::Red),
            Tone::Unknown => greyed(),
        }
    }
}

/// The flattened tree, plus the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    nodes: Vec<Node>,
    /// Indices into [`Self::nodes`], in render order.
    order: Vec<usize>,
    /// Indentation depth per entry of [`Self::order`], parallel to it.
    depth: Vec<u16>,
    /// The connector prefix per entry of [`Self::order`], parallel to it — see [`prefixes`].
    prefix: Vec<String>,
    /// An index into [`Self::order`], not into [`Self::nodes`]: the cursor moves by *rows*.
    cursor: usize,
}

impl Tree {
    /// Flatten `nodes` depth-first in input order.
    ///
    /// **Total**: every input node lands in the order exactly once, whatever its `parent` says.
    /// See the module doc for why that is a correctness property and not politeness.
    pub fn new(nodes: Vec<Node>) -> Self {
        let index = |id: &str| nodes.iter().position(|n| n.id == id);
        let mut order = Vec::with_capacity(nodes.len());
        let mut depth = Vec::with_capacity(nodes.len());
        let mut seen = vec![false; nodes.len()];

        // A node is a root iff its parent is absent from *this* snapshot. An orphan is therefore a
        // root, which is what stops a compacted parent from hiding a live child.
        let is_root = |n: &Node| n.parent.as_deref().and_then(&index).is_none();

        // Explicit stack rather than recursion: `max_depth` is 3 by default (§6.1) but nothing in
        // the wire format bounds it, and a client that blew its stack on a malformed snapshot would
        // be a denial of service reachable from the supervisor's own answer.
        for (root, _) in nodes.iter().enumerate().filter(|(_, n)| is_root(n)) {
            let mut stack = vec![(root, 0u16)];
            while let Some((i, d)) = stack.pop() {
                if std::mem::replace(&mut seen[i], true) {
                    continue;
                }
                order.push(i);
                depth.push(d);
                // Reversed, so children are visited in input order once popped.
                let children = nodes
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.parent.as_deref() == Some(nodes[i].id.as_str()))
                    .map(|(j, _)| (j, d.saturating_add(1)))
                    .collect::<Vec<_>>();
                stack.extend(children.into_iter().rev());
            }
        }
        // Anything a cycle kept out of the walk. Not reachable from a well-formed snapshot; listed
        // rather than dropped, because "marion cannot place this node" and "this node does not
        // exist" must not look the same on screen.
        for (i, walked) in seen.iter().enumerate() {
            if !walked {
                order.push(i);
                depth.push(0);
            }
        }
        debug_assert_eq!(order.len(), nodes.len(), "a node was dropped from the tree");

        let prefix = prefixes(&depth);
        Self {
            nodes,
            order,
            depth,
            prefix,
            cursor: 0,
        }
    }

    pub fn rows(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The node under the cursor, or `None` on an empty tree.
    pub fn selected(&self) -> Option<&Node> {
        self.order.get(self.cursor).map(|&i| &self.nodes[i])
    }

    /// Move the cursor. Clamped at both ends rather than wrapping: a list that jumps from the last
    /// node to the first on one extra keypress is how an operator acts on the wrong agent.
    pub fn move_by(&mut self, delta: isize) {
        if self.order.is_empty() {
            return;
        }
        let last = self.order.len() - 1;
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    /// Re-point the cursor at `id`, if the tree has it. Used after a refresh, so that a snapshot
    /// arriving while the operator is deciding does not move the selection under them.
    pub fn select(&mut self, id: &str) -> bool {
        match self.order.iter().position(|&i| self.nodes[i].id == id) {
            Some(row) => {
                self.cursor = row;
                true
            }
            None => false,
        }
    }

    /// The rendered rows, cursor included. Public because the supervisor's own tests read it, and
    /// because a caller sizing a scroll region needs the text width.
    pub fn lines(&self) -> Vec<String> {
        self.order
            .iter()
            .zip(&self.prefix)
            .map(|(&i, prefix)| {
                let n = &self.nodes[i];
                // State first. It is the fact an operator scans the column for, and a label that
                // runs past the column edge must clip itself rather than the state.
                format!("{prefix}{} {} {}", n.tone.glyph(), n.state, n.label)
            })
            .collect()
    }

    /// One row as its three spans — connectors, `glyph state`, ` label` — for a renderer that
    /// styles them differently. [`Self::lines`] is their concatenation, by construction.
    fn spans(&self, row: usize) -> Option<(&str, String, String)> {
        let n = &self.nodes[*self.order.get(row)?];
        Some((
            self.prefix[row].as_str(),
            format!("{} {}", n.tone.glyph(), n.state),
            format!(" {}", n.label),
        ))
    }
}

/// The connector prefix of every row, from the depths of a pre-order walk.
///
/// A child with a later sibling is `├`, the last child is `└`, and each shallower level shows `│`
/// while its own branch continues and blank once it has ended. Every child used to carry `└`, so
/// a row under `└ b` followed by another `└ c` could not be told from a row under `c` without
/// counting spaces — which is the one thing a tree drawing exists to spare an operator.
///
/// One pass from the bottom: a row at depth `d` has a later sibling iff a row at depth `d` was
/// seen below it before any row shallower than `d`, which is exactly what `open[d]` holds, and
/// `open[l]` for `l < d` answers the same question for the row's ancestor at level `l`.
fn prefixes(depth: &[u16]) -> Vec<String> {
    let deepest = depth.iter().copied().max().unwrap_or(0) as usize;
    let mut open = vec![false; deepest + 1];
    let mut out = vec![String::new(); depth.len()];
    for (row, &d) in depth.iter().enumerate().rev() {
        let d = d as usize;
        let mut prefix = String::with_capacity(d * 2);
        for (level, &continues) in open.iter().enumerate().take(d + 1).skip(1) {
            prefix.push_str(match (level == d, continues) {
                (true, true) => "├ ",
                (true, false) => "└ ",
                (false, true) => "│ ",
                (false, false) => "  ",
            });
        }
        out[row] = prefix;
        open[d] = true;
        for o in &mut open[d + 1..] {
            *o = false;
        }
    }
    out
}

/// Where the keyboard goes.
///
/// Two values, which is the whole focus model. §5.6 describes a tree pane and a content pane; the
/// minimum that makes them two panes rather than one is knowing which one a key is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Content,
}

/// What a keypress on the tree screen means.
///
/// A separate vocabulary from [`crate::Action`] — the pane's — and not an extension of it, because
/// the two are read in different modes. [`crate::Keys`] is a *filter*: marion's terminal is already
/// in raw mode and a pane forwards bytes verbatim. The tree screen forwards nothing, so every byte
/// it sees is a command, and folding the two would mean the pane's filter had to know about a
/// cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    /// Enter: attach to the selected node. The tree does not implement attaching — `marion attach`
    /// already exists and works — so this is the caller's cue to run it.
    Open,
    /// Tab: move focus between the tree and the content pane.
    ToggleFocus,
    /// `q`, or `^]` — the same escape the pane reserves, so an operator who has learned one way out
    /// of marion has learned both.
    Quit,
}

/// Decode a chunk of stdin into navigation. Bytes with no meaning here are dropped, not forwarded:
/// this screen has nowhere to forward them to.
pub fn nav(bytes: &[u8]) -> Vec<Nav> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // `CSI A`/`CSI B` before the single-byte arms, so an arrow key is not read as `[` + `A`.
        if bytes[i..].starts_with(b"\x1b[A") {
            out.push(Nav::Up);
            i += 3;
            continue;
        }
        if bytes[i..].starts_with(b"\x1b[B") {
            out.push(Nav::Down);
            i += 3;
            continue;
        }
        match bytes[i] {
            b'k' => out.push(Nav::Up),
            b'j' => out.push(Nav::Down),
            b'\r' | b'\n' => out.push(Nav::Open),
            b'\t' => out.push(Nav::ToggleFocus),
            b'q' | 0x1d => out.push(Nav::Quit),
            _ => {}
        }
        i += 1;
    }
    out
}

/// How wide the tree column is, in columns.
///
/// Fixed rather than proportional. A tree pane that grew with the window would resize the *content*
/// pane on every drag, and the content pane is a node's pty — §5.3 gives it one `TIOCSWINSZ` per
/// change and a harness repaints its whole screen for each.
/// 44, which is what `│ └ ◐ blocked:permission claude-impl 5b04` needs at depth 2 — marion's
/// longest state word, a typical agent type and the short id, at the deepest row §6.1's default
/// `max_depth` of 3 can produce. An `AgentId` is a UUID and does not fit at any width worth
/// spending on a sidebar; the tree is navigated with the cursor, not by reading ids back, and
/// `Enter` never asks the operator to type one. The detail pane shows the whole id.
pub const TREE_COLUMN: u16 = 44;

/// The tree screen's five regions.
///
/// This is [`crate::view::pty_size`]'s counterpart and the place its comment points at: an attach
/// subtracts no chrome because the node has the terminal, and a tree screen subtracts exactly this
/// much because it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    /// One row along the top: which project, how many nodes.
    pub status: Rect,
    pub tree: Rect,
    pub content: Rect,
    /// One full-width row at the bottom left: the keys this screen answers to. **Not the content
    /// pane's last row** — hints belong to the screen, and drawn inside the pane they started at
    /// `TREE_COLUMN + 2` and read as one more fact about the selected node.
    pub hints: Rect,
    /// The last row: the selected node's capabilities, greyed where absent.
    pub actions: Rect,
}

/// Split `area` into [`Panes`].
///
/// Degenerate areas are handled by arithmetic rather than by a guard: a terminal one row tall gives
/// the status row that row and everything else zero height, two rows gives the strip the second,
/// and `ratatui` draws nothing into a zero-height `Rect`. Inventing a minimum size here would make
/// marion refuse to start in a window the operator can see.
///
/// The bottom rows are claimed **before** the body, so shrinking a window costs the operator tree
/// rows rather than the two rows that say what the screen is and what it does.
pub fn split(area: Rect) -> Panes {
    let status = area.height.min(1);
    let strip = (area.height - status).min(1);
    let hints = (area.height - status - strip).min(1);
    let body = area.height - status - strip - hints;
    let body_y = area.y.saturating_add(status);
    let hints_y = body_y.saturating_add(body);
    let tree_w = TREE_COLUMN.min(area.width);
    Panes {
        status: Rect::new(area.x, area.y, area.width, status),
        tree: Rect::new(area.x, body_y, tree_w, body),
        content: Rect::new(
            area.x.saturating_add(tree_w),
            body_y,
            area.width - tree_w,
            body,
        ),
        hints: Rect::new(area.x, hints_y, area.width, hints),
        actions: Rect::new(area.x, hints_y.saturating_add(hints), area.width, strip),
    }
}

/// The key hints, pinned to the bottom left of the screen.
///
/// The words are the caller's, for the same reason [`Action::name`]'s are: this crate cannot say
/// what `Enter` will do to a node, because it cannot tell one node from another. All it owns is the
/// row and the margin.
pub struct Hints<'a> {
    pub keys: &'a str,
}

impl Widget for Hints<'_> {
    /// `keys` is `key verb` pairs separated by two spaces, with an optional ` · note` after them.
    /// The key of each pair is bold and the rest dim, so an operator can find the four keys in
    /// a row of words without reading it; the note is dim throughout, since none of it is a key.
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let dim = Style::default().add_modifier(Modifier::DIM);
        let key = Style::default().add_modifier(Modifier::BOLD);
        let end = area.x.saturating_add(area.width);
        let mut x = area.x;
        let mut put = |x: &mut u16, text: &str, style: Style| {
            let (next, _) =
                buf.set_stringn(*x, area.y, text, end.saturating_sub(*x) as usize, style);
            *x = next;
        };
        let (pairs, note) = match self.keys.split_once("  ·  ") {
            Some((pairs, note)) => (pairs, Some(note)),
            None => (self.keys, None),
        };
        for (i, pair) in pairs.split("  ").enumerate() {
            if i > 0 {
                put(&mut x, "  ", dim);
            }
            match pair.split_once(' ') {
                Some((k, verb)) => {
                    put(&mut x, k, key);
                    put(&mut x, " ", dim);
                    put(&mut x, verb, dim);
                }
                None => put(&mut x, pair, key),
            }
        }
        if let Some(note) = note {
            put(&mut x, "  ·  ", dim);
            put(&mut x, note, dim);
        }
    }
}

/// A path from the end that tells one checkout from another: its last two components, with `…/`
/// where something was dropped.
///
/// The head of a path is the part every checkout on a machine shares —
/// `/private/tmp/claude-501/…` — so a row that clips from the right showed an operator the shared
/// prefix and hid the two components that differ. A path already short enough to be its own answer
/// is returned whole, `…/` and all, rather than being decorated with an ellipsis for nothing.
pub fn short_path(path: &str) -> String {
    let parts = path
        .split('/')
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [.., a, b] if parts.len() > 2 => format!("…/{a}/{b}"),
        _ => path.trim_end_matches('/').to_string(),
    }
}

/// The status row: which project this tree is, and how big it is.
///
/// The counts are the caller's — `marion-tui` cannot say what "running" means any more than it can
/// say what a harness is — and the row only lays them out.
///
/// **Order is the whole design.** The counts come before the path because `set_stringn` clips from
/// the right, and the one thing this row must never lose is how many nodes there are: a path is
/// unbounded and a count is two words.
pub struct Status<'a> {
    /// The project key the supervisor was dialled on, as the operator would recognise it.
    pub repo: &'a str,
    pub nodes: usize,
    pub running: usize,
}

impl Widget for Status<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let (x, _) = buf.set_stringn(area.x, area.y, " marion", area.width as usize, bold);
        let rest = format!(
            " · {} nodes, {} running · {}",
            self.nodes,
            self.running,
            short_path(self.repo)
        );
        let remaining = area.x.saturating_add(area.width).saturating_sub(x) as usize;
        buf.set_stringn(x, area.y, &rest, remaining, Style::default());
    }
}

/// The tree column.
pub struct TreeView<'a> {
    pub tree: &'a Tree,
    /// Drawn only when the tree has the keyboard, so an operator can tell which pane `j` goes to.
    pub focused: bool,
}

impl Widget for TreeView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        for row in 0..self.tree.rows() {
            let Ok(y) = u16::try_from(row) else { break };
            if y >= area.height {
                break;
            }
            let Some((prefix, state, label)) = self.tree.spans(row) else {
                break;
            };
            let on_cursor = row == self.tree.cursor;
            let style = match (on_cursor, self.focused) {
                (true, true) => Style::default().add_modifier(Modifier::REVERSED),
                (true, false) => Style::default().add_modifier(Modifier::BOLD),
                (false, _) => Style::default(),
            };
            // The cursor row is painted across the **whole** column, not just its text, so a
            // reversed selection reads as a bar rather than as a ragged highlight.
            if on_cursor {
                for x in 0..area.width {
                    buf[(area.x + x, area.y + y)].set_style(style);
                }
            }
            let tone = self.tree.nodes[self.tree.order[row]].tone.style();
            let mut x = area.x;
            let end = area.x.saturating_add(area.width);
            // Connectors dim, the state in its tone, the label plain — and the cursor's
            // modifier over all three, so the bar stays one bar.
            for (text, s) in [
                (prefix, Style::default().add_modifier(Modifier::DIM)),
                (state.as_str(), tone),
                (label.as_str(), Style::default()),
            ] {
                let (next, _) = buf.set_stringn(
                    x,
                    area.y + y,
                    text,
                    end.saturating_sub(x) as usize,
                    s.patch(style),
                );
                x = next;
            }
        }
    }
}

/// The style an unavailable action is drawn in. **This is the greying**, and it is one value so
/// that a test can name it rather than re-describe it.
///
/// One explicit mid grey, and **no `DIM`**. The old value was `DarkGray` *and* `DIM`, which is two
/// reductions compounded: xterm's colour 8 is already `#808080`, `DIM` halves whatever it lands on
/// again, and the theme is free to have made colour 8 darker still. The absent half of the strip
/// came out at a contrast a first-time viewer read as a rendering fault rather than as information,
/// and §3.3's *unmeasured* is a fact the operator has to be able to read.
///
/// `#8a8a8a` is the darkest grey that clears WCAG AA (4.5:1) against `#1e1e1e` — the near-black most
/// dark terminal themes ship — and it clears 6:1 against pure black.
/// `an_absent_capability_is_legible_grey_rather_than_dimmed_dark_grey` measures both rather than
/// taking this sentence's word for it.
pub fn greyed() -> Style {
    Style::default().fg(Color::Rgb(0x8a, 0x8a, 0x8a))
}

/// The style an available action is drawn in: bold, so the offered set is the figure and the
/// greyed set the ground rather than the other way round.
pub fn offered() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// The capability strip for one node.
pub struct ActionBar<'a> {
    /// `None` on an empty tree, which is a real state — a supervisor answering `tree/subscribe`
    /// with no nodes is one that has none, not one that failed.
    pub node: Option<&'a Node>,
}

impl ActionBar<'_> {
    /// What the strip is a strip of. Unlabelled, ten short words along the bottom read as a menu.
    const LABEL: &'static str = "caps:";

    /// The label for one action, with a space either side so adjacent styles do not touch.
    fn cell(a: &Action) -> String {
        format!(" {} ", a.name)
    }
}

impl Widget for ActionBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(node) = self.node else {
            buf.set_stringn(
                area.x,
                area.y,
                "no nodes",
                area.width as usize,
                Style::default(),
            );
            return;
        };
        let cells = std::iter::once((Self::LABEL.to_string(), Style::default()))
            .chain(node.actions.iter().map(|a| {
                let style = if a.available { offered() } else { greyed() };
                (Self::cell(a), style)
            }))
            .chain(
                node.note
                    .iter()
                    .map(|n| (format!(" · {n}"), Style::default())),
            );
        let mut x = area.x;
        for (text, style) in cells {
            let remaining = area.x.saturating_add(area.width).saturating_sub(x) as usize;
            if remaining == 0 {
                break;
            }
            let (next, _) = buf.set_stringn(x, area.y, &text, remaining, style);
            x = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: Option<&str>) -> Node {
        Node {
            id: id.into(),
            parent: parent.map(str::to_string),
            label: id.into(),
            state: "Idle".into(),
            tone: Tone::Live,
            actions: Vec::new(),
            note: None,
        }
    }

    fn ids(t: &Tree) -> Vec<&str> {
        t.order
            .iter()
            .map(|&i| t.nodes[i].id.as_str())
            .collect::<Vec<_>>()
    }

    /// **The forbidden mutation: a live node the journal records, missing from the tree.**
    ///
    /// Three shapes that all produce it, and the third is the one a "descend from the roots"
    /// implementation actually ships with. Mutation: change `is_root` to `n.parent.is_none()`.
    /// The orphan case fails — `b` and its child `c` vanish entirely, which on screen is two
    /// running agents an operator cannot see.
    #[test]
    fn every_node_in_the_snapshot_is_a_row_including_the_ones_with_no_reachable_parent() {
        // (1) An ordinary tree.
        let t = Tree::new(vec![
            node("root", None),
            node("kid", Some("root")),
            node("grandkid", Some("kid")),
        ]);
        assert_eq!(ids(&t), ["root", "kid", "grandkid"]);
        assert_eq!(t.depth, [0, 1, 2]);

        // (2) An orphan: `b`'s parent was compacted out of this snapshot (§4.2).
        let t = Tree::new(vec![
            node("a", None),
            node("b", Some("gone")),
            node("c", Some("b")),
        ]);
        assert_eq!(
            ids(&t),
            ["a", "b", "c"],
            "a node whose parent is not in the snapshot must still be listed"
        );
        assert_eq!(
            t.depth,
            [0, 0, 1],
            "an orphan is a root, and its child is not"
        );

        // (3) A cycle, which §7.5's immutable parent should forbid and this refuses to trust.
        let t = Tree::new(vec![node("x", Some("y")), node("y", Some("x"))]);
        assert_eq!(t.rows(), 2, "a cycle drops nobody");
    }

    /// Ordering is input order at every level, so a tree redrawn from an unchanged snapshot does
    /// not shuffle under the cursor.
    #[test]
    fn siblings_render_in_the_order_the_supervisor_listed_them() {
        let t = Tree::new(vec![
            node("r", None),
            node("second", Some("r")),
            node("first", Some("r")),
        ]);
        assert_eq!(ids(&t), ["r", "second", "first"]);
    }

    /// The state leads the row. A UUID label is wider than the column, so a state written after
    /// it was clipped off on every real node; written first it survives any label length.
    #[test]
    fn a_row_leads_with_its_state_so_the_column_clip_cannot_hide_it() {
        let t = Tree::new(vec![node("a", None), node("kid", Some("a"))]);
        assert_eq!(t.lines(), ["● Idle a", "└ ● Idle kid"]);
    }

    /// **A grandchild must be attributable to its parent by the connectors alone.** Every child
    /// used to carry `└`, so a row under `└ b` followed by `└ c` could not be told from a row
    /// under `c` — the operator had to count spaces. A child that has a later sibling is `├`,
    /// the last one is `└`, and a `│` runs down every level whose branch continues.
    #[test]
    fn connectors_say_which_parent_a_row_hangs_from() {
        let t = Tree::new(vec![
            node("r", None),
            node("a", Some("r")),
            node("a1", Some("a")),
            node("b", Some("r")),
            node("b1", Some("b")),
            node("b2", Some("b")),
            node("s", None),
        ]);
        assert_eq!(
            t.lines(),
            [
                "● Idle r",
                "├ ● Idle a",
                "│ └ ● Idle a1",
                "└ ● Idle b",
                "  ├ ● Idle b1",
                "  └ ● Idle b2",
                "● Idle s",
            ]
        );
    }

    #[test]
    fn the_cursor_clamps_rather_than_wrapping() {
        let mut t = Tree::new(vec![node("a", None), node("b", None)]);
        assert_eq!(t.selected().unwrap().id, "a");
        t.move_by(-1);
        assert_eq!(t.selected().unwrap().id, "a", "up from the top stays");
        t.move_by(1);
        t.move_by(1);
        assert_eq!(
            t.selected().unwrap().id,
            "b",
            "down past the end stays, it does not wrap to `a`"
        );
        assert!(t.select("a"));
        assert_eq!(t.selected().unwrap().id, "a");
        assert!(!t.select("nobody"));
        assert_eq!(
            t.selected().unwrap().id,
            "a",
            "a failed select does not move"
        );
    }

    #[test]
    fn an_empty_tree_has_no_selection_and_does_not_panic() {
        let mut t = Tree::new(Vec::new());
        assert!(t.is_empty());
        assert_eq!(t.selected(), None);
        t.move_by(3);
        assert_eq!(t.selected(), None);
    }

    /// An arrow key is three bytes and must not be read as `[` then `A`.
    #[test]
    fn arrows_and_letters_both_navigate_and_neither_shadows_the_other() {
        assert_eq!(nav(b"\x1b[A\x1b[B"), [Nav::Up, Nav::Down]);
        assert_eq!(nav(b"jk"), [Nav::Down, Nav::Up]);
        assert_eq!(nav(b"\r"), [Nav::Open]);
        assert_eq!(nav(b"\t"), [Nav::ToggleFocus]);
        assert_eq!(nav(b"q"), [Nav::Quit]);
        assert_eq!(
            nav(b"\x1d"),
            [Nav::Quit],
            "the pane's escape works here too"
        );
        assert_eq!(
            nav(b"xyz"),
            [],
            "an unmapped key is dropped, never forwarded"
        );
    }

    #[test]
    fn the_split_leaves_the_content_pane_the_rest_and_nothing_overlaps() {
        let p = split(Rect::new(0, 0, 100, 40));
        assert_eq!(p.status, Rect::new(0, 0, 100, 1));
        assert_eq!(p.tree, Rect::new(0, 1, TREE_COLUMN, 37));
        assert_eq!(p.content, Rect::new(TREE_COLUMN, 1, 100 - TREE_COLUMN, 37));
        assert_eq!(p.actions, Rect::new(0, 39, 100, 1));

        // A window narrower than the tree column gives the tree what there is and the content none,
        // rather than underflowing.
        let p = split(Rect::new(0, 0, 10, 3));
        assert_eq!(p.tree.width, 10);
        assert_eq!(p.content.width, 0);
        // A one-row terminal is all status row: what marion is looking at, before what it found.
        let p = split(Rect::new(0, 0, 80, 1));
        assert_eq!(p.status, Rect::new(0, 0, 80, 1));
        assert_eq!(p.tree.height, 0);
        assert_eq!(p.actions.height, 0);
        // Two rows: status and strip, no body.
        let p = split(Rect::new(0, 0, 80, 2));
        assert_eq!(p.status, Rect::new(0, 0, 80, 1));
        assert_eq!(p.tree.height, 0);
        assert_eq!(p.actions, Rect::new(0, 1, 80, 1));
    }

    /// **The hints are the screen's, not the content pane's.**
    ///
    /// They used to be drawn as the content pane's own last row, which starts at `TREE_COLUMN + 2`:
    /// on a wide terminal that put the one row telling an operator which keys exist forty-six
    /// columns in from the left, floating under a pane whose text it had nothing to do with, and it
    /// read as part of the node's detail. They get their own full-width row pinned to the bottom
    /// left, immediately above the caps strip, which stays the last line.
    #[test]
    fn the_hints_own_a_full_width_row_at_the_bottom_left_above_the_caps_strip() {
        let p = split(Rect::new(0, 0, 100, 40));
        assert_eq!(
            p.hints,
            Rect::new(0, 38, 100, 1),
            "full width, at the left margin"
        );
        assert_eq!(
            p.actions,
            Rect::new(0, 39, 100, 1),
            "the caps strip is still the last line"
        );
        assert_eq!(
            p.hints.y,
            p.content.y + p.content.height,
            "no gap, and no overlap"
        );
        assert_eq!(p.tree.height, 37);

        // Degenerate heights give the rows away from the bottom up and never underflow.
        assert_eq!(split(Rect::new(0, 0, 80, 1)).hints.height, 0);
        assert_eq!(split(Rect::new(0, 0, 80, 2)).hints.height, 0);
        let p = split(Rect::new(0, 0, 80, 3));
        assert_eq!(p.hints, Rect::new(0, 1, 80, 1));
        assert_eq!(p.actions, Rect::new(0, 2, 80, 1));
        assert_eq!(p.content.height, 0);

        // And the widget writes from the pane's own left edge, not indented into the content pane.
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        Hints {
            keys: "enter attach  q quit",
        }
        .render(area, &mut buf);
        let row: String = (0..30).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.starts_with("enter attach"), "{row:?}");
    }

    /// **A first-time operator has to be able to find the keys in the hint row.** Drawn as one
    /// dim string, `enter attach  j/k move  tab focus  q quit` was eight words with nothing to
    /// say which four were keys; the key of each pair is now bold and its verb dim, and a note
    /// after ` · ` — why the last key did nothing — is dim throughout, since none of it is a key.
    #[test]
    fn a_hint_is_a_bold_key_and_a_dim_verb() {
        let area = Rect::new(0, 0, 50, 1);
        let mut buf = Buffer::empty(area);
        Hints {
            keys: "enter attach  q quit  ·  no pane to focus",
        }
        .render(area, &mut buf);
        let row: String = (0..50).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(row.trim_end(), "enter attach  q quit  ·  no pane to focus");
        let bold = |x: u16| buf[(x, 0)].style().add_modifier.contains(Modifier::BOLD);
        let dim = |x: u16| buf[(x, 0)].style().add_modifier.contains(Modifier::DIM);
        assert!(bold(0) && !dim(0), "`enter` is a key");
        assert!(dim(6) && !bold(6), "`attach` is what it does");
        assert!(bold(14) && !dim(14), "`q` is a key");
        assert!(dim(22) && !bold(22), "the separator is neither");
        assert!(
            dim(25) && !bold(25),
            "`no pane to focus` is a note, not a key"
        );
    }

    /// The status row says which project this is and how big the forest is — the two facts that
    /// tell an operator with three terminals open which one they are looking at.
    #[test]
    fn the_status_row_names_the_project_and_counts_the_forest() {
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        Status {
            repo: "/work/marion",
            nodes: 4,
            running: 3,
        }
        .render(area, &mut buf);
        let row: String = (0..60).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(
            row.trim_end(),
            " marion · 4 nodes, 3 running · /work/marion"
        );
        assert!(
            buf[(1, 0)].style().add_modifier.contains(Modifier::BOLD),
            "the program name is the anchor of the row"
        );
    }

    /// **The count outlives the path.** A deep scratch path is wider than any terminal, and the row
    /// that put it before the counts lost *"4 nodes, 3 running"* off the right edge — the two facts
    /// the row exists to carry. The path is shown from its informative end instead, and the count
    /// is laid out first so a clip can only ever eat the path.
    #[test]
    fn the_counts_survive_a_path_too_long_for_the_row() {
        assert_eq!(short_path("/work/marion"), "/work/marion");
        assert_eq!(short_path("marion"), "marion");
        assert_eq!(
            short_path("/private/tmp/claude-501/d0851ffa/scratchpad/walkthrough/work/repo"),
            "…/work/repo",
            "a long path is shown from the end that tells one checkout from another"
        );
        assert_eq!(
            short_path("/a/b/c/"),
            "…/b/c",
            "a trailing slash is not a component"
        );

        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        Status {
            repo: "/private/tmp/claude-501/d0851ffa/scratchpad/walkthrough/work/repo",
            nodes: 4,
            running: 3,
        }
        .render(area, &mut buf);
        let row: String = (0..40).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            row.contains("4 nodes, 3 running"),
            "the count was clipped off a narrow row: {row:?}"
        );
        assert!(
            !row.contains("claude-501"),
            "the head of the path is not the informative end: {row:?}"
        );
    }

    /// **The greying, as styles.** The label text is identical in both cases by design, so this is
    /// the only assertion in the tree that can tell an offered action from a greyed one. Absent is
    /// dim, not struck: a strikethrough over ten short words at low contrast read as a rendering
    /// fault, and it said "cannot" where §3.3's answer is "unmeasured". Present is bold, so the
    /// strip has a figure as well as a ground. The strip is labelled, so it is not a bare word list.
    #[test]
    fn a_greyed_action_differs_from_an_offered_one_only_in_style() {
        let n = Node {
            actions: vec![Action::new("interrupt", true), Action::new("fork", false)],
            ..node("a", None)
        };
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        ActionBar { node: Some(&n) }.render(area, &mut buf);

        let row: String = (0..40).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.starts_with("caps: interrupt  fork "), "{row:?}");

        // "caps:" occupies 0..5, " interrupt " 5..16, " fork " 16..22. The comparison is against
        // the *marks* rather than against a whole `Style`, because `Buffer::empty` seeds every cell
        // with an explicit `Color::Reset` that `Style::default()` does not carry — asserting
        // equality with `offered()` would then fail for a reason that has nothing to do with
        // greying.
        let grey = |x: u16| {
            let s = buf[(x, 0)].style();
            s.fg == greyed().fg && !s.add_modifier.contains(Modifier::CROSSED_OUT)
        };
        let bold = |x: u16| buf[(x, 0)].style().add_modifier.contains(Modifier::BOLD);
        assert!(
            !grey(6) && bold(6),
            "`interrupt` is available and was greyed out"
        );
        assert!(
            grey(17) && !bold(17),
            "`fork` is unavailable and was offered"
        );
        assert!(
            !grey(0) && !bold(0),
            "the label is neither offered nor greyed"
        );
        assert_ne!(offered(), greyed(), "the two styles must be tellable apart");
    }

    /// **A greyed capability must still be legible.** `DarkGray` plus `DIM` is two reductions
    /// compounded: xterm's colour 8 is `#808080`, and `DIM` is a *further* halving the terminal
    /// applies on top — a first-time viewer read the absent half of the strip as a rendering fault
    /// rather than as information. §3.3's answer is *unmeasured*, which an operator has to be able
    /// to read, so the greying is one explicit mid grey that clears WCAG AA against a dark ground
    /// and nothing else.
    #[test]
    fn an_absent_capability_is_legible_grey_rather_than_dimmed_dark_grey() {
        // WCAG 2.x relative luminance and contrast, so "4.5:1" is measured rather than asserted.
        fn luminance(rgb: (u8, u8, u8)) -> f64 {
            let chan = |c: u8| {
                let c = f64::from(c) / 255.0;
                if c <= 0.04045 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * chan(rgb.0) + 0.7152 * chan(rgb.1) + 0.0722 * chan(rgb.2)
        }
        let contrast = |fg: (u8, u8, u8), bg: (u8, u8, u8)| {
            let (a, b) = (luminance(fg), luminance(bg));
            (a.max(b) + 0.05) / (a.min(b) + 0.05)
        };

        let Some(Color::Rgb(r, g, b)) = greyed().fg else {
            panic!(
                "the greying must name its own colour rather than borrow a palette slot: {:?}",
                greyed().fg
            );
        };
        // Both grounds a "dark theme" terminal actually uses: pure black, and the near-black most
        // themes ship. The strip has to be readable on either.
        for ground in [(0u8, 0u8, 0u8), (0x1e, 0x1e, 0x1e)] {
            let ratio = contrast((r, g, b), ground);
            assert!(
                ratio >= 4.5,
                "absent caps are {ratio:.2}:1 on {ground:?}, below WCAG AA's 4.5:1"
            );
        }
        assert!(
            !greyed().add_modifier.contains(Modifier::DIM),
            "DIM halves the colour again in the terminal, undoing the contrast measured above"
        );
        assert!(
            offered().add_modifier.contains(Modifier::BOLD),
            "the present caps are the figure"
        );
    }

    /// A caveat about the key the actions were decided at — the version marion never read — is
    /// said on the strip, after the actions, rather than left to be inferred from all of them
    /// being greyed.
    #[test]
    fn the_strips_note_follows_the_actions() {
        let n = Node {
            actions: vec![Action::new("steer", false)],
            note: Some("harness version unknown".into()),
            ..node("a", None)
        };
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        ActionBar { node: Some(&n) }.render(area, &mut buf);
        let row: String = (0..60).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(row.trim_end(), "caps: steer  · harness version unknown");
    }

    /// **A failed node must not look like a finished one.** Every state was one bracketed word in
    /// the terminal's default colour, so `exited:failed`, `exited:timedout` and `orphaned` read
    /// exactly like `exited:ok` until each word was read. The state now carries a tone the
    /// supervisor decided — a glyph an operator can scan the column for in monochrome, and a
    /// colour where there is one — and the label stays plain so the tone is the state's alone.
    #[test]
    fn a_state_is_drawn_with_its_tones_glyph_and_colour() {
        let tones = [
            Tone::Live,
            Tone::Blocked,
            Tone::Done,
            Tone::Failed,
            Tone::Unknown,
        ];
        let glyphs = tones.map(Tone::glyph);
        for (i, g) in glyphs.iter().enumerate() {
            assert_eq!(g.chars().count(), 1, "one column: {g:?}");
            assert!(
                !glyphs[..i].contains(g),
                "two tones share the glyph {g:?}; in monochrome they are the same state"
            );
        }
        assert_eq!(Tone::Failed.style().fg, Some(Color::Red));
        assert_eq!(Tone::Live.style().fg, Some(Color::Green));
        assert_eq!(Tone::Blocked.style().fg, Some(Color::Yellow));
        assert_eq!(
            Tone::Unknown.style().fg,
            greyed().fg,
            "unknown is the same grey as an unmeasured capability: not a fault, not a fact"
        );
        assert_eq!(Tone::Done.style().fg, None, "finished-well recedes");

        let mut failed = node("f", None);
        failed.state = "exited:failed".into();
        failed.tone = Tone::Failed;
        let t = Tree::new(vec![failed, node("live", None)]);
        assert_eq!(t.lines(), ["✗ exited:failed f", "● Idle live"]);

        let area = Rect::new(0, 0, 30, 2);
        let mut buf = Buffer::empty(area);
        TreeView {
            tree: &t,
            focused: false,
        }
        .render(area, &mut buf);
        let fg = |x: u16, y: u16| buf[(x, y)].style().fg;
        assert_eq!(fg(0, 0), Some(Color::Red), "the glyph carries the tone");
        assert_eq!(fg(2, 0), Some(Color::Red), "so does the state word");
        assert_eq!(
            fg(16, 0),
            Some(Color::Reset),
            "the label is plain: the colour is the state's, not the row's"
        );
        assert_eq!(fg(0, 1), Some(Color::Green));
    }

    /// The cursor bar spans the column, and only when the tree has the keyboard is it reversed —
    /// which is the whole of the focus model made visible.
    #[test]
    fn focus_changes_how_the_selection_is_drawn() {
        let t = Tree::new(vec![node("a", None), node("b", None)]);
        let area = Rect::new(0, 0, 20, 2);
        let draw = |focused: bool| {
            let mut buf = Buffer::empty(area);
            TreeView { tree: &t, focused }.render(area, &mut buf);
            buf
        };
        let focused = draw(true);
        let blurred = draw(false);
        assert!(
            focused[(19, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "the bar must reach the end of the column"
        );
        assert!(
            !blurred[(19, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            blurred[(0, 0)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "an unfocused tree still shows where the cursor is"
        );
        // The unselected row is untouched either way.
        assert_eq!(focused[(19, 1)].style().add_modifier, Modifier::empty());
    }
}
