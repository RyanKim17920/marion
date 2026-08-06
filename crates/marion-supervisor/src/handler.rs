//! The registry, answering `node/get`, `tree/subscribe`, and §7.3.2's `session/quit`.
//!
//! [`serve`](crate::serve) knows about frames and connections and nothing about nodes;
//! [`registry`](crate::registry) knows about nodes and nothing about clients. This is the seam, and
//! it is where three questions the registry deliberately refused to answer get answered — or are
//! refused again, in words, which is the other honest outcome.
//!
//! # Projecting a node is fallible, and that is the finding
//!
//! `registry.rs` states that it will not build a [`NodeSummary`] because two of its fields —
//! `timeout` and `name` — *"have no source in the journal at all"*, and that building one means
//! fabricating both. Neither is fabricated here:
//!
//! * **`timeout`** is resolved from the node's recorded `agent_type` through
//!   [`marion_core::agent_type::builtin`] — the same registry that decided the bound when the node
//!   was spawned. §3.1 makes the agent type the source of truth for it and §9 *"re-resolves it fresh
//!   on resume"*, so reading it from the type is what the spec already prescribes; a journalled copy
//!   would be the second source of truth, not the first.
//! * **`name`** is `None`, which is not a placeholder but the truth. §2's `node/rename` is the only
//!   thing that sets `Node.name`, it does not exist, and nothing has ever written one. When it lands
//!   it lands as a journal record and is read here; until then `None` is what the journal says.
//! * **`agent_type`, `harness`, `depth`** come from the `SpawnIntent`, which `marion-core` makes an
//!   `Option` precisely because a journal whose head was compacted away, or whose intent record was
//!   lost, has records *about* a node and no identity *for* it.
//!
//! So a node can be **unprojectable**, and [`summarize`] returns that rather than inventing a
//! summary. Three ways, each named: no intent at all, an `agent_type` this build does not know, and
//! a depth that will not fit `NodeSummary`'s `u8`. A `node/get` for such a node is a refusal that
//! says which fact is missing, and — see below — a `tree/subscribe` counts them rather than dropping
//! them into silence.
//!
//! # The snapshot and the subscription begin at the same instant
//!
//! §7.3.3 makes the replay-to-subscribe seam *the* correctness question, and
//! [`marion_proto::result::TreeSubscribeResult`] says why the two travel together: *"a client that
//! renders a tree and **then** starts listening has a window it cannot account for."*
//!
//! Here that is a lock, not a promise. [`RegistryHandle::subscribe`] takes the shared state's lock,
//! performs **one** read of the registry, and from that single view it (a) flushes to existing
//! subscribers everything that changed since the last flush, (b) builds this subscriber's snapshot,
//! and (c) records the snapshot as the point this subscriber has been told about. A notification
//! cannot be produced between (b) and (c), because nothing else can hold the lock, so the new
//! subscriber can neither miss an event nor be told twice about one it already has.
//!
//! # Quit acts here; departure never does
//!
//! §7.3 gives two events one name and makes confusing them the crash-safety failure. The explicit
//! [`Call::SessionQuit`] is handled under `quit`'s decision lock: validate a confirmed render,
//! durably append an intent, perform one per-node §6.7 kill, observe death, then durably confirm.
//! [`Handle::gone`] performs none of those steps. Even when the transport reports
//! [`ClientGone::Quit`], the disposition was already accepted or refused by the call; applying it
//! again at EOF would make every successful quit happen twice and every refused quit happen once.
//!
//! §5.7's exit is deliberately split once more. A successful call can make exit *eligible*, but
//! the serve loop alone knows whether the client count stayed at zero for the configured grace.
//! Only its later [`Handle::begin_idle_exit`] callback appends `SupervisorExited` and flips
//! `exiting`; this is why the record cannot be written while the response's socket is still open.
//!
//! # What is deliberately not built
//!
//! `TreeSubscribeResult` has nowhere to say *"and there are N nodes I could not describe"*. Rather
//! than omit them into silence — the accept-and-ignore shape §11 item 23 keeps naming — the count is
//! kept on the supervisor's side and exposed as [`RegistryHandle::unprojectable`], where a test can
//! see it and a `doctor` will read it. Naming the gap is not the same as closing it, and this one is
//! open until the vocabulary has a field for it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use marion_core::agent_type;
use marion_core::contract::{AgentId, ProcessExit};
use marion_core::journal::{
    KillConfirmed, KillIntent, ReapConfirmed, ReapIntent, RecordKind, SupervisorExited,
};
use marion_core::node::{NodeState, ReapState};
use marion_core::registry::{Replay, ReplayedNode};
use marion_proto::notify::Event;
use marion_proto::result::{NodeGetResult, SessionQuitResult, TreeSubscribeResult};
use marion_proto::{
    Call, ClientGone, DetachGuidance, FailureKind, KilledNode, MethodResult, NodeSummary,
    QuitDisposition, QuitOutcome, ReplayPoint, ResidentReason, RpcError, SupervisorDisposition,
};

use crate::registry::{LiveRegistry, Registry};
use crate::serve::{ConnId, Departure, Handle, Outbound};

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
/// Pure: it reads the replayed node and this build's agent-type registry, and touches nothing else.
/// That is what makes every arm above testable without a socket, a journal or a thread.
pub fn summarize(node: &ReplayedNode) -> Result<NodeSummary, Unprojectable> {
    let intent = node.intent.as_ref().ok_or(Unprojectable::NoIntent)?;
    let ty = agent_type::builtin(&intent.agent_type)
        .ok_or_else(|| Unprojectable::UnknownAgentType(intent.agent_type.clone()))?;
    let depth =
        u8::try_from(intent.depth).map_err(|_| Unprojectable::DepthOutOfRange(intent.depth))?;
    Ok(NodeSummary {
        agent_id: node.agent_id.clone(),
        parent_id: intent.parent_id.clone(),
        // Not a placeholder. See the module doc: nothing sets `Node.name` yet, so `None` is what
        // the journal says rather than what marion does not know.
        name: None,
        agent_type: intent.agent_type.clone(),
        harness: intent.harness,
        depth,
        state: node.state,
        reap_state: node.reap_state,
        timeout: ty.timeout,
    })
}

/// What a subscriber has already been told about one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Told {
    state: NodeState,
    reap_state: ReapState,
}

#[derive(Default)]
struct Shared {
    subs: Vec<Outbound>,
    clients: HashSet<ConnId>,
    told: HashMap<AgentId, Told>,
    unprojectable: usize,
}

trait QuitRuntime: Send + Sync {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool;
}

struct SystemQuitRuntime;

impl QuitRuntime for SystemQuitRuntime {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
        crate::run::kill_process_tree_and_wait(pid)
    }
}

/// A [`Handle`](crate::serve::Handle) backed by a running registry.
///
/// Descriptions still come only from the journal. Quit is the deliberately different half: it
/// uses the journal's PID to apply §6.7's process-tree kill, observes death, and writes that new
/// fact back before any client can learn it. The handle remembers only connection/exit bookkeeping,
/// never a second copy of node state.
pub struct RegistryHandle {
    live: Arc<LiveRegistry>,
    shared: Mutex<Shared>,
    runtime: Arc<dyn QuitRuntime>,
    quit: Mutex<()>,
    /// An explicit `session/quit` arrived and left nothing in §5.7's exclusion list holding.
    ///
    /// **Not** what makes exit permissible — [`RegistryHandle::idle_exit_eligible`] answers that
    /// from §5.7's own two clauses and nothing else, because a supervisor whose client was
    /// SIGKILLed is in exactly the state §5.7 permits an exit from and has no way to say so
    /// (§7.3.1). What this flag decides is only the *grace*: §5.7 justifies the wait as one that
    /// *"should outlast an operator closing one window to open another"*, and a client that called
    /// `session/quit` has said the opposite in as many words. So a departure marion cannot read
    /// waits the full grace, and a decision marion was told about does not.
    quit_waived_grace: AtomicBool,
    /// Whether the log already carries the reason [`crate::registry::Status::Stopped`] was reached.
    /// The predicate is asked on every pass of the accept loop; the fault is reported once.
    stopped_reported: AtomicBool,
    exiting: AtomicBool,
}

impl RegistryHandle {
    pub fn new(live: Arc<LiveRegistry>) -> Arc<RegistryHandle> {
        Arc::new(RegistryHandle {
            live,
            shared: Mutex::new(Shared::default()),
            runtime: Arc::new(SystemQuitRuntime),
            quit: Mutex::new(()),
            quit_waived_grace: AtomicBool::new(false),
            stopped_reported: AtomicBool::new(false),
            exiting: AtomicBool::new(false),
        })
    }

    #[cfg(test)]
    fn with_runtime(live: Arc<LiveRegistry>, runtime: Arc<dyn QuitRuntime>) -> Arc<RegistryHandle> {
        Arc::new(RegistryHandle {
            live,
            shared: Mutex::new(Shared::default()),
            runtime,
            quit: Mutex::new(()),
            quit_waived_grace: AtomicBool::new(false),
            stopped_reported: AtomicBool::new(false),
            exiting: AtomicBool::new(false),
        })
    }

    /// How many nodes the journal knows about that marion could not describe to a client.
    ///
    /// See the module doc: `TreeSubscribeResult` has no field for this, so rather than dropping the
    /// nodes into silence the supervisor counts them here. It is the honest half of an admitted gap,
    /// not a substitute for closing it.
    pub fn unprojectable(&self) -> usize {
        lock(&self.shared).unprojectable
    }

    pub fn subscribers(&self) -> usize {
        lock(&self.shared).subs.len()
    }

    /// Push everything that changed since the last flush to every subscriber.
    ///
    /// Returns how many notifications were produced — **per event, not per delivery** — so a caller
    /// can tell "nothing changed" from "nothing was delivered", which are the same zero from the
    /// socket's side and different problems.
    pub fn flush(&self) -> usize {
        let mut g = lock(&self.shared);
        let events = self.live.read(|r| collect(r, &mut g));
        deliver(&mut g, &events);
        events.len()
    }

    /// §2's `tree/subscribe`: the snapshot, and the point live notifications begin from.
    fn subscribe(&self, out: &Outbound) -> TreeSubscribeResult {
        let mut g = lock(&self.shared);
        // **One read, three uses.** See the module doc: catching up existing subscribers, building
        // this one's snapshot, and recording what it has been told all happen against the same view,
        // under one lock, so there is no instant at which a notification could slip between the
        // snapshot and the subscription.
        let (events, nodes, read_point) = self.live.read(|r| {
            let events = collect(r, &mut g);
            let nodes = project(r.tree(), &mut g);
            (events, nodes, r.read_point())
        });
        deliver(&mut g, &events);
        g.subs.push(out.clone());
        TreeSubscribeResult { nodes, read_point }
    }

    fn node_get(&self, id: &AgentId) -> Result<NodeGetResult, RpcError> {
        self.live.read(|r| match r.tree().get(id) {
            None => Err(RpcError::not_found(
                &id.0,
                format!(
                    "this project's journal records no node `{}`. The registry is current as of \
                     {} records read; a node spawned by another process appears here once its \
                     `SpawnIntent` is on disk (§6.1 step 7).",
                    id.0,
                    r.read_point().records
                ),
                "§3.2",
            )),
            Some(node) => summarize(node)
                .map(|node| NodeGetResult { node })
                .map_err(|e| e.as_error(id)),
        })
    }

    /// §7.3.2's voluntary path. The mutex is not throughput machinery; it makes the rendered-set
    /// comparison and the first intent one indivisible decision. Without it, two clients can both
    /// confirm the same live render and each signal it after the other's confirmation.
    fn session_quit(&self, disposition: &QuitDisposition) -> Result<SessionQuitResult, RpcError> {
        let _decision = lock(&self.quit);
        self.live.refresh();
        let answered = match disposition {
            QuitDisposition::KillTree { confirmed } => self.kill_tree(confirmed),
            QuitDisposition::DetachAll => Ok(SessionQuitResult {
                outcome: self.detach_all(),
            }),
            QuitDisposition::ReapIdleDetachBusy => self.reap_idle_detach_busy(),
        };
        // **Recorded on the disposition, never on its answer.** A quit that was told `Resident` is
        // not a quit that failed — the client still left, and the clause holding the supervisor can
        // clear a millisecond later. Reading the waiver off the answer would make an operator who
        // quit while one node was mid-turn wait §5.7's full grace after it finished, while an
        // operator who quit a second later did not; the two said the same thing.
        //
        // A **refused** quit sets nothing: `kill_tree` rejects a stale render before it signals
        // anything, and that client has not left, it has been told to render again.
        if answered.is_ok() {
            self.quit_waived_grace.store(true, Ordering::SeqCst);
        }
        answered
    }

    fn nodes(&self) -> Vec<marion_core::registry::ReplayedNode> {
        self.live.read(|r| r.tree().nodes().to_vec())
    }

    fn journal_path(&self) -> PathBuf {
        self.live.read(|r| r.path().to_path_buf())
    }

    fn journal(&self) -> Result<crate::journal::Journal, RpcError> {
        crate::journal::Journal::open_path(&self.journal_path(), crate::journal::writer_id())
            .map_err(|e| {
                RpcError::internal(format!(
                    "session/quit could not open the journal, so it refused before changing any \
                     node: {e}"
                ))
            })
    }

    fn guidance(&self) -> DetachGuidance {
        let socket = self
            .journal_path()
            .parent()
            .map(|p| p.join("supervisor.sock"))
            .unwrap_or_else(|| PathBuf::from("supervisor.sock"));
        DetachGuidance {
            reattach: format!(
                "Reconnect to {} and call tree/subscribe. Until then, any named gate_exposed node \
                 that reaches a permission request burns its bound and is denied unattended; the \
                 far side receives an is_error:true tool_result (§7.3.2, §11 item 22, S9).",
                socket.display()
            ),
            stop_fleet: format!(
                "Reconnect to {} and call session/quit with KillTree confirmed against a fresh \
                 tree/subscribe render.",
                socket.display()
            ),
        }
    }

    fn active(nodes: &[marion_core::registry::ReplayedNode]) -> Vec<AgentId> {
        nodes
            .iter()
            .filter(|n| !n.state.is_exited() && n.reap_state == ReapState::Live)
            .map(|n| n.agent_id.clone())
            .collect()
    }

    /// §5.7's exclusion list, and the one thing it is **not** about.
    ///
    /// Every clause below asks the same underlying question — *is there work here that exiting
    /// would strand?* A node whose `SpawnAborted` is journaled has none: `RecordKind::SpawnAborted`
    /// is *"the intent's other resolution"*, written when marion abandoned the spawn, and §7.2 is
    /// emphatic that *"a node marion decided the fate of is never `Orphaned`"*. There is no process,
    /// there never was one, and nothing about it can ever change again.
    ///
    /// It has to be filtered explicitly because it does not show up as one. `NodeState` has no
    /// "never started" variant and `registry.rs` deliberately does not synthesise one — the abort is
    /// a separate recorded fact, not a state transition — so an aborted node's state stays
    /// `Spawning` forever and satisfies **two** clauses below on its own.
    ///
    /// **This was measured, not reasoned about.** Before the detached supervisor existed, nothing
    /// acted on this function's answer for longer than one process's life; wiring §10's split made
    /// a `marion run` whose root failed to launch leave a supervisor that would never exit, holding
    /// a project directory that had already been deleted. That is the *inverse* of what §5.7's
    /// exclusion list is for.
    ///
    /// **The exclusion is exactly as wide as that argument and no wider.** *"There is no process
    /// and there never was one"* is a claim about the journal's **other** records, not about the
    /// abort on its own. `run.rs`'s `AbortOnDrop` is armed across the whole synchronous child run —
    /// from before the process exists to after it is reaped — so an unwind anywhere in between
    /// writes `SpawnAborted` beside a `Spawned` that names a live pid, and a `std::process::Child`
    /// dropped rather than waited on does not kill what it holds. See [`Self::abandoned`].
    fn resident_reason(nodes: &[marion_core::registry::ReplayedNode]) -> Option<ResidentReason> {
        let holding: Vec<_> = nodes.iter().filter(|n| !Self::abandoned(n)).collect();
        if holding.iter().any(|n| n.reap_intent.is_some()) {
            Some(ResidentReason::UnconfirmedReapIntent)
        } else if holding
            .iter()
            .any(|n| matches!(n.state, NodeState::Blocked(_)))
        {
            Some(ResidentReason::BlockedNode)
        } else if holding.iter().any(|n| n.state == NodeState::Spawning) {
            Some(ResidentReason::SpawnOutstanding)
        } else if holding.iter().any(|n| !n.state.is_exited()) {
            Some(ResidentReason::NonTerminalNode)
        } else {
            None
        }
    }

    /// §5.7's exit predicate as this supervisor can actually answer it — the exclusion list, and
    /// **whether the list is being read off a tree that is still the journal's**.
    ///
    /// `registry.rs` stops following at a line it cannot parse and is right to (§7.4): *"an
    /// authority may not keep serving a tree from a file it no longer recognises."* What that costs
    /// one level up is not in §5.7 at all — the exclusion list is then evaluated against the prefix
    /// as it stood *before* the corruption, so a node that has since exited is reported
    /// non-terminal forever and nothing short of a signal ends the process.
    ///
    /// Failing closed is the right half of that and is kept. What is fixed here is the **answer**:
    /// the operator was told `Resident(NonTerminalNode)` and sent looking for a node, when the
    /// truth is that marion stopped reading. [`ResidentReason::RegistryStopped`] says so, and the
    /// reason and offset — which the `Copy` enum cannot carry — go to the supervisor's log once.
    ///
    /// **This does not clear the condition** and is not meant to; see §11 item 29.
    fn residency(&self) -> Option<ResidentReason> {
        if let Some(reason) = self.registry_stopped() {
            if !self.stopped_reported.swap(true, Ordering::SeqCst) {
                eprintln!(
                    "marion-supervisor: this project's journal stopped being followable and this \
                     supervisor is answering §5.7 from the tree as it stood before that point: \
                     {reason}. It will not exit while that reading holds (§7.4, §11 item 29)."
                );
            }
            return Some(ResidentReason::RegistryStopped);
        }
        Self::resident_reason(&self.nodes())
    }

    /// The reason the registry gave up following, if it has.
    fn registry_stopped(&self) -> Option<String> {
        self.live.read(|r| match r.status() {
            crate::registry::Status::Stopped { reason } => Some(reason.clone()),
            _ => None,
        })
    }

    /// A node marion abandoned **before there was anything to abandon**, which is the only shape
    /// §7.2's *"a node marion decided the fate of is never `Orphaned`"* licenses excluding.
    ///
    /// Four facts and not one, because the abort record alone does not carry the claim. `Spawned`
    /// is written when the process exists and carries its pid; either of those present means the
    /// abort was written *over* a live child — `run.rs`'s guard covers the whole run and unwinds
    /// through the reap, and dropping a `Child` does not signal it. A reap intent present is §7.2's
    /// own crash window and holds regardless of how the spawn ended.
    ///
    /// The b600d82 case — `SpawnIntent` then `SpawnAborted`, nothing else, which is exactly what a
    /// `marion run` whose root failed to launch journals — still satisfies all four.
    fn abandoned(n: &marion_core::registry::ReplayedNode) -> bool {
        n.spawn_aborted.is_some()
            && !n.spawn_confirmed
            && n.pid.is_none()
            && n.reap_intent.is_none()
    }

    fn detach_all(&self) -> QuitOutcome {
        let nodes = self.nodes();
        let detached = Self::active(&nodes);
        let supervisor = match self.residency() {
            Some(reason) => SupervisorDisposition::Resident(reason),
            None => SupervisorDisposition::Exiting,
        };
        QuitOutcome::Detached {
            gate_exposed: detached.clone(),
            detached,
            guidance: self.guidance(),
            supervisor,
        }
    }

    fn kill_tree(&self, confirmed: &[AgentId]) -> Result<SessionQuitResult, RpcError> {
        let nodes = self.nodes();
        let targets: Vec<_> = nodes.iter().filter(|n| !n.state.is_exited()).collect();
        let mut expected: Vec<_> = targets.iter().map(|n| n.agent_id.0.clone()).collect();
        let mut stated: Vec<_> = confirmed.iter().map(|id| id.0.clone()).collect();
        expected.sort();
        stated.sort();
        if expected != stated {
            return Err(RpcError::refused(
                "confirmed",
                format!(
                    "the confirmed kill list does not equal the supervisor's current non-terminal \
                     set; confirmed {stated:?}, current {expected:?}. Nothing was signalled. Render \
                     the list again and confirm that exact set (§7.3.2)."
                ),
                "§7.3.2",
            ));
        }
        if let Some(node) = targets
            .iter()
            .find(|n| n.reap_state != ReapState::ReapedIdle && n.pid.is_none())
        {
            return Err(RpcError::conflict(
                &node.agent_id.0,
                "the confirmed node has no recorded PID yet, so marion cannot prove a signal \
                 reaches it. Nothing was signalled; retry after its spawn resolves.",
                "§6.7, §7.3.2",
            ));
        }

        let mut journal = self.journal()?;
        let mut killed = Vec::with_capacity(targets.len());
        for node in targets {
            journal
                .append(RecordKind::KillIntent(KillIntent {
                    agent_id: node.agent_id.clone(),
                    was: node.state,
                }))
                .map_err(journal_failure_before_signal)?;
            let signal = node.reap_state != ReapState::ReapedIdle;
            if signal
                && !self.runtime.kill_process_tree_and_wait(
                    node.pid.expect("preflight required a signal target PID"),
                )
            {
                return Err(RpcError::internal(format!(
                    "marion journaled the kill intent for `{}` and signalled its per-node process \
                     tree, but could not observe its PID dead; the intent remains unconfirmed and \
                     the supervisor will not exit (§6.7, §5.7)",
                    node.agent_id.0
                )));
            }
            journal
                .append(RecordKind::KillConfirmed(KillConfirmed {
                    agent_id: node.agent_id.clone(),
                    exit: ProcessExit {
                        code: None,
                        signal: signal.then_some(9),
                        description: if signal {
                            "marion sent SIGKILL for confirmed session/quit KillTree".into()
                        } else {
                            "confirmed session/quit retired an already ReapedIdle node; no process \
                             existed to signal"
                                .into()
                        },
                    },
                }))
                .map_err(journal_failure_after_signal)?;
            killed.push(KilledNode {
                agent_id: node.agent_id.clone(),
                was: node.state,
            });
        }
        self.live.refresh();
        Ok(SessionQuitResult {
            outcome: QuitOutcome::Killed {
                nodes: killed,
                supervisor: SupervisorDisposition::Exiting,
            },
        })
    }

    fn reap_idle_detach_busy(&self) -> Result<SessionQuitResult, RpcError> {
        let nodes = self.nodes();
        let reaping: Vec<_> = nodes
            .iter()
            .filter(|n| {
                n.state == NodeState::Idle
                    && n.reap_state == ReapState::Live
                    && n.reap_intent.is_none()
                    // A non-terminal child is the node its parent's blocking `spawn` is waiting
                    // on. §7.2 names that as a separate refusal even if the child happens to be
                    // between turns and reports `Idle`; reaping it would strand the caller because
                    // ReapedIdle writes no Completion. Roots have no waiting spawn by construction.
                    && n.parent_id().is_none()
            })
            .collect();
        if let Some(node) = reaping.iter().find(|n| n.pid.is_none()) {
            return Err(RpcError::conflict(
                &node.agent_id.0,
                "the idle node has no recorded PID, so marion cannot perform and confirm §7.2's \
                 reap without inventing an observation. Nothing was reaped.",
                "§7.2",
            ));
        }
        let reaped_ids: Vec<_> = reaping.iter().map(|n| n.agent_id.clone()).collect();
        let detached: Vec<_> = nodes
            .iter()
            .filter(|n| {
                !n.state.is_exited()
                    && n.reap_state == ReapState::Live
                    && !reaped_ids.contains(&n.agent_id)
            })
            .map(|n| n.agent_id.clone())
            .collect();
        let mut journal = self.journal()?;
        for node in reaping {
            journal
                .append(RecordKind::ReapIntent(ReapIntent {
                    agent_id: node.agent_id.clone(),
                    reason: "session/quit reaped an idle node before detaching busy work".into(),
                }))
                .map_err(journal_failure_before_signal)?;
            let pid = node.pid.expect("preflight required every reap PID");
            if !self.runtime.kill_process_tree_and_wait(pid) {
                return Err(RpcError::internal(format!(
                    "marion journaled the reap intent for `{}` and signalled its per-node process \
                     tree, but could not observe PID {pid} dead; the intent remains unconfirmed and \
                     the supervisor will not exit (§7.2, §5.7)",
                    node.agent_id.0
                )));
            }
            journal
                .append(RecordKind::ReapConfirmed(ReapConfirmed {
                    agent_id: node.agent_id.clone(),
                }))
                .map_err(journal_failure_after_signal)?;
        }
        self.live.refresh();
        let supervisor = self
            .residency()
            .map(SupervisorDisposition::Resident)
            .unwrap_or(SupervisorDisposition::Exiting);
        let mut guidance = self.guidance();
        // §7.3.2(c) requires the reaped list and the fact that it is resumable. The ids already
        // have a typed field; the latter does not, so omitting it here would make a structurally
        // complete response still leave out the operator's most important recovery fact.
        guidance.reattach.push_str(
            " Nodes named in `reaped` are ReapedIdle, retain their transcripts and ownership, and \
             are resumable.",
        );
        Ok(SessionQuitResult {
            outcome: QuitOutcome::ReapedAndDetached {
                reaped: reaped_ids,
                gate_exposed: detached.clone(),
                detached,
                guidance,
                supervisor,
            },
        })
    }
}

fn journal_failure_before_signal(error: crate::journal::JournalError) -> RpcError {
    RpcError::internal(format!(
        "session/quit could not durably journal its intent, so it refused before signalling the \
         node: {error}"
    ))
}

fn journal_failure_after_signal(error: crate::journal::JournalError) -> RpcError {
    RpcError::internal(format!(
        "session/quit changed a process but could not durably journal its confirmation; its intent \
         remains for restart recovery and the supervisor will not exit: {error}"
    ))
}

impl Handle for RegistryHandle {
    fn connected(&self, conn: ConnId) {
        lock(&self.shared).clients.insert(conn);
    }

    fn call(&self, _conn: ConnId, call: &Call, out: &Outbound) -> Result<MethodResult, RpcError> {
        match call {
            Call::NodeGet(p) => self.node_get(&p.agent_id).map(MethodResult::NodeGet),
            Call::TreeSubscribe(_) => Ok(MethodResult::TreeSubscribe(self.subscribe(out))),
            Call::SessionQuit(p) => self
                .session_quit(&p.disposition)
                .map(MethodResult::SessionQuit),
            // Everything else is specified and not built. `Unimplemented` and not `Unsupported`,
            // per `error.rs`: the gap is marion's, not the harness's, and the operator's next move
            // is to check the milestone rather than the node.
            other => Err(RpcError::unimplemented(
                other.method().as_str(),
                format!(
                    "`{}` is specified (§2) and not built. This supervisor answers `node/get`, \
                     `tree/subscribe`, and `session/quit`; the remaining twelve methods land with \
                     the milestone that needs them.",
                    other.method().as_str()
                ),
                "§2",
            )),
        }
    }

    /// §7.3.1, and the whole of what this handler does about a departure.
    ///
    /// **Nothing happens to any node here**, in either case. For [`ClientGone::SocketClosed`] that
    /// is §7.3.1's invariant: no state, journal, reap, or orphan transition. For
    /// [`ClientGone::Quit`], the explicit call already completed or refused the disposition; doing
    /// it at departure would repeat a successful kill and, worse, turn a refused stale
    /// confirmation into an unconfirmed kill triggered by EOF.
    ///
    /// What does happen is bookkeeping this connection's subscription is dropped, so a supervisor
    /// with no clients holds no queues. It may make a previously requested exit eligible for the
    /// serve loop's grace timer, but `gone` itself neither journals nor exits.
    fn gone(&self, conn: ConnId, gone: &ClientGone, _why: &Departure) {
        debug_assert!(
            gone.nodes_must_be_untouched() || gone.disposition().is_some(),
            "§7.3.1 admits exactly two readings and both are handled"
        );
        let mut g = lock(&self.shared);
        g.subs.retain(|s| s.conn() != conn);
        g.clients.remove(&conn);
    }

    fn exiting(&self) -> bool {
        self.exiting.load(Ordering::SeqCst)
    }

    /// §5.7's exit predicate, in full and with nothing added to it.
    ///
    /// *"With **zero clients and zero non-terminal nodes**, the supervisor MAY exit after an idle
    /// grace period"* — two clauses, and neither of them is *"and some client asked nicely first"*.
    /// This used to be gated on an explicit `session/quit` having arrived, which meant the one
    /// departure §7.3.1 is actually about — a client that was killed and could say nothing — left
    /// a supervisor that could never exit at all, over a journal with nothing left in it. That is
    /// not §7.3.1's invariant. §7.3.1 is about **nodes**, and this touches none: an empty registry
    /// has nothing to touch, and a non-empty one is what [`Self::resident_reason`] answers with.
    ///
    /// The waiting is the accept loop's, and it is what covers the window before the starting
    /// client has connected: a supervisor is published and dialled within milliseconds, and §5.7's
    /// grace is five minutes. See [`crate::serve::DEFAULT_IDLE_GRACE`] — a supervisor launched with
    /// a grace of zero really could leave before its launcher arrived, which is one of the reasons
    /// `marion run` no longer asks for one.
    fn idle_exit_eligible(&self) -> bool {
        if !lock(&self.shared).clients.is_empty() {
            return false;
        }
        self.live.refresh();
        self.residency().is_none()
    }

    fn idle_exit_grace_waived(&self) -> bool {
        self.quit_waived_grace.load(Ordering::SeqCst)
    }

    fn begin_idle_exit(&self) -> bool {
        let _decision = lock(&self.quit);
        if !self.idle_exit_eligible() {
            return false;
        }
        let result = self.journal().and_then(|mut journal| {
            journal
                .append(RecordKind::SupervisorExited(SupervisorExited {}))
                .map(|_| ())
                .map_err(journal_failure_after_signal)
        });
        match result {
            Ok(()) => {
                self.live.refresh();
                self.exiting.store(true, Ordering::SeqCst);
                true
            }
            Err(error) => {
                eprintln!("marion: {error}");
                false
            }
        }
    }
}

/// The tree, as summaries, counting what could not be described.
fn project(tree: &Replay, g: &mut Shared) -> Vec<NodeSummary> {
    let mut out = Vec::new();
    let mut lost = 0usize;
    for n in tree.nodes() {
        match summarize(n) {
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
fn collect(r: &Registry, g: &mut Shared) -> Vec<Event> {
    let mut events = Vec::new();
    for n in r.tree().nodes() {
        let now = Told {
            state: n.state,
            reap_state: n.reap_state,
        };
        match g.told.get(&n.agent_id) {
            None => {
                // A node marion cannot describe produces no `tree/node-added` — there is no summary
                // to put in one — but it is still recorded as told, so it is not re-examined on
                // every flush. `project` is what counts it.
                if let Ok(node) = summarize(n) {
                    events.push(Event::NodeAdded {
                        node,
                        ts: journal_ts(n.first_ts),
                    });
                }
                g.told.insert(n.agent_id.clone(), now);
            }
            Some(before) if *before != now => {
                events.push(Event::NodeState {
                    agent_id: n.agent_id.clone(),
                    state: n.state,
                    reap_state: n.reap_state,
                    ts: journal_ts(n.state_ts),
                });
                g.told.insert(n.agent_id.clone(), now);
            }
            Some(_) => {}
        }
    }
    events
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

/// Send to every subscriber, dropping the ones that have gone.
///
/// [`Outbound::send`] never blocks, so this cannot be slowed by a client — see `serve.rs`: a full
/// queue is a verdict about that client, and §5.7 is what makes it the right one.
fn deliver(g: &mut Shared, events: &[Event]) {
    if events.is_empty() {
        return;
    }
    g.subs.retain(|s| {
        events.iter().all(|e| {
            s.send(&marion_proto::Frame::Notification(
                marion_proto::Notification::new(e.clone()),
            ))
        })
    });
}

/// A [`RegistryHandle`] flushed by a thread of its own.
///
/// The registry has its own follower (`LiveRegistry`) and this is a second loop over the result of
/// the first, which is deliberate: the follower's job is to be *current*, this one's is to be
/// *heard*, and a follower that also pushed would have a client's socket inside the lock that keeps
/// the tree current.
pub struct Broadcast {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Broadcast {
    /// Flush every `interval`.
    ///
    /// **The loop reads the stop flag before its flush and returns after it** — `LiveRegistry`'s
    /// rule, for the same reason: the transitions written just before a shutdown are exactly the
    /// ones a watching client cares about, and a loop that returned on the flag before flushing
    /// would drop them.
    pub fn start(handle: Arc<RegistryHandle>, interval: std::time::Duration) -> Broadcast {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                loop {
                    let done = stop.load(std::sync::atomic::Ordering::SeqCst);
                    handle.flush();
                    if done {
                        return;
                    }
                    std::thread::sleep(interval);
                }
            })
        };
        Broadcast {
            stop,
            thread: Some(thread),
        }
    }

    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Broadcast {
    fn drop(&mut self) {
        self.halt();
    }
}

/// A poisoned lock is taken, not unwrapped — `registry.rs`'s rule and §5.7's requirement.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The read point the registry is currently serving, for a caller that wants it without a client.
pub fn read_point(live: &LiveRegistry) -> ReplayPoint {
    live.read(|r| r.read_point())
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::{ExitStatus, ProcessExit};
    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::journal::{
        Exited, JournalRecord, RecordKind, SpawnIntent, Spawned, StateChanged, WriterId, encode,
    };
    use marion_core::node::BlockReason;
    use marion_proto::Frame;
    use marion_testsupport::scratch;
    use std::io::Write;
    use std::path::Path;

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    fn line(seq: u64, ms: u64, kind: RecordKind) -> Vec<u8> {
        encode(&JournalRecord {
            writer: WriterId("w".into()),
            seq,
            ts: SystemTime::from_unix_millis(ms),
            mono_ns: seq,
            provenance: marion_core::ir::Provenance::marion(),
            src_seq: None,
            kind,
        })
        .expect("a record encodes")
    }

    /// The harness comes from the agent type, as a real writer's would — and the projection reads
    /// it back from the **journal**, not from the type, because the journal records what was
    /// actually launched.
    fn intent(agent: &str, parent: Option<&str>, ty: &str, depth: u32) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id(agent),
            parent_id: parent.map(id),
            agent_type: ty.into(),
            harness: agent_type::builtin(ty)
                .map(|t| t.harness)
                .unwrap_or(Harness::Codex),
            depth,
            task_id: None,
        })
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    fn replay_of(records: &[Vec<u8>]) -> Replay {
        let mut r = Replay::default();
        for b in records {
            r.extend(b);
        }
        r
    }

    fn node_of(records: &[Vec<u8>], agent: &str) -> ReplayedNode {
        replay_of(records).get(&id(agent)).unwrap().clone()
    }

    /// The projection, on a node the journal fully describes — including the two fields
    /// `registry.rs` refused to fabricate.
    #[test]
    fn a_summary_resolves_its_bound_from_the_agent_type_and_names_nothing_it_was_not_told() {
        let n = node_of(
            &[line(0, 1, intent("child", Some("root"), "codex-impl", 1))],
            "child",
        );
        let s = summarize(&n).expect("a fully described node projects");
        assert_eq!(s.agent_id, id("child"));
        assert_eq!(s.parent_id, Some(id("root")));
        assert_eq!(s.agent_type, "codex-impl");
        assert_eq!(s.harness, Harness::Codex);
        assert_eq!(s.depth, 1);
        assert_eq!(s.state, NodeState::Spawning);
        assert_eq!(
            s.timeout,
            agent_type::builtin("codex-impl").unwrap().timeout,
            "§3.1 makes the agent type the source of the bound and §9 re-resolves it from there; \
             a journalled copy would be a second source of truth"
        );
        assert_eq!(
            s.timeout,
            marion_core::encoding::Duration::from_secs(agent_type::DEFAULT_TIMEOUT_SECS),
            "and that source really is §3.1's 900 s default, not a value invented here"
        );
        assert_eq!(
            s.name, None,
            "nothing sets `Node.name` yet, so `None` is what the journal says rather than a \
             placeholder for what marion does not know"
        );
    }

    /// **NC — a node marion cannot describe is refused by name, never summarized with invented
    /// fields.**
    ///
    /// Three ways, three sentences. The failure this rules out is the tempting one: default the
    /// missing fields (`Harness::Codex`, depth 0, the 900 s bound) and hand back a summary that
    /// reads exactly like a real node's. A client cannot tell those apart, which is the
    /// partial-presented-as-complete shape this repo keeps refusing.
    #[test]
    fn an_undescribable_node_is_named_rather_than_filled_in() {
        // (1) records about a node, no identity for it.
        let orphan = node_of(
            &[line(
                0,
                1,
                RecordKind::Spawned(Spawned {
                    agent_id: id("no-intent"),
                    harness_version: "0.9.0".into(),
                    model: None,
                    pid: Some(3),
                }),
            )],
            "no-intent",
        );
        assert_eq!(summarize(&orphan), Err(Unprojectable::NoIntent));
        let e = Unprojectable::NoIntent.as_error(&id("no-intent"));
        assert_eq!(e.kind(), Some(FailureKind::Internal));
        assert!(e.message.contains("SpawnIntent"), "{e}");

        // (2) an agent type this build does not have — so §3.1's bound has no source.
        let unknown = node_of(&[line(0, 1, intent("a", None, "codex-turbo", 0))], "a");
        assert_eq!(
            summarize(&unknown),
            Err(Unprojectable::UnknownAgentType("codex-turbo".into()))
        );
        let e = Unprojectable::UnknownAgentType("codex-turbo".into()).as_error(&id("a"));
        assert_eq!(
            e.kind(),
            Some(FailureKind::NotFound),
            "`error.rs` names an agent type among the things NotFound is for"
        );
        assert!(e.message.contains("codex-turbo"), "{e}");

        // (3) a depth `NodeSummary`'s u8 cannot hold. §6.1's default max_depth is 3, so this is a
        // writer producing nonsense — and saturating it would place the node somewhere it is not.
        let deep = node_of(&[line(0, 1, intent("a", None, "codex-impl", 300))], "a");
        assert_eq!(summarize(&deep), Err(Unprojectable::DepthOutOfRange(300)));
        assert!(
            summarize(&node_of(
                &[line(0, 1, intent("a", None, "codex-impl", 255))],
                "a"
            ))
            .is_ok(),
            "255 fits, so the boundary is the type's and not an arbitrary cap"
        );
    }

    /// Build a handle over a journal file, plus the recording sink a subscriber would be.
    struct Fx {
        _dir: marion_testsupport::Scratch,
        path: std::path::PathBuf,
        handle: Arc<RegistryHandle>,
    }

    fn fx(tag: &str) -> Fx {
        fx_with(tag, vec![intent("root", None, "claude", 0)])
    }

    fn fx_with(tag: &str, records: Vec<RecordKind>) -> Fx {
        fx_with_runtime(tag, records, Arc::new(SystemQuitRuntime))
    }

    /// **The registry boots before the records are written, which is the production order.**
    ///
    /// `marion run` starts the supervisor and *then* journals its root, so every node these tests
    /// are about is a node the supervisor watched arrive. Writing the journal first and booting
    /// over it is a different situation entirely — §7.2's restart, where a node already `Live` at
    /// boot is one this supervisor has no record of deciding and is marked `Orphaned`
    /// (`restart.rs`). A fixture in that shape would have every test below asserting over a tree of
    /// orphans while claiming to describe a live fleet. `registry.rs` covers the restart order
    /// directly.
    fn fx_with_runtime(tag: &str, records: Vec<RecordKind>, runtime: Arc<dyn QuitRuntime>) -> Fx {
        let dir = scratch(tag);
        let path = dir.join("journal.jsonl");
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot_path(&path).unwrap(),
            std::time::Duration::from_millis(2),
        ));
        for (seq, kind) in records.into_iter().enumerate() {
            append(&path, &line(seq as u64, 1_000 + seq as u64, kind));
        }
        assert!(
            live.read(|r| r.restart_marks().is_empty()),
            "the supervisor booted over an empty journal; it lost nothing"
        );
        live.refresh();
        Fx {
            _dir: dir,
            path,
            handle: RegistryHandle::with_runtime(live, runtime),
        }
    }

    /// Records the process-tree operations selected by a disposition without asking the host
    /// process table to cooperate. The real runtime delegates to `run.rs`; these tests are about
    /// the handler's selection and ordering, so an injected observation makes a missed or extra
    /// per-node operation an exact assertion rather than a timing-dependent survivor check.
    #[derive(Default)]
    struct RecordingRuntime {
        killed: Mutex<Vec<i32>>,
    }

    impl RecordingRuntime {
        fn killed(&self) -> Vec<i32> {
            lock(&self.killed).clone()
        }
    }

    impl QuitRuntime for RecordingRuntime {
        fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
            lock(&self.killed).push(pid);
            true
        }
    }

    fn recording_fx_with(tag: &str, records: Vec<RecordKind>) -> (Fx, Arc<RecordingRuntime>) {
        let runtime = Arc::new(RecordingRuntime::default());
        let fx = fx_with_runtime(tag, records, runtime.clone());
        (fx, runtime)
    }

    fn spawned(agent: &str, pid: i32) -> RecordKind {
        RecordKind::Spawned(Spawned {
            agent_id: id(agent),
            harness_version: "test".into(),
            model: None,
            pid: Some(pid),
        })
    }

    fn state(agent: &str, state: NodeState) -> RecordKind {
        RecordKind::StateChanged(StateChanged {
            agent_id: id(agent),
            state,
        })
    }

    fn quit(
        fx: &Fx,
        disposition: marion_proto::QuitDisposition,
    ) -> Result<marion_proto::result::SessionQuitResult, RpcError> {
        let out = crate::serve::sink(ConnId(9));
        match fx.handle.call(
            ConnId(9),
            &Call::SessionQuit(marion_proto::params::SessionQuitParams { disposition }),
            &out,
        )? {
            MethodResult::SessionQuit(r) => Ok(r),
            other => panic!("wrong result: {}", other.method().as_str()),
        }
    }

    fn journal_tags(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                value["kind"]
                    .as_object()
                    .and_then(|o| o.keys().next())
                    .cloned()
                    .unwrap_or_else(|| value["kind"].as_str().unwrap_or("unknown").to_string())
            })
            .collect()
    }

    /// A pair of `Outbound`s is only obtainable from a live connection, so the subscription tests
    /// run a real server over a real socket — which is also the only way to assert that the
    /// notification reaches a *client* rather than a channel.
    struct Wired {
        fx: Fx,
        server: Option<crate::serve::Server>,
        dir: std::path::PathBuf,
        sock: std::path::PathBuf,
    }

    impl Wired {
        fn new(tag: &str) -> Wired {
            let fx = fx(tag);
            let dir = std::path::PathBuf::from(format!("/tmp/mh-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let paths = crate::socket::socket_paths(&dir, Path::new("/p"), 1);
            let crate::socket::Acquired::Serving(serving) = crate::socket::acquire(&paths).unwrap()
            else {
                panic!("nothing was listening")
            };
            let server = crate::serve::Server::start(
                serving,
                Arc::clone(&fx.handle) as Arc<dyn crate::serve::Handle>,
            );
            Wired {
                fx,
                server: Some(server),
                sock: paths.socket().to_path_buf(),
                dir,
            }
        }

        fn dial(&self) -> std::os::unix::net::UnixStream {
            let s = std::os::unix::net::UnixStream::connect(&self.sock).expect("dial");
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            s
        }
    }

    impl Drop for Wired {
        fn drop(&mut self) {
            if let Some(s) = self.server.take() {
                s.stop();
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn call(s: &mut std::os::unix::net::UnixStream, call: Call, id: i64) {
        let f = Frame::Request(marion_proto::Request::new(
            marion_proto::RequestId::Number(id),
            call,
        ));
        s.write_all(f.to_line().as_bytes()).unwrap();
        s.flush().unwrap();
    }

    fn next_frame(r: &mut std::io::BufReader<std::os::unix::net::UnixStream>) -> Frame {
        use std::io::BufRead;
        let mut line = String::new();
        assert!(r.read_line(&mut line).unwrap() > 0, "the socket closed");
        Frame::from_line(&line).expect("well-formed")
    }

    fn until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        cond()
    }

    /// `node/get` over the socket, against a tree that came out of a journal — the request/response
    /// shape, end to end.
    #[test]
    fn node_get_answers_from_the_journal_and_refuses_a_node_it_has_no_record_of() {
        let w = Wired::new("handler-get");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());

        call(
            &mut c,
            Call::NodeGet(marion_proto::params::NodeGetParams {
                agent_id: id("root"),
            }),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::NodeGet(got) =
            marion_proto::Method::NodeGet.decode_result(&body).unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(got.node.agent_id, id("root"));
        assert_eq!(got.node.harness, Harness::ClaudeCode);
        assert_eq!(got.node.depth, 0);

        // A node the journal does not record is a refusal that says how much has been read, so an
        // operator can tell "no such node" from "not yet".
        call(
            &mut c,
            Call::NodeGet(marion_proto::params::NodeGetParams {
                agent_id: id("nobody"),
            }),
            2,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Error(e) = resp.outcome else {
            panic!("expected a refusal")
        };
        assert_eq!(e.kind(), Some(FailureKind::NotFound));
        assert!(e.is_refusal(), "a missing node is the caller's business");
        assert!(e.message.contains("records read"), "{e}");
    }

    /// **The subscription shape, against the real journal**: a snapshot, then a notification for a
    /// node that appeared afterwards — written by a *second* writer, which is the case the registry
    /// tails the file for in the first place.
    #[test]
    fn tree_subscribe_returns_a_snapshot_and_then_narrates_what_the_journal_says_next() {
        let w = Wired::new("handler-sub");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        let _b = Broadcast::start(
            Arc::clone(&w.fx.handle),
            std::time::Duration::from_millis(2),
        );

        call(
            &mut c,
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(snap.nodes.len(), 1, "the root the journal already had");
        assert_eq!(snap.nodes[0].agent_id, id("root"));
        assert_eq!(snap.read_point.records, 1);

        // A different process appends a child and then moves it.
        append(
            &w.fx.path,
            &line(1, 2_000, intent("child", Some("root"), "codex-impl", 1)),
        );
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("a node that appeared must arrive as tree/node-added")
        };
        let Event::NodeAdded { node, ts } = n.event else {
            panic!("expected tree/node-added, got {:?}", n.event.method())
        };
        assert_eq!(node.agent_id, id("child"));
        assert_eq!(node.parent_id, Some(id("root")));
        assert_eq!(
            ts,
            SystemTime::from_unix_millis(2_000),
            "the journal's time, not the follower's — a client renders this as when it happened"
        );

        append(
            &w.fx.path,
            &line(
                2,
                3_500,
                RecordKind::StateChanged(StateChanged {
                    agent_id: id("child"),
                    state: NodeState::Running,
                }),
            ),
        );
        let Frame::Notification(n) = next_frame(&mut r) else {
            panic!("expected a notification")
        };
        let Event::NodeState {
            agent_id,
            state,
            reap_state,
            ts,
        } = n.event
        else {
            panic!("expected node/state")
        };
        assert_eq!(agent_id, id("child"));
        assert_eq!(state, NodeState::Running);
        assert_eq!(
            reap_state,
            ReapState::Live,
            "§7.6 gates on the disjunction, so the two travel in one message"
        );
        assert_eq!(ts, SystemTime::from_unix_millis(3_500));
    }

    /// **NC — a node the snapshot could not describe is excluded *and counted*, never silently
    /// dropped.**
    ///
    /// `TreeSubscribeResult` has nowhere to say "and there are N I could not describe", so the count
    /// lives on the supervisor. The assertion is that it is not zero: a gap that is admitted is a
    /// different thing from a gap that is invisible, and the invisible version is the one §11 item
    /// 23 keeps naming.
    #[test]
    fn a_node_the_snapshot_cannot_describe_is_counted_rather_than_dropped_into_silence() {
        let w = Wired::new("handler-lost");
        // A node with records and no identity, written by a second writer.
        append(
            &w.fx.path,
            &line(
                1,
                2_000,
                RecordKind::Spawned(Spawned {
                    agent_id: id("headless"),
                    harness_version: "0.9.0".into(),
                    model: None,
                    pid: Some(9),
                }),
            ),
        );
        assert!(until(
            || w.fx.handle.live.read(|r| r.tree().nodes().len()) == 2
        ));

        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        call(
            &mut c,
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(
            snap.nodes
                .iter()
                .map(|n| n.agent_id.clone())
                .collect::<Vec<_>>(),
            [id("root")],
            "a node with no identity is not described"
        );
        assert_eq!(
            w.fx.handle.unprojectable(),
            1,
            "and it is not invisible either"
        );
    }

    /// A subscriber that goes away stops being one, so a supervisor with no clients holds no queues
    /// — and, per §7.3.1, nothing else happens at all.
    #[test]
    fn a_departed_client_stops_being_a_subscriber_and_nothing_else_changes() {
        let w = Wired::new("handler-gone");
        let before = w.fx.handle.live.read(|r| r.tree().nodes().len());
        {
            let mut c = w.dial();
            let mut r = std::io::BufReader::new(c.try_clone().unwrap());
            call(
                &mut c,
                Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
                1,
            );
            next_frame(&mut r);
            assert_eq!(w.fx.handle.subscribers(), 1);
        }
        assert!(
            until(|| w.fx.handle.subscribers() == 0),
            "a closed connection is not a subscriber"
        );
        assert_eq!(
            w.fx.handle.live.read(|r| r.tree().nodes().len()),
            before,
            "§7.3.1: from the registry's point of view nothing happened"
        );
    }

    /// **NC — disposition (b) changes no node and cannot end the supervisor.**
    ///
    /// The journal bytes are the authority on the first half: comparing a summary before and after
    /// could miss an appended record that happens not to project into state. The second call on the
    /// same socket is the authority on the second half: a `Resident` word in a response is not proof
    /// that the server actually remained resident.
    #[test]
    fn detach_all_leaves_the_journal_and_node_untouched_and_the_supervisor_serving() {
        let fx = fx("handler-quit-detach");
        let before = std::fs::read(&fx.path).unwrap();
        let state = fx
            .handle
            .live
            .read(|r| r.tree().get(&id("root")).unwrap().state);
        let out = crate::serve::sink(ConnId(1));

        let result = fx
            .handle
            .call(
                ConnId(1),
                &Call::SessionQuit(marion_proto::params::SessionQuitParams {
                    disposition: marion_proto::QuitDisposition::DetachAll,
                }),
                &out,
            )
            .expect("detach is implemented, not refused");
        let MethodResult::SessionQuit(result) = result else {
            panic!("wrong result type")
        };
        let marion_proto::QuitOutcome::Detached {
            detached,
            gate_exposed,
            guidance,
            supervisor,
        } = result.outcome
        else {
            panic!("detach returned another disposition's outcome")
        };
        assert_eq!(detached, [id("root")]);
        assert_eq!(gate_exposed, [id("root")]);
        assert!(guidance.reattach.contains("tree/subscribe"));
        assert!(guidance.stop_fleet.contains("session/quit"));
        assert!(guidance.reattach.contains("denied unattended"));
        assert!(guidance.reattach.contains("is_error:true"));
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::SpawnOutstanding
            )
        );
        assert_eq!(std::fs::read(&fx.path).unwrap(), before);
        assert_eq!(
            fx.handle
                .live
                .read(|tree| tree.tree().get(&id("root")).unwrap().state),
            state
        );

        assert!(matches!(
            fx.handle.call(
                ConnId(1),
                &Call::NodeGet(marion_proto::params::NodeGetParams {
                    agent_id: id("root")
                }),
                &out,
            ),
            Ok(MethodResult::NodeGet(_))
        ));
        assert!(!fx.handle.exiting());
        assert!(!fx.handle.idle_exit_eligible());
    }

    /// **NC — disposition (a) is unreachable when the confirmation is not the live render.**
    ///
    /// The empty runtime trace is the negative control: checking only for a refusal could pass
    /// after a buggy implementation signalled first and noticed the mismatch second. It and the
    /// byte-identical journal prove the refusal preceded every side effect.
    #[test]
    fn kill_tree_refuses_a_missing_or_stale_confirmed_list_before_signalling_anything() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-kill-unconfirmed",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 101),
                state("root", NodeState::Running),
            ],
        );
        let before = std::fs::read(&fx.path).unwrap();

        let error = quit(
            &fx,
            marion_proto::QuitDisposition::KillTree { confirmed: vec![] },
        )
        .expect_err("an empty render did not confirm a live root");
        assert_eq!(error.kind(), Some(FailureKind::Refused));
        assert!(error.message.contains("confirmed"), "{error}");
        assert!(
            runtime.killed().is_empty(),
            "the mismatch was checked after signalling"
        );
        assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    }

    /// Disposition (a), positively: every live node is killed through its own recorded PID, its
    /// prior activity is returned, each intent precedes its confirmation, and only then is the
    /// supervisor exit recorded. Two distinct recorded PIDs make a one-node or one-group
    /// implementation visible in the runtime trace.
    #[test]
    fn kill_tree_kills_each_non_terminal_node_and_journals_each_pair_before_exit() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-kill",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 101),
                state("root", NodeState::Running),
                intent("child", Some("root"), "codex-impl", 1),
                spawned("child", 202),
                state("child", NodeState::Blocked(BlockReason::Permission)),
                intent("done", Some("root"), "codex-impl", 1),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );

        let disposition = marion_proto::QuitDisposition::KillTree {
            confirmed: vec![id("child"), id("root")],
        };
        fx.handle.connected(ConnId(9));
        let result = quit(&fx, disposition.clone()).expect("the exact set was confirmed");
        let marion_proto::QuitOutcome::Killed { nodes, supervisor } = result.outcome else {
            panic!("kill returned another disposition's outcome")
        };
        assert_eq!(
            nodes,
            [
                marion_proto::KilledNode {
                    agent_id: id("root"),
                    was: NodeState::Running,
                },
                marion_proto::KilledNode {
                    agent_id: id("child"),
                    was: NodeState::Blocked(BlockReason::Permission),
                },
            ]
        );
        assert_eq!(supervisor, marion_proto::SupervisorDisposition::Exiting);
        assert_eq!(runtime.killed(), [101, 202], "one operation per live node");
        let mut tags = journal_tags(&fx.path);
        assert_eq!(
            &tags[8..],
            ["KillIntent", "KillConfirmed", "KillIntent", "KillConfirmed",],
            "one intent/act/confirm pair per node"
        );
        assert!(
            !fx.handle.begin_idle_exit(),
            "§5.7 forbids the exit record while even the quitting client remains"
        );
        fx.handle.gone(
            ConnId(9),
            &ClientGone::Quit(disposition),
            &Departure::QuitCompleted,
        );
        assert!(
            fx.handle.begin_idle_exit(),
            "after the configured grace, zero clients and zero non-terminals permit exit"
        );
        tags = journal_tags(&fx.path);
        assert_eq!(tags.last().unwrap(), "SupervisorExited");
        assert!(fx.handle.exiting());
        let replay = crate::journal::read_path(&fx.path).unwrap();
        assert_eq!(
            replay.get(&id("root")).unwrap().state,
            NodeState::Exited(ExitStatus::Cancelled)
        );
        assert_eq!(
            replay.get(&id("child")).unwrap().state,
            NodeState::Exited(ExitStatus::Cancelled)
        );
        assert_eq!(
            replay.get(&id("done")).unwrap().state,
            NodeState::Exited(ExitStatus::Ok),
            "quitting does not rewrite an existing terminal"
        );
    }

    /// **NC — disposition (c) applies §7.2's predicate, not a convenient approximation.**
    ///
    /// Idle processes die and get exactly one reap pair. Running, every `Blocked(_)`, and a
    /// spawning node survive untouched and are returned with detach guidance; an already-terminal
    /// node appears in neither list. The exact runtime trace is the mutation control against an
    /// implementation that simply sweeps every PID it can see.
    #[test]
    fn reap_idle_detach_busy_reaps_only_idle_and_detaches_every_refusal_class() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-reap",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 11),
                state("idle", NodeState::Idle),
                intent("running", Some("idle"), "codex-impl", 1),
                spawned("running", 22),
                state("running", NodeState::Running),
                intent("permission", Some("idle"), "codex-impl", 1),
                spawned("permission", 33),
                state("permission", NodeState::Blocked(BlockReason::Permission)),
                intent("elicitation", Some("idle"), "codex-impl", 1),
                spawned("elicitation", 44),
                state("elicitation", NodeState::Blocked(BlockReason::Elicitation)),
                intent("descendants", Some("idle"), "codex-impl", 1),
                spawned("descendants", 55),
                state("descendants", NodeState::Blocked(BlockReason::Descendants)),
                intent("waiting-parent", None, "claude", 0),
                spawned("waiting-parent", 66),
                state(
                    "waiting-parent",
                    NodeState::Blocked(BlockReason::Descendants),
                ),
                intent("idle-spawn-target", Some("waiting-parent"), "codex-impl", 1),
                spawned("idle-spawn-target", 77),
                state("idle-spawn-target", NodeState::Idle),
                intent("spawning", Some("idle"), "codex-impl", 1),
                intent("done", Some("idle"), "codex-impl", 1),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );

        let result = quit(&fx, marion_proto::QuitDisposition::ReapIdleDetachBusy)
            .expect("the default disposition is implemented");
        let marion_proto::QuitOutcome::ReapedAndDetached {
            reaped,
            detached,
            gate_exposed,
            guidance,
            supervisor,
        } = result.outcome
        else {
            panic!("reap returned another disposition's outcome")
        };
        assert_eq!(reaped, [id("idle")]);
        assert_eq!(
            detached,
            [
                id("running"),
                id("permission"),
                id("elicitation"),
                id("descendants"),
                id("waiting-parent"),
                id("idle-spawn-target"),
                id("spawning"),
            ]
        );
        assert_eq!(gate_exposed, detached);
        assert!(guidance.reattach.contains("tree/subscribe"));
        assert!(guidance.reattach.contains("reaped"));
        assert!(guidance.reattach.contains("resumable"));
        assert!(guidance.stop_fleet.contains("session/quit"));
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::BlockedNode
            )
        );
        assert!(!fx.handle.idle_exit_eligible());
        assert_eq!(runtime.killed(), [11], "§7.2 refusal classes were detached");
        let tags = journal_tags(&fx.path);
        assert_eq!(&tags[24..], ["ReapIntent", "ReapConfirmed"]);
        let replay = crate::journal::read_path(&fx.path).unwrap();
        assert_eq!(
            replay.get(&id("idle")).unwrap().reap_state,
            ReapState::ReapedIdle
        );
        for agent in [
            "running",
            "permission",
            "elicitation",
            "descendants",
            "waiting-parent",
            "idle-spawn-target",
            "spawning",
        ] {
            let node = replay.get(&id(agent)).unwrap();
            assert_eq!(node.reap_state, ReapState::Live, "{agent}");
            assert!(node.reap_intent.is_none(), "{agent}");
        }
    }

    /// **NC — a spawn marion *abandoned* is not a spawn that is *outstanding*.**
    ///
    /// The node's state is `Spawning` and stays `Spawning` forever, because `registry.rs` records
    /// the abort as a separate fact rather than as a transition — so on the unfiltered reading this
    /// one node satisfies two of §5.7's clauses at once and the supervisor can never exit.
    ///
    /// That is not a hypothetical. A `marion run` whose root failed to launch journals exactly
    /// these two records (`root.rs`'s `Err` arm), and with the detached supervisor wired up it left
    /// a process resident over a project directory the run had already deleted. §7.2's rule is the
    /// argument in one line: *"a node marion decided the fate of is never `Orphaned`"* — its fate
    /// is decided, there is no process, and there is nothing for exiting to strand.
    ///
    /// The second half is what stops the fix from being a blanket "ignore `Spawning`": an intent
    /// with **no** abort beside it still holds the supervisor, because that one really is
    /// outstanding.
    #[test]
    fn an_aborted_spawn_does_not_keep_the_supervisor_resident_but_an_outstanding_one_does() {
        let aborted = fx_with(
            "handler-quit-spawn-aborted",
            vec![
                intent("root", None, "claude", 0),
                RecordKind::SpawnAborted(marion_core::journal::SpawnAborted {
                    agent_id: id("root"),
                    reason: "the harness binary was not found".into(),
                }),
            ],
        );
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&aborted, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Exiting,
            "an abandoned spawn strands nothing, so §5.7's exclusion list does not name it"
        );
        assert!(
            aborted.handle.idle_exit_eligible(),
            "and the accept loop may act on that"
        );

        let outstanding = fx_with(
            "handler-quit-spawn-outstanding",
            vec![intent("root", None, "claude", 0)],
        );
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&outstanding, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::SpawnOutstanding
            ),
            "an intent with no resolution beside it is exactly what §5.7 means by outstanding"
        );
        assert!(!outstanding.handle.idle_exit_eligible());
    }

    /// **NC — an abort written *after* the process existed is not evidence that it does not.**
    ///
    /// The narrow reading above — *no `Spawned`, no pid, so nothing to strand* — is the whole of
    /// what an abandoned spawn licenses. `run.rs`'s `AbortOnDrop` stays armed across the entire
    /// synchronous child run, and a `Child` that is dropped rather than reaped does **not** kill
    /// the process it holds, so a panic anywhere between `command.spawn()` and the disarm writes
    /// `SpawnAborted` beside a `Spawned` that names a live pid. Discarding that node would let the
    /// supervisor exit over a process it can name.
    #[test]
    fn an_abort_written_after_the_child_was_spawned_still_holds_the_supervisor() {
        let fx = fx_with(
            "handler-quit-abort-after-spawn",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 4242),
                RecordKind::SpawnAborted(marion_core::journal::SpawnAborted {
                    agent_id: id("root"),
                    reason: "marion left the spawn path before the child reached a terminal record"
                        .into(),
                }),
            ],
        );
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::SpawnOutstanding
            ),
            "the journal names pid 4242 and never says it died; exiting here strands it"
        );
        assert!(
            !fx.handle.idle_exit_eligible(),
            "and the accept loop must not act on the discarded reading either"
        );
    }

    /// **NC — a registry that stopped following says *that*, not whichever stale clause the frozen
    /// prefix happens to satisfy.**
    ///
    /// Every node here is terminal, so the honest answer to §5.7 is `Exiting` and the *only* thing
    /// keeping this supervisor is that it can no longer read the file it would answer from
    /// (§7.4). Failing closed is right and is unchanged. What is asserted is the sentence: an
    /// operator told `NonTerminalNode` goes looking for a node that finished, while the fact is
    /// that marion stopped reading at a byte — and only one of those two is actionable.
    #[test]
    fn a_registry_that_stopped_following_is_reported_as_that_and_not_as_a_stale_node() {
        let fx = fx_with(
            "handler-quit-registry-stopped",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 12),
                RecordKind::Exited(Exited {
                    agent_id: id("root"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "finished".into(),
                    },
                }),
            ],
        );
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Exiting,
            "the control: with the journal readable, nothing here holds it"
        );

        append(&fx.path, b"this is a complete line and not a record\n");
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::RegistryStopped
            ),
            "§7.4: the tree is frozen, so no clause read off it may be quoted as the reason"
        );
        assert!(
            !fx.handle.idle_exit_eligible(),
            "and the accept loop fails closed on the same reading"
        );
    }

    /// **NC — a detach that found work still arms the exit that the work later releases.**
    ///
    /// §5.7's predicate is evaluated by the accept loop, continuously, not once at the instant a
    /// client asked. A quit whose answer is `Resident` is not a quit that failed: the client still
    /// left, and the clause that held the supervisor can clear a millisecond later. If the answer
    /// at that one instant decided whether the timer may ever start, a fleet that finishes just
    /// after the last window closes keeps a supervisor forever.
    #[test]
    fn a_detach_that_found_work_still_arms_the_exit_that_work_later_releases() {
        let fx = fx_with(
            "handler-quit-resident-then-released",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 77),
                state("root", NodeState::Running),
            ],
        );
        fx.handle.connected(ConnId(9));
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .unwrap()
                .outcome
        else {
            panic!("DetachAll answers Detached")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::NonTerminalNode
            )
        );
        fx.handle.gone(
            ConnId(9),
            &ClientGone::Quit(marion_proto::QuitDisposition::DetachAll),
            &Departure::QuitCompleted,
        );
        assert!(
            !fx.handle.idle_exit_eligible(),
            "while the node runs, the node is the answer"
        );

        append(
            &fx.path,
            &line(
                3,
                1_003,
                RecordKind::Exited(Exited {
                    agent_id: id("root"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "the node finished a moment after the window closed".into(),
                    },
                }),
            ),
        );
        assert!(
            fx.handle.idle_exit_eligible(),
            "nothing in §5.7's exclusion list holds any more, and no second client is coming to \
             ask again"
        );
    }

    /// **NC — §5.7's exit predicate is zero clients and zero non-terminal nodes, and nothing else.**
    ///
    /// A dropped socket is not a quit (§2, §7.3.1) and this changes nothing about that: no node is
    /// touched, nothing is journaled about the departure, and every clause of the exclusion list
    /// still decides the answer. What it must not do is make the *supervisor's own* lifetime
    /// conditional on a client having been polite — a TUI that was SIGKILLed leaves a supervisor
    /// with nothing to supervise, and §5.7 says that supervisor MAY go.
    #[test]
    fn a_client_that_vanished_without_quitting_still_leaves_an_empty_supervisor_free_to_exit() {
        let fx = fx_with(
            "handler-exit-after-socket-closed",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 55),
                RecordKind::Exited(Exited {
                    agent_id: id("root"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "the run finished".into(),
                    },
                }),
            ],
        );
        fx.handle.connected(ConnId(9));
        assert!(
            !fx.handle.idle_exit_eligible(),
            "a client is attached, which is §5.7's absolute clause"
        );
        fx.handle
            .gone(ConnId(9), &ClientGone::SocketClosed, &Departure::Eof);
        assert!(
            fx.handle.idle_exit_eligible(),
            "zero clients and zero non-terminal nodes is the whole predicate (§5.7)"
        );
        assert_eq!(
            journal_tags(&fx.path),
            ["SpawnIntent", "Spawned", "Exited"],
            "§7.3.1: nothing is journaled about a client's death"
        );
    }

    /// `ReapedIdle` is resumable and therefore not `Exited(_)`. §5.7's zero-non-terminal rule is
    /// literal: reaping the last process does not permit the supervisor to journal an exit while
    /// that resumable node remains in the registry.
    #[test]
    fn a_reaped_idle_node_still_keeps_the_supervisor_resident() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-reaped-resident",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 88),
                state("idle", NodeState::Idle),
            ],
        );
        let outcome = quit(&fx, marion_proto::QuitDisposition::ReapIdleDetachBusy)
            .unwrap()
            .outcome;
        let marion_proto::QuitOutcome::ReapedAndDetached { supervisor, .. } = outcome else {
            panic!("wrong disposition outcome")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::NonTerminalNode
            )
        );
        assert_eq!(runtime.killed(), [88]);
        assert!(!fx.handle.idle_exit_eligible());
        assert!(
            journal_tags(&fx.path)
                .iter()
                .all(|tag| tag != "SupervisorExited")
        );
    }

    #[test]
    fn an_unconfirmed_reap_intent_forbids_the_supervisor_exit_record() {
        let fx = fx_with(
            "handler-quit-unconfirmed-reap",
            vec![
                intent("idle", None, "claude", 0),
                state("idle", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("idle"),
                    reason: "a prior supervisor decided to reap".into(),
                }),
            ],
        );
        let outcome = quit(&fx, marion_proto::QuitDisposition::DetachAll)
            .unwrap()
            .outcome;
        let marion_proto::QuitOutcome::Detached { supervisor, .. } = outcome else {
            panic!("wrong disposition outcome")
        };
        assert_eq!(
            supervisor,
            marion_proto::SupervisorDisposition::Resident(
                marion_proto::ResidentReason::UnconfirmedReapIntent
            )
        );
        assert!(!fx.handle.idle_exit_eligible());
        assert!(
            journal_tags(&fx.path)
                .iter()
                .all(|tag| tag != "SupervisorExited")
        );
    }

    /// **NC — EOF has no default disposition.** An idle node is the sharp control because the
    /// explicit default would reap it; `gone(SocketClosed)` must leave both its runtime trace and
    /// every journal byte alone.
    #[test]
    fn a_dropped_socket_is_not_any_quit_disposition_and_does_not_apply_the_default() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-eof",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 77),
                state("idle", NodeState::Idle),
            ],
        );
        let before = std::fs::read(&fx.path).unwrap();
        fx.handle
            .gone(ConnId(44), &ClientGone::SocketClosed, &Departure::Eof);
        assert!(runtime.killed().is_empty());
        assert_eq!(std::fs::read(&fx.path).unwrap(), before);
        assert_eq!(
            fx.handle
                .live
                .read(|r| r.tree().get(&id("idle")).unwrap().reap_state),
            ReapState::Live
        );
    }

    /// Observes the journal at the instant of each signal, which no ordinary assertion can: the
    /// finished file records *intent, confirm* for both a correct implementation and one that
    /// appends the confirmation before sending anything. §4.3's order is what separates them, and
    /// a confirmation that precedes its act is a durable claim marion never earned.
    #[derive(Default)]
    struct OrderingRuntime {
        path: Mutex<Option<PathBuf>>,
        at_signal: Mutex<Vec<Vec<String>>>,
    }

    impl QuitRuntime for OrderingRuntime {
        fn kill_process_tree_and_wait(&self, _pid: i32) -> bool {
            let path = lock(&self.path).clone().expect("the fixture set its path");
            lock(&self.at_signal).push(journal_tags(&path));
            true
        }
    }

    /// A runtime that signals and cannot observe death, which is the failure `run.rs`'s bounded
    /// wait returns rather than asserting away.
    struct UnobservableRuntime;

    impl QuitRuntime for UnobservableRuntime {
        fn kill_process_tree_and_wait(&self, _pid: i32) -> bool {
            false
        }
    }

    /// **NC — (a)'s PID preflight refuses rather than signals into the dark.**
    ///
    /// A confirmed node whose spawn has not resolved has no recorded PID. Proceeding would append
    /// a durable kill intent for a process marion cannot address, which is the half-happened kill
    /// the confirmed list exists to prevent — and the refusal is a `Conflict`, because the
    /// operator's next move is to re-render, not to file a bug.
    #[test]
    fn kill_tree_refuses_a_confirmed_node_whose_pid_is_not_recorded_yet() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-kill-no-pid",
            vec![intent("root", None, "claude", 0)],
        );
        let before = std::fs::read(&fx.path).unwrap();

        let error = quit(
            &fx,
            marion_proto::QuitDisposition::KillTree {
                confirmed: vec![id("root")],
            },
        )
        .expect_err("a node with no PID cannot be proven to have been reached");
        assert_eq!(error.kind(), Some(FailureKind::Conflict));
        assert!(error.message.contains("no recorded PID"), "{error}");
        assert!(runtime.killed().is_empty(), "nothing was signalled");
        assert_eq!(std::fs::read(&fx.path).unwrap(), before);
    }

    /// **NC — each node's confirmation is appended after its signal, not before it.**
    ///
    /// The finished journal is identical either way, so the assertion has to be made *during* the
    /// signal. §7.2's recovery reads an intent without a confirmation as "marion may have killed
    /// this"; a confirmation written first would make the opposite claim durable in the one window
    /// where it is false.
    #[test]
    fn every_kill_is_signalled_before_its_confirmation_becomes_durable() {
        let runtime = Arc::new(OrderingRuntime::default());
        let fx = fx_with_runtime(
            "handler-quit-kill-order",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 301),
                state("root", NodeState::Running),
                intent("child", Some("root"), "codex-impl", 1),
                spawned("child", 302),
                state("child", NodeState::Running),
            ],
            runtime.clone(),
        );
        *lock(&runtime.path) = Some(fx.path.clone());

        quit(
            &fx,
            marion_proto::QuitDisposition::KillTree {
                confirmed: vec![id("root"), id("child")],
            },
        )
        .expect("the exact set was confirmed");

        let snapshots = lock(&runtime.at_signal).clone();
        assert_eq!(snapshots.len(), 2, "one signal per node");
        for (i, tags) in snapshots.iter().enumerate() {
            assert_eq!(
                tags.last().map(String::as_str),
                Some("KillIntent"),
                "node {i}'s intent is durable at the moment it is signalled"
            );
            assert_eq!(
                tags.iter().filter(|t| *t == "KillConfirmed").count(),
                i,
                "node {i} was not confirmed before it was signalled"
            );
        }
    }

    /// **NC — a node §7.2 already reaped is retired without a second signal.**
    ///
    /// `ReapedIdle` is not `Exited`, so (a) must still account for it, but its process is already
    /// gone. Signalling its recorded PID again would address whatever now owns that number, and
    /// recording `signal: 9` would claim marion did something it did not do.
    #[test]
    fn kill_tree_retires_an_already_reaped_node_without_signalling_it_again() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-kill-reaped",
            vec![
                intent("reaped", None, "claude", 0),
                spawned("reaped", 501),
                state("reaped", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("reaped"),
                    reason: "an earlier session/quit reaped it".into(),
                }),
                RecordKind::ReapConfirmed(ReapConfirmed {
                    agent_id: id("reaped"),
                }),
                intent("live", None, "claude", 0),
                spawned("live", 502),
                state("live", NodeState::Running),
            ],
        );

        quit(
            &fx,
            marion_proto::QuitDisposition::KillTree {
                confirmed: vec![id("live"), id("reaped")],
            },
        )
        .expect("both non-terminal nodes were confirmed");
        assert_eq!(
            runtime.killed(),
            [502],
            "a ReapedIdle node has no process left to signal"
        );
        let replay = crate::journal::read_path(&fx.path).unwrap();
        assert_eq!(
            replay
                .get(&id("reaped"))
                .unwrap()
                .exit
                .as_ref()
                .unwrap()
                .signal,
            None,
            "the record does not claim a signal marion never sent"
        );
        assert_eq!(
            replay
                .get(&id("live"))
                .unwrap()
                .exit
                .as_ref()
                .unwrap()
                .signal,
            Some(9)
        );
    }

    /// **NC — a kill marion cannot observe dead is a refusal, not a confirmation.**
    ///
    /// This is the one path `run.rs`'s bounded wait exists to produce, and it is the path that
    /// must not end in an exit: an unconfirmed intent is exactly what §7.2's recovery needs to
    /// find, and a supervisor that left anyway would take that recovery with it.
    #[test]
    fn a_kill_that_cannot_be_observed_dead_leaves_the_intent_unconfirmed_and_no_exit() {
        let fx = fx_with_runtime(
            "handler-quit-kill-unobserved",
            vec![
                intent("root", None, "claude", 0),
                spawned("root", 909),
                state("root", NodeState::Running),
            ],
            Arc::new(UnobservableRuntime),
        );

        let error = quit(
            &fx,
            marion_proto::QuitDisposition::KillTree {
                confirmed: vec![id("root")],
            },
        )
        .expect_err("marion did not observe the process dead");
        assert_eq!(error.kind(), Some(FailureKind::Internal));
        let tags = journal_tags(&fx.path);
        assert_eq!(tags.last().map(String::as_str), Some("KillIntent"));
        assert!(
            !tags.iter().any(|t| t == "KillConfirmed"),
            "nothing confirmed a death nobody saw: {tags:?}"
        );
        assert!(!fx.handle.idle_exit_eligible());
        assert!(!fx.handle.begin_idle_exit());
        assert!(!fx.handle.exiting());
    }

    /// **NC — (c) refuses each busy class on that node's own state.**
    ///
    /// Every refusal class here is a *root*, so §7.2's "a node a spawn is blocked on" guard cannot
    /// stand in for the state predicate. Without this, `reaping` could test nothing but parentage
    /// and still detach every busy node in a tree-shaped fixture.
    #[test]
    fn reap_idle_detach_busy_refuses_each_busy_root_on_its_own_state() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-reap-roots",
            vec![
                intent("idle", None, "claude", 0),
                spawned("idle", 1),
                state("idle", NodeState::Idle),
                intent("running", None, "claude", 0),
                spawned("running", 2),
                state("running", NodeState::Running),
                intent("permission", None, "claude", 0),
                spawned("permission", 3),
                state("permission", NodeState::Blocked(BlockReason::Permission)),
                intent("elicitation", None, "claude", 0),
                spawned("elicitation", 4),
                state("elicitation", NodeState::Blocked(BlockReason::Elicitation)),
                intent("descendants", None, "claude", 0),
                spawned("descendants", 5),
                state("descendants", NodeState::Blocked(BlockReason::Descendants)),
                intent("spawning", None, "claude", 0),
            ],
        );

        let marion_proto::QuitOutcome::ReapedAndDetached {
            reaped, detached, ..
        } = quit(&fx, marion_proto::QuitDisposition::ReapIdleDetachBusy)
            .expect("no busy root blocks the reap of an idle one")
            .outcome
        else {
            panic!("reap returned another disposition's outcome")
        };
        assert_eq!(reaped, [id("idle")]);
        assert_eq!(
            detached,
            [
                id("running"),
                id("permission"),
                id("elicitation"),
                id("descendants"),
                id("spawning"),
            ]
        );
        assert_eq!(runtime.killed(), [1], "only the idle root was signalled");
    }

    /// **NC — an idle root already under, or past, a reap is not reaped a second time.**
    ///
    /// An unconfirmed intent means some other actor may already be mid-reap, and a `ReapedIdle`
    /// node has no process left; either way a second intent/confirm pair would journal an act that
    /// did not happen to a process that is not there.
    #[test]
    fn reap_idle_detach_busy_skips_an_idle_root_already_under_or_past_a_reap() {
        let (fx, runtime) = recording_fx_with(
            "handler-quit-reap-twice",
            vec![
                intent("fresh", None, "claude", 0),
                spawned("fresh", 10),
                state("fresh", NodeState::Idle),
                intent("intended", None, "claude", 0),
                spawned("intended", 20),
                state("intended", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("intended"),
                    reason: "someone else decided to reap it".into(),
                }),
                intent("already", None, "claude", 0),
                spawned("already", 30),
                state("already", NodeState::Idle),
                RecordKind::ReapIntent(ReapIntent {
                    agent_id: id("already"),
                    reason: "an earlier session/quit reaped it".into(),
                }),
                RecordKind::ReapConfirmed(ReapConfirmed {
                    agent_id: id("already"),
                }),
            ],
        );

        let marion_proto::QuitOutcome::ReapedAndDetached {
            reaped, detached, ..
        } = quit(&fx, marion_proto::QuitDisposition::ReapIdleDetachBusy)
            .expect("one idle root was reapable")
            .outcome
        else {
            panic!("reap returned another disposition's outcome")
        };
        assert_eq!(reaped, [id("fresh")]);
        assert_eq!(runtime.killed(), [10]);
        assert_eq!(
            detached,
            [id("intended")],
            "a ReapedIdle node is no longer something a client can be detached from"
        );
    }

    /// **NC — (b) names the nodes an operator is walking away from, and not the ones that finished.**
    ///
    /// A `detached` list padded with terminal nodes tells the operator that work is still out there
    /// when it is not, which is the same lie as omitting a live one, in the other direction.
    #[test]
    fn detach_names_only_the_nodes_that_are_still_someones_agent() {
        let fx = fx_with(
            "handler-quit-detach-list",
            vec![
                intent("live", None, "claude", 0),
                spawned("live", 61),
                state("live", NodeState::Idle),
                intent("done", None, "claude", 0),
                spawned("done", 62),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );

        let marion_proto::QuitOutcome::Detached {
            detached,
            gate_exposed,
            ..
        } = quit(&fx, marion_proto::QuitDisposition::DetachAll)
            .expect("detach is implemented")
            .outcome
        else {
            panic!("detach returned another disposition's outcome")
        };
        assert_eq!(detached, [id("live")]);
        assert_eq!(gate_exposed, [id("live")]);
    }

    /// **NC — a departure decides nothing, and *"nothing"* is about nodes and about time, not
    /// about the supervisor's right to leave an empty project.**
    ///
    /// §7.3.1 is a rule about **agents**: *"a crashed, SIGKILLed, or otherwise vanished client MUST
    /// leave every node exactly as it was"*, and *"nothing is journaled about the client's death"*.
    /// Both are asserted here, byte for byte.
    ///
    /// What §7.3.1 does **not** say is that the supervisor must outlive its own emptiness. §5.7's
    /// permission is two clauses — *"zero clients and zero non-terminal nodes"* — and neither
    /// mentions a quit. Reading one in is what left a supervisor immortal after every client that
    /// died rather than resigned, which is not a stricter reading of the crash invariant but a leak
    /// wearing its name; `handler-exit-after-socket-closed` above is the same fact stated
    /// positively.
    ///
    /// The distinction that *does* survive is timing, and it is asserted here: a departure marion
    /// could not read does not waive §5.7's grace, because for all marion knows the replacement
    /// window is already opening. Only an explicit `session/quit` does.
    #[test]
    fn a_departure_decides_nothing_and_does_not_shorten_the_wait() {
        let fx = fx_with(
            "handler-quit-eof-eligibility",
            vec![
                intent("done", None, "claude", 0),
                spawned("done", 7),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );
        let before = std::fs::read(&fx.path).unwrap();

        fx.handle.connected(ConnId(3));
        fx.handle
            .gone(ConnId(3), &ClientGone::SocketClosed, &Departure::Eof);
        assert_eq!(
            std::fs::read(&fx.path).unwrap(),
            before,
            "§7.3.1: nothing is journaled about a client's death, and no node moved"
        );
        assert!(
            !fx.handle.exiting(),
            "`gone` itself never commits to an exit; §5.7 splits the decision from the record"
        );
        assert!(
            !fx.handle.idle_exit_grace_waived(),
            "§7.3.1: a close said nothing, so it cannot have said 'and do not wait'"
        );
        assert!(
            fx.handle.idle_exit_eligible(),
            "§5.7's two clauses are both satisfied; the accept loop still owes the whole grace"
        );
    }

    /// **NC — `exiting` follows the exit record; it does not precede it.**
    ///
    /// §5.7's record is what distinguishes *finished and left* from *died*. A supervisor that
    /// committed to exiting and only then failed to journal would produce exactly the ambiguity
    /// the record exists to remove, and would do it on the one path — a journal marion cannot write
    /// — where the evidence is least recoverable.
    #[test]
    fn the_supervisor_does_not_commit_to_exiting_before_its_record_is_durable() {
        let fx = fx_with(
            "handler-quit-exit-undurable",
            vec![
                intent("done", None, "claude", 0),
                spawned("done", 8),
                RecordKind::Exited(Exited {
                    agent_id: id("done"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "already done".into(),
                    },
                }),
            ],
        );
        let marion_proto::QuitOutcome::Detached { supervisor, .. } =
            quit(&fx, marion_proto::QuitDisposition::DetachAll)
                .expect("detach is implemented")
                .outcome
        else {
            panic!("detach returned another disposition's outcome")
        };
        assert_eq!(supervisor, marion_proto::SupervisorDisposition::Exiting);
        assert!(fx.handle.idle_exit_eligible());

        // A path the journal cannot be appended to. Nothing else about the decision changes, so
        // the only reason to stay is the one under test.
        std::fs::remove_file(&fx.path).unwrap();
        std::fs::create_dir(&fx.path).unwrap();
        assert!(!fx.handle.begin_idle_exit());
        assert!(
            !fx.handle.exiting(),
            "an exit that could not be recorded did not happen"
        );
    }

    /// A method that is specified and not built says so — `Unimplemented`, not `Unsupported`, and
    /// not silence.
    #[test]
    fn a_specified_but_unbuilt_method_is_refused_with_the_milestone_named() {
        let w = Wired::new("handler-unimpl");
        let mut c = w.dial();
        let mut r = std::io::BufReader::new(c.try_clone().unwrap());
        call(
            &mut c,
            Call::NodeCancel(marion_proto::params::NodeCancelParams {
                agent_id: id("root"),
            }),
            1,
        );
        let Frame::Response(resp) = next_frame(&mut r) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Error(e) = resp.outcome else {
            panic!("expected a refusal")
        };
        assert_eq!(e.kind(), Some(FailureKind::Unimplemented));
        assert!(e.message.contains("node/cancel"), "{e}");
    }

    /// **NC — a subscriber is told about a change exactly once, and a subscription that starts late
    /// is not told about what its own snapshot already contained.**
    ///
    /// §7.3.3's seam, as a test rather than as an argument: the snapshot and the point notifications
    /// begin from are taken under one lock from one read, so there is no instant between them for an
    /// event to be lost in or duplicated across.
    #[test]
    fn a_snapshot_and_its_subscription_meet_exactly_with_no_gap_and_no_overlap() {
        let w = Wired::new("handler-seam");
        // Two clients: one subscribes before the child appears, one after.
        let mut early = w.dial();
        let mut er = std::io::BufReader::new(early.try_clone().unwrap());
        call(
            &mut early,
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );
        next_frame(&mut er);

        append(
            &w.fx.path,
            &line(1, 2_000, intent("child", Some("root"), "codex-impl", 1)),
        );
        assert!(until(
            || w.fx.handle.live.read(|r| r.tree().nodes().len()) == 2
        ));

        let mut late = w.dial();
        let mut lr = std::io::BufReader::new(late.try_clone().unwrap());
        call(
            &mut late,
            Call::TreeSubscribe(marion_proto::params::TreeSubscribeParams {}),
            1,
        );

        // The late subscriber's snapshot already has the child…
        let Frame::Response(resp) = next_frame(&mut lr) else {
            panic!("expected a response")
        };
        let marion_proto::Outcome::Result(body) = resp.outcome else {
            panic!("expected a result")
        };
        let MethodResult::TreeSubscribe(snap) = marion_proto::Method::TreeSubscribe
            .decode_result(&body)
            .unwrap()
        else {
            panic!("wrong result type")
        };
        assert_eq!(snap.nodes.len(), 2, "the snapshot is current, not stale");

        // …and the early subscriber was told about it, as the flush that the late subscribe
        // performed on its way in.
        let Frame::Notification(n) = next_frame(&mut er) else {
            panic!("the early subscriber must hear about the child")
        };
        assert_eq!(n.event.method(), "tree/node-added");

        // Now a change after both are subscribed reaches both, once each.
        append(
            &w.fx.path,
            &line(
                2,
                3_000,
                RecordKind::Exited(Exited {
                    agent_id: id("child"),
                    status: ExitStatus::Ok,
                    exit: ProcessExit {
                        code: Some(0),
                        signal: None,
                        description: "clean exit".into(),
                    },
                }),
            ),
        );
        assert!(until(|| w.fx.handle.flush() == 0
            && w.fx.handle.live.read(|r| r
                .tree()
                .get(&id("child"))
                .unwrap()
                .state
                .is_exited())));
        for r in [&mut er, &mut lr] {
            let Frame::Notification(n) = next_frame(r) else {
                panic!("both subscribers hear the exit")
            };
            let Event::NodeState { state, ts, .. } = n.event else {
                panic!("expected node/state")
            };
            assert_eq!(state, NodeState::Exited(ExitStatus::Ok));
            assert_eq!(ts, SystemTime::from_unix_millis(3_000));
        }
        // And exactly once: a second flush produces nothing, so nothing further arrives.
        assert_eq!(w.fx.handle.flush(), 0, "a told transition is not re-told");
    }
}
