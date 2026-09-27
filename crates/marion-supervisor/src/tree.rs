//! The forest's projection — §5.6's tree pane, and **M5 clause 3's greying** (§9, §3.3).
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
//!   which is precisely the defect clause 3 names. [`marion_core::proto::NodeSummary::pane`] is how the
//!   third component reaches a client.
//!
//! # Where the screen went
//!
//! The screen that drew this projection is now the home screen's Watch tab
//! ([`crate::home`]), which `marion ls` and `marion tree` open. What stays here is the part both it
//! and the line-oriented verbs (`list`, `steer`, `cancel`, `ls <id>`) read: the projection of a
//! [`NodeSummary`] into a row, its greying, its attention reason, short ids, and [`Subscription`] —
//! one live `tree/subscribe` kept current. Enter still runs [`crate::attach::run`] after leaving
//! the screen, for the reason this module always gave: a second attach client would be a second
//! write-lease story, and §5.3's one-writer rule is exactly the kind of invariant two
//! implementations diverge on.
//!
//! # What is not here
//!
//! §5.6 also lists *"permission and elicitation queues"*. They are not built. No acceptance
//! criterion names them, marion denies every permission today (§11 item 22), and a queue rendering
//! decisions marion has already made would be a widget with nothing to show.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use marion_core::contract::AgentId;
use marion_core::harness::Harness;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::{Call, Event, Frame, MethodResult, NodeSummary, RequestId};
use marion_harness::{Capabilities, ExecutionSurfaces};
use marion_tui::tree::{self, Action, Tone, Tree};

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
        label: label_of(node),
        state: state_label(node.state, node.reap_state),
        tone: tone_of(node.state, node.reap_state),
        actions: actions_for(node),
        // The conservative key is the right answer for an unread version (§3.3), and the strip
        // should say that is why, rather than let ten greyed words read as "this harness cannot".
        note: node
            .harness_version
            .is_none()
            .then(|| "harness version unknown".to_string()),
    }
}

/// **What one node is called, in the one place that decides it.**
///
/// The tree row and the detail pane's title are the same string by construction rather than by
/// agreement: two spellings of a node's name is how an operator ends up unsure whether the `8ea3`
/// in the sidebar and the `01a091ba-8ea3-…` in the pane are the same agent.
pub fn label_of(node: &NodeSummary) -> String {
    node.name
        .clone()
        .unwrap_or_else(|| format!("{} {}", node.agent_type, short_id(&node.agent_id.0)))
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

/// **The node `marion steer <id|short-id>` means**, read off one snapshot.
///
/// A whole id is itself. Otherwise the one node whose [`short_id`] is `arg` — the id the tree row
/// shows, so an operator can type what they see. Two nodes sharing a short id is refused with every
/// candidate's whole id, because guessing between them would steer an agent nobody chose. An arg
/// that names nothing is returned as-is: the supervisor's own `not found` sentence is the better
/// answer, and a second one here would be a second spelling of it.
pub fn resolve_target(arg: &str, nodes: &[NodeSummary]) -> Result<AgentId, String> {
    if nodes.iter().any(|n| n.agent_id.0 == arg) {
        return Ok(AgentId(arg.to_string()));
    }
    let matches: Vec<&str> = nodes
        .iter()
        .map(|n| n.agent_id.0.as_str())
        .filter(|id| short_id(id) == arg)
        .collect();
    match matches.as_slice() {
        [] => Ok(AgentId(arg.to_string())),
        [one] => Ok(AgentId((*one).to_string())),
        many => Err(format!(
            "`{arg}` is the short id of {} nodes ({}); name the one you mean by its whole id",
            many.len(),
            many.join(", ")
        )),
    }
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

/// The [`Tone`] of a node's state, decided from the same two fields as [`state_label`] and by
/// the same precedence: an exit outranks a reap state, which outranks the live state.
///
/// `Cancelled` and `Unreported` are not failures — the first is a decision and the second an
/// absence — and `Orphaned` is §7.2's *"no record of deciding"*, so all three take the grey the
/// strip uses for *unmeasured*: not a fault, and not a fact either.
fn tone_of(state: NodeState, reap: ReapState) -> Tone {
    use marion_core::contract::ExitStatus;
    match (state, reap) {
        (NodeState::Exited(ExitStatus::Ok), _) => Tone::Done,
        (NodeState::Exited(ExitStatus::Failed | ExitStatus::TimedOut | ExitStatus::Killed), _) => {
            Tone::Failed
        }
        (NodeState::Exited(ExitStatus::Cancelled | ExitStatus::Unreported), _) => Tone::Unknown,
        (_, ReapState::Orphaned | ReapState::ReapedIdle) => Tone::Unknown,
        (NodeState::Blocked(_), ReapState::Live) => Tone::Blocked,
        (_, ReapState::Live) => Tone::Live,
    }
}

/// **Why `node` needs an operator, or `None` when it does not** — the attention queue's one rule.
///
/// Three classes, and the reason is the row's own [`state_label`] word so the queue and the tree
/// can never name one node two ways:
///
/// * **blocked** — waiting on somebody: a permission, an elicitation, its own descendants;
/// * **an exit that went wrong or went unreported** — `Failed`, `TimedOut`, `Killed`,
///   `Unreported`. A failed verification is already `Exited(Failed)` (§5.4), so it is here with
///   no field of its own;
/// * **orphaned** — §7.2's *no record of deciding*: marion cannot say what became of it.
///
/// Not a clean exit, not a cancel (a decision somebody already made), not a reap, not a live
/// state. The precedence is [`state_label`]'s: an exit outranks a reap state, which outranks the
/// live state. Decided from the summary's two state fields only — never from a pane's screen.
pub fn attention_of(node: &NodeSummary) -> Option<String> {
    use marion_core::contract::ExitStatus;
    let (state, reap) = (node.state, node.reap_state);
    let needs = match (state, reap) {
        (NodeState::Exited(ExitStatus::Ok | ExitStatus::Cancelled), _) => false,
        (NodeState::Exited(_), _) => true,
        (_, ReapState::Orphaned) => true,
        (_, ReapState::ReapedIdle) => false,
        (NodeState::Blocked(_), ReapState::Live) => true,
        (_, ReapState::Live) => false,
    };
    needs.then(|| state_label(state, reap))
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
            "{} {} is headless; Enter opens pane nodes only.",
            node.agent_type,
            short_id(&node.agent_id.0)
        ))
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

/// **One `tree/subscribe` snapshot, and nothing kept open** — what `marion list` prints.
///
/// The screen's own dial and the screen's own subscribe, so the list and the tree cannot disagree
/// about which supervisor they asked or what its answer adds up to; the notifications that arrive
/// before the answer are folded exactly as the screen folds them. Refuses as [`run`] does, for
/// [`run`]'s reason, when nobody is serving.
pub fn snapshot(repo: &Path, state_dir: &Path) -> Result<Vec<NodeSummary>, Refusal> {
    Ok(Subscription::open(repo, state_dir)?.nodes)
}

/// **One node as one `marion list` line**: `<glyph> <state> <agent_type> <agent_id>`, then
/// `parent <short>` for a child.
///
/// The state word is [`state_label`]'s and so, for a node that needs attention, it *is*
/// [`attention_of`]'s reason — `--attention` filters these lines rather than printing another
/// shape. The whole id, unlike the tree row's short one, because a list is what `marion attach`
/// is copied from; the parent short, because it is only there to place the node.
pub fn list_line(node: &NodeSummary) -> String {
    let (state, reap) = (node.state, node.reap_state);
    let mut line = format!(
        "{} {} {} {}",
        tone_of(state, reap).glyph(),
        state_label(state, reap),
        node.agent_type,
        node.agent_id.0
    );
    if let Some(parent) = &node.parent_id {
        line.push_str(" parent ");
        line.push_str(short_id(&parent.0));
    }
    line
}

/// Dial the supervisor for `repo`, **refusing to start one** (see [`run`]), and say how the status
/// row should name the project and where a steer dials.
fn dial(repo: &Path, state_dir: &Path) -> Result<(UnixStream, String, PathBuf), Refusal> {
    let key = crate::socket::project_root(repo);
    let paths = crate::socket::socket_paths(state_dir, &key, crate::socket::own_uid());
    if crate::socket::nobody_is_serving(&paths) {
        // One line. Why marion will not start a supervisor here is `run`'s doc comment.
        return Err(format!(
            "no supervisor is serving `{}` under state dir `{}`; start a session here first \
             (`marion <harness>` or `marion run`) with the same --state-dir / $MARION_STATE_DIR",
            key.display(),
            state_dir.display()
        ));
    }
    let stream = UnixStream::connect(paths.socket())
        .map_err(|e| format!("dialling the supervisor for `{}`: {e}", key.display()))?;
    stream
        .set_read_timeout(Some(POLL))
        .map_err(|e| format!("setting a read bound on the supervisor socket: {e}"))?;
    // The status row names the worktree, not its `.git`: §2 keys on the common dir, and an
    // operator with three windows open recognises the directory they ran marion in.
    let shown = match key.file_name().and_then(|f| f.to_str()) {
        Some(".git") => key.parent().unwrap_or(&key),
        _ => &key,
    };
    Ok((
        stream,
        shown.display().to_string(),
        paths.socket().to_path_buf(),
    ))
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

/// How many of `nodes` need the operator, by [`attention_of`]. The status row's count, and the
/// native status line's over a subtree, so the two cannot count by different rules.
pub fn attention_count(nodes: &[NodeSummary]) -> usize {
    nodes.iter().filter(|n| attention_of(n).is_some()).count()
}

/// Fold one subscription notification into a `tree/subscribe` snapshot. Returns whether `nodes`
/// changed.
///
/// **Unknown events are ignored and known ones are never inferred**: a `node/state` for a node the
/// snapshot has not been told about is dropped rather than used to invent a summary, because a
/// summary invented here would have a fabricated harness and would then be greyed from the wrong
/// row of doctor's table. Shared by the tree screen and the native relay's status line, so the
/// two never disagree about what a snapshot plus its notifications adds up to.
pub(crate) fn fold_tree_event(nodes: &mut Vec<NodeSummary>, event: &Event) -> bool {
    match event {
        Event::NodeAdded { node, .. } => {
            match nodes.iter_mut().find(|n| n.agent_id == node.agent_id) {
                Some(existing) => *existing = node.clone(),
                None => nodes.push(node.clone()),
            }
            true
        }
        Event::NodeState {
            agent_id,
            state,
            reap_state,
            ..
        } => match nodes.iter_mut().find(|n| n.agent_id == *agent_id) {
            Some(n) => {
                n.state = *state;
                n.reap_state = *reap_state;
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// **A live `tree/subscribe`**: the snapshot, kept current from the subscription's notifications.
///
/// What a screen that watches the forest needs and nothing else — the home screen's Watch tab
/// holds one. Its connection carries only the subscription; an errand (a steer, a kill, a
/// `node/get`) dials its own through [`crate::courier`], so an answer never has to be told apart
/// from the notifications around it.
pub struct Subscription {
    lines: BufReader<UnixStream>,
    /// The forest as of the last notification folded.
    pub nodes: Vec<NodeSummary>,
    /// The project as the operator recognises it: the worktree, not its `.git`.
    pub shown: String,
    /// §2's socket, for the errands.
    pub socket: PathBuf,
}

impl Subscription {
    /// Dial this project's supervisor and subscribe. Refuses, as `marion tree` does, when nobody is
    /// serving: a supervisor started here would have an empty forest to show.
    pub fn open(repo: &Path, state_dir: &Path) -> Result<Subscription, Refusal> {
        let (mut stream, shown, socket) = dial(repo, state_dir)?;
        let frame = Frame::Request(marion_core::proto::Request::new(
            RequestId::Number(1),
            Call::TreeSubscribe(marion_core::proto::params::TreeSubscribeParams {}),
        ));
        stream
            .write_all(frame.to_line().as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|e| format!("sending tree/subscribe: {e}"))?;
        let mut sub = Subscription {
            lines: BufReader::new(stream),
            nodes: Vec::new(),
            shown,
            socket,
        };
        let mut early = Vec::new();
        let response = loop {
            match sub.frame()? {
                Some(Frame::Response(r)) => break r,
                Some(Frame::Notification(n)) => early.push(n.event),
                Some(_) | None => continue,
            }
        };
        let body = match response.outcome {
            marion_core::proto::Outcome::Result(b) => b,
            marion_core::proto::Outcome::Error(e) => {
                return Err(format!(
                    "the supervisor refused tree/subscribe: {}",
                    e.message
                ));
            }
        };
        let MethodResult::TreeSubscribe(snapshot) = marion_core::proto::Method::TreeSubscribe
            .decode_result(&body)
            .map_err(|e| format!("the supervisor's tree/subscribe answer did not decode: {e}"))?
        else {
            return Err(
                "the supervisor answered tree/subscribe with another method's result".into(),
            );
        };
        sub.nodes = snapshot.nodes;
        for event in &early {
            fold_tree_event(&mut sub.nodes, event);
        }
        Ok(sub)
    }

    /// Wait up to the socket's read bound for one notification and fold it. `Ok(Some(event))` when
    /// it changed the forest, `Ok(None)` when nothing arrived or nothing changed, and `Err` when
    /// the supervisor went away.
    pub fn poll(&mut self) -> Result<Option<Event>, Refusal> {
        match self.frame()? {
            Some(Frame::Notification(n)) if fold_tree_event(&mut self.nodes, &n.event) => {
                Ok(Some(n.event))
            }
            _ => Ok(None),
        }
    }

    fn frame(&mut self) -> Result<Option<Frame>, Refusal> {
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

    /// The snapshot is kept current by folding, and the fold is shared with the native relay's
    /// status line, so its rules are asserted once here: a known node is updated in place, an
    /// unknown node's state is dropped rather than invented, and only a change reports `true`.
    #[test]
    fn fold_tree_event_updates_known_nodes_and_never_invents_one() {
        let mut nodes = vec![summary("a", Harness::Codex, false, None)];
        let ts = marion_core::encoding::SystemTime::from_unix_millis(0);
        let a_exits = Event::NodeState {
            agent_id: AgentId("a".into()),
            state: NodeState::Exited(marion_core::contract::ExitStatus::Ok),
            reap_state: ReapState::Live,
            ts,
        };
        assert!(fold_tree_event(&mut nodes, &a_exits));
        assert!(nodes[0].state.is_exited());

        let stranger = Event::NodeState {
            agent_id: AgentId("nobody".into()),
            state: NodeState::Running,
            reap_state: ReapState::Live,
            ts,
        };
        assert!(!fold_tree_event(&mut nodes, &stranger));
        assert_eq!(
            nodes.len(),
            1,
            "a state for an unknown node invented a summary"
        );

        let added = Event::NodeAdded {
            node: summary("b", Harness::Codex, false, None),
            ts,
        };
        assert!(fold_tree_event(&mut nodes, &added));
        assert_eq!(nodes.len(), 2);
        assert!(
            fold_tree_event(&mut nodes, &added),
            "a re-add replaces in place"
        );
        assert_eq!(nodes.len(), 2);

        let unrelated = Event::SupervisorExiting { ts, held_by: None };
        assert!(!fold_tree_event(&mut nodes, &unrelated));
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

    /// The refusal an operator meets first is one line: the fact and the command. The paragraph
    /// it replaces explained why marion would not start a supervisor here, which is `run`'s doc
    /// comment's job and not stderr's.
    #[test]
    fn the_no_supervisor_refusal_is_one_sentence_and_the_command_to_run() {
        let dir = std::env::temp_dir().join(format!(
            "marion-tree-refusal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let state = dir.join("state");
        let refusal = Subscription::open(&dir, &state)
            .err()
            .expect("nobody is serving a fresh directory");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(refusal.lines().count(), 1, "{refusal}");
        assert!(refusal.contains("no supervisor is serving"), "{refusal}");
        // The state dir it looked in, because a supervisor started under another one (a different
        // `$MARION_STATE_DIR`) is the likeliest reason nothing answers.
        assert!(refusal.contains(&state.display().to_string()), "{refusal}");
        // Any run starts a supervisor; `--pane` is not what makes one appear.
        assert!(!refusal.contains("--pane"), "{refusal}");
        assert!(refusal.contains("`marion "), "{refusal}");
        assert!(
            refusal.len() < 2 * state.display().to_string().len() + 170,
            "{} chars is a paragraph, not a hint: {refusal}",
            refusal.len()
        );
    }

    /// **A one-shot snapshot refuses exactly as the home screen's subscription does** when nobody
    /// is serving: it dials the same way and, like the screen, starts no supervisor — an empty forest from a supervisor
    /// started here would read as "nothing needs attention" rather than "wrong project".
    #[test]
    fn a_snapshot_with_no_supervisor_refuses_with_the_screens_sentence() {
        let dir = std::env::temp_dir().join(format!(
            "marion-snapshot-refusal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let from_snapshot = snapshot(&dir, &dir).expect_err("nobody is serving a fresh directory");
        let from_screen = Subscription::open(&dir, &dir)
            .err()
            .expect("nobody is serving a fresh directory");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(from_snapshot, from_screen);
        assert!(
            crate::socket::nobody_is_serving(&crate::socket::socket_paths(
                &dir,
                &crate::socket::project_root(&dir),
                crate::socket::own_uid()
            )),
            "a snapshot must not have started a supervisor"
        );
    }

    /// **What `marion steer <id|short-id>` addresses**: a whole id as itself, a short id as the one
    /// node that carries it, an ambiguous short id refused with every candidate named, and anything
    /// else passed through untouched so the supervisor's own `not found` sentence answers it.
    #[test]
    fn a_steer_target_is_a_whole_id_or_one_nodes_short_id() {
        let a = "01a091ba-8ea3-7000-8000-000000000001";
        let b = "01a091ba-5b04-7000-8000-000000000002";
        let c = "01a091bb-5b04-7000-8000-000000000003";
        let nodes: Vec<_> = [a, b, c]
            .iter()
            .map(|id| summary(id, Harness::Codex, false, None))
            .collect();
        assert_eq!(resolve_target(a, &nodes), Ok(AgentId(a.into())));
        assert_eq!(resolve_target("8ea3", &nodes), Ok(AgentId(a.into())));
        let ambiguous = resolve_target("5b04", &nodes).expect_err("two nodes carry 5b04");
        assert!(
            ambiguous.contains(b) && ambiguous.contains(c),
            "{ambiguous}"
        );
        assert_eq!(
            resolve_target("nobody", &nodes),
            Ok(AgentId("nobody".into()))
        );
        assert_eq!(
            resolve_target("root-claude", &[]),
            Ok(AgentId("root-claude".into()))
        );
    }

    /// **`marion list`'s line: glyph, state, type, the whole id, and the parent's short id.** The
    /// whole id because a list is what an operator copies `marion attach` from; the parent short,
    /// because it is only there to place the node, and the tree's own short id is how it is named
    /// everywhere else. A root has no parent clause at all rather than an empty one.
    #[test]
    fn a_list_line_is_glyph_state_type_id_and_parent() {
        use marion_core::contract::ExitStatus;
        let root = summary(
            "01a091ba-8ea3-7000-8000-000000000001",
            Harness::Codex,
            false,
            None,
        );
        assert_eq!(
            list_line(&root),
            "● idle codex-impl 01a091ba-8ea3-7000-8000-000000000001"
        );
        let child = NodeSummary {
            parent_id: Some(root.agent_id.clone()),
            state: NodeState::Exited(ExitStatus::Failed),
            ..summary("kid", Harness::Codex, false, None)
        };
        assert_eq!(
            list_line(&child),
            "✗ exited:failed codex-impl kid parent 8ea3"
        );
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

    /// **The tone is decided here, with the label, from the same two fields.** A failed exit, a
    /// timeout and a kill are all trouble; a clean exit is done; a cancel, an unreported exit,
    /// an orphan and a reap are things marion does not claim to know the outcome of; anything
    /// blocked is waiting on someone; and everything else is alive.
    #[test]
    fn the_tone_partitions_every_state_the_label_can_say() {
        use marion_core::contract::ExitStatus;
        use marion_core::node::BlockReason;
        use marion_tui::tree::Tone;
        let live = ReapState::Live;
        for s in [
            NodeState::Spawning,
            NodeState::Ready,
            NodeState::Running,
            NodeState::Idle,
        ] {
            assert_eq!(tone_of(s, live), Tone::Live, "{s:?}");
        }
        for r in [
            BlockReason::Descendants,
            BlockReason::Permission,
            BlockReason::Elicitation,
        ] {
            assert_eq!(tone_of(NodeState::Blocked(r), live), Tone::Blocked, "{r:?}");
        }
        assert_eq!(tone_of(NodeState::Exited(ExitStatus::Ok), live), Tone::Done);
        for e in [ExitStatus::Failed, ExitStatus::TimedOut, ExitStatus::Killed] {
            assert_eq!(tone_of(NodeState::Exited(e), live), Tone::Failed, "{e:?}");
        }
        for e in [ExitStatus::Cancelled, ExitStatus::Unreported] {
            assert_eq!(tone_of(NodeState::Exited(e), live), Tone::Unknown, "{e:?}");
        }
        assert_eq!(
            tone_of(NodeState::Running, ReapState::Orphaned),
            Tone::Unknown,
            "§7.2: orphaned claims nothing about the process, so it is neither live nor failed"
        );
        assert_eq!(
            tone_of(NodeState::Idle, ReapState::ReapedIdle),
            Tone::Unknown
        );
        assert_eq!(
            tone_of(NodeState::Exited(ExitStatus::Failed), ReapState::Orphaned),
            Tone::Failed,
            "the exit is the more specific fact, for the tone as for the label"
        );
        let n = summary("x", Harness::Codex, false, None);
        assert_eq!(row(&n).tone, Tone::Live, "the row carries the tone");
    }
    /// **Which nodes need an operator, over every `(NodeState, ReapState)` pair there is.**
    ///
    /// Each class the attention queue names is here by literal, with the word it is reported by:
    /// a node waiting on somebody, an exit that went wrong or went unreported, and an orphan. A
    /// clean exit, a cancel (a decision somebody already made), a reap and every live state are
    /// not. The precedence is the label's — an exit outranks a reap state, which outranks the live
    /// state — so an orphan that exited cleanly is done, and a reaped node that was blocked is not
    /// still blocked. Every pair is listed, so a new state is a compile-visible hole in this table
    /// rather than a silent `None`.
    #[test]
    fn attention_is_blocked_failed_unreported_or_orphaned_and_nothing_else() {
        use NodeState::{Blocked, Exited, Idle, Ready, Running, Spawning};
        use ReapState::{Live, Orphaned, ReapedIdle};
        use marion_core::contract::ExitStatus::*;
        use marion_core::node::BlockReason::*;
        let table: &[(NodeState, ReapState, Option<&str>)] = &[
            (Spawning, Live, None),
            (Ready, Live, None),
            (Running, Live, None),
            (Idle, Live, None),
            (Blocked(Permission), Live, Some("blocked:permission")),
            (Blocked(Elicitation), Live, Some("blocked:elicitation")),
            (Blocked(Descendants), Live, Some("blocked:descendants")),
            (Exited(Ok), Live, None),
            (Exited(Cancelled), Live, None),
            (Exited(Failed), Live, Some("exited:failed")),
            (Exited(TimedOut), Live, Some("exited:timedout")),
            (Exited(Killed), Live, Some("exited:killed")),
            (Exited(Unreported), Live, Some("exited:unreported")),
            (Spawning, Orphaned, Some("orphaned")),
            (Ready, Orphaned, Some("orphaned")),
            (Running, Orphaned, Some("orphaned")),
            (Idle, Orphaned, Some("orphaned")),
            (Blocked(Permission), Orphaned, Some("orphaned")),
            (Blocked(Elicitation), Orphaned, Some("orphaned")),
            (Blocked(Descendants), Orphaned, Some("orphaned")),
            (Exited(Ok), Orphaned, None),
            (Exited(Cancelled), Orphaned, None),
            (Exited(Failed), Orphaned, Some("exited:failed")),
            (Exited(TimedOut), Orphaned, Some("exited:timedout")),
            (Exited(Killed), Orphaned, Some("exited:killed")),
            (Exited(Unreported), Orphaned, Some("exited:unreported")),
            (Spawning, ReapedIdle, None),
            (Ready, ReapedIdle, None),
            (Running, ReapedIdle, None),
            (Idle, ReapedIdle, None),
            (Blocked(Permission), ReapedIdle, None),
            (Blocked(Elicitation), ReapedIdle, None),
            (Blocked(Descendants), ReapedIdle, None),
            (Exited(Ok), ReapedIdle, None),
            (Exited(Cancelled), ReapedIdle, None),
            (Exited(Failed), ReapedIdle, Some("exited:failed")),
            (Exited(TimedOut), ReapedIdle, Some("exited:timedout")),
            (Exited(Killed), ReapedIdle, Some("exited:killed")),
            (Exited(Unreported), ReapedIdle, Some("exited:unreported")),
        ];
        assert_eq!(table.len(), 13 * 3, "every state under every reap state");
        for &(state, reap, want) in table {
            let n = NodeSummary {
                state,
                reap_state: reap,
                ..summary("x", Harness::Codex, false, None)
            };
            assert_eq!(attention_of(&n).as_deref(), want, "({state:?}, {reap:?})");
            if let Some(word) = want {
                assert_eq!(
                    word,
                    state_label(state, reap),
                    "the reason is the row's own state word, so the queue and the tree agree"
                );
            }
        }
    }
}
