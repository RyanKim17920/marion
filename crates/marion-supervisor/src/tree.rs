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
        // What the operator is asked to do comes first; otherwise the conservative key is the
        // right answer for an unread version (§3.3), and the strip should say that is why, rather
        // than let ten greyed words read as "this harness cannot".
        note: node.attention.clone().or_else(|| {
            node.harness_version
                .is_none()
                .then(|| "harness version unknown".to_string())
        }),
    }
}

/// **What one node is called, in the one place that decides it.**
///
/// The tree row and the detail pane's title are the same string by construction rather than by
/// agreement: two spellings of a node's name is how an operator ends up unsure whether the `8ea3`
/// in the sidebar and the `01a091ba-8ea3-…` in the pane are the same agent.
/// An endpoint node's `provider:model` follows, so which model a node runs on is on its row.
pub fn label_of(node: &NodeSummary) -> String {
    let name = node
        .name
        .clone()
        .unwrap_or_else(|| format!("{} {}", node.agent_type, short_id(&node.agent_id.0)));
    let label = match &node.endpoint {
        Some(e) => format!("{name} {}", e.label()),
        None => name,
    };
    let label = match review_note(node).or_else(|| race_note(node)) {
        Some(note) => format!("{label} · {note}"),
        None => label,
    };
    let label = match workflow_note(node) {
        Some(note) => format!("{label} · {note}"),
        None => label,
    };
    match widened_note(node) {
        Some(note) => format!("{label} · {note}"),
        None => label,
    }
}

/// **What a seat row adds**: its seat, and once the race is decided, how it came out. `None` for a
/// node in no race. The tree and Home Watch both say it through this.
pub fn race_note(node: &NodeSummary) -> Option<String> {
    use marion_core::race::SeatVerdict;
    let badge = node.race.as_ref()?;
    let seat = format!("agent {}", badge.seat);
    Some(match badge.verdict {
        None => seat,
        Some(v) => {
            let word = match v {
                SeatVerdict::Won => "★ won",
                SeatVerdict::Lost => "lost",
                SeatVerdict::Failed => "failed",
                SeatVerdict::Cancelled => "cancelled",
                SeatVerdict::Unlaunched => "not started",
            };
            format!("{seat} {word}")
        }
    })
}

/// **What a workflow step's row adds**: its step, and the review round past the first. `None` for a
/// node in no workflow run. The tree and Home Watch both say it through this.
pub fn workflow_note(node: &NodeSummary) -> Option<String> {
    let badge = node.workflow.as_ref()?;
    let step = if badge.step_id.is_empty() {
        format!("step {}", u16::from(badge.step) + 1)
    } else {
        format!("step {}", badge.step_id)
    };
    Some(match badge.round {
        0 => step,
        r => format!("{step} r{}", u16::from(r) + 1),
    })
}

/// The tree id of a workflow run's header row. Not an agent id, like a race's
/// ([`race_row_id`]).
pub fn workflow_row_id(wf_id: &marion_core::workflow::WorkflowId) -> String {
    format!("workflow:{}", wf_id.0)
}

/// The workflow run a tree row heads, if it is a run's header row.
pub fn workflow_of_row(id: &str) -> Option<&str> {
    id.strip_prefix("workflow:")
}

/// **A workflow run, summed from its step nodes** — the header row's facts, as a race's are from
/// its seats: the run's name and step count, the step it is on (and the review round), which node
/// that is, its tokens against its budget, its clock, and how it closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowSummary {
    pub name: String,
    pub steps: u8,
    /// `(step, round, step id)` of the latest step any node ran.
    pub current: Option<(u8, u8, String)>,
    /// The node the run is on: the latest step's running node, else its last.
    pub current_node: Option<String>,
    pub tokens: Option<u64>,
    pub budget: Option<u64>,
    /// From the first step's start to now, or to the last end once closed.
    pub elapsed_secs: Option<u64>,
    pub closed: Option<marion_core::workflow::Outcome>,
}

impl WorkflowSummary {
    pub fn of(
        wf_id: &marion_core::workflow::WorkflowId,
        nodes: &[NodeSummary],
        now: std::time::SystemTime,
    ) -> WorkflowSummary {
        let steps: Vec<(&NodeSummary, &marion_core::proto::model::WorkflowBadge)> = nodes
            .iter()
            .filter_map(|n| Some((n, n.workflow.as_ref().filter(|b| &b.wf_id == wf_id)?)))
            .collect();
        let latest = steps.iter().map(|(_, b)| (b.step, b.round)).max();
        let at_latest = || {
            steps
                .iter()
                .filter(move |(_, b)| Some((b.step, b.round)) == latest)
        };
        let current_node = at_latest()
            .find(|(n, _)| !n.state.is_exited())
            .or_else(|| at_latest().next_back())
            .map(|(n, _)| n.agent_id.0.clone());
        let first = steps.first().map(|(_, b)| *b);
        let closed = steps.iter().find_map(|(_, b)| b.closed);
        let started = steps
            .iter()
            .filter_map(|(n, _)| n.started_at)
            .map(|t| t.0)
            .min();
        let until = if closed.is_some() {
            steps
                .iter()
                .filter_map(|(n, _)| n.ended_at)
                .map(|t| t.0)
                .max()
        } else {
            Some(now)
        };
        WorkflowSummary {
            name: first.map(|b| b.name.clone()).unwrap_or_default(),
            steps: first.map_or(0, |b| b.steps),
            current: at_latest()
                .next()
                .map(|(_, b)| (b.step, b.round, b.step_id.clone())),
            current_node,
            tokens: steps
                .iter()
                .filter_map(|(n, _)| n.tokens)
                .fold(None, |sum: Option<u64>, t| Some(sum.unwrap_or(0) + t)),
            budget: first.and_then(|b| b.budget_tokens),
            elapsed_secs: started
                .zip(until)
                .and_then(|(s, u)| u.duration_since(s).ok())
                .map(|d| d.as_secs()),
            closed,
        }
    }

    /// The header row's label: `workflow ship · 3/4 gate r2 · Σ1.4M/2M · 12m04s`, or how it closed
    /// in place of the step once it has.
    pub fn label(&self) -> String {
        use marion_tui::home::text::{elapsed, tokens};
        let mut parts = vec![format!("workflow {}", self.name)];
        match (self.closed, &self.current) {
            (Some(outcome), _) => parts.push(outcome.word().to_string()),
            (None, Some((step, round, id))) => {
                let mut at = format!("{}/{} {id}", u16::from(*step) + 1, self.steps);
                if *round > 0 {
                    at.push_str(&format!(" r{}", u16::from(*round) + 1));
                }
                parts.push(at);
            }
            (None, None) => parts.push("starting".into()),
        }
        match (self.tokens, self.budget) {
            (t, Some(b)) => parts.push(format!("Σ{}/{}", tokens(t.unwrap_or(0)), tokens(b))),
            (Some(t), None) => parts.push(format!("Σ{}", tokens(t))),
            (None, None) => {}
        }
        if let Some(secs) = self.elapsed_secs {
            parts.push(elapsed(secs));
        }
        parts.join(" · ")
    }

    pub fn tone(&self) -> Tone {
        match self.closed {
            Some(marion_core::workflow::Outcome::Succeeded) => Tone::Done,
            Some(_) => Tone::Failed,
            None => Tone::Live,
        }
    }
}

/// The tree id of a race's header row. Not an agent id: nothing is launched or attached by it, and
/// [`race_of_row`] is how a view tells the two apart.
pub fn race_row_id(race_id: &marion_core::race::RaceId) -> String {
    format!("race:{}", race_id.0)
}

/// The race a tree row heads, if it is a race's header row.
pub fn race_of_row(id: &str) -> Option<&str> {
    id.strip_prefix("race:")
}

/// **A race, summed from its seats** — the header row's facts. A race has no node of its own, so
/// everything here is read off the seats' summaries: how many, how many ended, the verdict once
/// decided, and the tokens they have spent so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaceSummary {
    pub race_id: marion_core::race::RaceId,
    pub seats: usize,
    pub ended: usize,
    /// The winning seat's label, or `Some(None)` once decided with no winner.
    pub decided: Option<Option<String>>,
    pub tokens: Option<u64>,
}

impl RaceSummary {
    pub fn of(race_id: &marion_core::race::RaceId, nodes: &[NodeSummary]) -> RaceSummary {
        use marion_core::race::SeatVerdict;
        let seats: Vec<&NodeSummary> = nodes
            .iter()
            .filter(|n| n.race.as_ref().is_some_and(|b| &b.race_id == race_id))
            .collect();
        let decided = seats
            .iter()
            .any(|n| n.race.as_ref().is_some_and(|b| b.verdict.is_some()))
            .then(|| {
                seats
                    .iter()
                    .find(|n| {
                        n.race
                            .as_ref()
                            .is_some_and(|b| b.verdict == Some(SeatVerdict::Won))
                    })
                    .map(|n| {
                        let seat = n.race.as_ref().map_or(0, |b| b.seat);
                        format!("#{seat} {}", n.agent_type)
                    })
            });
        let tokens = seats
            .iter()
            .filter_map(|n| n.tokens)
            .fold(None, |sum: Option<u64>, t| Some(sum.unwrap_or(0) + t));
        RaceSummary {
            race_id: race_id.clone(),
            seats: seats.len(),
            ended: seats.iter().filter(|n| n.state.is_exited()).count(),
            decided,
            tokens,
        }
    }

    /// The header row's label: the race, its seat count, and how far it has got.
    pub fn label(&self) -> String {
        let head = format!("race {} · {} agents", short_id(&self.race_id.0), self.seats);
        match &self.decided {
            Some(Some(winner)) => format!("{head} · {winner} won"),
            Some(None) => format!("{head} · no winner"),
            None => format!("{head} · {} of {} done", self.ended, self.seats),
        }
    }

    pub fn tone(&self) -> Tone {
        match &self.decided {
            Some(Some(_)) => Tone::Done,
            Some(None) => Tone::Failed,
            None => Tone::Live,
        }
    }
}

/// **What a node's row says when the operator's opt-in let it start with more than its caller**:
/// "uncontained child (operator opt-in)" when its caller ran sandboxed and it does not, else the
/// axes it widened. `None` for every node within its caller's authority.
pub fn widened_note(node: &NodeSummary) -> Option<String> {
    match node.widened.as_slice() {
        [] => None,
        [one] if one == "containment" => Some("uncontained child (operator opt-in)".into()),
        axes => Some(format!(
            "wider than its parent: {} (operator opt-in)",
            axes.join(", ")
        )),
    }
}

/// **What a reviewer row adds**: that it is a review, and once its report is read, how many
/// findings it made and how many block. `None` for every node that is not a reviewer. The tree
/// and Home Watch both say it through this, so the two cannot count differently.
pub fn review_note(node: &NodeSummary) -> Option<String> {
    node.review_of.as_ref()?;
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    Some(match node.review {
        Some(t) if t.blocking > 0 => format!(
            "review: {}, {} blocking",
            plural(t.findings, "finding"),
            t.blocking
        ),
        Some(t) => format!("review: {}", plural(t.findings, "finding")),
        None if node.state.is_exited() => "review: no report read".to_string(),
        None => "reviewing".to_string(),
    })
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

/// **The agent an operator means by `arg`**, out of the ids they could have meant — the one rule
/// every command that takes an agent uses (`ls`, `attach`, `steer`, `cancel`, `resume`, and the
/// MCP tools' `id`).
///
/// A whole id is itself. Otherwise the one id whose [`short_id`] is `arg` — what the tree row
/// shows, so an operator can type what they see — or which starts with `arg`, so a copied prefix
/// works too. More than one candidate is refused with every whole id named, because guessing
/// between them would act on an agent nobody chose. An arg that names nothing is returned as-is:
/// the supervisor's own `not found` sentence is the better answer, and a second one here would be a
/// second spelling of it.
pub fn resolve_node<'a>(
    arg: &str,
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<AgentId, String> {
    let ids: Vec<&str> = ids.into_iter().collect();
    if arg.is_empty() || ids.contains(&arg) {
        return Ok(AgentId(arg.to_string()));
    }
    let matches: Vec<&str> = ids
        .into_iter()
        .filter(|id| short_id(id) == arg || id.starts_with(arg))
        .collect();
    match matches.as_slice() {
        [] => Ok(AgentId(arg.to_string())),
        [one] => Ok(AgentId((*one).to_string())),
        many => Err(format!(
            "`{arg}` matches {} agents ({}); name the one you mean by more of its id",
            many.len(),
            many.join(", ")
        )),
    }
}

/// [`resolve_node`] over one `tree/subscribe` snapshot.
pub fn resolve_target(arg: &str, nodes: &[NodeSummary]) -> Result<AgentId, String> {
    resolve_node(arg, nodes.iter().map(|n| n.agent_id.0.as_str()))
}

/// A node's state in one short word.
///
/// `reap_state` is folded in rather than shown beside it, and only when it is not `Live`: §7.2 is
/// emphatic that `Orphaned` is *not* a claim the process died, so it is shown as its own word
/// instead of being collapsed into an exit. A node that is both `Exited` and reaped shows the exit,
/// which is the more specific fact.
pub fn state_label(state: NodeState, reap: ReapState) -> String {
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
/// Not a clean exit, not a cancel (a decision somebody already made) — **except a cancel whose
/// abort was ignored**, `abort ignored`: the node's row had an abort, and marion still had to kill
/// it — not a reap, not a live state. The precedence is [`state_label`]'s: an exit outranks a reap
/// state, which outranks the live state. Decided from the summary's state fields and its cancel
/// only — never from a pane's screen — with the supervisor's [`NodeSummary::attention`] after the
/// word where it says what to do.
pub fn attention_of(node: &NodeSummary) -> Option<String> {
    use marion_core::contract::ExitStatus;
    let (state, reap) = (node.state, node.reap_state);
    // A cancel is a decision, but a node that had an abort and ignored it — marion had to kill
    // it — is worth a look: its harness did not stop when asked.
    if matches!(state, NodeState::Exited(ExitStatus::Cancelled))
        && node
            .cancel
            .as_ref()
            .is_some_and(|c| c.forced && c.had_abort)
    {
        return Some("abort ignored".into());
    }
    let needs = match (state, reap) {
        (NodeState::Exited(ExitStatus::Ok | ExitStatus::Cancelled), _) => false,
        (NodeState::Exited(_), _) => true,
        (_, ReapState::Orphaned) => true,
        (_, ReapState::ReapedIdle) => false,
        (NodeState::Blocked(_), ReapState::Live) => true,
        (_, ReapState::Live) => false,
    };
    let label = state_label(state, reap);
    needs.then(|| match &node.attention {
        Some(what) => format!("{label}: {what}"),
        None => label,
    })
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
            "{} {} is headless; Enter opens pane agents only.",
            node.agent_type,
            short_id(&node.agent_id.0)
        ))
    }
}

/// Build the flattened tree from a snapshot, preserving the selection where the node survives.
pub fn build(nodes: &[NodeSummary], keep: Option<&str>) -> Tree {
    let mut t = Tree::new(with_race_rows(nodes));
    if let Some(id) = keep {
        t.select(id);
    }
    t
}

/// Every node's row, with each race's seats gathered under one header row placed where its seats'
/// shared parent put them, and each workflow run's step nodes — a race step's header included —
/// under one header row of the run's. Neither has a node of its own, so each header is made here,
/// from the nodes under it, and carries no actions.
fn with_race_rows(nodes: &[NodeSummary]) -> Vec<tree::Node> {
    let mut rows: Vec<tree::Node> = Vec::with_capacity(nodes.len());
    let mut races: Vec<marion_core::race::RaceId> = Vec::new();
    let mut runs: Vec<marion_core::workflow::WorkflowId> = Vec::new();
    let now = std::time::SystemTime::now();
    for n in nodes {
        let mut r = row(n);
        if let Some(wf) = &n.workflow {
            if !runs.contains(&wf.wf_id) {
                runs.push(wf.wf_id.clone());
                let run = WorkflowSummary::of(&wf.wf_id, nodes, now);
                rows.push(tree::Node {
                    id: workflow_row_id(&wf.wf_id),
                    parent: r.parent.clone(),
                    label: run.label(),
                    state: match run.closed {
                        Some(outcome) => outcome.word().into(),
                        None => "running".into(),
                    },
                    tone: run.tone(),
                    actions: Vec::new(),
                    note: None,
                });
            }
            // A step node is the operator's own; a race step's seats go under the race's header,
            // which goes under the run's.
            if r.parent.is_none() {
                r.parent = Some(workflow_row_id(&wf.wf_id));
            }
        }
        if let Some(badge) = &n.race {
            if !races.contains(&badge.race_id) {
                races.push(badge.race_id.clone());
                let race = RaceSummary::of(&badge.race_id, nodes);
                rows.push(tree::Node {
                    id: race_row_id(&badge.race_id),
                    parent: r.parent.clone(),
                    label: race.label(),
                    state: if race.decided.is_some() {
                        "decided".into()
                    } else {
                        "racing".into()
                    },
                    tone: race.tone(),
                    actions: Vec::new(),
                    note: None,
                });
            }
            r.parent = Some(race_row_id(&badge.race_id));
        }
        rows.push(r);
    }
    rows
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
    if let Some(what) = attention_of(node).and(node.attention.as_deref()) {
        line.push_str(" — ");
        line.push_str(what);
    }
    line
}

/// **A subtree in the words `marion ls` prints**: `Σ1.3M 14f 12m04s 6 nodes (2 running)` — tokens
/// where some node reported them, files its contracts changed, wall clock, and nodes.
pub fn subtree_words(t: &crate::rollup::Totals) -> String {
    use marion_tui::home::text::{elapsed, tokens};
    let mut words = Vec::new();
    if t.claimed > 0 {
        words.push(format!("Σ{}", tokens(t.tokens)));
    }
    words.push(format!("{}f", t.changed));
    if let Some(wall) = t.wall_to(std::time::SystemTime::now()) {
        words.push(elapsed(wall.as_secs()));
    }
    words.push(format!("{} nodes ({} running)", t.nodes, t.live));
    words.join(" ")
}

/// Dial the supervisor for `repo`, **refusing to start one** (see [`run`]), and say how the status
/// row should name the project and where a steer dials.
fn dial(key: &Path, state_dir: &Path) -> Result<(UnixStream, String, PathBuf), Refusal> {
    let key = key.to_path_buf();
    let paths = crate::socket::socket_paths(state_dir, &key, crate::socket::own_uid());
    if crate::socket::nobody_is_serving(&paths) {
        // One line. Why marion will not start a supervisor here is `run`'s doc comment.
        return Err(nobody_serving(&key, state_dir));
    }
    // No read bound: the subscribe's own reads block until the answer, and a screen that follows
    // the forest waits on [`Subscription::fd`] in its own `poll(2)` with the socket non-blocking.
    let stream = crate::client_auth::dial(&paths)
        .map_err(|e| format!("dialling the supervisor for `{}`: {e}", key.display()))?;
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

/// The refusal for a project no supervisor serves: one line naming the project key and the state
/// dir, and how to start a session there. [`dial`]'s, and [`JournalView::open`]'s for a project
/// whose journal records nothing, so the two cannot drift into two wordings of one fact.
fn nobody_serving(key: &Path, state_dir: &Path) -> Refusal {
    format!(
        "no supervisor is serving `{}` under state dir `{}`; start a session here first \
         (`marion <harness>` or `marion run`) with the same --state-dir / $MARION_STATE_DIR",
        key.display(),
        state_dir.display()
    )
}

/// **The forest as the journal left it**, for a line-oriented verb asked about a project nobody
/// serves any more — every node finished and the supervisor gone.
///
/// The journal is the record the supervisor itself folds (§4.3), so reading it here is the same
/// fold ([`crate::journal::read`]), projected by the same [`crate::handler::summarize`] and detailed
/// by the same [`crate::node_detail::read`] a `tree/subscribe` and a `node/get` answer from. What
/// only a live supervisor knows is absent rather than guessed: no node has a pane, and a spend is
/// the journal's record with no run in progress on top.
///
/// **Strictly a reader.** It starts no supervisor, takes no lock beyond the instant's probe
/// [`crate::socket::nobody_is_serving`] already makes, appends nothing and creates no directory: a
/// look at a finished project must leave it exactly as it was, or looking would change what the
/// next supervisor replays.
pub struct JournalView {
    project: marion_core::paths::ProjectDir,
    replay: marion_core::registry::Replay,
}

impl JournalView {
    /// The journal of the project `repo` names under `state_dir` — **only when nobody is serving
    /// it**. `Ok(None)` is a supervisor serving: its answer is the live one, and the caller dials
    /// it. A project whose journal records no node refuses as [`dial`] does, because an empty
    /// listing would read as "nothing ran here" when the likelier fact is a different
    /// `--state-dir`.
    pub fn open(repo: &Path, state_dir: &Path) -> Result<Option<JournalView>, Refusal> {
        let key = crate::socket::project_root(repo);
        let paths = crate::socket::socket_paths(state_dir, &key, crate::socket::own_uid());
        if !crate::socket::nobody_is_serving(&paths) {
            return Ok(None);
        }
        let project = marion_core::paths::ProjectDir::new(state_dir, &key);
        let replay = crate::journal::read(&project).map_err(|e| {
            format!(
                "reading the journal at `{}`: {e}",
                project.journal().display()
            )
        })?;
        if replay.nodes().is_empty() {
            return Err(nobody_serving(&key, state_dir));
        }
        Ok(Some(JournalView { project, replay }))
    }

    /// Every node the journal describes, in journal order — what `tree/subscribe`'s snapshot would
    /// have said. A node the supervisor could not project is left out here as it is there.
    pub fn nodes(&self) -> Vec<NodeSummary> {
        let nothing_live = crate::spending::Spending::default();
        self.replay
            .nodes()
            .iter()
            .filter_map(|n| crate::handler::summarize_spent(n, false, &nothing_live).ok())
            .map(stopped_with_its_supervisor)
            .collect()
    }

    /// One node and its detail, by whole or short id ([`resolve_target`]) — what `node/get` with
    /// the activity's tail would have answered.
    pub fn node(
        &self,
        target: &str,
    ) -> Result<(NodeSummary, marion_core::proto::result::NodeDetail), Refusal> {
        let id = resolve_target(target, &self.nodes())?;
        let replayed = self
            .replay
            .get(&id)
            .ok_or_else(|| format!("no node `{target}` in this project's journal"))?;
        let mut node =
            crate::handler::summarize(replayed, false).map_err(|e| e.as_error(&id).message)?;
        let detail = match crate::node_detail::inputs(replayed) {
            Some(i) => crate::node_detail::read(
                &self.project,
                &id,
                &i,
                Some(marion_core::proto::params::ActivityCursor::Tail),
                None,
            ),
            None => Default::default(),
        };
        node.tokens = detail.usage.map(|u| u.total());
        Ok((stopped_with_its_supervisor(node), detail))
    }
}

/// **A node the journal leaves live, read while nobody serves the project, stopped with its
/// supervisor.** No process is owned for it any more, so it reads as §7.2's orphan — marion has no
/// record of deciding its fate — with what happened and the one command that brings it back:
/// `stopped (supervisor gone) — marion resume <id>`. A live-looking `running` line here would
/// describe a node nothing is running.
fn stopped_with_its_supervisor(mut node: NodeSummary) -> NodeSummary {
    if !node.state.is_exited() && node.reap_state == ReapState::Live {
        node.reap_state = ReapState::Orphaned;
        node.attention = Some(format!(
            "stopped (supervisor gone) — marion resume {}",
            node.agent_id.0
        ));
    }
    node
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
                Some(existing) => *existing = (**node).clone(),
                None => nodes.push((**node).clone()),
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
    /// A frame whose end has not arrived yet: kept across reads, so a read that stops mid-line on
    /// a non-blocking socket loses nothing.
    partial: Vec<u8>,
    /// Desktop notices this connection was sent after [`Self::claim_notices`], oldest first, for
    /// the screen to ring between frames: `(title, body, ring)`.
    pub notices: Vec<(String, String, String)>,
}

impl Subscription {
    /// Dial this project's supervisor and subscribe. Refuses, as `marion tree` does, when nobody is
    /// serving: a supervisor started here would have an empty forest to show.
    pub fn open(repo: &Path, state_dir: &Path) -> Result<Subscription, Refusal> {
        Self::open_at(&crate::socket::project_root(repo), state_dir)
    }

    /// [`Self::open`] for a caller that has already resolved the project root §2 keys on
    /// (`socket::project_root`, which asks git), so a screen that dials again and again does not
    /// start a `git` process each time.
    pub fn open_at(project_root: &Path, state_dir: &Path) -> Result<Subscription, Refusal> {
        let (mut stream, shown, socket) = dial(project_root, state_dir)?;
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
            partial: Vec::new(),
            notices: Vec::new(),
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

    /// **Ask to be shown the supervisor's desktop notices** where it has no desktop notifier
    /// (`notify/claim`). The answer arrives as a frame [`Self::drain`] skips; the notices then
    /// arrive in [`Self::notices`] while this connection is first in line.
    pub fn claim_notices(&mut self) -> Result<(), Refusal> {
        let frame = Frame::Request(marion_core::proto::Request::new(
            RequestId::Number(2),
            Call::NotifyClaim(marion_core::proto::params::NotifyClaimParams {}),
        ));
        let stream = self.lines.get_mut();
        stream
            .write_all(frame.to_line().as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|e| format!("sending notify/claim: {e}"))
    }

    /// For a caller that waits on [`Self::fd`] itself: reads stop rather than block, and
    /// [`Self::drain`] folds whatever has arrived.
    pub fn nonblocking(&mut self) -> std::io::Result<()> {
        self.lines.get_ref().set_nonblocking(true)
    }

    /// The socket, for a caller's `poll(2)`. Poll it only after [`Self::drain`] has returned:
    /// until then the reader may hold whole frames the socket no longer shows as readable.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.lines.get_ref().as_fd()
    }

    /// Fold every notification that has arrived, on a [`Self::nonblocking`] subscription.
    /// `Ok(true)` when the forest changed; `Err` when the supervisor went away.
    pub fn drain(&mut self) -> Result<bool, Refusal> {
        let mut changed = false;
        loop {
            match self.frame()? {
                Some(Frame::Notification(n)) => match n.event {
                    Event::NotifyNotice { title, body, ring } => {
                        self.notices.push((title, body, ring));
                    }
                    event => changed |= fold_tree_event(&mut self.nodes, &event),
                },
                Some(_) => {}
                None => return Ok(changed),
            }
        }
    }

    fn frame(&mut self) -> Result<Option<Frame>, Refusal> {
        // Bytes, not a `String`: a read that stops inside a multi-byte character must keep the
        // half it has, and `read_line` would drop it on the way to reporting the stop.
        match self.lines.read_until(b'\n', &mut self.partial) {
            Ok(0) => Err("the supervisor closed the connection".into()),
            Ok(_) if self.partial.last() != Some(&b'\n') => Ok(None),
            Ok(_) => {
                let line = std::mem::take(&mut self.partial);
                Frame::from_line(&String::from_utf8_lossy(&line))
                    .map(Some)
                    .map_err(|e| format!("the supervisor sent a frame marion cannot read: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(format!("reading from the supervisor: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::AgentId;
    use marion_core::harness::Harness;

    /// **A notice is kept for the screen to ring, never folded into the forest**, and a claim is
    /// one `notify/claim` request line.
    #[test]
    fn a_notice_on_the_subscription_is_kept_for_the_screen_and_changes_no_node() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut sub = Subscription {
            lines: BufReader::new(a),
            nodes: Vec::new(),
            shown: String::new(),
            socket: PathBuf::new(),
            partial: Vec::new(),
            notices: Vec::new(),
        };
        sub.nonblocking().unwrap();
        sub.claim_notices().unwrap();
        let mut sent = String::new();
        b.set_nonblocking(true).unwrap();
        let _ = std::io::Read::read_to_string(&mut b, &mut sent);
        assert!(sent.contains("\"notify/claim\""), "{sent}");
        let line =
            Frame::Notification(marion_core::proto::Notification::new(Event::NotifyNotice {
                title: "marion · p".into(),
                body: "codex-impl 8ea3 (codex) failed — exited:failed".into(),
                ring: "osc9".into(),
            }))
            .to_line();
        b.write_all(line.as_bytes()).unwrap();
        assert_eq!(sub.drain(), Ok(false), "no node moved");
        assert_eq!(sub.notices.len(), 1);
        assert_eq!(sub.notices[0].2, "osc9");
    }

    /// The home screen polls the socket itself and reads it without blocking, so a frame can
    /// arrive in pieces — even split inside a multi-byte character — and must be folded whole
    /// once its end arrives, never dropped or garbled.
    /// An endpoint node's row carries its `provider:model` after its name; any other node's does not.
    #[test]
    fn an_endpoint_nodes_label_carries_its_provider_and_model() {
        let mut n = summary(
            "01a091ba-8ea3-7000-8000-000000000001",
            Harness::Codex,
            false,
            None,
        );
        let plain = label_of(&n);
        assert!(!plain.contains(':'), "{plain}");
        n.endpoint = Some(marion_core::proto::NodeEndpoint {
            provider: "openrouter".into(),
            model: Some("qwen/qwen3-coder".into()),
            route: Some("native".into()),
        });
        assert_eq!(label_of(&n), format!("{plain} openrouter:qwen/qwen3-coder"));
        assert_eq!(
            row(&n).label,
            label_of(&n),
            "the row and the pane title agree"
        );
    }

    #[test]
    fn a_nonblocking_subscription_keeps_a_frame_split_across_reads() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut sub = Subscription {
            lines: BufReader::new(a),
            nodes: Vec::new(),
            shown: String::new(),
            socket: PathBuf::new(),
            partial: Vec::new(),
            notices: Vec::new(),
        };
        sub.nonblocking().unwrap();
        assert_eq!(sub.drain(), Ok(false), "nothing yet, and no wait for it");
        let mut node = summary("019f-a", Harness::Codex, false, None);
        node.name = Some("é-worker".into());
        let line = Frame::Notification(marion_core::proto::Notification::new(Event::NodeAdded {
            node: Box::new(node),
            ts: marion_core::encoding::SystemTime(std::time::SystemTime::now()),
        }))
        .to_line();
        let bytes = line.as_bytes();
        let split = line.find('é').unwrap() + 1;
        b.write_all(&bytes[..split]).unwrap();
        assert_eq!(sub.drain(), Ok(false), "half a frame folds nothing");
        b.write_all(&bytes[split..]).unwrap();
        assert_eq!(sub.drain(), Ok(true));
        assert_eq!(sub.nodes.len(), 1);
        assert_eq!(sub.nodes[0].name.as_deref(), Some("é-worker"));
        // `shutdown`, not only a drop: on macOS a test that forks concurrently can inherit `b`
        // before its close-on-exec flag lands, and a drop would then close only this copy.
        b.shutdown(std::net::Shutdown::Both).unwrap();
        drop(b);
        assert!(sub.drain().is_err(), "a closed supervisor is an error");
    }

    fn summary(id: &str, harness: Harness, pane: bool, version: Option<&str>) -> NodeSummary {
        NodeSummary {
            widened: vec![],
            budget: None,
            changed: None,
            review_of: None,
            review: None,
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
            started_at: None,
            ended_at: None,
            tokens: None,
            attention: None,
            endpoint: None,
            race: None,
            cancel: None,
            workflow: None,
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
            node: Box::new(summary("b", Harness::Codex, false, None)),
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

    /// **A reviewer is labelled as the review it is**, placed under the node it reviews, and says
    /// its findings once they are read — in the tree and in Home Watch alike.
    #[test]
    fn a_reviewer_row_says_it_reviews_and_counts_its_findings() {
        use marion_core::review::{Decision, ReviewTally};
        let mut n = summary("rev", Harness::ClaudeCode, false, None);
        n.parent_id = Some(AgentId("kid".into()));
        assert_eq!(review_note(&n), None, "not a reviewer");
        n.review_of = n.parent_id.clone();
        n.state = NodeState::Running;
        assert_eq!(row(&n).label, "codex-impl rev · reviewing");
        n.state = NodeState::Exited(marion_core::contract::ExitStatus::Ok);
        assert_eq!(review_note(&n).as_deref(), Some("review: no report read"));
        n.review = Some(ReviewTally {
            findings: 1,
            blocking: 0,
            decision: Decision::Allow,
        });
        assert_eq!(review_note(&n).as_deref(), Some("review: 1 finding"));
        n.review = Some(ReviewTally {
            findings: 3,
            blocking: 2,
            decision: Decision::Block,
        });
        assert_eq!(
            row(&n).label,
            "codex-impl rev · review: 3 findings, 2 blocking"
        );
    }

    /// **A child the operator's opt-in let a sandboxed caller start says so on its row**.
    #[test]
    fn a_widened_child_says_so_on_its_row() {
        let mut n = summary("kid", Harness::ClaudeCode, false, None);
        assert_eq!(widened_note(&n), None);
        n.widened = vec!["containment".into()];
        assert_eq!(
            row(&n).label,
            "codex-impl kid · uncontained child (operator opt-in)"
        );
        n.widened = vec!["write".into(), "shell".into()];
        assert_eq!(
            row(&n).label,
            "codex-impl kid · wider than its parent: write, shell (operator opt-in)"
        );
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

    /// **A unique prefix of a whole id names its agent too**, so an id copied short works; a prefix
    /// several ids share is refused with each of them, like an ambiguous short id.
    #[test]
    fn a_unique_prefix_of_a_whole_id_names_its_agent() {
        let a = "01a091ba-8ea3-7000-8000-000000000001";
        let b = "01a091bb-5b04-7000-8000-000000000003";
        assert_eq!(resolve_node("01a091ba", [a, b]), Ok(AgentId(a.into())));
        assert_eq!(resolve_node("01a091bb-5", [a, b]), Ok(AgentId(b.into())));
        let both = resolve_node("01a091", [a, b]).expect_err("both start with it");
        assert!(both.contains(a) && both.contains(b), "{both}");
        assert_eq!(resolve_node("", [a]), Ok(AgentId(String::new())));
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

    /// **A held node says what to do, wherever it is listed.** The supervisor's
    /// [`NodeSummary::attention`] follows the state word in the attention reason (Home's row, `!`),
    /// takes the tree strip's note, and ends the `marion list` line — but only while the node is in
    /// a state the queue shows; an exited node's stale words are not repeated.
    /// **Read with nobody serving, a node the journal left live stopped with its supervisor**, and
    /// its line says so and names the resume; an exited node and a supervisor's own orphan keep
    /// their lines.
    ///
    /// Mutation: return the node unchanged and the line says `running`.
    #[test]
    fn an_offline_live_node_reads_as_stopped_with_its_supervisor_and_names_its_resume() {
        let running = NodeSummary {
            state: NodeState::Running,
            ..summary("01a0e64e-433f", Harness::Codex, false, None)
        };
        let line = list_line(&stopped_with_its_supervisor(running));
        assert!(
            line.contains(" orphaned ")
                && line.ends_with(" — stopped (supervisor gone) — marion resume 01a0e64e-433f"),
            "{line}"
        );
        let done = NodeSummary {
            state: NodeState::Exited(marion_core::contract::ExitStatus::Ok),
            ..summary("d", Harness::Codex, false, None)
        };
        assert_eq!(stopped_with_its_supervisor(done.clone()), done);
    }

    #[test]
    fn attention_carries_the_supervisors_words_after_the_state_word() {
        use marion_core::node::BlockReason::BootDialog;
        let words = "claude is waiting on its boot dialog: trust /r in claude once";
        let held = NodeSummary {
            state: NodeState::Blocked(BootDialog),
            attention: Some(words.into()),
            ..summary("x", Harness::ClaudeCode, true, None)
        };
        assert_eq!(
            attention_of(&held).as_deref(),
            Some(format!("blocked:bootdialog: {words}").as_str())
        );
        assert_eq!(row(&held).note.as_deref(), Some(words));
        assert!(
            list_line(&held).ends_with(&format!(" — {words}")),
            "{}",
            list_line(&held)
        );
        let done = NodeSummary {
            state: NodeState::Exited(marion_core::contract::ExitStatus::Ok),
            ..held.clone()
        };
        assert_eq!(attention_of(&done), None);
        assert!(!list_line(&done).contains(words));
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
        // Also: `attention_carries_the_supervisors_words_after_the_state_word` below.
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
            (Blocked(BootDialog), Live, Some("blocked:bootdialog")),
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
            (Blocked(BootDialog), Orphaned, Some("orphaned")),
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
            (Blocked(BootDialog), ReapedIdle, None),
            (Exited(Ok), ReapedIdle, None),
            (Exited(Cancelled), ReapedIdle, None),
            (Exited(Failed), ReapedIdle, Some("exited:failed")),
            (Exited(TimedOut), ReapedIdle, Some("exited:timedout")),
            (Exited(Killed), ReapedIdle, Some("exited:killed")),
            (Exited(Unreported), ReapedIdle, Some("exited:unreported")),
        ];
        assert_eq!(table.len(), 14 * 3, "every state under every reap state");
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

    /// A finished project under a scratch dir: its repo (no git, so the key is the directory),
    /// its state dir, and what the fixture wrote there.
    fn finished(
        tag: &str,
    ) -> (
        marion_testsupport::Scratch,
        PathBuf,
        PathBuf,
        marion_testsupport::FinishedProject,
    ) {
        let dir = marion_testsupport::scratch(tag);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state = dir.join("state");
        let project =
            marion_core::paths::ProjectDir::new(&state, &crate::socket::project_root(&repo));
        let fx = marion_testsupport::finished_project(&project);
        (dir, repo, state, fx)
    }

    /// Every path under `root` and its bytes, so "reading changed nothing" is a comparison.
    fn contents(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut todo = vec![root.to_path_buf()];
        while let Some(dir) = todo.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    out.push((path.clone(), Vec::new()));
                    todo.push(path);
                } else {
                    out.push((path.clone(), std::fs::read(&path).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    /// **The journal's listing is the live listing's lines**: the same [`list_line`] over the same
    /// projection, so a finished node reads exactly as it did while a supervisor served it — and
    /// reading it leaves every file under the state dir as it was.
    #[test]
    fn a_finished_project_lists_from_its_journal_and_is_left_untouched() {
        let (_dir, repo, state, fx) = finished("tree-journal-list");
        let before = contents(&state);
        let view = JournalView::open(&repo, &state)
            .expect("the journal reads")
            .expect("nobody is serving, so the journal answers");
        let lines: Vec<String> = view.nodes().iter().map(list_line).collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains(&fx.root.0), "{lines:?}");
        assert!(!lines[0].contains("parent"), "the root has none: {lines:?}");
        assert!(lines[1].contains(&fx.child.0), "{lines:?}");
        assert!(
            lines[1].ends_with(&format!("parent {}", short_id(&fx.root.0))),
            "{lines:?}"
        );
        assert!(
            lines.iter().all(|l| l.contains(" exited:ok ")),
            "both ended ok: {lines:?}"
        );
        drop(view.node(&fx.child.0).expect("the child resolves"));
        assert_eq!(
            contents(&state),
            before,
            "a look at a finished project wrote, created or removed something under its state dir"
        );
    }

    /// `ls <id>` from the journal: the node by its short id, with what its contract says — the
    /// task, the workspace and branch, how it ended — and the spend its run recorded.
    #[test]
    fn a_node_from_the_journal_carries_its_contract_and_its_recorded_spend() {
        let (_dir, repo, state, fx) = finished("tree-journal-node");
        let view = JournalView::open(&repo, &state).unwrap().unwrap();
        let (node, detail) = view
            .node(short_id(&fx.child.0))
            .expect("the short id resolves");
        assert_eq!(node.agent_id, fx.child);
        assert_eq!(node.tokens, Some(1200 + 340 + 5000), "{node:?}");
        let task = detail.task.expect("the contract's task");
        assert!(task.prompt.contains("--top N"), "{task:?}");
        let done = detail.completion.expect("the contract's completion");
        assert_eq!(done.status, marion_core::contract::ExitStatus::Ok);
        assert_eq!(done.branch.as_deref(), Some(fx.branch.as_str()));
        assert!(
            matches!(
                detail.workspace,
                Some(marion_core::contract::Workspace::Worktree { ref branch, .. })
                    if *branch == fx.branch
            ),
            "{:?}",
            detail.workspace
        );
        assert!(detail.stream.is_some(), "the activity's tail was asked for");
    }

    /// A target the journal does not record is refused by name, not answered with an empty node.
    #[test]
    fn a_target_the_journal_does_not_record_is_refused_by_name() {
        let (_dir, repo, state, _fx) = finished("tree-journal-unknown");
        let view = JournalView::open(&repo, &state).unwrap().unwrap();
        let refusal = view.node("nobody").expect_err("no such node");
        assert!(refusal.contains("`nobody`"), "{refusal}");
    }

    /// A project that never ran here refuses as the live path does — naming the state dir, since a
    /// different `--state-dir` is the likelier explanation than an empty forest — and looking does
    /// not create the state dir it looked in.
    #[test]
    fn a_project_with_no_journal_refuses_like_the_live_path_and_creates_nothing() {
        let dir = marion_testsupport::scratch("tree-journal-none");
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let state = dir.join("state");
        let refusal = match JournalView::open(&repo, &state) {
            Err(e) => e,
            Ok(v) => panic!("an empty project answered: {:?}", v.map(|v| v.nodes())),
        };
        assert!(refusal.contains("no supervisor is serving"), "{refusal}");
        assert!(refusal.contains(&state.display().to_string()), "{refusal}");
        assert!(!state.exists(), "reading created the state dir");
    }

    /// **While a supervisor serves, the journal is not the answer**: `open` steps aside so the
    /// caller dials the live one, whose view includes what only it knows (panes, runs in progress).
    #[test]
    fn a_served_project_is_left_to_the_live_path() {
        let (_dir, repo, state, _fx) = finished("tree-journal-served");
        let paths = crate::socket::socket_paths(
            &state,
            &crate::socket::project_root(&repo),
            crate::socket::own_uid(),
        );
        let crate::socket::Acquired::Serving(_serving) = crate::socket::acquire(&paths).unwrap()
        else {
            panic!("nobody else can be serving a scratch project")
        };
        assert!(
            JournalView::open(&repo, &state).unwrap().is_none(),
            "the journal answered for a project a supervisor serves"
        );
    }

    /// **A cancel needs no one — unless its abort was ignored**: a node whose row had an abort and
    /// still had to be killed is `abort ignored`; one killed because its row had none, or one that
    /// closed in its grace, is a decision somebody already made.
    #[test]
    fn a_cancel_needs_attention_only_when_its_abort_was_ignored() {
        use marion_core::contract::ExitStatus::Cancelled;
        let cancelled = |forced, had_abort| NodeSummary {
            state: NodeState::Exited(Cancelled),
            cancel: Some(marion_core::proto::NodeCancel {
                by: marion_core::journal::CancelBy::Operator,
                forced,
                had_abort,
            }),
            ..summary("x", Harness::Pi, false, None)
        };
        assert_eq!(
            attention_of(&cancelled(true, true)).as_deref(),
            Some("abort ignored")
        );
        assert_eq!(
            attention_of(&cancelled(true, false)),
            None,
            "no abort to ignore"
        );
        assert_eq!(
            attention_of(&cancelled(false, true)),
            None,
            "closed in its grace"
        );
    }
}
