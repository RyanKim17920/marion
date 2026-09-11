//! The values the fifteen methods carry, as distinct from the methods themselves.
//!
//! Everything the registry is authoritative about is imported from `marion-core`. What is declared
//! here is only what is *about the conversation*: how a node is projected for a tree pane, which
//! of two delivery verbs a state calls for, what a re-attach hands back, and what a quit did.

use crate::contract::AgentId;
use crate::encoding::{Duration, Millis};
use crate::harness::Harness;
use crate::ir::SrcSeq;
use crate::node::{BlockReason, NodeState, ReapState};
use serde::{Deserialize, Serialize};

/// §3.2's `Node`, projected for a client.
///
/// **What it deliberately does not carry: `caps` (§3.3) — and what it carries instead.**
/// `Capabilities` lives in `marion-harness` beside the adapters, along with `ExecutionSurfaces`,
/// its ceiling. Declaring a `Capabilities` here would create the parallel copy §10's layout exists
/// to prevent, and declaring a lone `steer: bool` would create half of one. So a client that wants
/// to render a capability is given **§3.3's key** — `(harness, harness_version, surfaces)` — and
/// resolves it through the same `static_caps` `marion doctor` calls, rather than being handed a
/// resolved answer that could be stale by the time it is drawn. The third component is
/// [`Self::pane`]: §3.4 gives one harness two surface shapes, and doctor prints one row per
/// `(harness, role)` for exactly that reason.
///
/// A client that will not resolve the key still learns a missing capability the way §11 item 23
/// says a caller should learn anything marion will not do: from a refusal that says so —
/// [`crate::proto::FailureKind::Unsupported`] on the `node/steer` that needed it.
///
/// `binary_path`, `harness_session`, `harness_pane` and §7.6's evidence flags are likewise absent:
/// they are supervisor-internal, and a tree pane that could render them would be a tree pane that
/// could leak them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSummary {
    pub agent_id: AgentId,
    /// §7.5: immutable. A client may cache the tree shape and never re-parent a node.
    pub parent_id: Option<AgentId>,
    /// §2's `node/rename` target, and the address other agents use with `send`. `None` until named.
    pub name: Option<String>,
    pub agent_type: String,
    pub harness: Harness,
    /// The **middle third of §3.3's key**, as the journal recorded it at launch (`Spawned`).
    ///
    /// `Option`, and the `None` is load-bearing rather than a gap: §3.3 says a version marion
    /// cannot read is a version marion has not measured, and `advertised` already treats an
    /// unparseable version as unmeasured. A node whose `Spawned` record predates this field, or a
    /// node still `Spawning`, therefore resolves to the conservative set — which is the direction
    /// §3.3's *degrade visibly* requires an unknown to fall in.
    ///
    /// `#[serde(default)]` per this module's rule for every added field: a client one version older
    /// must keep parsing, and reading `None` is exactly right for one that never knew to ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    pub depth: u8,
    pub state: NodeState,
    pub reap_state: ReapState,
    /// The node's bound (§3.1 `timeout_secs`, §9). A *bound*, so `marion-core`'s seconds-rounded-up
    /// encoding — never `Millis`, which would let a 900 s bound arrive as a measurement and be
    /// rendered as elapsed time.
    pub timeout: Duration,
    /// **Whether this node runs on its harness's pane surfaces** — §3.3's third key component,
    /// reduced to the one bit a client can act on.
    ///
    /// Not cosmetic, and not the same question as *"can I attach to it"*. §3.4 gives claude-code a
    /// `Typed(StreamJson)` control plane on its node surfaces and §3.4's `opaque` on its pane ones,
    /// and `Capabilities::ceiling` clips `permissions` off the second: the same binary, at two
    /// keys, publishing two different sets. `marion doctor` prints both rows for that reason, and a
    /// client that keyed every node on the node row would offer a pane node a permission routing it
    /// does not have — the exact defect §9's M5 clause 3 asks the UI not to have.
    ///
    /// Answered from the supervisor's live pty map rather than from the journal, which is the same
    /// source `node/attach` builds [`crate::proto::result::PaneAttach`] from. There is no `SpawnIntent`
    /// field for it, and inventing one would be a second record of a fact the supervisor already
    /// holds.
    ///
    /// `#[serde(default)]`: `false` is what a journal-only reader and an older client both mean.
    #[serde(default)]
    pub pane: bool,
}

/// Which of §6.3's two verbs a node's state calls for.
///
/// This is a *client-facing* rule and it is not §5.4's. See the module doc: §5.4 denies an agent's
/// `send` against `Blocked(Descendants)`, but §7.6's hold is released *by a re-prompt* and §6.3
/// names the user's `node/prompt` as one of the two callers that reach that path. A held node is
/// therefore promptable by the operator and not by a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    /// A new turn on a node that does not have one in flight (`node/prompt`).
    Prompt,
    /// Mid-flight injection (`node/steer`). Requires `caps.steer`; a harness without it answers
    /// [`crate::proto::FailureKind::Unsupported`], per §6.3's *"and `Unsupported` otherwise"*.
    Steer,
}

impl Delivery {
    /// Total over [`NodeState`], which is the point: §6.3's rule is a *classification of every
    /// state*, and writing it as a scattered set of `if`s at the call sites is how
    /// `Blocked(Permission)` ends up treated as idle in one branch and running in another.
    ///
    /// `None` means the node cannot take input at all yet. That is `Spawning` alone — the process
    /// does not exist, so there is nothing to prompt and nothing to interrupt.
    ///
    /// The three arms that are easy to get wrong, each with its citation:
    ///
    /// * `Blocked(Permission)` and `Blocked(Elicitation)` are **`Steer`**. §6.3: such a node *"has
    ///   a turn in flight, so it is treated as `Running` for delivery"*. Reading them as idle would
    ///   send a `prompt` into a live turn.
    /// * `Blocked(Descendants)` is **`Prompt`**. §7.6's hold is *"resolved by a re-prompt or a
    ///   timeout"*; there is no turn in flight to inject into.
    /// * `Exited(_)` is **`Prompt`**. §6.3: resuming a finished node is `continue_()` then
    ///   `prompt()`, performed as one atomic registry operation. Whether the harness *can* resume
    ///   is `caps.resume`, which is a separate refusal — not a reason to misclassify the verb.
    pub const fn for_state(state: NodeState) -> Option<Delivery> {
        match state {
            NodeState::Spawning => None,
            NodeState::Ready | NodeState::Idle => Some(Delivery::Prompt),
            NodeState::Running => Some(Delivery::Steer),
            NodeState::Blocked(BlockReason::Permission | BlockReason::Elicitation) => {
                Some(Delivery::Steer)
            }
            NodeState::Blocked(BlockReason::Descendants) => Some(Delivery::Prompt),
            NodeState::Exited(_) => Some(Delivery::Prompt),
        }
    }
}

/// §4.2's ordering evidence, as a resumption point.
///
/// §7.3.3 makes the replay-to-subscribe seam the correctness question of re-attach and answers it
/// by `src_seq`: *"replay to the journal's own read point, then subscribe from there."*
///
/// `src_seq` is an `Option` for the reason §4.2 states and this module must not paper over: on
/// Codex app-server and Claude Code `headless` there is no source-side ordering evidence at all,
/// and *"where it is `None`, marion cannot detect loss and must not imply otherwise."* A
/// non-optional field would have forced a fabricated `Ordinal(0)` into exactly the case where
/// marion knows least.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPoint {
    /// Journal records replayed. Always knowable, because marion wrote them.
    pub records: u64,
    /// The last source-side ordering evidence seen, where the harness supplies any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_seq: Option<SrcSeq>,
}

/// §7.3.3's re-attach, **split by node rather than by session** — which is the whole finding of
/// that section, and the reason this is an enum on the per-node result instead of a flag on a
/// per-session one. One attach across a tree can legitimately answer all three ways.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachMode {
    /// A live node. §6.2: the supervisor has held the channel since `t=0`, so this is a view
    /// switch, never a connection event, and the client MUST NOT re-open, re-connect or re-spawn
    /// anything — the double-open hazard is unreachable only because there was never a first close.
    ///
    /// Carries a [`ReplayPoint`] because §7.3.3's *"Both, in one attach"* is not optional: a node
    /// that was running at detach and produced events while nobody was listening needs the replay
    /// leg **and** the subscribe leg, and a variant without the point could not express the seam.
    ResubscribeFrom(ReplayPoint),
    /// A node that finished while detached. Replay only; there is no channel to resume, and
    /// offering one would invent a subscription that can never deliver.
    ReplayOnly(ReplayPoint),
    /// `ReapedIdle` (§7.2, reached from §7.3.2's disposition (c)) or `Orphaned` (§7.2, marked at
    /// supervisor restart). Replay plus the fact that it is resumable — *"they have no channel to
    /// re-subscribe to until resumed"*. Distinct from `ReplayOnly` because the operator's options
    /// differ: this node can be brought back. An orphan is never `ResubscribeFrom`: the supervisor
    /// answering holds no channel for it, whatever its process may still be doing.
    ReplayResumable(ReplayPoint),
}

impl AttachMode {
    pub fn replay_point(&self) -> &ReplayPoint {
        match self {
            AttachMode::ResubscribeFrom(p)
            | AttachMode::ReplayOnly(p)
            | AttachMode::ReplayResumable(p) => p,
        }
    }

    /// Whether live events will follow. The one bit a client must branch on, and it is derived
    /// from the mode rather than sent alongside it, so the two cannot disagree.
    pub fn is_live(&self) -> bool {
        matches!(self, AttachMode::ResubscribeFrom(_))
    }
}

/// A harness-native permission request id (Claude Code's `request_id`, §5.2). A newtype and not a
/// `String` because it is answered by exactly one method and must not be interchangeable with an
/// elicitation id, which is answered by a different one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRequestId(pub String);

/// The elicitation counterpart. §2: `elicitation/reply` exists because ACP and Codex both have
/// structured input requests **distinct from** permissions; harnesses lacking the concept simply
/// never produce the event. One newtype per queue is what keeps a client from answering a
/// permission with a form response.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ElicitationRequestId(pub String);

/// The operator's answer to a permission prompt.
///
/// `Deny` carries a required reason. §5.2 measured what the far side sees: an `is_error: true`
/// `tool_result` reading *"Claude requested permissions to use X, but you haven't granted it yet"*
/// — a sentence the model reads and routes around. A denial with no reason hands the model that
/// generic string; a denial with one lets marion say why, which is the difference between an agent
/// that reconsiders and an agent that retries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
}

/// The operator's answer to a structured input request.
///
/// `Provided` carries opaque JSON, and that is not laziness. §11 item 23's rule is about
/// parameters marion *performs* and silently drops; this payload marion does not interpret at all
/// — it is the harness's own schema, elicited by the harness, validated by the harness. marion is
/// the courier. Typing it would mean marion inventing a schema language for a request it did not
/// author, and getting it wrong would corrupt a value that was already well-formed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ElicitationResponse {
    Provided(serde_json::Value),
    /// The operator declined to answer. Distinct from a `Provided(null)`, which is an answer.
    Declined {
        reason: String,
    },
}

/// What a `permission/reply` or `elicitation/reply` accomplished.
///
/// `Stale` exists because §7.3.1's discussion of a dead process is explicit: *"the queued request
/// names a dead process, so `permission/reply` resolves to nothing"*. Returning success there
/// would tell the operator they unblocked a node that no longer exists, which is worse than an
/// error — it is a false report about work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyOutcome {
    /// The answer reached the node; here is the state it moved to.
    Delivered { state: NodeState },
    /// The request is no longer answerable. `reason` is a sentence for the same reason
    /// [`crate::proto::RpcError`] carries one: "stale" alone does not say whether the process died or the
    /// bound expired, and those look identical to an operator and different to a debugger.
    Stale { reason: String },
}

/// §7.3.2's three dispositions. **There is no `Default` impl, and that is load-bearing.**
///
/// §7.3.1: *"a client that closes without calling it has chosen nothing, and marion MUST treat
/// that as the crash case, not as a quit."* A `Default` would give the absence of a call a value,
/// and the moment a disposition has a value it can be applied — which is precisely how a
/// crash-safety invariant becomes conditional on something a dead process configured. See
/// [`ClientGone`], where the distinction is a type rather than a convention.
///
/// [`QuitDisposition::DEFAULT`] is the *UI's* preselection, which is a different thing: it names
/// what an operator who chose (c) by pressing return chose, not what marion does when nobody
/// pressed anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuitDisposition {
    /// (a) kill the tree. Every non-terminal node → `Cancelled` (§6.7's classification for
    /// `node/kill`; quitting invents no new terminal state).
    ///
    /// **The confirmed list is required, and it is not bookkeeping.** §7.3.2: *"it MUST be
    /// confirmed against a rendered list of the affected nodes. A keybinding that reaches (a)
    /// without that list is a bug, not a shortcut."* Making the list a field means an unconfirmed
    /// kill-tree quit is not a value that can be constructed, so the rule survives a refactor that
    /// forgets it. The supervisor compares the list against its own live set and refuses on
    /// mismatch — an operator who confirmed against a stale render confirmed against something
    /// else.
    ///
    /// **Reach is bounded by §11 item 25**: this is the per-node kill §7.3.2 states, not a
    /// tree-wide signal, because whether a detached supervisor's session and process groups let one
    /// signal reach the whole tree is unmeasured.
    KillTree { confirmed: Vec<AgentId> },
    /// (b) detach everything. Nothing happens to any node and the supervisor does **not** exit
    /// (§5.7 forbids it with non-terminal nodes).
    DetachAll,
    /// (c) reap the idle, detach the busy. §7.2's reap, applied to what it will accept — running
    /// nodes, **any** `Blocked(_)` node, and any node a `spawn` is blocked on are detached instead.
    ReapIdleDetachBusy,
}

impl QuitDisposition {
    /// §7.3.2's default: (c), *"the only disposition that is not lossy in either direction"*.
    /// Recorded by the spec as a judgement rather than a measurement.
    ///
    /// It is a constant and not a `Default` impl on purpose — see the type doc. `Default` is
    /// reached implicitly, and the one thing this disposition must never be is implicit.
    pub const DEFAULT: QuitDisposition = QuitDisposition::ReapIdleDetachBusy;
}

/// Why the supervisor is not leaving. §5.7's exclusion list, which is §7.2's reap exclusions
/// applied one level up and for the same reason: each one strands something that can never be
/// resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResidentReason {
    NonTerminalNode,
    /// **Any** `Blocked(_)`, including `Blocked(Descendants)`.
    BlockedNode,
    SpawnOutstanding,
    /// A reap intent journaled without its confirmation (§7.2). Exiting here reproduces exactly
    /// the crash window intent-then-confirm exists to close.
    UnconfirmedReapIntent,
    /// **Not a node: the supervisor no longer knows what the nodes are.**
    ///
    /// `registry.rs` stops following a journal for good at a line that is not a record, or at a
    /// file that got shorter (§7.4), and it is right to — *"an authority may not keep serving a
    /// tree from a file it no longer recognises"*. One level up, that freezes §5.7's exit predicate
    /// on the tree as it stood **before** the corruption, and §5.7's exclusion list has four
    /// clauses of which *"the registry stopped following"* is not one.
    ///
    /// So the supervisor stays, which is the safe half — a frozen tree may name live work — but it
    /// says *this* rather than reporting whichever stale clause the frozen prefix happens to
    /// satisfy. An operator told `NonTerminalNode` goes looking for a node; an operator told
    /// `RegistryStopped` goes looking at the journal, which is where the problem is. The byte
    /// offset and the reason are on the supervisor's own log, since this enum is `Copy` and
    /// carrying them here would widen every response that has nothing to do with corruption.
    ///
    /// **Clearing the condition is not implemented** — §11 item 29.
    RegistryStopped,
}

/// What the supervisor will do after answering the quit.
///
/// `Resident` cannot be stated without a reason, because §5.7's rule is a list of conditions and
/// "still here" without one is unauditable — the operator cannot tell a supervisor that is holding
/// work from a supervisor that failed to exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorDisposition {
    /// Zero clients and zero non-terminal nodes: it MAY exit after §5.7's grace period, and its
    /// exit is journaled as an ordinary record.
    Exiting,
    Resident(ResidentReason),
}

/// The two sentences §7.3.2 requires before any detach.
///
/// *"They MUST tell the operator both how to re-attach and how to stop the fleet without one.
/// Detaching into silence is worse than killing, because the operator does not know they now own
/// something."* Both are non-optional `String`s and both are inside every detaching variant of
/// [`QuitOutcome`], so "never silently detach" is a shape the type enforces rather than a rule a
/// reviewer has to remember.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachGuidance {
    pub reattach: String,
    pub stop_fleet: String,
}

/// One node the kill disposition reached. §6.7 requires the intent and its confirmation to be
/// journaled per node; this is the confirmation, reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KilledNode {
    pub agent_id: AgentId,
    /// What it was doing, so the operator's record matches the list they confirmed against.
    pub was: NodeState,
}

/// What the quit actually did — **one variant per disposition, not a bag of optional lists.**
///
/// The bag was the obvious shape and it is wrong: it can represent a detach with no guidance
/// (§7.3.2 forbids it), a kill that reports reaped nodes (no disposition does both), and a quit
/// that says nothing at all. Every one of those is a bug the type now cannot hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuitOutcome {
    Killed {
        nodes: Vec<KilledNode>,
        supervisor: SupervisorDisposition,
    },
    Detached {
        detached: Vec<AgentId>,
        /// §7.3.2's non-obvious cost of (b), reported rather than assumed: per §11 item 22 marion
        /// has **no route from a permission request to a human**, so a detached node that reaches a
        /// gate burns its `Blocked` bound and is **denied unattended** — and S9 measured that the
        /// far side sees an `is_error: true` `tool_result`, not a question, and routes around it
        /// and finishes looking clean.
        ///
        /// The spec requires these nodes to be named *at the point of choosing*, which is before
        /// this call. No method in §2 asks "what would (b) cost"; that pre-call surface is not
        /// invented here, and this field is the after-the-fact half only.
        gate_exposed: Vec<AgentId>,
        guidance: DetachGuidance,
        supervisor: SupervisorDisposition,
    },
    ReapedAndDetached {
        reaped: Vec<AgentId>,
        detached: Vec<AgentId>,
        /// §7.2 refuses to reap `Blocked(_)` nodes, so (c) detaches precisely the nodes most
        /// exposed to §11 item 22's gap. The spec says so and says not to lean on (c) as a
        /// mitigation.
        gate_exposed: Vec<AgentId>,
        guidance: DetachGuidance,
        supervisor: SupervisorDisposition,
    },
}

/// How a client stopped being there — the §7.3.1 distinction, as a type.
///
/// This is not a wire message. It is the supervisor's own reading of an ended connection, and it
/// is in this module because the protocol is what makes the two cases distinguishable at all: the
/// supervisor sees an identical socket close either way, and the *only* evidence of intent that
/// ever exists is a `session/quit` that arrived before it. §7.3.1: *"a dropped socket is
/// therefore **not** a quit"*, and *"guessing intent from a disconnect is how work gets killed by
/// accident."*
///
/// The type has no `From<()>`, no `Default`, and no constructor that turns a close into a
/// disposition. [`ClientGone::disposition`] returns `None` for `SocketClosed` and there is no path
/// that makes it return anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientGone {
    /// The socket closed with no `session/quit`. §7.3.1: **nothing happens to agents.** Not a
    /// policy, an invariant — nodes do not change state and nothing is journaled, because from the
    /// registry's point of view nothing happened.
    SocketClosed,
    /// `session/quit` arrived. The only value in this enum that carries intent.
    Quit(QuitDisposition),
}

impl ClientGone {
    /// `None` is the crash case, and it is `None` structurally rather than by convention.
    pub fn disposition(&self) -> Option<&QuitDisposition> {
        match self {
            ClientGone::SocketClosed => None,
            ClientGone::Quit(d) => Some(d),
        }
    }

    /// §7.3.1's invariant, in one predicate: a departure that was not a quit MUST leave every node
    /// exactly as it was.
    pub fn nodes_must_be_untouched(&self) -> bool {
        matches!(self, ClientGone::SocketClosed)
    }
}

/// §8's two `marion doctor` modes. §8 is emphatic that the second matters more: *"Feature flags
/// drift less than behavior does, and every retraction in §12 was a behavioral surprise, not a
/// missing capability."*
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeMode {
    /// `--capabilities`: what the harness advertises. The static half of §3.3's two-stage
    /// resolution.
    Capabilities,
    /// `--adapter`: a micro-contract test against the installed binary — spawn, prompt, assert
    /// response shape, interrupt, assert clean termination, kill if still alive.
    Adapter,
}

/// One harness's answer to `doctor/run`.
///
/// `capabilities` is **not** a `Capabilities` struct, for the reason [`NodeSummary`] gives: the
/// type does not exist yet. It is `harness_version` plus a found/not-found answer, which is what a
/// probe can honestly report today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessReport {
    pub harness: Harness,
    /// `None` when the binary was not found. §7.7's symlink resolution is what produced the path
    /// this version was read from.
    #[serde(default)]
    pub harness_version: Option<String>,
    /// `None` under [`ProbeMode::Capabilities`], which does not run one. An `Option` rather than a
    /// `false` because "did not run" and "ran and failed" are the distinction §8 says the mode
    /// exists to make.
    #[serde(default)]
    pub adapter_check: Option<bool>,
    /// A sentence per finding. §8's probe reports *why* a harness is unusable, not merely that it
    /// is — a version too old and a binary that hangs on interrupt are the same boolean and
    /// different problems.
    #[serde(default)]
    pub notes: Vec<String>,
    /// A *measurement* — milliseconds rounded down, per `marion-core`'s encoding rule. A probe that
    /// took 412 ms must not report 1 s, and a sub-second one must not report 0.
    pub elapsed: Millis,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ExitStatus;

    fn agent(s: &str) -> AgentId {
        AgentId(s.to_string())
    }

    #[test]
    fn delivery_is_total_over_node_state() {
        // Every state classified, so §6.3 cannot be half-applied at a call site.
        let cases = [
            (NodeState::Spawning, None),
            (NodeState::Ready, Some(Delivery::Prompt)),
            (NodeState::Idle, Some(Delivery::Prompt)),
            (NodeState::Running, Some(Delivery::Steer)),
            (
                NodeState::Blocked(BlockReason::Permission),
                Some(Delivery::Steer),
            ),
            (
                NodeState::Blocked(BlockReason::Elicitation),
                Some(Delivery::Steer),
            ),
            (
                NodeState::Blocked(BlockReason::Descendants),
                Some(Delivery::Prompt),
            ),
            (NodeState::Exited(ExitStatus::Ok), Some(Delivery::Prompt)),
            (
                NodeState::Exited(ExitStatus::Cancelled),
                Some(Delivery::Prompt),
            ),
        ];
        for (state, want) in cases {
            assert_eq!(Delivery::for_state(state), want, "state {state:?}");
        }
    }

    #[test]
    fn a_blocked_node_awaiting_an_answer_is_steered_not_prompted() {
        // §6.3's sharpest rule: such a node has a turn in flight. Treating it as idle would send a
        // new prompt into a live turn.
        for r in [BlockReason::Permission, BlockReason::Elicitation] {
            assert_eq!(
                Delivery::for_state(NodeState::Blocked(r)),
                Some(Delivery::Steer)
            );
        }
        // And the third BlockReason is the opposite case, not the same one.
        assert_eq!(
            Delivery::for_state(NodeState::Blocked(BlockReason::Descendants)),
            Some(Delivery::Prompt),
            "§7.6's hold is released by a re-prompt; §5.4's agent-facing denial is a different surface"
        );
    }

    #[test]
    fn a_dropped_socket_carries_no_disposition() {
        assert_eq!(ClientGone::SocketClosed.disposition(), None);
        assert!(ClientGone::SocketClosed.nodes_must_be_untouched());
        let quit = ClientGone::Quit(QuitDisposition::DetachAll);
        assert_eq!(quit.disposition(), Some(&QuitDisposition::DetachAll));
        assert!(!quit.nodes_must_be_untouched());
    }

    #[test]
    fn kill_tree_is_never_the_default() {
        assert_eq!(
            QuitDisposition::DEFAULT,
            QuitDisposition::ReapIdleDetachBusy
        );
        assert_ne!(
            QuitDisposition::DEFAULT,
            QuitDisposition::KillTree { confirmed: vec![] },
            "§7.3.2: (a) MUST NOT be the default under any reading"
        );
    }

    #[test]
    fn kill_tree_cannot_be_expressed_without_the_confirmed_list() {
        // Not a runtime assertion — a compile-time one. `KillTree` has no other constructor, so
        // this test's value is that it fails to compile if the field is ever made optional.
        let d = QuitDisposition::KillTree {
            confirmed: vec![agent("a"), agent("b")],
        };
        let QuitDisposition::KillTree { confirmed } = &d else {
            panic!("wrong variant")
        };
        assert_eq!(confirmed.len(), 2);
    }

    #[test]
    fn attach_mode_derives_liveness_rather_than_carrying_it() {
        let p = ReplayPoint {
            records: 12,
            src_seq: Some(SrcSeq::Ordinal(7)),
        };
        assert!(AttachMode::ResubscribeFrom(p.clone()).is_live());
        assert!(!AttachMode::ReplayOnly(p.clone()).is_live());
        assert!(!AttachMode::ReplayResumable(p.clone()).is_live());
        // Every mode carries the seam, because §7.3.3's "both, in one attach" needs it on the
        // live leg too.
        for m in [
            AttachMode::ResubscribeFrom(p.clone()),
            AttachMode::ReplayOnly(p.clone()),
            AttachMode::ReplayResumable(p.clone()),
        ] {
            assert_eq!(m.replay_point(), &p);
        }
    }

    #[test]
    fn a_replay_point_without_ordering_evidence_says_so() {
        // §4.2: on Codex app-server and Claude Code headless there is none, and marion must not
        // imply otherwise. Absent from the wire entirely, not `null` and not a fabricated 0.
        let p = ReplayPoint {
            records: 3,
            src_seq: None,
        };
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"records":3}"#,
            "a missing src_seq must not serialize as a value"
        );
        assert_eq!(
            serde_json::from_str::<ReplayPoint>(r#"{"records":3}"#).unwrap(),
            p
        );
    }

    #[test]
    fn model_types_pin_their_wire_shapes() {
        let cases: Vec<(String, &str)> = vec![
            (
                serde_json::to_string(&Delivery::Steer).unwrap(),
                r#""Steer""#,
            ),
            (
                serde_json::to_string(&PermissionDecision::Allow).unwrap(),
                r#""Allow""#,
            ),
            (
                serde_json::to_string(&PermissionDecision::Deny {
                    reason: "out of scope".into(),
                })
                .unwrap(),
                r#"{"Deny":{"reason":"out of scope"}}"#,
            ),
            (
                serde_json::to_string(&ElicitationResponse::Declined {
                    reason: "not now".into(),
                })
                .unwrap(),
                r#"{"Declined":{"reason":"not now"}}"#,
            ),
            (
                serde_json::to_string(&QuitDisposition::ReapIdleDetachBusy).unwrap(),
                r#""ReapIdleDetachBusy""#,
            ),
            (
                serde_json::to_string(&QuitDisposition::KillTree {
                    confirmed: vec![agent("a")],
                })
                .unwrap(),
                r#"{"KillTree":{"confirmed":["a"]}}"#,
            ),
            (
                serde_json::to_string(&SupervisorDisposition::Resident(
                    ResidentReason::UnconfirmedReapIntent,
                ))
                .unwrap(),
                r#"{"Resident":"UnconfirmedReapIntent"}"#,
            ),
            (
                serde_json::to_string(&AttachMode::ResubscribeFrom(ReplayPoint {
                    records: 1,
                    src_seq: Some(SrcSeq::Predecessor(crate::ir::EventId("u".into()))),
                }))
                .unwrap(),
                r#"{"ResubscribeFrom":{"records":1,"src_seq":{"Predecessor":"u"}}}"#,
            ),
            (
                serde_json::to_string(&ReplyOutcome::Stale {
                    reason: "the process is gone".into(),
                })
                .unwrap(),
                r#"{"Stale":{"reason":"the process is gone"}}"#,
            ),
            (
                serde_json::to_string(&ProbeMode::Adapter).unwrap(),
                r#""Adapter""#,
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }

    #[test]
    fn a_node_summary_round_trips_and_encodes_its_bound_in_seconds() {
        let n = NodeSummary {
            agent_id: agent("0199c0ff-ee00-7000-8000-000000000001"),
            parent_id: None,
            name: Some("impl".into()),
            agent_type: "codex-impl".into(),
            harness: Harness::Codex,
            harness_version: Some("0.146.0".into()),
            depth: 0,
            state: NodeState::Blocked(BlockReason::Permission),
            reap_state: ReapState::Live,
            // 900_500 ms: a bound, so it must round UP to 901 and never down to 900.
            timeout: Duration(std::time::Duration::from_millis(900_500)),
            pane: true,
        };
        let wire = serde_json::to_string(&n).unwrap();
        assert_eq!(
            wire,
            r#"{"agent_id":"0199c0ff-ee00-7000-8000-000000000001","parent_id":null,"name":"impl","agent_type":"codex-impl","harness":"codex","harness_version":"0.146.0","depth":0,"state":{"Blocked":"Permission"},"reap_state":"Live","timeout":901,"pane":true}"#
        );
        let back: NodeSummary = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.state, n.state);
        assert_eq!(back.harness, n.harness);
        // The bound is now 901 s exactly; a round trip through the wire is lossy in the safe
        // direction only, which is what "bounds round up" buys.
        assert_eq!(back.timeout, Duration::from_secs(901));
    }

    /// **§3.3's key survives a client one version older, in both directions.**
    ///
    /// `harness_version` and `pane` are the two components a client needs to resolve a capability
    /// (§3.3 keys `(harness, harness_version, surfaces)`), and both were added after the field set
    /// was already on the wire. This crate's rule for an added field is that an older peer keeps
    /// parsing; the halves that matter here are that the *absent* values are the conservative ones,
    /// because §3.3's *degrade visibly* makes an optimistic default a greyed-in action that fails.
    #[test]
    fn a_summary_written_before_the_capability_key_existed_still_parses_conservatively() {
        let old = r#"{"agent_id":"a","parent_id":null,"name":null,"agent_type":"codex-impl","harness":"codex","depth":0,"state":"Idle","reap_state":"Live","timeout":900}"#;
        let n: NodeSummary = serde_json::from_str(old).expect("an older summary must still parse");
        assert_eq!(
            n.harness_version, None,
            "an unstated version is unmeasured, which `advertised` already treats as claiming \
             nothing"
        );
        assert!(
            !n.pane,
            "an unstated surface role must be the node role, never the pane one: the pane row is \
             the *narrower* key on claude-code, so defaulting to it would understate; defaulting \
             the other way is what a reader of an old summary can safely assume, since a build \
             that had no panes wrote none"
        );
        // A summary with no version omits the field rather than writing a null, so the byte stream
        // an older build produced and the one this build produces for the same node are identical.
        let wire = serde_json::to_string(&n).unwrap();
        assert!(!wire.contains("harness_version"), "{wire}");
        assert!(wire.contains(r#""pane":false"#), "{wire}");
    }

    #[test]
    fn a_harness_report_encodes_its_measurement_in_milliseconds() {
        let r = HarnessReport {
            harness: Harness::ClaudeCode,
            harness_version: Some("2.1.220".into()),
            adapter_check: Some(true),
            notes: vec!["keystroke-injection submit check passed".into()],
            elapsed: Millis(std::time::Duration::from_micros(412_900)),
        };
        let wire = serde_json::to_string(&r).unwrap();
        assert!(
            wire.contains(r#""elapsed":412"#),
            "a measurement rounds down: {wire}"
        );
        let back: HarnessReport = serde_json::from_str(&wire).unwrap();
        // The 900 µs are gone, and that is the encoding working: a measurement is truncated on the
        // way out, so the round trip is lossy in the direction that never inflates a duration.
        assert_eq!(back.elapsed, Millis(std::time::Duration::from_millis(412)));
        assert_eq!(back.harness_version, r.harness_version);
        assert_eq!(back.adapter_check, r.adapter_check);
        assert_eq!(back.notes, r.notes);
    }

    #[test]
    fn a_detach_outcome_cannot_omit_its_guidance() {
        // §7.3.2's "never silently detach", as a shape. Both detaching variants require
        // `guidance`; removing the field is the only way to construct one without it, and that is
        // a compile error rather than a review comment.
        let o = QuitOutcome::Detached {
            detached: vec![agent("a")],
            gate_exposed: vec![agent("a")],
            guidance: DetachGuidance {
                reattach: "run `marion` in this project root".into(),
                stop_fleet: "run `marion kill --all`".into(),
            },
            supervisor: SupervisorDisposition::Resident(ResidentReason::NonTerminalNode),
        };
        let wire = serde_json::to_string(&o).unwrap();
        assert!(wire.contains("reattach") && wire.contains("stop_fleet"));
        assert_eq!(serde_json::from_str::<QuitOutcome>(&wire).unwrap(), o);
    }

    #[test]
    fn a_kill_outcome_cannot_report_reaped_nodes() {
        // The bag-of-lists shape could. This one has no field to put them in, which is the point.
        let o = QuitOutcome::Killed {
            nodes: vec![KilledNode {
                agent_id: agent("a"),
                was: NodeState::Running,
            }],
            supervisor: SupervisorDisposition::Exiting,
        };
        assert_eq!(
            serde_json::to_string(&o).unwrap(),
            r#"{"Killed":{"nodes":[{"agent_id":"a","was":"Running"}],"supervisor":"Exiting"}}"#
        );
    }
}
