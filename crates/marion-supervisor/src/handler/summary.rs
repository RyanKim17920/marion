use std::collections::HashSet;

use marion_core::agent_type;
use marion_core::contract::AgentId;
use marion_core::journal::SpawnIntent;
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::notify::Event;
use marion_core::proto::{FailureKind, NodeSummary, RpcError};
use marion_core::registry::{Replay, ReplayedNode};

use super::Shared;
use crate::registry::Registry;

/// Why a node the journal knows about cannot be described to a client.
///
/// An enum and not a `None`, because the three have different causes and different fixes, and a
/// client told only *"cannot describe it"* would have no idea whether to look at the journal, at
/// this build's agent types, or at a writer that produced a nonsense depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unprojectable {
    /// No `SpawnIntent`: the journal has records *about* this node and no identity *for* it. Its
    /// head was compacted, or the record was lost (§4.2's `Ordinal` loss is exactly this shape).
    NoIntent,
    /// The journal names an agent type this build does not have, so the node's `timeout` — §3.1's
    /// bound, which §9 re-resolves from the type — cannot be resolved from anywhere.
    UnknownAgentType(String),
    /// The node ran on a harness this build has retired
    /// ([`marion_core::harness::RETIRED`]). The journal keeps it and replay places it, but a
    /// `NodeSummary` names a `Harness`, and there is none to name.
    RetiredHarness(String),
    /// A depth `NodeSummary`'s `u8` cannot hold. §6.1's default `max_depth` is 3, so this is a
    /// writer producing nonsense rather than a deep tree, and saturating it would silently place the
    /// node somewhere it is not.
    DepthOutOfRange(u32),
}

impl Unprojectable {
    /// The refusal a client sees, with the citation for the fact that is missing.
    pub fn as_error(&self, agent: &AgentId) -> RpcError {
        match self {
            Unprojectable::NoIntent => RpcError::of(
                FailureKind::Internal,
                Some(&agent.0),
                "the journal has records about this node but no `SpawnIntent` for it, so marion \
                 does not know its agent type, its harness, its parent or its depth. Replay reports \
                 that absence rather than filling it in, and this call will not invent one \
                 (§4.2, §7.4).",
                "§4.2",
            ),
            Unprojectable::UnknownAgentType(t) => RpcError::not_found(
                t,
                format!(
                    "this node is recorded as agent type `{t}`, which this build of marion does \
                     not have. §3.1 makes the agent type the source of its timeout bound and §9 \
                     re-resolves that bound from the type rather than from a copy, so marion \
                     cannot describe the node without it."
                ),
                "§3.1",
            ),
            Unprojectable::RetiredHarness(h) => RpcError::of(
                FailureKind::Unsupported,
                Some(&agent.0),
                format!(
                    "this node ran on the `{h}` harness, which this build of marion has retired. \
                     The journal keeps its record and its place in the tree, but marion no longer \
                     has a row to describe, resume or steer it."
                ),
                "§3.1",
            ),
            Unprojectable::DepthOutOfRange(d) => RpcError::of(
                FailureKind::Internal,
                Some(&agent.0),
                format!(
                    "this node is recorded at depth {d}, which is not a depth a tree marion built \
                     can reach (§6.1's default max_depth is 3). Reporting a saturated depth would \
                     place the node somewhere it is not."
                ),
                "§6.1",
            ),
        }
    }
}

/// §3.2's node, projected for a client — or the reason it cannot be.
///
/// Pure: it reads the replayed node, this build's agent-type registry and one fact the caller
/// supplies, and touches nothing else. That is what makes every arm above testable without a
/// socket, a journal or a thread.
///
/// `pane` is a **parameter and not a field of `node`** because it is not a journal fact: the
/// supervisor's live pty map is what knows whether a node has a display plane, and `ReplayedNode`
/// is a fold over records. Passing it in keeps replay honest — no record is invented for it — and
/// makes every caller state which answer it is giving, which is what stops a caller that has no
/// pty map from quietly defaulting one.
pub fn summarize(node: &ReplayedNode, pane: bool) -> Result<NodeSummary, Unprojectable> {
    if let Some(h) = node.retired_harness() {
        return Err(Unprojectable::RetiredHarness(h.to_string()));
    }
    let intent = node.intent.as_ref().ok_or(Unprojectable::NoIntent)?;
    // A built-in resolves here; a `.marion/agents.toml` row does not, because this runs under
    // the shared registry lock and reads no file. The type was only ever needed for the bound,
    // and every production writer journals the bound — so a recorded bound projects a node of
    // any type, and only an unresolvable type *with no recorded bound* is unprojectable.
    let ty = agent_type::builtin(&intent.agent_type);
    if ty.is_none() && intent.timeout_secs.is_none() {
        return Err(Unprojectable::UnknownAgentType(intent.agent_type.clone()));
    }
    let depth =
        u8::try_from(intent.depth).map_err(|_| Unprojectable::DepthOutOfRange(intent.depth))?;
    Ok(NodeSummary {
        widened: node.widened.clone(),
        agent_id: node.agent_id.clone(),
        parent_id: intent.parent_id.clone(),
        // Not a placeholder. See the module doc: nothing sets `Node.name` yet, so `None` is what
        // the journal says rather than what marion does not know.
        name: None,
        agent_type: intent.agent_type.clone(),
        harness: intent.harness,
        // §3.3's middle key component, straight off `Spawned`. `None` until the process exists,
        // which is the honest answer for a node that has not launched: there is no version yet.
        harness_version: node.harness_version.clone(),
        depth,
        state: node.state,
        reap_state: node.reap_state,
        // **The bound this node was launched under, and the agent type only where the journal is
        // silent.** §3.1 makes the type the *default*; the intent records what the launch actually
        // resolved (`--timeout`, `spawn`'s `timeout_secs`), and a pane that prints the default for
        // a node running under a different clock is telling an operator the wrong number in the one
        // place they look to decide whether a run has time left. `None` is an older journal or a
        // launch marion put no bound of its own on, and the type is the honest answer for both.
        timeout: match (intent.timeout_secs, ty) {
            (Some(secs), _) => marion_core::encoding::Duration::from_secs(secs),
            (None, Some(ty)) => ty.timeout,
            (None, None) => unreachable!("refused above"),
        },
        pane,
        started_at: clock(node).0,
        ended_at: clock(node).1,
        tokens: None,
        attention: attention(node),
        review_of: intent.review_of.clone(),
        review: tally(node),
        // Off the latest `Spawned`, with the harness's own spelling of the model undone — the
        // `provider:model` an operator would type to ask for it again.
        endpoint: node
            .provider
            .as_ref()
            .map(|provider| marion_core::proto::NodeEndpoint {
                provider: provider.clone(),
                model: node.model.as_deref().map(|m| {
                    marion_harness::adapter_for_type(intent.harness, None)
                        .map_or_else(|_| m.to_string(), |a| a.endpoint_model(m))
                }),
                route: node.route.clone(),
            }),
        race: race_badge(intent, node),
        cancel: node
            .cancel
            .as_ref()
            .map(|c| marion_core::proto::NodeCancel {
                by: c.by.clone(),
                forced: c.forced,
                had_abort: c.verb != marion_harness::spec::AbortVerb::NONE_KIND,
            }),
        changed: node.contracts.last().and_then(|c| c.changed),
        budget: intent.budget,
        workflow: intent.workflow.as_ref().map(|seat| {
            let head = node.workflow_head.as_ref();
            marion_core::proto::model::WorkflowBadge {
                wf_id: seat.wf_id.clone(),
                step: seat.step,
                round: seat.round,
                verdict: node.workflow_verdict,
                closed: node.workflow_closed,
                name: head.map(|h| h.name.clone()).unwrap_or_default(),
                steps: head.map_or(0, |h| h.steps),
                step_id: head.map(|h| h.step_id.clone()).unwrap_or_default(),
                budget_tokens: head.and_then(|h| h.budget_tokens),
            }
        }),
    })
}

/// The seat badge a node's summary carries: its race and seat off the intent, its verdict off the
/// `RaceDecided` replay folded onto it.
fn race_badge(
    intent: &SpawnIntent,
    node: &ReplayedNode,
) -> Option<marion_core::proto::model::RaceBadge> {
    let race = intent.race.as_ref()?;
    Some(marion_core::proto::model::RaceBadge {
        race_id: race.race_id.clone(),
        seat: race.seat()?,
        verdict: node.race_verdict,
    })
}

/// A reviewer's tally, off its latest contract record.
fn tally(node: &ReplayedNode) -> Option<marion_core::review::ReviewTally> {
    node.contracts.last().and_then(|c| c.review)
}

/// What the operator is asked to do about the node's current state, where its last `StateChanged`
/// said — only while it is `Blocked`, which is the state the attention queue shows it for.
fn attention(node: &ReplayedNode) -> Option<String> {
    matches!(node.state, NodeState::Blocked(_))
        .then(|| node.state_reason.clone())
        .flatten()
}

/// When a node started and ended, in the journal's own times — never this process's clock (see
/// `first_ts`): started at its latest `Spawned`, else its first record; ended at the record that
/// moved it to `Exited`.
pub(crate) fn clock(
    node: &ReplayedNode,
) -> (
    Option<marion_core::encoding::SystemTime>,
    Option<marion_core::encoding::SystemTime>,
) {
    (
        node.spawned_ts.or(node.first_ts),
        node.state.is_exited().then_some(node.state_ts).flatten(),
    )
}

/// What a subscriber has already been told about one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Told {
    state: NodeState,
    reap_state: ReapState,
    /// The summary's facts that move without a state change — its clock and its spend. A change
    /// here re-sends the whole summary as `tree/node-added`, which every client, old or new, folds
    /// as a replacement in place (`tree::fold_tree_event`); a new notification kind would make an
    /// older client read a frame it cannot decode as the supervisor going away.
    extra: Extra,
}

/// See [`Told::extra`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Extra {
    started_at: Option<marion_core::encoding::SystemTime>,
    ended_at: Option<marion_core::encoding::SystemTime>,
    tokens: Option<u64>,
    /// A hash of [`NodeSummary::attention`]: a new reason under the same `Blocked` state is news.
    attention: Option<u64>,
    // A reviewer's tally lands after its exit, so it must be a change a subscriber is told of.
    review: Option<marion_core::review::ReviewTally>,
    /// A seat's verdict moves when its race is decided, with no change to the node's state.
    race_verdict: Option<marion_core::race::SeatVerdict>,
    // Journaled just after the intent, so it can land after the node was first told.
    widened: bool,
    /// Whether a cancel reached the node, and whether it was forced — so a cancel starting, or
    /// ending in a kill, re-sends the summary.
    cancel: Option<bool>,
    /// A contract's changed-file count lands with its record, after the exit.
    changed: Option<u32>,
    /// A workflow step's verdict and its run's close land after the node's exit, with no change to
    /// its state.
    workflow: (
        Option<marion_core::workflow::StepVerdict>,
        Option<marion_core::workflow::Outcome>,
    ),
}

impl Extra {
    /// Read without projecting the node: this runs for every node on every flush.
    pub(super) fn of(n: &ReplayedNode, spending: &crate::spending::Spending) -> Extra {
        let (started_at, ended_at) = clock(n);
        Extra {
            started_at,
            ended_at,
            tokens: spending.shown_total(n),
            attention: attention(n).map(|a| {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                a.hash(&mut h);
                h.finish()
            }),
            review: tally(n),
            race_verdict: n.race_verdict,
            widened: !n.widened.is_empty(),
            cancel: n.cancel.as_ref().map(|c| c.forced),
            changed: n.contracts.last().and_then(|c| c.changed),
            workflow: (n.workflow_verdict, n.workflow_closed),
        }
    }
}

/// The tree, as summaries, counting what could not be described.
///
/// `panes` is the set of nodes this supervisor holds a pty for, read **before** the shared lock was
/// taken — see [`RegistryHandle::pane_ids`](super::RegistryHandle::pane_ids) for why it is a snapshot passed in rather than a map
/// consulted here.
pub(super) fn project(
    tree: &Replay,
    g: &mut Shared,
    panes: &HashSet<AgentId>,
    spending: &crate::spending::Spending,
) -> Vec<NodeSummary> {
    let mut out = Vec::new();
    let mut lost = 0usize;
    for n in tree.nodes() {
        match summarize_spent(n, panes.contains(&n.agent_id), spending) {
            Ok(s) => out.push(s),
            Err(_) => lost += 1,
        }
    }
    g.unprojectable = lost;
    out
}

/// What has changed since the last time anybody was told, in journal order.
///
/// Two events and not one: §2's `tree/node-added` exists because *"a client that learned of nodes
/// only from state changes would show a tree that is missing exactly the nodes currently being
/// created — the ones an operator is most likely watching."*
///
/// Every `ts` is the journal's, never this process's clock — see
/// [`marion_core::registry::ReplayedNode::first_ts`] for why a follower's `now()` is the wrong
/// answer on a field a client renders as when the thing occurred.
pub(super) fn collect(
    r: &Registry,
    g: &mut Shared,
    panes: &HashSet<AgentId>,
    spending: &crate::spending::Spending,
) -> Vec<Event> {
    #[cfg(test)]
    {
        g.collects += 1;
    }
    let mut events = Vec::new();
    for n in r.tree().nodes() {
        let now = Told {
            state: n.state,
            reap_state: n.reap_state,
            extra: Extra::of(n, spending),
        };
        let before = g.told.get(&n.agent_id).copied();
        if before == Some(now) {
            continue;
        }
        // Projected only when there is something to say, not for every node on every flush.
        let summary = summarize_spent(n, panes.contains(&n.agent_id), spending).ok();
        match before {
            None => {
                // A node marion cannot describe produces no `tree/node-added` — there is no summary
                // to put in one — but it is still recorded as told, so it is not re-examined on
                // every flush. `project` is what counts it.
                if let Some(node) = summary {
                    events.push(Event::NodeAdded {
                        node: Box::new(node),
                        ts: journal_ts(n.first_ts),
                    });
                }
            }
            Some(before) => {
                if (before.state, before.reap_state) != (now.state, now.reap_state) {
                    events.push(Event::NodeState {
                        agent_id: n.agent_id.clone(),
                        state: n.state,
                        reap_state: n.reap_state,
                        ts: journal_ts(n.state_ts),
                    });
                }
                if before.extra != now.extra
                    && let Some(node) = summary
                {
                    events.push(Event::NodeAdded {
                        node: Box::new(node),
                        ts: journal_ts(n.state_ts.or(n.first_ts)),
                    });
                }
            }
        }
        g.told.insert(n.agent_id.clone(), now);
    }
    events
}

/// [`summarize`], with the node's spend so far ([`crate::spending::Spending::shown_total`]) — read
/// from memory, no file.
pub(crate) fn summarize_spent(
    n: &ReplayedNode,
    pane: bool,
    spending: &crate::spending::Spending,
) -> Result<NodeSummary, Unprojectable> {
    let mut s = summarize(n, pane)?;
    s.tokens = spending.shown_total(n);
    Ok(s)
}

/// The journal's own time for a transition.
///
/// The `None` case is reachable and is not a decision this function may duck: a node replayed from
/// records written before `first_ts`/`state_ts` existed has neither. The epoch is used rather than
/// `now()` deliberately — a timestamp a client can *see* is wrong is better than one that is wrong
/// and plausible, which is the field-name-lies class this codebase refuses elsewhere.
fn journal_ts(ts: Option<marion_core::encoding::SystemTime>) -> marion_core::encoding::SystemTime {
    ts.unwrap_or_else(|| marion_core::encoding::SystemTime::from_unix_millis(0))
}
