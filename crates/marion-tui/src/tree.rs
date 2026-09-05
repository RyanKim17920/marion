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
//! Available is the terminal's own foreground; unavailable is [`Color::DarkGray`] plus
//! [`Modifier::CROSSED_OUT`]. The label text is **identical** either way, which is deliberate: if
//! the greyed form also changed the characters, a snapshot would still catch a mis-greyed action
//! while the *style* path — the part an operator actually reads at a glance — went untested. The
//! only signal is the style, so a test that asserts styles is asserting the whole mechanism.

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
/// Deliberately stringly-typed. `marion-tui` does not depend on `marion-proto` and must not: the
/// render path is `ratatui` and a grid, and a widget that knew `NodeState`'s variants would be a
/// second place the wire vocabulary is interpreted. The supervisor projects; this draws.
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
    pub actions: Vec<Action>,
}

/// The flattened tree, plus the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    nodes: Vec<Node>,
    /// Indices into [`Self::nodes`], in render order.
    order: Vec<usize>,
    /// Indentation depth per entry of [`Self::order`], parallel to it.
    depth: Vec<u16>,
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

        Self {
            nodes,
            order,
            depth,
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
            .zip(&self.depth)
            .map(|(&i, &d)| {
                let n = &self.nodes[i];
                let indent = "  ".repeat(d as usize);
                let edge = if d == 0 { "" } else { "└ " };
                // State first. It is the fact an operator scans the column for, and a label that
                // runs past the column edge must clip itself rather than the state.
                format!("{indent}{edge}[{}] {}", n.state, n.label)
            })
            .collect()
    }
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
/// 44, which is what `    └ [blocked:permission] claude-impl 5b04` needs at depth 2 — marion's
/// longest state word, a typical agent type and the short id, at the deepest row §6.1's default
/// `max_depth` of 3 can produce. An `AgentId` is a UUID and does not fit at any width worth
/// spending on a sidebar; the tree is navigated with the cursor, not by reading ids back, and
/// `Enter` never asks the operator to type one. The detail pane shows the whole id.
pub const TREE_COLUMN: u16 = 44;

/// The tree screen's three regions.
///
/// This is [`crate::view::pty_size`]'s counterpart and the place its comment points at: an attach
/// subtracts no chrome because the node has the terminal, and a tree screen subtracts exactly this
/// much because it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    pub tree: Rect,
    pub content: Rect,
    /// One row along the bottom: the selected node's capabilities, greyed where absent.
    pub actions: Rect,
}

/// Split `area` into [`Panes`].
///
/// Degenerate areas are handled by arithmetic rather than by a guard: a terminal one row tall gives
/// the actions strip that row and the two panes zero height, and `ratatui` draws nothing into a
/// zero-height `Rect`. Inventing a minimum size here would make marion refuse to start in a window
/// the operator can see.
pub fn split(area: Rect) -> Panes {
    let strip = area.height.min(1);
    let body = area.height - strip;
    let tree_w = TREE_COLUMN.min(area.width);
    Panes {
        tree: Rect::new(area.x, area.y, tree_w, body),
        content: Rect::new(
            area.x.saturating_add(tree_w),
            area.y,
            area.width - tree_w,
            body,
        ),
        actions: Rect::new(area.x, area.y.saturating_add(body), area.width, strip),
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
        for (row, line) in self.tree.lines().iter().enumerate() {
            let Ok(row) = u16::try_from(row) else { break };
            if row >= area.height {
                break;
            }
            let on_cursor = row as usize == self.tree.cursor;
            let style = match (on_cursor, self.focused) {
                (true, true) => Style::default().add_modifier(Modifier::REVERSED),
                (true, false) => Style::default().add_modifier(Modifier::BOLD),
                (false, _) => Style::default(),
            };
            // The cursor row is painted across the **whole** column, not just its text, so a
            // reversed selection reads as a bar rather than as a ragged highlight.
            if on_cursor {
                for x in 0..area.width {
                    buf[(area.x + x, area.y + row)].set_style(style);
                }
            }
            buf.set_stringn(area.x, area.y + row, line, area.width as usize, style);
        }
    }
}

/// The style an unavailable action is drawn in. **This is the greying**, and it is one value so
/// that a test can name it rather than re-describe it.
pub fn greyed() -> Style {
    Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::CROSSED_OUT)
}

/// The style an available action is drawn in: the terminal's own, deliberately unstyled.
pub fn offered() -> Style {
    Style::default()
}

/// The capability strip for one node.
pub struct ActionBar<'a> {
    /// `None` on an empty tree, which is a real state — a supervisor answering `tree/subscribe`
    /// with no nodes is one that has none, not one that failed.
    pub node: Option<&'a Node>,
}

impl ActionBar<'_> {
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
        let mut x = area.x;
        for action in &node.actions {
            let text = Self::cell(action);
            let style = if action.available {
                offered()
            } else {
                greyed()
            };
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
            actions: Vec::new(),
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
        assert_eq!(t.lines(), ["[Idle] a", "  └ [Idle] kid"]);
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
        assert_eq!(p.tree, Rect::new(0, 0, TREE_COLUMN, 39));
        assert_eq!(p.content, Rect::new(TREE_COLUMN, 0, 100 - TREE_COLUMN, 39));
        assert_eq!(p.actions, Rect::new(0, 39, 100, 1));

        // A window narrower than the tree column gives the tree what there is and the content none,
        // rather than underflowing.
        let p = split(Rect::new(0, 0, 10, 3));
        assert_eq!(p.tree.width, 10);
        assert_eq!(p.content.width, 0);
        // And a one-row terminal is all strip.
        let p = split(Rect::new(0, 0, 80, 1));
        assert_eq!(p.tree.height, 0);
        assert_eq!(p.actions, Rect::new(0, 0, 80, 1));
    }

    /// **The greying, as styles.** The label text is identical in both cases by design, so this is
    /// the only assertion in the tree that can tell an offered action from a greyed one.
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
        assert!(row.starts_with(" interrupt  fork "), "{row:?}");

        // " interrupt " occupies 0..11, " fork " 11..17. The comparison is against the *marks*
        // rather than against a whole `Style`, because `Buffer::empty` seeds every cell with an
        // explicit `Color::Reset` that `Style::default()` does not carry — asserting equality with
        // `offered()` would then fail for a reason that has nothing to do with greying.
        let grey = |x: u16| {
            let s = buf[(x, 0)].style();
            s.fg == Some(Color::DarkGray) && s.add_modifier.contains(Modifier::CROSSED_OUT)
        };
        assert!(!grey(1), "`interrupt` is available and was greyed out");
        assert!(grey(12), "`fork` is unavailable and was offered");
        assert_ne!(offered(), greyed(), "the two styles must be tellable apart");
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
