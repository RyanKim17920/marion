//! The fifteen methods' results.
//!
//! Tolerant of unknown fields, unlike [`crate::proto::params`] — the module doc argues why the asymmetry is
//! deliberate rather than an oversight. The short form: an ignored parameter changes what runs, an
//! ignored result field only narrows what is shown, and a client that refuses to parse a supervisor
//! one version newer than itself has turned an additive change into an outage.
//!
//! A result is a struct even where it holds one field. `node/get` returning a bare `NodeSummary`
//! would be shorter and would make the first added field a wire break for every client.

use crate::contract::{AgentId, Oid, ResultStatus, TaskId, TokenUsage, Workspace};
use crate::node::{NodeState, ReapState};
use serde::{Deserialize, Serialize};

use crate::proto::PaneReadyTokenV1;
use crate::proto::model::{
    AttachMode, Delivery, HarnessReport, NodeSummary, QuitOutcome, ReplayPoint, ReplyOutcome,
};

/// `tree/subscribe` — the snapshot, and the point live notifications begin from.
///
/// The snapshot and the read point travel together because a snapshot without one has the same
/// seam problem §7.3.3 identifies for re-attach: a client that renders a tree and *then* starts
/// listening has a window it cannot account for. Here the answer is the same as there — the
/// snapshot is as of `read_point`, and everything after arrives as a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSubscribeResult {
    pub nodes: Vec<NodeSummary>,
    pub read_point: ReplayPoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeGetResult {
    pub node: NodeSummary,
    /// What a person looking at this one node wants beside its row: the task, what it is doing,
    /// what it spent, where it works and how it ended. `#[serde(default)]`, so a client one version
    /// older keeps parsing and one talking to an older supervisor reads an empty detail.
    #[serde(default, skip_serializing_if = "NodeDetail::is_empty")]
    pub detail: NodeDetail,
}

/// `node/get`'s detail. Every field is optional because a node reports them at different times: a
/// spawning node has no stream yet, a running one no completion, a root no contract and so no task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDetail {
    /// What marion sent it: the contract's instructions, criteria and checks. A root has no
    /// contract and so `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskSent>,
    /// The messages queued for it (steers, a child's end), oldest first: who, when, how long, and
    /// what came of each. **Never their text** — the journal keeps a length and a digest only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<MessageLine>,
    /// A page of what it is running, when the call asked for one with
    /// [`crate::proto::params::NodeGetParams::activity`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<ActivityPage>,
    /// The tokens its stream says it spent so far. `None` when the harness states none — which is
    /// not zero. Tokens only: marion never prices a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// Where it works: its worktree and branch, or the checkout it shares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<Workspace>,
    /// How its latest contract ended, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<CompletionSummary>,
    /// The tokens each turn (or step) spent, oldest first, where the harness reports usage per
    /// turn: the sparkline a watcher draws. Empty when it reports only a run total, or nothing —
    /// a client then draws no sparkline rather than an empty one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<u64>,
}

/// The task as the node received it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSent {
    /// The prompt as delivered, the agent type's prefix included, up to marion's own appended text.
    pub prompt: String,
    /// What marion appended after the prompt (its one instruction on how to report), split off so
    /// a reader can show it as marion's rather than the operator's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appended: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    /// The verification commands, one line each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification: Vec<String>,
}

/// One message queued for a node, from the journal's delivery records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageLine {
    /// When it was queued (RFC3339).
    pub at: String,
    /// Who it was from: `operator`, `ancestor <id>`, `child <id> ended <status>`.
    pub from: String,
    /// Its length in bytes.
    pub len: u32,
    /// `queued`, `delivered via <verb>`, or `dropped: <reason>`.
    pub outcome: String,
}

/// A page of a node's activity stream: the items read from byte `from` of its `events.jsonl` up to
/// byte `next`, which is where the next page starts. A client polls with `next` and so reads each
/// byte once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityPage {
    pub from: u64,
    pub next: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<ActionLine>,
    /// Set when marion reads no activity from this node's stream (its row has no activity rule),
    /// which is not the same as the node having done nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread: Option<String>,
}

/// One thing a node did: a tool call, a call's end or a message, one line, bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionLine {
    /// When marion recorded it (RFC3339).
    pub at: String,
    pub kind: ActionKind,
    pub text: String,
    /// The harness's id for the call this line is, or ends, where its stream gives one: a reader
    /// paging the stream matches a call seen again on a later page, and an end to its call, by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Call,
    /// A call that changed files: the text is `~ ` and the paths, whose ends say most.
    Files,
    /// A call finished: the text is its outcome (`✓`, `exit 1`, `failed`), not the call again.
    Ended,
    Said,
}

impl NodeDetail {
    pub fn is_empty(&self) -> bool {
        self == &NodeDetail::default()
    }
}

/// A [`crate::contract::Completion`], cut to what a row can show: the full record stays in the
/// contract file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionSummary {
    pub status: ResultStatus,
    /// The narrative the node reported, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrative: Option<String>,
    /// The branch its work landed on, and that branch's commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<Oid>,
    /// How many paths it changed.
    #[serde(default)]
    pub changed_paths: usize,
    /// What its landed branch changed against the commit it started from, from git: `None` when
    /// it landed no branch or git could not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffStat>,
    /// The process exit, as marion described it.
    #[serde(default)]
    pub exit: String,
}

/// Lines added and removed, and files touched: `git diff --shortstat`'s three numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStat {
    pub added: u32,
    pub removed: u32,
    pub files: u32,
}

/// `node/attach`. The mode is §7.3.3's per-node answer; see [`AttachMode`].
///
/// `pane` is the **display plane's** half of the same attach, and it is `Option` because most
/// nodes have none: §3.4 implements `DisplayPlane` iff `display == NativePty`, and a headless node
/// attached to over this method is answered with a replay and nothing else. `None` is therefore a
/// fact about the node, not a failure of the attach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAttachResult {
    pub node: NodeSummary,
    pub mode: AttachMode,
    /// `#[serde(default)]` for the reason the module doc gives for every result field: a client one
    /// version older must keep parsing, and a client that does not know about panes reading `None`
    /// is exactly right — it was never going to render one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneAttach>,
}

/// What a client attaching to a node **with** a display plane got.
///
/// # Why the write half is answered here and nowhere else
///
/// §5.3 gives one node one writer: two clients typing into one pty interleave at whatever
/// granularity their reads happen to have, and the pty echoes the mess back to both operators
/// identically, so neither can tell it from a harness misbehaving. The supervisor therefore leases
/// the write half, and **`node/attach`'s response is the one place a client can be told whether it
/// got it** — the inbound keystroke channel is a notification with no answer, so a refusal
/// delivered there would be a refusal nobody is listening for, repeated once per key held down.
///
/// `held_by` names the connection that has it rather than reporting a bare "busy", because §5.3's
/// refusal is required to be a sentence: a client told only that it may not type cannot tell a
/// colleague in the same node from a lease its own crashed predecessor never released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneReadyDescriptorV1 {
    pub token: PaneReadyTokenV1,
    pub cut: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAttach {
    /// The pty's size **now**, as the supervisor set it before the child existed. A client uses it
    /// to decide whether its first act is a `node/resize`, and a client that renders without
    /// asking is rendering the geometry some earlier attacher chose.
    pub cols: u16,
    pub rows: u16,
    /// Whether this client may send `node/pty-write` and `node/resize` for this node.
    pub writable: bool,
    /// The connection holding the write half, when it is not this one. `None` when `writable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<u64>,
    /// The pane generation this attach reserved has **already ended**: its child is gone and the
    /// supervisor is draining it or has drained it. Then `writable` is false for every client —
    /// including the one that was granted this node's writer lease — because there is nothing
    /// left to type into.
    ///
    /// It is a separate field because `writable: false` otherwise means the opposite thing: that
    /// somebody else is typing into a node that is still running. A client which cannot tell the
    /// two apart reports a finished node as a lease it lost, and a native relay that does so
    /// throws away the very output it attached to replay.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ended: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_ready: Option<PaneReadyDescriptorV1>,
}

/// `node/detach` — returns the node's state, which is the *evidence* that detaching did nothing.
///
/// An empty acknowledgement was the alternative and is weaker. §6.2 and §7.3.1 both rest on
/// detaching being inert; a result that hands back the unchanged state lets a client assert it,
/// and lets an operator reading a log see that a detach and a reap are different events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDetachResult {
    pub state: NodeState,
    pub reap_state: ReapState,
}

/// `node/prompt` and `node/steer` share this result — and only this result. The params are
/// separate types (see [`crate::proto::params::NodeSteerParams`]) because the two calls are refused under
/// different conditions; the *answers* are genuinely the same question: which verb was performed,
/// and what state did the node move to.
///
/// `delivered_as` is present rather than implied because §6.3's resume path makes it
/// non-obvious: a `node/prompt` against an `Exited(_)` node performed `continue_()` **then**
/// `prompt()` as one atomic registry operation, and a client that renders "prompted" without
/// knowing a resume happened will show a fresh turn on a session that was reopened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryResult {
    pub delivered_as: Delivery,
    pub state: NodeState,
    /// True iff the node was terminal and §6.3's atomic `continue_()` + `prompt()` ran.
    #[serde(default)]
    pub resumed: bool,
    /// Marion's name for the message this call handed over — the `message_id` its journal records
    /// carry — so a caller can match a later delivery or drop to this call. `None` where the call
    /// named none.
    ///
    /// **Additive**: `#[serde(default)]` so an older supervisor's result decodes, and absent from
    /// the wire when `None` so a result without one is byte-identical to what earlier builds wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// True iff the message was accepted for the node's next turn rather than delivered now. False
    /// (and absent from the wire) is the immediate delivery every earlier build performed.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub queued: bool,
    /// When a queued message reaches the node's model, in the supervisor's words and read off the
    /// node's row ("when its current run ends: …"), so an acknowledgement promises only what the
    /// row measured. `None` where nothing was queued, or from a supervisor that predates it.
    ///
    /// **Additive**, like `message_id`: absent from the wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrives: Option<String>,
}

/// `node/collected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCollectedResult {
    /// A queued announcement of the child's end was withdrawn. `false` means none was queued yet,
    /// and the announcement will be resolved when it is made.
    pub withdrawn: bool,
}

/// `node/cancel`. §6.7 requires a cancel to reach a process that exists; where it does, the node's
/// terminal is `Cancelled`, and that classification is the node's state here rather than a
/// separate boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCancelResult {
    pub state: NodeState,
    /// **Every node the cancel ended**, the named one and each descendant, deepest first — and
    /// whether marion had to kill it (`forced`: it ignored its abort, or its row has none).
    /// Additive, and absent from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<CancelledNode>,
}

/// One node a `node/cancel` ended. See [`NodeCancelResult::nodes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelledNode {
    pub agent_id: AgentId,
    pub forced: bool,
}

/// `node/kill`. Same shape as cancel and a different type, because §6.7 gives them different
/// preconditions and §7.3.2's disposition (a) is built from this one only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeKillResult {
    pub state: NodeState,
}

/// `node/rename`.
///
/// `previous_name` is returned so a client can undo, and so a journal reader can see the rename as
/// a transition rather than a fact. Nothing here reports `allow_peers`: a bound grant did not move
/// (§2, §5.4) and an unbound one is not a property of this call — it resolves at the *peer's* next
/// call, which may never come.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRenameResult {
    #[serde(default)]
    pub previous_name: Option<String>,
    pub name: String,
}

/// `permission/reply` and `elicitation/reply`. See [`ReplyOutcome`] for why `Stale` is a variant
/// and not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyResult {
    pub outcome: ReplyOutcome,
}

/// `agent/spawn`.
///
/// Returns the id and the state, **not a `TaskContract`**. §5.4 rejects `report` on a root and
/// `wait` has no contract to return there, so a root has none to hand back — the contract belongs
/// to the parent↔child relationship, and a client-spawned root has no parent. The client watches
/// the run through `tree/subscribe` and `node/attach`, which is the same path it uses for every
/// other node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpawnResult {
    pub agent_id: AgentId,
    /// `Running` once the confirmation has been followed — §6.1 step 7 journals the intent, starts
    /// the process, journals `Spawned` and the `Running` beside it, and this returns once the
    /// node exists in the registry — or `Spawning` when the registry's tail has not yet reached
    /// those records. Never a state the journal does not say.
    pub state: NodeState,
    /// **The name of the file this run's contract will be written to**, for a child; `None` for a
    /// root, which has none (§9).
    ///
    /// A field and not a sixteenth method, and not something the caller reconstructs. §11 item 28
    /// step 5 makes the agent-facing synchronous `spawn` a *client-side composition* — this call,
    /// then `node/attach`, then read `agents/<agent_id>/contracts/<task_id>.json` — because a call
    /// that blocked until the contract existed would put a minutes-long request on this wire. The
    /// composing client therefore has to know which file to read, and only the supervisor can say:
    /// the id is minted inside `agent/spawn` from marion's own entropy so that two runs can never
    /// share a contract file, so a caller that "worked it out" would be minting a second one and
    /// reading a path nothing writes.
    ///
    /// `Option`, and the two values are the two node kinds rather than a presence flag: §9 gives a
    /// root no `TaskContract`, so `None` here is a fact about the node and not an omission.
    #[serde(default)]
    pub task_id: Option<TaskId>,
    /// **Something the caller must know about the node it just started**, in marion's words — a
    /// reviewer on a harness that cannot be made read-only (`review::UNGUARDED`). Skipped on the
    /// wire when absent, so every other answer is byte for byte what it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// **A race's seats**, when the spawn named `candidates`. `agent_id` and `task_id` above are
    /// then the first launched seat's; a client waits on the race by its id instead. Absent for
    /// every other spawn, so its frame is byte for byte what it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub race: Option<RaceStarted>,
}

/// The seats a race started with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaceStarted {
    pub race_id: crate::race::RaceId,
    pub seats: Vec<SeatStarted>,
}

/// One seat as launched: its node and contract, or why it could not start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatStarted {
    pub seat: u8,
    /// `agent_type[:model]`, as asked.
    pub candidate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// marion's sentence for a seat that did not start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorRunResult {
    pub reports: Vec<HarnessReport>,
}

/// `node/resume` — the node relaunched into its **own** id (`plan-restart-resume.md` step 6).
///
/// `agent_id` is the id the caller asked for, echoed back so a client watching over
/// `tree/subscribe` knows the same node it lost is the one that came back — a resume that minted a
/// new id would be a spawn, and the whole point is that it is not. `spawn_generation` is the
/// lifetime count from replay: `2` on the first resume, more on later ones, and the field a caller
/// reads to tell "relaunched" from "was never lost". `state` is what the registry holds at the
/// same instant `agent/spawn` returns at — see [`AgentSpawnResult::state`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeResumeResult {
    pub agent_id: AgentId,
    pub state: NodeState,
    pub spawn_generation: u32,
}

/// `session/quit`. See [`QuitOutcome`] — one variant per disposition, so a detach cannot be
/// reported without its guidance and a kill cannot report reaped nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionQuitResult {
    pub outcome: QuitOutcome,
}

/// `notify/claim`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyClaimResult {
    /// Notices are on, and they reach a terminal: this claim can receive `notify/notice`. `false`
    /// where notifications are off or a desktop notifier shows them.
    pub terminal: bool,
    /// This connection is first in line.
    pub head: bool,
}

/// `session/hello`: the principal the connection now speaks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionPrincipal {
    /// The operator: every method.
    Operator,
    /// This node: what a node may do, about itself and the nodes below it.
    Node(AgentId),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHelloResult {
    pub principal: SessionPrincipal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ExitStatus;
    use crate::proto::model::{DetachGuidance, ResidentReason, SupervisorDisposition};

    /// **`node/get`'s detail is additive**: an answer from a supervisor that predates it parses
    /// with an empty detail, an empty detail is not written, and a full one round-trips.
    #[test]
    fn node_get_detail_is_optional_on_the_wire() {
        let node = serde_json::json!({
            "agent_id": "a", "parent_id": null, "name": null, "agent_type": "codex",
            "harness": "codex", "harness_version": null, "depth": 0, "state": "Running",
            "reap_state": "Live", "timeout": 60000
        });
        let old: NodeGetResult =
            serde_json::from_value(serde_json::json!({ "node": node })).expect("an old answer");
        assert!(old.detail.is_empty());
        let written = serde_json::to_value(&old).unwrap();
        assert!(written.get("detail").is_none(), "{written}");

        let full = NodeGetResult {
            node: old.node.clone(),
            detail: NodeDetail {
                task: Some(TaskSent {
                    prompt: "add a limiter".into(),
                    appended: Some("report when done".into()),
                    acceptance: vec!["tests pass".into()],
                    verification: vec!["cargo test".into()],
                }),
                messages: vec![MessageLine {
                    at: "2026-09-27T14:30:00.000Z".into(),
                    from: "operator".into(),
                    len: 42,
                    outcome: "delivered via turn".into(),
                }],
                stream: Some(ActivityPage {
                    from: 0,
                    next: 812,
                    lines: vec![ActionLine {
                        at: "2026-09-27T14:30:01.000Z".into(),
                        kind: ActionKind::Call,
                        text: "$ cargo test".into(),
                        id: Some("item_1".into()),
                    }],
                    unread: None,
                }),
                usage: Some(TokenUsage {
                    input: 10,
                    output: 2,
                    cache_read: 5,
                    cache_write: 0,
                    reasoning: None,
                }),
                workspace: Some(Workspace::Worktree {
                    path: "/w/t-1".into(),
                    branch: "marion/t-1".into(),
                }),
                completion: Some(CompletionSummary {
                    status: ExitStatus::Ok,
                    narrative: Some("done".into()),
                    branch: Some("marion/t-1".into()),
                    commit: Some(Oid("abc".into())),
                    changed_paths: 3,
                    exit: "exit 0".into(),
                    diff: None,
                }),
                turns: Vec::new(),
            },
        };
        let s = serde_json::to_string(&full).unwrap();
        assert_eq!(
            full,
            serde_json::from_str::<NodeGetResult>(&s).unwrap(),
            "{s}"
        );
    }

    #[test]
    fn results_round_trip() {
        macro_rules! rt {
            ($v:expr) => {{
                let v = $v;
                let s = serde_json::to_string(&v).unwrap();
                assert_eq!(v, serde_json::from_str(&s).unwrap(), "round trip: {s}");
            }};
        }
        rt!(NodeDetachResult {
            state: NodeState::Running,
            reap_state: ReapState::Live
        });
        rt!(DeliveryResult {
            delivered_as: Delivery::Steer,
            state: NodeState::Running,
            resumed: false,
            message_id: None,
            queued: false,
            arrives: None
        });
        rt!(DeliveryResult {
            delivered_as: Delivery::Prompt,
            state: NodeState::Running,
            resumed: true,
            message_id: None,
            queued: false,
            arrives: None
        });
        rt!(NodeCancelResult {
            state: NodeState::Exited(ExitStatus::Cancelled),
            nodes: vec![],
        });
        rt!(NodeCancelResult {
            state: NodeState::Exited(ExitStatus::Cancelled),
            nodes: vec![
                CancelledNode {
                    agent_id: AgentId("child".into()),
                    forced: true,
                },
                CancelledNode {
                    agent_id: AgentId("a".into()),
                    forced: false,
                },
            ],
        });
        rt!(NodeKillResult {
            state: NodeState::Exited(ExitStatus::Cancelled)
        });
        rt!(NodeRenameResult {
            previous_name: None,
            name: "impl".into()
        });
        rt!(ReplyResult {
            outcome: ReplyOutcome::Delivered {
                state: NodeState::Running
            }
        });
        rt!(AgentSpawnResult {
            agent_id: AgentId("a".into()),
            state: NodeState::Spawning,
            task_id: None,
            note: None,
            race: None,
        });
        rt!(AgentSpawnResult {
            agent_id: AgentId("a".into()),
            state: NodeState::Spawning,
            task_id: Some(TaskId("task-1".into())),
            note: None,
            race: None,
        });
        rt!(DoctorRunResult { reports: vec![] });
        rt!(SessionQuitResult {
            outcome: QuitOutcome::ReapedAndDetached {
                reaped: vec![AgentId("a".into())],
                detached: vec![AgentId("b".into())],
                gate_exposed: vec![AgentId("b".into())],
                guidance: DetachGuidance {
                    reattach: "run `marion` here".into(),
                    stop_fleet: "run `marion kill --all`".into()
                },
                supervisor: SupervisorDisposition::Resident(ResidentReason::BlockedNode),
            }
        });
    }

    #[test]
    fn a_result_tolerates_a_field_it_has_never_heard_of() {
        // The forward-compatibility half of the crate's unknown-field decision. A supervisor one
        // version ahead must not break a client that is one behind.
        let r: NodeCancelResult = serde_json::from_str(
            r#"{"state":"Running","signalled_at":"2026-08-05T00:00:00.000Z"}"#,
        )
        .unwrap();
        assert_eq!(r.state, NodeState::Running);
    }

    /// **A spawn answered without a `task_id` is a spawn with no contract, never a parse failure.**
    ///
    /// Two callers land here and they must not be told apart by whether the frame deserializes: a
    /// root spawn, which has no `TaskContract` at all (§9), and a supervisor built before the field
    /// existed. Both mean "there is no contract file for you to read", which is exactly what the
    /// composing client (§11 item 28 step 5) has to branch on — so the absence is a value and the
    /// `default` is what makes it one.
    #[test]
    fn a_spawn_result_without_a_task_id_is_a_node_with_no_contract() {
        let r: AgentSpawnResult =
            serde_json::from_str(r#"{"agent_id":"a","state":"Spawning"}"#).unwrap();
        assert_eq!(r.task_id, None);
    }

    #[test]
    fn pane_attach_without_readiness_keeps_the_exact_legacy_wire_shape() {
        let legacy = PaneAttach {
            cols: 80,
            rows: 24,
            writable: true,
            held_by: None,
            ended: false,
            pane_ready: None,
        };
        let json = r#"{"cols":80,"rows":24,"writable":true}"#;

        assert_eq!(serde_json::to_string(&legacy).unwrap(), json);
        assert_eq!(serde_json::from_str::<PaneAttach>(json).unwrap(), legacy);
    }

    /// `ended` and `held_by` are the two reasons a pane is read-only and they must stay
    /// separable on the wire: an older supervisor that never sends `ended` reads as a pane that
    /// has not ended, and a busy write half never sets it.
    #[test]
    fn pane_attach_says_separately_that_a_pane_ended_and_that_a_writer_holds_it() {
        let busy = r#"{"cols":80,"rows":24,"writable":false,"held_by":3}"#;
        let decoded = serde_json::from_str::<PaneAttach>(busy).unwrap();
        assert_eq!(decoded.held_by, Some(3));
        assert!(!decoded.ended, "an absent `ended` is not an ended pane");
        assert_eq!(serde_json::to_string(&decoded).unwrap(), busy);

        let ended = PaneAttach {
            cols: 80,
            rows: 24,
            writable: false,
            held_by: None,
            ended: true,
            pane_ready: None,
        };
        let json = r#"{"cols":80,"rows":24,"writable":false,"ended":true}"#;
        assert_eq!(serde_json::to_string(&ended).unwrap(), json);
        assert_eq!(serde_json::from_str::<PaneAttach>(json).unwrap(), ended);
    }

    #[test]
    fn pane_attach_readiness_round_trips_cut_boundaries() {
        for cut in [0, u64::MAX] {
            let pane = PaneAttach {
                cols: 80,
                rows: 24,
                writable: true,
                held_by: None,
                ended: false,
                pane_ready: Some(PaneReadyDescriptorV1 {
                    token: PaneReadyTokenV1::new([
                        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
                        0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
                        0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
                    ]),
                    cut,
                }),
            };

            let json = serde_json::to_string(&pane).unwrap();
            let decoded = serde_json::from_str::<PaneAttach>(&json).unwrap();
            assert_eq!(decoded, pane);
            assert_eq!(decoded.pane_ready.unwrap().cut, cut);
        }
    }

    #[test]
    fn pane_attach_readiness_requires_both_token_and_cut() {
        for invalid in [
            r#"{"cols":80,"rows":24,"writable":true,"pane_ready":{"cut":0}}"#,
            r#"{"cols":80,"rows":24,"writable":true,"pane_ready":{"token":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="}}"#,
        ] {
            assert!(
                serde_json::from_str::<PaneAttach>(invalid).is_err(),
                "accepted incomplete pane readiness descriptor: {invalid}"
            );
        }
    }

    #[test]
    fn results_pin_their_wire_shapes() {
        assert_eq!(
            serde_json::to_string(&DeliveryResult {
                delivered_as: Delivery::Prompt,
                state: NodeState::Running,
                resumed: true,
                message_id: None,
                queued: false,
                arrives: None
            })
            .unwrap(),
            r#"{"delivered_as":"Prompt","state":"Running","resumed":true}"#
        );
        assert_eq!(
            serde_json::to_string(&NodeDetachResult {
                state: NodeState::Idle,
                reap_state: ReapState::Live
            })
            .unwrap(),
            r#"{"state":"Idle","reap_state":"Live"}"#
        );
        assert_eq!(
            serde_json::to_string(&ReplyResult {
                outcome: ReplyOutcome::Delivered {
                    state: NodeState::Running
                }
            })
            .unwrap(),
            r#"{"outcome":{"Delivered":{"state":"Running"}}}"#
        );
    }

    /// **`message_id`, `queued` and `arrives` are additive in both directions.** A result from a supervisor
    /// that predates them decodes as an immediate, unnamed delivery, and a result that has neither
    /// writes the line earlier builds wrote; a queued one carries both, and round-trips.
    #[test]
    fn a_delivery_result_names_a_queued_message_and_omits_what_it_lacks() {
        let old = r#"{"delivered_as":"Steer","state":"Running","resumed":false}"#;
        let r: DeliveryResult = serde_json::from_str(old).expect("an older result must decode");
        assert_eq!(r.message_id, None);
        assert!(!r.queued);
        assert_eq!(serde_json::to_string(&r).unwrap(), old);
        let older = r#"{"delivered_as":"Steer","state":"Running"}"#;
        assert_eq!(serde_json::from_str::<DeliveryResult>(older).unwrap(), r);

        assert_eq!(r.arrives, None);
        let queued = DeliveryResult {
            message_id: Some("m-1".into()),
            queued: true,
            arrives: Some("when its current turn ends".into()),
            ..r
        };
        let line = serde_json::to_string(&queued).unwrap();
        assert_eq!(
            line,
            r#"{"delivered_as":"Steer","state":"Running","resumed":false,"message_id":"m-1","queued":true,"arrives":"when its current turn ends"}"#
        );
        assert_eq!(
            serde_json::from_str::<DeliveryResult>(&line).unwrap(),
            queued
        );
    }

    #[test]
    fn a_resumed_prompt_says_so() {
        // §6.3's resume is invisible in the state alone: the node is Running either way. A client
        // rendering "new turn" on a session that was reopened is showing the wrong history.
        let fresh = DeliveryResult {
            delivered_as: Delivery::Prompt,
            state: NodeState::Running,
            resumed: false,
            message_id: None,
            queued: false,
            arrives: None,
        };
        let resumed = DeliveryResult {
            resumed: true,
            message_id: None,
            queued: false,
            ..fresh.clone()
        };
        assert_ne!(fresh, resumed);
        assert_eq!(fresh.state, resumed.state);
    }
}
