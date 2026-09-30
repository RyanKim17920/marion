use std::sync::atomic::Ordering;

use marion_core::contract::{AgentId, ProcessExit, TaskId};
use marion_core::journal::{KillConfirmed, KillIntent, ReapConfirmed, ReapIntent, RecordKind};
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::result::SessionQuitResult;
use marion_core::proto::{
    DetachGuidance, KilledNode, QuitDisposition, QuitOutcome, ResidentReason, RpcError,
    SpawnCaller, SupervisorDisposition,
};

use super::launch::root_spawn_authorized;
use super::{Ending, NodeHandle, NodeOutcome, RegistryHandle, cancel, getpgid, lock};
use crate::serve::Peer;

pub(super) trait QuitRuntime: Send + Sync {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool;
    /// Kill whatever is left in a group whose leader ended on its own. A no-op by default, for a
    /// runtime that signals nothing.
    fn sweep_group(&self, _pgid: i32) {}
}

pub(super) struct SystemQuitRuntime;

impl QuitRuntime for SystemQuitRuntime {
    fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
        crate::kill::kill_process_tree_and_wait(pid)
    }

    fn sweep_group(&self, pgid: i32) {
        crate::kill::sweep_group(pgid);
    }
}

impl RegistryHandle {
    /// **A kill's half of [`Ending`]**: claim the node's end before signalling it. `false` means
    /// the node's own thread already saw its process end and is recording that exit, so there is
    /// nothing left to signal. A node this supervisor does not own has no thread to race and is
    /// always claimable — its end is the kill's to record.
    pub(super) fn claim_kill(&self, agent_id: &AgentId) -> bool {
        let mut nodes = lock(&self.nodes);
        let Some(node) = nodes.get_mut(agent_id) else {
            return true;
        };
        if node.ending == Ending::ProcessEnded || !node.running() {
            return false;
        }
        // A kill of a node already cancelling is the cancel's escalation: the cancel keeps its
        // attribution, and the kill signals now rather than after the grace.
        if !matches!(node.ending, Ending::CancelRequested(_)) {
            node.ending = Ending::KillRequested;
        }
        true
    }

    /// **A cancel's half of [`Ending`]**, the graceful sibling of [`Self::claim_kill`]: claim the
    /// node's end for `by` before anything is written to it. `false` for a node whose thread
    /// already saw its process end, one a kill or an earlier cancel already claimed, and one this
    /// supervisor does not own — which has no driver to abort, and is the kill's.
    pub(super) fn claim_cancel(
        &self,
        agent_id: &AgentId,
        by: &marion_core::journal::CancelBy,
    ) -> bool {
        let mut nodes = lock(&self.nodes);
        match nodes.get_mut(agent_id) {
            Some(node) if node.running() && node.ending == Ending::Running => {
                node.ending = Ending::CancelRequested(by.clone());
                true
            }
            _ => false,
        }
    }

    /// A journal failure after [`Self::claim_cancel`]: hand the node's end back, so its thread
    /// records its own exit rather than wait for a cancel that will never confirm it.
    pub(super) fn unclaim_cancel(&self, agent_id: &AgentId) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id)
            && matches!(node.ending, Ending::CancelRequested(_))
        {
            node.ending = Ending::Running;
        }
    }

    /// The node's thread has seen its process end.
    pub(super) fn process_gone(&self, agent_id: &AgentId) -> bool {
        lock(&self.nodes)
            .get(agent_id)
            .is_some_and(|n| n.process_gone)
    }

    /// **`node/kill` on a node being cancelled**: tell the cancel to stop waiting out the grace.
    fn escalate(&self, agent_id: &AgentId) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.escalated = true;
        }
        self.ended.notify_all();
    }

    /// Wait until every node in `ids` has finished its thread, or `deadline`.
    pub(super) fn wait_finished(&self, ids: &[AgentId], deadline: std::time::Instant) {
        let mut nodes = lock(&self.nodes);
        loop {
            let pending = ids
                .iter()
                .any(|id| nodes.get(id).is_some_and(NodeHandle::running));
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            if !pending || wait.is_zero() {
                return;
            }
            nodes = self
                .ended
                .wait_timeout(nodes, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Whether the node is being cancelled — what `agent/spawn` and `node/steer` refuse a
    /// cancelling node for.
    pub(crate) fn cancelling(&self, agent_id: &AgentId) -> bool {
        lock(&self.nodes)
            .get(agent_id)
            .is_some_and(|n| matches!(n.ending, Ending::CancelRequested(_)))
    }

    /// **A node being cancelled spawns nothing** — no child and no race seat. Its subtree was
    /// frozen under [`Self::spawn_decision`], which the caller holds, and a child started now would
    /// outlive the cancel that was meant to end it.
    pub(super) fn refuse_if_cancelling(&self, caller_id: &SpawnCaller) -> Result<(), RpcError> {
        if self.cancelling(&caller_id.agent_id) {
            return Err(RpcError::refused(
                "caller",
                "the calling node is being cancelled, so it may not spawn: its turn is being ended \
                 and everything below it with it.",
                "§6.7",
            ));
        }
        Ok(())
    }

    /// **Who ended the node, if marion did**, settling nothing: see
    /// [`crate::run::SpawnObserver::ended_by`]. A kill is always the operator's — only an operator
    /// may call `node/kill` or confirm a KillTree.
    pub(crate) fn ended_by(&self, agent_id: &AgentId) -> Option<marion_core::journal::CancelBy> {
        match &lock(&self.nodes).get(agent_id)?.ending {
            Ending::KillRequested => Some(marion_core::journal::CancelBy::Operator),
            Ending::CancelRequested(by) => Some(by.clone()),
            Ending::Running | Ending::ProcessEnded => None,
        }
    }

    /// Wait until every node in `ids` this supervisor owns has seen its process end, or
    /// `deadline`, or any node in `watch` was escalated — on [`Self::ended`], never on a timer.
    /// Answers the ones still running.
    pub(super) fn wait_processes_gone(
        &self,
        ids: &[AgentId],
        deadline: std::time::Instant,
        watch: &[AgentId],
    ) -> Vec<AgentId> {
        let mut nodes = lock(&self.nodes);
        loop {
            let left: Vec<AgentId> = ids
                .iter()
                .filter(|id| {
                    nodes
                        .get(*id)
                        .is_some_and(|n| !n.process_gone && n.running())
                })
                .cloned()
                .collect();
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            let escalated = watch
                .iter()
                .any(|id| nodes.get(id).is_some_and(|n| n.escalated));
            if left.is_empty() || wait.is_zero() || escalated {
                return left;
            }
            nodes = self
                .ended
                .wait_timeout(nodes, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// **A node thread's half of [`Ending`]**, asked the instant its process has ended and before
    /// anything terminal is written: `true` means marion killed it on request, so the thread
    /// records the cancellation and leaves the terminal record to the kill's `KillConfirmed`.
    /// `false` settles the other way — the exit is the thread's to record, and a kill arriving now
    /// is refused rather than signalling a reaped pid.
    pub(crate) fn process_ended(&self, agent_id: &AgentId) -> bool {
        let mut nodes = lock(&self.nodes);
        let Some(node) = nodes.get_mut(agent_id) else {
            return false;
        };
        node.process_gone = true;
        self.ended.notify_all();
        match node.ending {
            Ending::KillRequested | Ending::CancelRequested(_) => true,
            Ending::Running | Ending::ProcessEnded => {
                node.ending = Ending::ProcessEnded;
                false
            }
        }
    }

    /// **A native root this supervisor claimed has reached its end**, and its table entry says so.
    ///
    /// A managed node's entry is finished by the thread that ran it ([`Self::mark_finished`] from
    /// `run_spawn`'s return). A native node has no such thread — the supervisor owns its pane, not
    /// its turn — so its recorder closes the entry instead, at the same two moments the journal
    /// gets a terminal record. Without this, [`Self::idle_exit_eligible`](crate::serve::Handle::idle_exit_eligible)'s second guard would read
    /// a native session that ended hours ago as a running node and no supervisor with a native
    /// launch in its history could ever leave.
    ///
    /// [`NodeOutcome::Root`] because a native node **is** a root: its result is its stream and its
    /// exit, both on disk (§9).
    pub(crate) fn finished_native(&self, agent_id: &AgentId, outcome: Result<(), String>) {
        self.mark_finished(agent_id, NodeOutcome::Root(outcome));
    }

    /// A process exists for a node this supervisor owns. Called from
    /// [`crate::run::SpawnObserver::started`], immediately after `Spawned { pid: Some(_) }` is
    /// journaled.
    pub(super) fn mark_started(&self, agent_id: &AgentId, pid: i32) {
        // SAFETY: reads the process group of a pid this process just created; cannot fail other
        // than by returning -1, which is recorded as "not known" rather than as a group id.
        let pgid = unsafe { getpgid(pid) };
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.pid = Some(pid);
            node.pgid = (pgid > 0).then_some(pgid);
            node.started_at = Some(std::time::SystemTime::now());
        }
        self.register_budget(agent_id);
    }

    /// `run_spawn` returned. **The entry is kept, not removed**: it is what tells a later caller
    /// "that node finished" from "no such node", and §5.7's exit predicate reads liveness off it
    /// rather than membership.
    pub(super) fn mark_finished(&self, agent_id: &AgentId, outcome: NodeOutcome) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.outcome = Some(outcome);
        }
        self.ended.notify_all();
        // §5.7's second guard just moved; nothing else would wake the accept loop to re-read it.
        self.live.changes().notify();
        // Sealed with the node, and every message still waiting is dropped by name: no later turn
        // of this process will take it.
        self.inboxes.close(
            agent_id,
            "the node ended before its next turn took the message",
        );
        // A requester that ends abandons the races it asked for.
        for race_id in self.races.requested_by(agent_id) {
            self.drive_race(&race_id);
        }
    }

    /// **Why a node this supervisor owns did not finish cleanly**, in one sentence.
    ///
    /// `None` for a node still running, a node that finished cleanly, and a node this supervisor
    /// does not own — three states this deliberately does not distinguish, because the question it
    /// answers is *"is there a fault to report about this node"* and the other three surfaces
    /// ([`Self::owned_running`], [`Self::owned_nodes`]) answer the rest. The sentence is marion's
    /// own for a root and the spawn path's for a child; neither is derived from an exit code.
    pub fn owned_failure(&self, agent_id: &AgentId) -> Option<String> {
        match lock(&self.nodes).get(agent_id)?.outcome.as_ref()? {
            NodeOutcome::Child(r) => r.as_ref().as_ref().err().map(|e| e.to_string()),
            NodeOutcome::Root(r) => r.as_ref().err().cloned(),
        }
    }

    /// How many nodes this supervisor owns, finished or not. For tests and a future `doctor`.
    pub fn owned_nodes(&self) -> usize {
        lock(&self.nodes).len()
    }

    /// How many of them are still running, by [`NodeHandle::running`]'s reading.
    pub fn running_nodes(&self) -> usize {
        lock(&self.nodes).values().filter(|n| n.running()).count()
    }

    /// The pid this supervisor recorded for a node it owns, if a process exists yet.
    pub fn owned_pid(&self, agent_id: &AgentId) -> Option<i32> {
        lock(&self.nodes).get(agent_id).and_then(|n| n.pid)
    }

    /// Whether one node this supervisor owns is still running. `None` if it owns no such node.
    pub fn owned_running(&self, agent_id: &AgentId) -> Option<bool> {
        lock(&self.nodes).get(agent_id).map(|n| n.running())
    }

    /// The node's own process group, as `getpgid(2)` reported it — §6.7's `killpg` target.
    pub fn owned_pgid(&self, agent_id: &AgentId) -> Option<i32> {
        lock(&self.nodes).get(agent_id).and_then(|n| n.pgid)
    }

    /// §9's contract this node runs under.
    /// **`None` for a node this supervisor does not own *and* for a root**, which has no contract
    /// at all (§9). The two are told apart by [`Self::owned_running`], which answers `None` only
    /// for the first.
    pub fn owned_task_id(&self, agent_id: &AgentId) -> Option<TaskId> {
        lock(&self.nodes)
            .get(agent_id)
            .and_then(|n| n.task_id.clone())
    }

    /// When the node's process came into existence.
    pub fn owned_started_at(&self, agent_id: &AgentId) -> Option<std::time::SystemTime> {
        lock(&self.nodes).get(agent_id).and_then(|n| n.started_at)
    }

    /// **Join every finished node's thread**, and answer whether any refused to be joined.
    ///
    /// §5.7's exit needs the *thread* joined and not merely its answer read — `background.rs` makes
    /// the same distinction for the same reason: a thread that has sent its outcome is still
    /// running its own epilogue (dropping the `Child`, removing the worktree, unwinding
    /// `AbortOnDrop`), and a process that exits underneath that leaves the epilogue undone.
    ///
    /// Only *finished* nodes, so this can never block: a handle whose `outcome` is set has had
    /// `run_spawn` return on that thread, so the join is a formality. A running node is not joined
    /// here because [`Handle::idle_exit_eligible`](crate::serve::Handle::idle_exit_eligible) has already refused to exit while one exists.
    pub(super) fn join_finished_nodes(&self) {
        let joins: Vec<_> = lock(&self.nodes)
            .values_mut()
            .filter(|n| !n.running())
            .filter_map(|n| n.join.take())
            .collect();
        for j in joins {
            // A panicking node's thread is not this supervisor's failure to report at exit — the
            // panic already became this node's outcome, and so its terminal state, in `caught`.
            // That claim was false until `caught` existed: nothing constructed `SpawnError::Panicked`
            // anywhere, and a panicking thread left `outcome: None` for ever.
            let _ = j.join();
        }
    }

    /// **§6.1 step 2's third argument, read off the registry rather than off a caller's table.**
    ///
    /// `background.rs` used to argue the opposite and was right at the time: a bridge's table was
    /// authoritative *by construction*, because every child of a node went through that node's own
    /// bridge, while a journal read could not see a child between "thread started" and "process
    /// observed" — `Spawned` was written after the whole run. **Step 1 inverted the premise.**
    /// `SpawnIntent` is journaled before any side effect and `Spawned` at `command.spawn()`, so a
    /// node counted here exists from the first instant it exists at all, and the count survives a
    /// restart, which no in-process table does.
    ///
    /// Counted from the **intent**, not from `Spawned`: a child whose worktree is still being made
    /// occupies its parent's slot exactly as much as one already running, and counting the case it
    /// cannot rule out is `background.rs`'s own over-count-is-the-safe-direction rule.
    pub(super) fn live_children_of(&self, parent: &AgentId) -> u32 {
        let n = self.live.read(|r| {
            r.tree()
                .nodes()
                .iter()
                .filter(|n| {
                    n.parent_id().as_ref() == Some(&parent)
                        && !n.state.is_exited()
                        && n.reap_state == ReapState::Live
                        && !Self::abandoned(n)
                })
                .count()
        });
        // Saturating for `Background::live_children`'s reason: a count that wrapped to 0 would
        // silently *open* the gate.
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// §7.3.2's voluntary path. The mutex is not throughput machinery; it makes the rendered-set
    /// comparison and the first intent one indivisible decision. Without it, two clients can both
    /// confirm the same live render and each signal it after the other's confirmation.
    pub(super) fn session_quit(
        &self,
        disposition: &QuitDisposition,
    ) -> Result<SessionQuitResult, RpcError> {
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

    pub(super) fn guidance(&self) -> DetachGuidance {
        let socket = &self.socket_path;
        DetachGuidance {
            reattach: format!(
                "`marion ls` shows them again (a client: tree/subscribe on {}). A node named in \
                 gate_exposed is refused any permission it asks for, since nobody can approve it; \
                 it sees the refusal as an error result.",
                socket.display()
            ),
            stop_fleet: format!(
                "`marion cancel <id>` stops one (a client: session/quit with KillTree on {}, \
                 confirmed against a fresh tree/subscribe).",
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
    ///
    /// **§7.2's `Orphaned` is deliberately not a second exclusion**, and the temptation to make it
    /// one is worth answering rather than leaving to be rediscovered.
    ///
    /// The case for excluding it looks strong: the node was recorded before this process existed,
    /// marion holds no `Child` and no channel for it, §7.6 already counts it terminal for gating,
    /// and holding is permanent — a supervisor booting over a journal a crashed one left is
    /// `Resident` from its first instant. Every clause of that is true and the conclusion is still
    /// wrong, because it reads `Orphaned` as *"nothing here to act on"* when §7.2 defines it as the
    /// opposite: *"both are 'marion does not know', **both require the same user resolution**"*.
    /// The resolution is the operator's, over this socket — and a supervisor that exited to avoid
    /// holding has taken the resolution away rather than performed it. `Spawned` can carry a real
    /// pid, in which case a confirmed `session/quit` KillTree signals the surviving process; that
    /// is a live node this supervisor is the only handle on.
    /// `detached_supervisor.rs`'s `a_supervisor_holding_a_non_terminal_node_refuses_to_exit_
    /// until_that_node_finishes` boots over exactly that journal, with a process it really started.
    ///
    /// So `Orphaned` is the *reason* to stay, not a reason to leave — and the honest cost is that a
    /// node whose `Spawned` recorded no pid can be neither killed nor resolved, so it holds forever
    /// with no path out. That is §11 item 28's absent pid, not a residency rule to loosen; loosening
    /// it here would trade a supervisor that cannot exit for a fleet that cannot be stopped.
    fn resident_reason(nodes: &[marion_core::registry::ReplayedNode]) -> Option<ResidentReason> {
        let holding: Vec<_> = nodes.iter().filter(|n| !Self::abandoned(n)).collect();
        // **The intent holds only while the death is unobserved.** §7.2's crash window is *"the
        // supervisor died before the kill landed"*, and what makes it a window is that a process
        // may still be running. A terminal record for the node shuts it: §7.2 resolves an
        // unconfirmed intent *by checking for the process*, and an observed exit or a confirmed
        // kill **is** that check, already made. No `ReapConfirmed` follows — nothing here
        // fabricates a record — so the intent stays outstanding on the tree forever, and reading
        // it without asking about the exit beside it is a supervisor that can never leave.
        //
        // Reachable in one supervisor's life and without a crash: `reap_idle_detach_busy` journals
        // the intent, signals, and returns rather than confirming when it cannot observe the death;
        // a later confirmed `session/quit` KillTree then writes the `KillConfirmed` that does
        // observe it.
        if holding
            .iter()
            .any(|n| n.reap_intent.is_some() && !n.state.is_exited())
        {
            Some(ResidentReason::UnconfirmedReapIntent)
        } else if holding
            .iter()
            .any(|n| matches!(n.state, NodeState::Blocked(_)))
        {
            Some(ResidentReason::BlockedNode)
        } else if holding.iter().any(|n| n.state == NodeState::Spawning) {
            // An intent whose confirmation has not landed. A managed node's `Spawned` is followed
            // by `StateChanged(Running)` (`journal::confirm_spawned`), so a node marion holds a
            // process for reads as `NonTerminalNode` below, not here; both keep the supervisor
            // resident, and the word names which of the two the operator is looking at.
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
    pub(super) fn residency(&self) -> Option<ResidentReason> {
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
    /// **The first sentence became true rather than aspirational with §11 item 28 step 1**, and
    /// this predicate got stricter in the safe direction as a result. `run_spawn` now appends
    /// `Spawned { pid: Some(_) }` between `command.spawn()` and the child's first byte of stdin,
    /// so the two middle clauses have teeth on a child: any failure *after* the process exists
    /// leaves a record marion cannot mistake for a spawn that never happened. **It is true of a
    /// root too**, and this sentence used to say it was not: item 28's step 6 gave `root.rs`'s
    /// `launch_inner` an `on_started` hook, so a root's `Spawned` is written at `command.spawn()`
    /// with a real pid exactly like a child's. The `pid: None` arm survives only for a launch that
    /// never reached a process.
    ///
    /// The b600d82 case — `SpawnIntent` then `SpawnAborted`, nothing else, which is exactly what a
    /// `marion run` whose root failed to launch journals — still satisfies all four, and now does
    /// so for a stronger reason: a launch that fails before there is a process writes no `Spawned`
    /// at all, so the shape is *evidence* that no process exists rather than merely consistent
    /// with it. `background_spawn.rs`'s
    /// `a_launch_that_fails_before_the_process_exists_journals_no_spawned_record` produces that
    /// shape from a real failing launch; the residency reading of it is
    /// `an_aborted_spawn_does_not_keep_the_supervisor_resident_but_an_outstanding_one_does`.
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

        let mut killed = Vec::with_capacity(targets.len());
        for node in targets {
            // A node whose own thread already saw its process end is still ended here, exactly as
            // before `node/kill` existed: the operator confirmed this exact set, and KillTree has
            // no refusal for a node that happens to be finishing. The claim only tells an owned
            // node's thread that marion ended it, when marion got there first.
            self.claim_kill(&node.agent_id);
            self.kill_node(node, KillBy::QuitKillTree)?;
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

    /// **§6.7's kill of one node**: the intent made durable, the per-node process tree signalled
    /// and observed dead, the confirmation made durable — and nothing else. Shared by
    /// `session/quit`'s KillTree, which runs it per confirmed node, and by `node/kill`; each caller
    /// owns its own preflight, and neither the supervisor's exit nor any other node is this
    /// function's business.
    ///
    /// A `ReapedIdle` node is retired rather than signalled: §7.2 already ended its process, and
    /// its recorded pid may name something else by now. The confirmation then records no signal,
    /// because marion sent none.
    ///
    /// The caller has checked that a node to be signalled has a recorded pid.
    pub(super) fn kill_node(
        &self,
        node: &marion_core::registry::ReplayedNode,
        by: KillBy,
    ) -> Result<(), RpcError> {
        self.journal_append(RecordKind::KillIntent(KillIntent {
            agent_id: node.agent_id.clone(),
            was: node.state,
        }))
        .map_err(|e| journal_failure_before_signal_in(by.verb(), e))?;
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
        self.journal_append(RecordKind::KillConfirmed(KillConfirmed {
            agent_id: node.agent_id.clone(),
            exit: ProcessExit {
                code: None,
                signal: signal.then_some(9),
                description: if signal {
                    by.signalled().into()
                } else {
                    by.retired().into()
                },
            },
        }))
        .map_err(|e| journal_failure_after_signal_in(by.verb(), e))
    }

    /// **§2's `node/kill` — end one node**, which §6.7 classifies `Exited(Cancelled)`.
    ///
    /// The per-node half of §7.3.2's disposition (a), through the same [`Self::kill_node`], and
    /// deliberately *only* that half: no confirmed-set comparison (the caller named one node), and
    /// no change to the supervisor's disposition — §5.7 goes on deciding that from whatever else is
    /// still running.
    ///
    /// The refusals come before any side effect, in the order that decides them:
    ///
    /// 1. the caller is this supervisor's own user — the check every operator call makes
    ///    ([`root_spawn_authorized`]); a node cannot kill through this verb;
    /// 2. the node is on the journal (`NotFound` otherwise), named by its whole id as `node/steer`
    ///    names it;
    /// 3. it has not already ended (`Refused`, naming its state) — its old pid is never signalled;
    /// 4. a node to be signalled has a recorded pid (`Conflict`: still spawning, retry);
    /// 5. **for a node this supervisor owns, its own thread has not already seen its process end**
    ///    (`Conflict`). This is the thread race, settled under one lock by [`Self::claim_kill`]
    ///    and [`Self::process_ended`]: whichever reaches the node's end first decides who writes
    ///    its terminal record. A kill that wins tells the thread, which then records its outcome
    ///    as the cancellation and writes no `Exited` over the `KillConfirmed`; a thread that wins
    ///    is already recording an exit it observed, and a kill would aim at a reaped pid.
    ///
    /// Held under the same lock as `session/quit`, so a KillTree and a `node/kill` cannot both
    /// journal an intent for one node.
    pub(super) fn node_kill(
        &self,
        p: &marion_core::proto::params::NodeKillParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::NodeKillResult, RpcError> {
        root_spawn_authorized(peer)?;
        let _decision = lock(&self.quit);
        self.live.refresh();
        let node = self
            .live
            .read(|r| r.tree().get(&p.agent_id).cloned())
            .ok_or_else(|| {
                RpcError::not_found(
                    &p.agent_id.0,
                    format!(
                        "this project's journal records no node `{}`, so there is nothing to end. \
                         Nothing was signalled.",
                        p.agent_id.0
                    ),
                    "§2, §6.7",
                )
            })?;
        if node.state.is_exited() {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` has already ended ({:?}); there is no process to end, and a terminal \
                     node's recorded PID is never signalled. Nothing was signalled.",
                    p.agent_id.0, node.state
                ),
                "§6.7",
            ));
        }
        if node.reap_state != ReapState::ReapedIdle && node.pid.is_none() {
            return Err(RpcError::conflict(
                &p.agent_id.0,
                "the node has no recorded PID yet, so marion cannot prove a signal reaches it. \
                 Nothing was signalled; retry after its spawn resolves.",
                "§6.7",
            ));
        }
        // **A kill of a node being cancelled is the cancel's escalation**: the cancel kills what is
        // left now rather than after its grace, and keeps the attribution and the confirmation.
        if self.cancelling(&p.agent_id) {
            self.escalate(&p.agent_id);
            drop(_decision);
            self.wait_processes_gone(
                std::slice::from_ref(&p.agent_id),
                std::time::Instant::now() + cancel::ESCALATE_WAIT,
                &[],
            );
            self.live.refresh();
            return Ok(marion_core::proto::result::NodeKillResult {
                state: self.spawned_state(&p.agent_id),
            });
        }
        // A `ReapedIdle` node is retired, not signalled, so there is no process end to race for:
        // §7.2's reap already ended it, and its thread's own reading of that is long settled.
        if node.reap_state != ReapState::ReapedIdle && !self.claim_kill(&p.agent_id) {
            return Err(RpcError::conflict(
                &p.agent_id.0,
                "the node's process has already ended on its own and its thread is recording how; \
                 its terminal state lands on the journal in a moment. Nothing was signalled.",
                "§6.7",
            ));
        }
        self.kill_node(&node, KillBy::NodeKill)?;
        self.live.refresh();
        Ok(marion_core::proto::result::NodeKillResult {
            state: self.spawned_state(&p.agent_id),
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
                    // ReapedIdle writes no Completion. Roots have no waiting spawn by construction;
                    // a top-level contracted node may, from the operator's own client.
                    && n.is_root()
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
        for node in reaping {
            self.journal_append(RecordKind::ReapIntent(ReapIntent {
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
            self.journal_append(RecordKind::ReapConfirmed(ReapConfirmed {
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

pub(super) fn journal_failure_before_signal(error: crate::journal::JournalError) -> RpcError {
    journal_failure_before_signal_in("session/quit", error)
}

pub(super) fn journal_failure_after_signal(error: crate::journal::JournalError) -> RpcError {
    journal_failure_after_signal_in("session/quit", error)
}

pub(super) fn journal_failure_before_signal_in(
    verb: &str,
    error: crate::journal::JournalError,
) -> RpcError {
    RpcError::internal(format!(
        "{verb} could not durably journal its intent, so it refused before signalling the node: \
         {error}"
    ))
}

pub(super) fn journal_failure_after_signal_in(
    verb: &str,
    error: crate::journal::JournalError,
) -> RpcError {
    RpcError::internal(format!(
        "{verb} changed a process but could not durably journal its confirmation; its intent \
         remains for restart recovery and the supervisor will not exit: {error}"
    ))
}

/// Which operator act a [`RegistryHandle::kill_node`] carries out — what its journal records say
/// marion did, and which verb its failures name. §6.7 wants the description to record marion as
/// the sender; the act is what tells a reader why.
#[derive(Clone, Copy)]
pub(super) enum KillBy {
    /// §7.3.2's disposition (a), one confirmed node at a time.
    QuitKillTree,
    /// §2's `node/kill`, for the one node it names.
    NodeKill,
    /// marion's own decision, through [`RegistryHandle::stop_node`].
    Stop(StopReason),
}

/// Why marion stops a node nobody asked it to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StopReason {
    /// A `first` race has its winner; the seats still running have lost.
    RaceDecided,
    /// The race's requester ended, so no seat's result will be read.
    RaceAbandoned,
}

impl KillBy {
    fn verb(self) -> &'static str {
        match self {
            KillBy::QuitKillTree => "session/quit",
            KillBy::NodeKill => "node/kill",
            KillBy::Stop(_) => "race",
        }
    }

    fn signalled(self) -> &'static str {
        match self {
            KillBy::QuitKillTree => "marion sent SIGKILL for confirmed session/quit KillTree",
            KillBy::NodeKill => "marion sent SIGKILL for the operator's node/kill",
            KillBy::Stop(StopReason::RaceDecided) => {
                "marion sent SIGKILL: its race was decided by an earlier seat"
            }
            KillBy::Stop(StopReason::RaceAbandoned) => {
                "marion sent SIGKILL: the node that asked for its race has ended"
            }
        }
    }

    fn retired(self) -> &'static str {
        match self {
            KillBy::QuitKillTree => {
                "confirmed session/quit retired an already ReapedIdle node; no process existed to \
                 signal"
            }
            KillBy::NodeKill => {
                "node/kill retired an already ReapedIdle node; no process existed to signal"
            }
            KillBy::Stop(_) => {
                "a race retired an already ReapedIdle seat; no process existed to signal"
            }
        }
    }
}
