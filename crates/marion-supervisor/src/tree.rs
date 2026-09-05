//! `marion tree` — §5.6's tree pane, and **M5 clause 3's greying** (§9, §3.3).
//!
//! # The two halves, and where the seam is
//!
//! §9's M5 asks for *"`marion doctor` reporting their differing capabilities and the UI greying out
//! what they cannot do"*. `marion-tui`'s [`marion_tui::tree`] is the *drawing*: it paints an
//! unavailable action in [`marion_tui::tree::greyed`] and an available one plain, and it has no way
//! to find out which is which — it does not depend on `marion-harness` and cannot name a harness.
//!
//! This module is the half that decides, and it decides through
//! [`crate::doctor::capabilities_at`] — **the same function `marion doctor` builds its capability
//! column from, on the same `(harness, harness_version, surfaces)` key §3.3 specifies**. Not a
//! table that agrees with doctor's; doctor's.
//! `the_trees_greying_is_doctors_own_answer_at_every_key` is what says so in a way that fails if it
//! stops being true.
//!
//! ## Why the key needs all three components, with the row that proves each
//!
//! * **harness** — obviously.
//! * **version** — `codex exec resume` was measured on 0.146.0 and marion claims nothing before it
//!   (§9's M1). A tree that dropped the version would publish `resume` for a 0.145.0 node.
//! * **surfaces** — §3.4 gives claude-code a `Typed(StreamJson)` control plane headless and §3.4's
//!   `opaque` on a pane, and the ceiling clips `permissions` off the second. A tree that keyed
//!   every node on the node row would offer a pane node a permission routing it does not have,
//!   which is precisely the defect clause 3 names. [`marion_proto::NodeSummary::pane`] is how the
//!   third component reaches a client.
//!
//! # Navigation, and why attaching is not reimplemented here
//!
//! `↑`/`↓`/`j`/`k` move the cursor, `Tab` moves focus between the tree and the content pane, `q`
//! (or the pane's own `^]`) leaves. **`Enter` runs [`crate::attach::run`]** — the same function
//! `marion attach <agent-id>` calls, reached by leaving this screen first and re-entering it when
//! the attach returns. A tree that spoke `node/attach` itself would be a second attach client with
//! a second write-lease story and a second `SIGWINCH` handler, and §5.3's one-writer rule is
//! exactly the kind of invariant two implementations diverge on.
//!
//! The cost is that this screen's socket and the attach's socket are two connections. That is
//! correct rather than merely tolerable: §5.3 leases the write half **per connection**, and an
//! attach that shared this one would hold the keyboard for a node the operator had already
//! navigated away from.
//!
//! # What is not here
//!
//! §5.6 also lists *"permission and elicitation queues"*. They are not built. No acceptance
//! criterion names them, marion denies every permission today (§11 item 22), and a queue rendering
//! decisions marion has already made would be a widget with nothing to show.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use marion_core::harness::Harness;
use marion_core::node::{NodeState, ReapState};
use marion_harness::{Capabilities, ExecutionSurfaces};
use marion_proto::{Call, Event, Frame, MethodResult, NodeSummary, RequestId};
use marion_tui::tree::{self, Action, Focus, Nav, Tree};
use marion_tui::{Screen, ScreenBackend, Sticky};
use ratatui::Terminal;

use crate::doctor::{SurfaceRole, capabilities_at, surfaces_at};

/// Same as an attach's: a read bound so the loop can look at the keyboard between frames.
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// **The greying, at §3.3's whole key.** All ten fields, always — the absent ones greyed rather
/// than omitted, because §9's clause is *"greying out what they cannot do"* and an action that is
/// simply not drawn tells an operator nothing about whether it exists.
///
/// The `None` arm is a node the supervisor reports as running on a pane surface whose adapter
/// declares none. `marion run --pane` refuses such a harness by name, so it is unreachable through
/// marion's own commands; it is answered with [`Capabilities::NONE`] rather than by falling back to
/// the node row, because §3.3's *degrade visibly* makes the understated answer the safe one and the
/// fallback would be the widening this whole module exists to prevent.
pub fn actions_for(node: &NodeSummary) -> Vec<Action> {
    let (harness, version, surfaces) = key_of(node);
    let caps = match surfaces {
        Some(s) => capabilities_at(harness, version, &s),
        None => Capabilities::NONE,
    };
    let granted = caps.granted();
    Capabilities::FIELDS
        .iter()
        .map(|f| Action::new(*f, granted.contains(f)))
        .collect()
}

/// **§3.3's key, read off one node summary** — `(harness, harness_version, surfaces)`.
///
/// Split out from [`actions_for`] rather than inlined, and the reason is a hole a test could not
/// otherwise reach. **No shipped adapter's surfaces vary with the version today**: claude-code's
/// `advertised` row ignores it, and codex's `resume` — the one row that reads it — is clipped by
/// the `LaunchOnly` ceiling on the surfaces `CodexAdapter` actually declares. So a mutation that
/// dropped `harness_version` on the way into `capabilities_at` would change **no capability of any
/// node marion can spawn**, and every end-to-end assertion would still pass while §3.3's key had
/// quietly become two components.
///
/// `the_version_reaches_the_key_rather_than_being_dropped_on_the_way` asserts against *this*
/// instead, which is a claim about the plumbing and stays true the day an adapter's surfaces do
/// vary with the version — which is the day the hole would otherwise become a wrong answer.
pub fn key_of(node: &NodeSummary) -> (Harness, Option<&str>, Option<ExecutionSurfaces>) {
    (
        node.harness,
        node.harness_version.as_deref(),
        surfaces_at(node.harness, SurfaceRole::of(node.pane)),
    )
}

/// One wire summary as one tree row.
///
/// The label is the node's `name` where it has one — §2's `node/rename` is what puts a name there —
/// and its agent type plus [`short_id`] where it does not. Not the whole id: a UUID is wider than
/// the tree column, and every id in one forest shares its leading time-ordered group, so the whole
/// id told nodes apart by exactly the characters that were clipped. The full id is in the detail
/// pane, where `marion attach` can be copied from.
pub fn row(node: &NodeSummary) -> tree::Node {
    let id = node.agent_id.0.as_str();
    tree::Node {
        id: id.to_string(),
        parent: node.parent_id.as_ref().map(|p| p.0.clone()),
        label: node
            .name
            .clone()
            .unwrap_or_else(|| format!("{} {}", node.agent_type, short_id(id))),
        state: state_label(node.state, node.reap_state),
        actions: actions_for(node),
        // The conservative key is the right answer for an unread version (§3.3), and the strip
        // should say that is why, rather than let ten greyed words read as "this harness cannot".
        note: node
            .harness_version
            .is_none()
            .then(|| "harness version unknown".to_string()),
    }
}

/// The part of an id that tells one node from its siblings.
///
/// An `AgentId` is a UUIDv7: the first group is a timestamp every node spawned in the same second
/// shares, so the second group is the earliest one that varies. Anything not shaped like a UUID —
/// a test fixture's `root-claude` — is shown whole, because guessing at its structure would hide
/// characters the caller chose.
pub fn short_id(id: &str) -> &str {
    let uuid_shaped = id.len() == 36
        && id.is_ascii()
        && id
            .char_indices()
            .all(|(i, c)| (c == '-') == matches!(i, 8 | 13 | 18 | 23));
    if uuid_shaped { &id[9..13] } else { id }
}

/// A node's state in one short word.
///
/// `reap_state` is folded in rather than shown beside it, and only when it is not `Live`: §7.2 is
/// emphatic that `Orphaned` is *not* a claim the process died, so it is shown as its own word
/// instead of being collapsed into an exit. A node that is both `Exited` and reaped shows the exit,
/// which is the more specific fact.
fn state_label(state: NodeState, reap: ReapState) -> String {
    match (state, reap) {
        (NodeState::Exited(s), _) => format!("exited:{s:?}").to_lowercase(),
        (_, ReapState::Orphaned) => "orphaned".into(),
        (_, ReapState::ReapedIdle) => "reaped".into(),
        (NodeState::Blocked(r), _) => format!("blocked:{r:?}").to_lowercase(),
        (s, ReapState::Live) => format!("{s:?}").to_lowercase(),
    }
}

/// What `Enter` does to `node`: the id to attach to, or the sentence saying why not.
///
/// [`NodeSummary::pane`] is the whole test. `attach::run` would reach the same refusal from the
/// supervisor — a headless node has no display plane to lease — but only after this screen had
/// left the alternate buffer, so its `eprintln!` landed under the frame that repainted over it.
pub fn open_target(node: &NodeSummary) -> Result<&str, String> {
    if node.pane {
        Ok(node.agent_id.0.as_str())
    } else {
        Err(format!(
            "{} {} is headless: it has no display plane to open. Enter opens pane nodes only.",
            node.agent_type,
            short_id(&node.agent_id.0)
        ))
    }
}

/// Fold one window measurement into `size`, saying whether it moved.
///
/// `None` — a pipe, or a tty the kernel has not sized — leaves the last geometry in place: a frame
/// sized to what the operator was last known to be looking at beats one sized to a guess.
fn geometry_changed(size: &mut (u16, u16), measured: Option<(u16, u16)>) -> bool {
    match measured {
        Some(now) if now != *size => {
            *size = now;
            true
        }
        _ => false,
    }
}

/// Build the flattened tree from a snapshot, preserving the selection where the node survives.
pub fn build(nodes: &[NodeSummary], keep: Option<&str>) -> Tree {
    let mut t = Tree::new(nodes.iter().map(row).collect());
    if let Some(id) = keep {
        t.select(id);
    }
    t
}

/// Everything that can stop `marion tree` before it starts. See `attach.rs` for why this is a
/// `String`: one exit code, one shape of message, one caller that prints it.
type Refusal = String;

/// `marion tree [--repo <path>] [--state-dir <path>]`.
///
/// Dials the supervisor the same way an attach does — §2 keys one on the git common dir — and, like
/// an attach, **refuses to start one**. A supervisor started here would answer `tree/subscribe`
/// with an empty forest, which reads as "no agents are running" rather than as "marion is not
/// looking where you think".
pub fn run(repo: &Path, state_dir: &Path) -> Result<(), Refusal> {
    let key = crate::socket::project_root(repo);
    let paths = crate::socket::socket_paths(state_dir, &key, crate::attach::uid());
    if crate::socket::nobody_is_serving(&paths) {
        return Err(format!(
            "no supervisor is serving `{}`, so there is no tree to show. Starting one here would \
             show an empty forest, which reads as \"no agents are running\" rather than as \
             \"marion is not looking where you think\". Run `marion run <agent-type>` in this \
             project first.",
            key.display()
        ));
    }
    let stream = UnixStream::connect(paths.socket())
        .map_err(|e| format!("dialling the supervisor for `{}`: {e}", key.display()))?;
    stream
        .set_read_timeout(Some(POLL))
        .map_err(|e| format!("setting a read bound on the supervisor socket: {e}"))?;
    Session::open(stream, key.display().to_string())?.pump(repo, state_dir)
}

/// How many of `nodes` the status row calls running: the ones §7.6 does not count as terminal.
/// An idle node is running — it is a process marion owns; an orphan is not counted, because §7.2
/// declines to say whether it is.
pub fn running(nodes: &[NodeSummary]) -> usize {
    nodes
        .iter()
        .filter(|n| !n.state.is_exited() && !n.reap_state.is_terminal_for_gating())
        .count()
}

/// One `marion tree` screen.
struct Session {
    stream: UnixStream,
    lines: BufReader<UnixStream>,
    /// The snapshot, kept current from the subscription rather than re-fetched: §2's
    /// `tree/node-added` exists precisely so a client is not polling for nodes being created.
    nodes: Vec<NodeSummary>,
    tree: Tree,
    focus: Focus,
    /// The project key, for the status row.
    repo: String,
    /// The last refused `Enter`, shown in the detail pane until the cursor moves.
    notice: Option<String>,
}

impl Session {
    fn open(stream: UnixStream, repo: String) -> Result<Session, Refusal> {
        let lines = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("cloning the supervisor socket: {e}"))?,
        );
        let mut s = Session {
            stream,
            lines,
            nodes: Vec::new(),
            tree: Tree::new(Vec::new()),
            focus: Focus::Tree,
            repo,
            notice: None,
        };
        s.subscribe()?;
        Ok(s)
    }

    fn subscribe(&mut self) -> Result<(), Refusal> {
        let frame = Frame::Request(marion_proto::Request::new(
            RequestId::Number(1),
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
        ));
        self.stream
            .write_all(frame.to_line().as_bytes())
            .and_then(|()| self.stream.flush())
            .map_err(|e| format!("sending tree/subscribe: {e}"))?;
        let response = loop {
            match self.next_frame()? {
                Some(Frame::Response(r)) => break r,
                Some(other) => {
                    self.absorb(other);
                    continue;
                }
                None => continue,
            }
        };
        let body = match response.outcome {
            marion_proto::Outcome::Result(b) => b,
            marion_proto::Outcome::Error(e) => {
                return Err(format!(
                    "the supervisor refused tree/subscribe: {}",
                    e.message
                ));
            }
        };
        let MethodResult::TreeSubscribe(snapshot) = marion_proto::Method::TreeSubscribe
            .decode_result(&body)
            .map_err(|e| format!("the supervisor's tree/subscribe answer did not decode: {e}"))?
        else {
            return Err(
                "the supervisor answered tree/subscribe with another method's result".into(),
            );
        };
        self.nodes = snapshot.nodes;
        self.rebuild();
        Ok(())
    }

    /// Fold one notification into the snapshot. **Unknown events are ignored and known ones are
    /// never inferred**: a `node/state` for a node this client has not been told about is dropped
    /// rather than used to invent a summary, because a summary invented here would have a fabricated
    /// harness and would then be greyed from the wrong row of doctor's table.
    fn absorb(&mut self, frame: Frame) {
        let Frame::Notification(n) = frame else {
            return;
        };
        match n.event {
            Event::NodeAdded { node, .. } => {
                match self.nodes.iter_mut().find(|n| n.agent_id == node.agent_id) {
                    Some(existing) => *existing = node,
                    None => self.nodes.push(node),
                }
                self.rebuild();
            }
            Event::NodeState {
                agent_id,
                state,
                reap_state,
                ..
            } => {
                if let Some(n) = self.nodes.iter_mut().find(|n| n.agent_id == agent_id) {
                    n.state = state;
                    n.reap_state = reap_state;
                    self.rebuild();
                }
            }
            _ => {}
        }
    }

    fn rebuild(&mut self) {
        let keep = self.tree.selected().map(|n| n.id.clone());
        self.tree = build(&self.nodes, keep.as_deref());
    }

    fn next_frame(&mut self) -> Result<Option<Frame>, Refusal> {
        let mut line = String::new();
        match self.lines.read_line(&mut line) {
            Ok(0) => Err("the supervisor closed the connection".into()),
            Ok(_) => Frame::from_line(&line)
                .map(Some)
                .map_err(|e| format!("the supervisor sent a frame marion cannot read: {e}")),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(format!("reading from the supervisor: {e}")),
        }
    }

    /// The loop: paint, poll the socket, read the keyboard, repeat.
    ///
    /// **Single-threaded, unlike an attach's**, and that is what makes `Enter` work. An attach reads
    /// stdin on its own thread because it has to forward every byte; this screen consumes a handful
    /// of keys, and a reader thread here would still be blocked in `read(2)` while the attach it
    /// launched was trying to read the same fd — stealing the operator's first keystroke into a
    /// pane. A bounded `poll` of stdin on this one thread has no such window — and it is a `poll`,
    /// not `O_NONBLOCK`, because that flag lives on the tty description stdout shares and would
    /// make the paint fail on a full pty buffer instead of waiting.
    fn pump(&mut self, repo: &Path, state_dir: &Path) -> Result<(), Refusal> {
        loop {
            // Re-measured each pass: an attach the operator just left may have been resized, and
            // `marion attach` forwards its own geometry to the node rather than this screen's.
            let (cols, rows) = marion_tui::guard::window_size(0).unwrap_or((80, 24));
            let screen = Screen::enter(std::io::stdout(), 0, &Sticky::initial(cols, rows))
                .map_err(|e| format!("entering the terminal: {e}"))?;
            let opened = self.draw_loop(screen, cols, rows)?;
            let Some(agent) = opened else { return Ok(()) };
            // The attach owns the terminal from here until the operator detaches with `^] d`. Its
            // refusal — a lease another client holds, say — comes back as the pane's notice rather
            // than ending the session or going to a stderr the next frame erases.
            if let Err(e) = crate::attach::run(&agent, repo, state_dir) {
                self.notice = Some(format!("attach refused: {e}"));
            }
        }
    }

    /// One pass of the screen, returning the node to attach to if the operator chose one.
    fn draw_loop(
        &mut self,
        screen: Screen,
        cols: u16,
        rows: u16,
    ) -> Result<Option<String>, Refusal> {
        // Before the renderer, so a refused keyboard leaves through `screen`'s own drop.
        let mut stdin = marion_tui::guard::Keyboard::open(0)
            .map_err(|e| format!("watching the tree's keyboard: {e}"))?;
        let mut terminal = Terminal::new(ScreenBackend::new(screen, cols, rows))
            .map_err(|e| format!("starting the tree's renderer: {e}"))?;
        // **The terminal is restored on every exit from this function**, including the one that
        // hands stdin to an attach. `ScreenBackend` owns the guard and `Drop` would do it — but the
        // order matters on the way out for the same reason `attach::Session::drop` says: an error
        // printed onto the alternate screen disappears with it.
        let leave = |t: &Terminal<ScreenBackend>| t.backend().screen().leave();
        let mut buf = [0u8; 256];
        let mut size = (cols, rows);
        loop {
            // The window is re-measured every pass rather than on `SIGWINCH`: this loop already
            // wakes every `POLL` to glance at the keyboard, and one `TIOCGWINSZ` per wake is cheaper
            // than a handler. `Terminal::draw` reads the backend's size back and clears on change.
            if geometry_changed(&mut size, marion_tui::guard::window_size(0)) {
                terminal.backend_mut().set_size(size.0, size.1);
            }
            self.paint(&mut terminal);
            match self.next_frame() {
                Ok(Some(frame)) => self.absorb(frame),
                Ok(None) => {}
                // The supervisor going away closes the tree, and is not a failure of it.
                Err(_) => {
                    leave(&terminal);
                    return Ok(None);
                }
            }
            // The socket read above is the pacing; the keyboard is only glanced at, so a key that
            // is not already there waits for the next pass rather than holding up a frame.
            let n = match stdin.read_within(&mut buf, std::time::Duration::ZERO) {
                Ok(Some(0)) => {
                    leave(&terminal);
                    return Ok(None);
                }
                Ok(Some(n)) => n,
                Ok(None) | Err(_) => 0,
            };
            for action in tree::nav(&buf[..n]) {
                match action {
                    Nav::Quit => {
                        leave(&terminal);
                        return Ok(None);
                    }
                    Nav::ToggleFocus => {
                        self.focus = match self.focus {
                            Focus::Tree => Focus::Content,
                            Focus::Content => Focus::Tree,
                        }
                    }
                    Nav::Up if self.focus == Focus::Tree => {
                        self.notice = None;
                        self.tree.move_by(-1);
                    }
                    Nav::Down if self.focus == Focus::Tree => {
                        self.notice = None;
                        self.tree.move_by(1);
                    }
                    Nav::Up | Nav::Down => {}
                    // Decided here, from the summary, before the terminal changes hands: an attach
                    // would refuse a headless node too, but onto a screen the next frame erases.
                    Nav::Open => match self.selected_summary().map(open_target) {
                        Some(Ok(id)) => {
                            let id = id.to_string();
                            leave(&terminal);
                            return Ok(Some(id));
                        }
                        Some(Err(refusal)) => self.notice = Some(refusal),
                        None => {}
                    },
                }
            }
        }
    }

    fn paint(&mut self, terminal: &mut Terminal<ScreenBackend>) {
        let tree = &self.tree;
        let focus = self.focus;
        let selected = self.selected_summary();
        let status = tree::Status {
            repo: &self.repo,
            nodes: self.nodes.len(),
            running: running(&self.nodes),
        };
        let notice = self.notice.as_deref();
        let _ = terminal.draw(|f| {
            let panes = tree::split(f.area());
            f.render_widget(status, panes.status);
            f.render_widget(
                tree::TreeView {
                    tree,
                    focused: focus == Focus::Tree,
                },
                panes.tree,
            );
            f.render_widget(
                tree::ActionBar {
                    node: tree.selected(),
                },
                panes.actions,
            );
            f.render_widget(
                Detail {
                    node: selected,
                    notice,
                },
                panes.content,
            );
        });
    }

    /// The wire summary under the cursor. The tree row is a projection of it; the detail pane
    /// wants the whole thing.
    fn selected_summary(&self) -> Option<&NodeSummary> {
        let id = &self.tree.selected()?.id;
        self.nodes.iter().find(|n| n.agent_id.0 == *id)
    }
}

/// What the content pane shows before anything is attached: **the selected node, spelled out.**
///
/// The tree row is a state and a short label, and that is all a 44-column sidebar can carry. The
/// rest of the [`NodeSummary`] — the harness and the version the greying is keyed on, whether the
/// node has a display plane `Enter` can open, its depth, parent, bound and whole id — is here, in
/// the pane that has the width. The whole id is the one line an operator copies: `marion attach`
/// takes it.
///
/// Still not a terminal. §5.6's content pane is a node's grid, and this screen deliberately does
/// not open one until `Enter`: two panes both feeding a `marion_term::Term` would hold two
/// [`marion_tui::MAX_SCROLLBACK`] budgets for a node the operator is only browsing past.
struct Detail<'a> {
    /// `None` on an empty forest, which is a real state and is said as one.
    node: Option<&'a NodeSummary>,
    /// One line this screen wants the operator to read — a refused `Enter`, for instance. Drawn
    /// here rather than printed, because a `eprintln!` under the alternate screen is erased by the
    /// next frame before anyone sees it.
    notice: Option<&'a str>,
}

/// The pane's last row: every key this screen answers to, and what `Enter` will and will not open.
const KEYS: &str = "enter open pane nodes only  j/k move  tab focus  q quit  ^] d detach";

impl Detail<'_> {
    fn lines(node: &NodeSummary) -> Vec<String> {
        let version = node.harness_version.as_deref().unwrap_or("version unknown");
        let surface = if node.pane {
            "pane (Enter attaches)"
        } else {
            "headless (no display plane; Enter refuses, and so would `marion attach`)"
        };
        let parent = node
            .parent_id
            .as_ref()
            .map_or("none (root)", |p| p.0.as_str());
        let mut out = Vec::with_capacity(8);
        if let Some(name) = &node.name {
            out.push(format!("name     {name}"));
        }
        out.push(format!("type     {}", node.agent_type));
        out.push(format!("harness  {} {version}", node.harness));
        out.push(format!(
            "state    {}  (reap: {})",
            state_label(node.state, node.reap_state),
            format!("{:?}", node.reap_state).to_lowercase()
        ));
        out.push(format!("surface  {surface}"));
        out.push(format!("depth    {}   parent  {parent}", node.depth));
        out.push(format!("timeout  {}s", node.timeout.0.as_secs()));
        out.push(format!("id       {}", node.agent_id.0));
        out
    }
}

impl ratatui::widgets::Widget for Detail<'_> {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        if area.height == 0 {
            return;
        }
        let style = ratatui::style::Style::default;
        let put = |buf: &mut ratatui::buffer::Buffer, y: u16, text: &str, style| {
            buf.set_stringn(
                area.x.saturating_add(2),
                area.y.saturating_add(y),
                text,
                area.width.saturating_sub(2) as usize,
                style,
            );
        };
        let mut lines = match self.node {
            Some(n) => Self::lines(n),
            None => vec!["no nodes in this forest".into()],
        };
        if let Some(notice) = self.notice {
            lines.push(String::new());
            lines.push(notice.to_string());
        }
        // The hints own the last row; the facts get every row above it.
        let last = area.height - 1;
        for (i, line) in lines.iter().enumerate() {
            let Ok(y) = u16::try_from(i) else { break };
            if y >= last {
                break;
            }
            let bold = self.notice.is_some() && i + 1 == lines.len();
            let s = if bold {
                style().add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                style()
            };
            put(buf, y, line, s);
        }
        put(
            buf,
            last,
            KEYS,
            style().add_modifier(ratatui::style::Modifier::DIM),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::AgentId;
    use marion_core::harness::Harness;

    fn summary(id: &str, harness: Harness, pane: bool, version: Option<&str>) -> NodeSummary {
        NodeSummary {
            agent_id: AgentId(id.into()),
            parent_id: None,
            name: None,
            agent_type: "codex-impl".into(),
            harness,
            harness_version: version.map(str::to_string),
            depth: 0,
            state: NodeState::Idle,
            reap_state: ReapState::Live,
            timeout: marion_core::encoding::Duration::from_secs(900),
            pane,
        }
    }

    fn offered(node: &NodeSummary) -> Vec<String> {
        actions_for(node)
            .into_iter()
            .filter(|a| a.available)
            .map(|a| a.name)
            .collect()
    }

    /// **The clause-3 test, and the one that says the greying is not a second table.**
    ///
    /// It walks every key the tree can construct — every harness, both surface roles, and a version
    /// on each side of the one floor marion has measured — and asserts the tree's answer is
    /// `doctor`'s at that exact key. Keyed, not compared as sets: a comparison of "the actions the
    /// tree offers somewhere" against "the capabilities doctor publishes somewhere" would pass
    /// while the two rows were **swapped**, which is the failure a previous test in this repo had.
    ///
    /// Required mutations, each of which fails here:
    /// * grey something the node can do — drop `interrupt` from claude-code's row, or return
    ///   [`Capabilities::NONE`] from `actions_for`;
    /// * offer something it cannot — use `SurfaceRole::Node` for every node, or drop the version
    ///   from the key;
    /// * source the greying from a second table — any table that is not
    ///   [`crate::doctor::capabilities_at`] must differ at one of the keys below, and the
    ///   `differing` assertion is what guarantees the keys are not all the same answer.
    #[test]
    fn the_trees_greying_is_doctors_own_answer_at_every_key() {
        let mut distinct = std::collections::BTreeSet::new();
        let mut checked = 0;
        for harness in Harness::ALL {
            for pane in [false, true] {
                let role = SurfaceRole::of(pane);
                let Some(surfaces) = surfaces_at(harness, role) else {
                    // No pane shape for this harness. `actions_for` must claim nothing rather than
                    // fall through to the node row.
                    let n = summary("x", harness, pane, Some("9.9.9"));
                    assert!(
                        offered(&n).is_empty(),
                        "{harness} has no {} surfaces and the tree offered {:?} anyway",
                        role.as_str(),
                        offered(&n)
                    );
                    continue;
                };
                for version in [None, Some("0.145.0"), Some("0.146.0"), Some("9.9.9")] {
                    let node = summary("x", harness, pane, version);
                    let doctor = capabilities_at(harness, version, &surfaces);
                    let want: Vec<String> =
                        doctor.granted().into_iter().map(str::to_string).collect();
                    assert_eq!(
                        offered(&node),
                        want,
                        "the tree and `marion doctor` disagree at ({harness}, {version:?}, {})",
                        role.as_str()
                    );
                    // Every field is drawn, available or not — clause 3 is about *greying*, and an
                    // action that is simply absent is not greyed.
                    assert_eq!(
                        actions_for(&node)
                            .iter()
                            .map(|a| a.name.as_str())
                            .collect::<Vec<_>>(),
                        Capabilities::FIELDS.to_vec(),
                        "every capability must be drawn, the unavailable ones greyed"
                    );
                    distinct.insert(want);
                    checked += 1;
                }
            }
        }
        assert!(checked >= 20, "only {checked} keys were exercised");
        assert!(
            distinct.len() >= 3,
            "every key produced the same answer ({distinct:?}), so this would pass for a tree \
             that ignored its key entirely"
        );
    }

    /// **The two named rows the general test above would pass for if it were only a set
    /// comparison.** Both directions of the clause, spelled out.
    #[test]
    fn a_pane_node_is_not_offered_what_only_its_headless_key_can_do() {
        // claude-code headless: §3.4's `Typed(StreamJson)`, and S9 measured a real `can_use_tool`
        // round trip — so `permissions` is offered.
        let headless = summary("a", Harness::ClaudeCode, false, Some("2.1.223"));
        assert_eq!(offered(&headless), ["interrupt", "permissions"]);

        // The same binary on a pane: §3.4's `opaque`, where marion "can write, but cannot address
        // a turn". The ceiling clips `permissions`, and `interrupt` survives because S1/S11
        // measured a real interrupt protocol over a pty.
        let paned = summary("a", Harness::ClaudeCode, true, Some("2.1.223"));
        assert_eq!(
            offered(&paned),
            ["interrupt"],
            "a pane node was offered a permission routing its surfaces cannot carry — §9's M5 \
             clause 3, failed"
        );
        // And the greyed one is *drawn*, not dropped.
        let permissions = actions_for(&paned)
            .into_iter()
            .find(|a| a.name == "permissions")
            .expect("`permissions` must still appear, greyed");
        assert!(!permissions.available);
    }

    /// **The version is the middle third of §3.3's key, and this is the only test that can catch
    /// it being dropped.** See [`key_of`]: no shipped adapter's surfaces vary with the version, so
    /// a `capabilities_at(h, None, &s)` in `actions_for` would change no visible answer and pass
    /// every other assertion in this file.
    ///
    /// Mutation: replace `node.harness_version.as_deref()` in `key_of` with `None`. This fails and
    /// nothing else in the workspace does.
    #[test]
    fn the_version_reaches_the_key_rather_than_being_dropped_on_the_way() {
        use marion_harness::TypedKind;

        for v in [None, Some("0.145.0"), Some("0.146.0")] {
            let node = summary("x", Harness::Codex, false, v);
            assert_eq!(
                key_of(&node).1,
                v,
                "the summary's recorded version must be the one the key is built from"
            );
        }

        // And the component is not inert *in the function it is handed to* — dropping it there
        // would publish `resume` on a binary that never had it, the moment a surface can carry one.
        let app_server = ExecutionSurfaces::headless(TypedKind::AppServer);
        assert!(
            !capabilities_at(Harness::Codex, Some("0.145.0"), &app_server).resume,
            "marion measured `codex exec resume` on 0.146.0 and claims nothing before it"
        );
        assert!(capabilities_at(Harness::Codex, Some("0.146.0"), &app_server).resume);
        assert!(
            !capabilities_at(Harness::Codex, None, &app_server).resume,
            "a node with no recorded version is unmeasured, not optimistically resumable"
        );
    }

    /// A node the tree is told about must be a row. The forbidden mutation is the one where a
    /// summary is dropped between the wire and the widget.
    #[test]
    fn every_summary_the_supervisor_sends_becomes_a_row() {
        let mut kid = summary("kid", Harness::Codex, false, None);
        kid.parent_id = Some(AgentId("root".into()));
        let mut orphan = summary("orphan", Harness::Codex, false, None);
        orphan.parent_id = Some(AgentId("compacted-away".into()));
        let nodes = vec![summary("root", Harness::Codex, false, None), kid, orphan];

        let t = build(&nodes, None);
        assert_eq!(
            t.rows(),
            nodes.len(),
            "a live node the journal records is missing"
        );
        let ids: Vec<String> = t.lines();
        for n in &nodes {
            assert!(
                ids.iter().any(|l| l.contains(&n.agent_id.0)),
                "`{}` is not on the screen: {ids:?}",
                n.agent_id.0
            );
        }
    }

    /// A UUID does not fit the tree column and every node in one forest shares its first eight
    /// characters, so an unnamed node is labelled by what does tell nodes apart: its agent type and
    /// the second group of its id. A name, once given, replaces both; an id that is not a UUID is
    /// shown whole.
    #[test]
    fn an_unnamed_node_is_labelled_by_type_and_short_id_and_a_named_one_by_its_name() {
        let uuid = "01a07275-5b04-78d7-8f77-ad154316f985";
        let mut n = summary(uuid, Harness::Codex, false, None);
        assert_eq!(row(&n).label, "codex-impl 5b04");
        n.name = Some("reviewer".into());
        assert_eq!(row(&n).label, "reviewer");
        assert_eq!(short_id("root-claude"), "root-claude");
        assert_eq!(short_id(uuid), "5b04");
    }

    fn painted(area: ratatui::layout::Rect, w: impl ratatui::widgets::Widget) -> Vec<String> {
        let mut buf = ratatui::buffer::Buffer::empty(area);
        w.render(area, &mut buf);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// **The content pane carries every fact the row cannot.** A row is a state and a short label;
    /// the harness, its version, the surface, the depth, the parent, the bound and the whole id —
    /// which is what `marion attach` wants typed — are all in `NodeSummary` and were shown nowhere.
    /// The key hints stay, as the pane's last row, and say which nodes `Enter` can open.
    #[test]
    fn the_detail_pane_names_every_fact_the_row_cannot_carry() {
        let mut n = summary(
            "01a07275-5c4a-73ac-88f8-7df80dc5095c",
            Harness::ClaudeCode,
            false,
            Some("2.1.261 (Claude Code)"),
        );
        n.parent_id = Some(AgentId("01a07275-5b04-78d7-8f77-ad154316f985".into()));
        n.depth = 1;
        n.agent_type = "claude-impl".into();
        let area = ratatui::layout::Rect::new(0, 0, 76, 12);
        let rows = painted(
            area,
            Detail {
                node: Some(&n),
                notice: None,
            },
        );
        let text = rows.join("\n");
        for fact in [
            "claude-impl",
            "claude-code 2.1.261 (Claude Code)",
            "idle",
            "headless",
            "depth    1",
            "01a07275-5b04-78d7-8f77-ad154316f985",
            "900s",
            "01a07275-5c4a-73ac-88f8-7df80dc5095c",
        ] {
            assert!(text.contains(fact), "`{fact}` is not in the pane:\n{text}");
        }
        let keys = rows.last().expect("a last row");
        assert!(keys.contains("^] d"), "detach hint: {keys}");
        assert!(keys.contains("pane nodes only"), "what Enter opens: {keys}");

        // The version that was never read is said, not blanked.
        n.harness_version = None;
        n.pane = true;
        let text = painted(
            area,
            Detail {
                node: Some(&n),
                notice: None,
            },
        )
        .join("\n");
        assert!(text.contains("version unknown"), "{text}");
        assert!(text.contains("pane"), "{text}");

        // An empty forest is a sentence, and the hints still show.
        let rows = painted(
            area,
            Detail {
                node: None,
                notice: None,
            },
        );
        assert!(rows[0].contains("no nodes"), "{rows:?}");
        assert!(rows.last().unwrap().contains("q quit"), "{rows:?}");
    }

    /// **`Enter` on a headless node is refused on the screen, not on stderr.** The old path ran
    /// `attach::run`, which refused correctly and `eprintln!`ed the reason under the alternate
    /// screen — where the next frame erased it, so the operator saw a flicker and nothing else. The
    /// decision is made here from `NodeSummary::pane`, before the terminal is handed over, and the
    /// reason is drawn in the detail pane until the cursor moves.
    #[test]
    fn enter_on_a_headless_node_is_refused_in_the_pane_and_a_pane_node_opens() {
        let headless = summary("h", Harness::ClaudeCode, false, None);
        let refusal = open_target(&headless).expect_err("a headless node has nothing to open");
        assert!(refusal.contains("headless"), "{refusal}");
        assert!(refusal.contains("pane nodes only"), "{refusal}");

        let paned = summary("p", Harness::Codex, true, None);
        assert_eq!(open_target(&paned), Ok("p"));

        // And the refusal is on the screen, in the pane, emphasised.
        let area = ratatui::layout::Rect::new(0, 0, 90, 14);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(
            Detail {
                node: Some(&headless),
                notice: Some(&refusal),
            },
            area,
            &mut buf,
        );
        let rows = painted(
            area,
            Detail {
                node: Some(&headless),
                notice: Some(&refusal),
            },
        );
        let (y, _) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains("pane nodes only") && !r.contains("j/k"))
            .unwrap_or_else(|| panic!("the refusal is not in the pane: {rows:?}"));
        let y = u16::try_from(y).unwrap();
        assert!(
            buf[(2, y)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "a refusal must stand out from the facts"
        );
    }

    /// **A resize reaches the tree screen while it is up.** The loop measured the window once per
    /// screen entry, so a tree dragged from 120x40 to 80x24 kept painting a 120-column frame into
    /// 80 columns until the operator left and came back. The geometry is re-read every pass; a
    /// window that answers nothing — a pipe — keeps the last known size rather than inventing one.
    #[test]
    fn a_resize_is_applied_on_the_next_pass_and_a_pipe_keeps_the_last_geometry() {
        let mut size = (120, 40);
        assert!(
            !geometry_changed(&mut size, Some((120, 40))),
            "unchanged is not a resize"
        );
        assert!(geometry_changed(&mut size, Some((80, 24))));
        assert_eq!(
            size,
            (80, 24),
            "the new geometry is what the next frame is sized to"
        );
        assert!(
            !geometry_changed(&mut size, None),
            "no answer is not a resize"
        );
        assert_eq!(size, (80, 24));
    }

    /// A node whose version was never read is greyed at the conservative key, and the strip says
    /// so rather than leaving ten greyed words to be read as "this harness can do nothing".
    #[test]
    fn an_unread_version_is_named_on_the_strip() {
        let unread = summary("x", Harness::Codex, false, None);
        assert_eq!(
            row(&unread).note.as_deref(),
            Some("harness version unknown")
        );
        let read = summary("x", Harness::Codex, false, Some("0.146.0"));
        assert_eq!(row(&read).note, None);
    }

    /// "Running" on the status row is §7.6's complement: a node that is neither exited nor reaped
    /// nor orphaned. An idle node is running — it is a process marion owns — and an orphan is not
    /// counted, since §7.2 will not claim to know.
    #[test]
    fn the_status_rows_running_count_is_the_non_terminal_nodes() {
        use marion_core::contract::ExitStatus;
        let mut exited = summary("e", Harness::Codex, false, None);
        exited.state = NodeState::Exited(ExitStatus::Ok);
        let mut orphan = summary("o", Harness::Codex, false, None);
        orphan.reap_state = ReapState::Orphaned;
        let idle = summary("i", Harness::Codex, false, None);
        let mut busy = summary("b", Harness::Codex, true, None);
        busy.state = NodeState::Running;
        assert_eq!(running(&[exited, orphan, idle, busy]), 2);
        assert_eq!(running(&[]), 0);
    }

    /// A refresh must not move the cursor out from under the operator.
    #[test]
    fn the_selection_survives_a_snapshot_arriving() {
        let nodes = vec![
            summary("a", Harness::Codex, false, None),
            summary("b", Harness::Codex, false, None),
        ];
        let mut t = build(&nodes, None);
        t.move_by(1);
        assert_eq!(t.selected().unwrap().id, "b");
        let with_a_new_node = {
            let mut v = nodes.clone();
            v.insert(0, summary("zero", Harness::Codex, false, None));
            v
        };
        let t2 = build(&with_a_new_node, Some("b"));
        assert_eq!(
            t2.selected().unwrap().id,
            "b",
            "a node appearing above the cursor moved the selection"
        );
    }

    #[test]
    fn a_state_is_labelled_by_the_more_specific_of_the_two_fields() {
        use marion_core::contract::ExitStatus;
        assert_eq!(state_label(NodeState::Idle, ReapState::Live), "idle");
        assert_eq!(
            state_label(NodeState::Running, ReapState::Orphaned),
            "orphaned",
            "§7.2: orphaned is not an exit, so it is its own word"
        );
        assert_eq!(
            state_label(NodeState::Exited(ExitStatus::Ok), ReapState::Orphaned),
            "exited:ok",
            "an exit is the more specific fact"
        );
        assert!(
            state_label(
                NodeState::Blocked(marion_core::node::BlockReason::Permission),
                ReapState::Live
            )
            .contains("permission")
        );
    }
}
