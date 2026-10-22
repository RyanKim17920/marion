//! **§2's `node/cancel`** — end a node gracefully, with everything below it.
//!
//! A kill ends a node mid-write and loses what it was about to commit. A cancel gives each running
//! turn the chance to close first — by the row's abort verb ([`marion_harness::spec::AbortVerb`]),
//! measured per harness and shape — and kills only what is still running when its grace ends. The
//! work each node committed is kept, and each ends `Cancelled`.
//!
//! # The order, and why
//!
//! 1. **Freeze, top-down**, under `quit` and `spawn_decision`: claim each node's end for the
//!    cancel ([`RegistryHandle::claim_cancel`]), journal its `CancelRequested` before any byte
//!    reaches it, and cancel its inbox — every queued steer dropped, nothing more accepted, no new
//!    child spawned. Holding the spawn decision is what keeps a child whose intent is being written
//!    right now from escaping the walk.
//! 2. **Abort, bottom-up by depth level**: every node at a level is sent its abort at once, the
//!    level waits (on [`RegistryHandle::ended`], never a timer) up to the longest grace among
//!    them, stragglers are killed, and each gets its `KillConfirmed` — no signal where it closed in
//!    its grace, `SIGKILL` where marion had to. Children go first so their `Cancelled` contracts,
//!    with their partial work committed, reach the parent that is still waiting on them. The whole
//!    cancel is bounded by the sum of its levels' graces.
//! 3. **Sweep**: a node that ended in its grace may have left something in its process group (its
//!    MCP bridge); the group is swept by its recorded pgid, never by a reaped pid.
//!
//! A `node/kill` of a node being cancelled is the cancel's escalation: it kills what is left now
//! rather than after the grace, and the cancel keeps its attribution.
//!
//! **A node still launching is cancelled, not refused.** An owned node claimed before its process
//! exists — a fresh spawn, or a resume's relaunch, which is claimed before the orphan's process is
//! killed while the journal still names that process — is claimed by the freeze like any other,
//! and its level waits on [`RegistryHandle::ended`] until the launch has a process (then killed
//! by the normal tree kill) or ends without one (then confirmed with no signal). Its
//! `CancelRequested` is journaled once that is known, so it lands after the life's own `Spawned`.
//! The launch's own claim keeps the cancel ([`RegistryHandle::claim`]), and a resume that sees it
//! before launching does not launch.
//!
//! # Where a cancel is a kill
//!
//! A lane takes the abort only once its driver's port is attached, which is just before the first
//! turn goes out (ACP's `session/prompt`, app-server's `turn/start`). A cancel that lands earlier,
//! while a launch or a relaunch is still in its handshake, finds no port, and the canceller kills
//! the node at once. That is deliberate rather than a gap closed later: before the first turn
//! there is no turn to end and no work to commit, so the kill loses nothing a grace would keep.
//! Attaching the port before the handshake would make every handshake wait read a cancel as a
//! failed handshake. A cancel after the turn has gone out but before the server has named it waits
//! for the name (`app_server`'s `started_turn`) and interrupts the turn.
//!
//! # Who may cancel whom (§5.4)
//!
//! Exactly as `node/steer`: `caller: None` is the operator ([`super::root_spawn_authorized`]),
//! `caller: Some` a node proved by its token, cancelling only a strict descendant.
//!
//! [`super::root_spawn_authorized`]: crate::handler::root_spawn_authorized

use std::time::{Duration, Instant};

use marion_core::contract::{AgentId, ProcessExit};
use marion_core::encoding::Millis;
use marion_core::journal::{CancelBy, CancelRequested, KillConfirmed, RecordKind};
use marion_core::node::ReapState;
use marion_core::proto::RpcError;
use marion_core::proto::params::NodeCancelParams;
use marion_core::proto::result::{CancelledNode, NodeCancelResult};
use marion_harness::spec::{AbortVerb, NodeShape, abort_for};

use super::launch::LAUNCH_BOUND;
use super::lifecycle::Launch;
use super::{
    RegistryHandle, journal_failure_after_signal_in, journal_failure_before_signal_in, lock,
    root_spawn_authorized,
};
use crate::serve::Peer;

const VERB: &str = "node/cancel";

/// How long the nodes of a level that ended are given to finish their contracts — commit their
/// partial work, persist, close their stream — before the level above is aborted. Bounded because
/// a node thread wedged in its epilogue must not hold a cancel for ever; nothing waits on it when
/// every thread finished promptly.
const FINISH_BOUND: Duration = Duration::from_secs(10);

/// How long a `node/kill` that escalates a cancel waits for the node's process to go, so it answers
/// with the node's terminal state rather than the state it was killed from.
pub(super) const ESCALATE_WAIT: Duration = Duration::from_secs(10);

/// One node a cancel froze, as its abort phase needs it.
#[derive(Debug, Clone)]
struct Member {
    agent_id: AgentId,
    depth: u32,
    pid: Option<i32>,
    /// This supervisor runs its thread; `false` for a node it does not own, which has no driver to
    /// abort and is killed at its level.
    owned: bool,
    verb: AbortVerb,
    by: CancelBy,
    /// The `CancelRequested` of an owned node claimed before its process existed — a launch, or a
    /// resume's relaunch, still coming up — journaled once its launch has a process (or has ended
    /// without one), so it lands after that life's `Spawned` and replay keeps it on the life it
    /// ends.
    deferred: Option<CancelRequested>,
}

impl RegistryHandle {
    pub(super) fn node_cancel(
        &self,
        p: &NodeCancelParams,
        peer: Peer,
    ) -> Result<NodeCancelResult, RpcError> {
        self.live.refresh();
        let by = match &p.caller {
            None => {
                root_spawn_authorized(peer)?;
                CancelBy::Operator
            }
            Some(c) => {
                self.authorize_descendant(c, &p.agent_id)?;
                CancelBy::Node {
                    caller: c.agent_id.clone(),
                }
            }
        };
        let nodes = self.cancel_tree(&p.agent_id, by)?;
        self.live.refresh();
        Ok(NodeCancelResult {
            state: self.spawned_state(&p.agent_id),
            nodes,
        })
    }

    /// **Cancel `target` and every live node below it**, in the order the module docs give, and
    /// answer each node ended with whether it had to be killed. Shared by `node/cancel` and, with
    /// budgets, by a limit that trips.
    pub(crate) fn cancel_tree(
        &self,
        target: &AgentId,
        by: CancelBy,
    ) -> Result<Vec<CancelledNode>, RpcError> {
        let (members, frozen) = self.freeze(target, by)?;
        let ended = self.abort_bottom_up(&members)?;
        // A journal failure part-way through the freeze still aborts what was frozen — those
        // nodes are claimed and durably intended — and then reports the failure.
        frozen.map(|()| ended)
    }

    /// Step 1: claim, journal and freeze every node of the subtree, top-down. The first `Err` is a
    /// refusal before any side effect; the inner one is a journal failure after some were frozen.
    #[allow(clippy::type_complexity)]
    fn freeze(
        &self,
        target: &AgentId,
        by: CancelBy,
    ) -> Result<(Vec<Member>, Result<(), RpcError>), RpcError> {
        let _quit = lock(&self.quit);
        let _spawns = lock(&self.spawn_decision);
        self.live.refresh();
        let (node, below) = self.live.read(|r| {
            let tree = r.tree();
            let below: Vec<_> = crate::descendant_gate::live_descendants(tree, target)
                .iter()
                .filter_map(|id| tree.get(id).cloned())
                .collect();
            (tree.get(target).cloned(), below)
        });
        let node = node.ok_or_else(|| {
            RpcError::not_found(
                &target.0,
                format!(
                    "this project's journal records no node `{}`, so there is nothing to cancel. \
                     Nothing was signalled.",
                    target.0
                ),
                "§2, §6.7",
            )
        })?;
        // A node a resume is relaunching still reads as lost on the journal until its new process
        // exists; the claim is what says it is coming back, and the cancel ends that.
        let relaunching =
            self.owned_running(target) == Some(true) && self.owned_pid(target).is_none();
        if (node.state.is_exited() || node.reap_state != ReapState::Live) && !relaunching {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` has already ended ({:?}); there is no turn to end. Nothing was signalled.",
                    target.0, node.state
                ),
                "§6.7",
            ));
        }
        if self.cancelling(target) {
            return Err(RpcError::conflict(
                &target.0,
                "the node is already being cancelled; its turn is being ended by its row's abort. \
                 `node/kill` (`marion cancel --force`) ends it now.",
                "§6.7",
            ));
        }
        let mut all = vec![node];
        all.extend(below);
        all.sort_by_key(|n| n.depth().unwrap_or(0));
        // An owned node's pid is its live process's, from its thread, read when it is signalled: a
        // node still launching — a resume's relaunch among them, claimed while the journal still
        // names the process it replaced — is waited for. A node this supervisor does not own is
        // signalled by its journaled pid, so it must have one.
        if let Some(n) = all
            .iter()
            .find(|n| n.pid.is_none() && self.owned_running(&n.agent_id) != Some(true))
        {
            return Err(RpcError::conflict(
                &n.agent_id.0,
                "a node in the subtree has no recorded PID yet, so marion cannot prove a cancel \
                 reaches it. Nothing was cancelled; retry after its spawn resolves.",
                "§6.7",
            ));
        }
        if self.owned_running(target) == Some(true) && !self.claim_cancel(target, &by) {
            return Err(RpcError::conflict(
                &target.0,
                "the node's process has already ended on its own and its thread is recording how; \
                 its terminal state lands on the journal in a moment. Nothing was signalled.",
                "§6.7",
            ));
        }

        let mut members = Vec::with_capacity(all.len());
        for n in all {
            let id = n.agent_id.clone();
            let is_target = &id == target;
            let node_by = if is_target {
                by.clone()
            } else {
                CancelBy::Cascade {
                    from: target.clone(),
                }
            };
            let owned = match self.owned_running(&id) {
                // Its thread has finished; its own terminal record is on the way.
                Some(false) => continue,
                Some(true) => {
                    if !is_target && !self.claim_cancel(&id, &node_by) {
                        // Already ending — its process ended on its own and its thread is still
                        // finishing (a continuation node between generations), or a kill claimed
                        // it. It is not this cancel's to confirm, but it takes no further turn.
                        self.inboxes.cancel(&id);
                        continue;
                    }
                    true
                }
                None => false,
            };
            let verb = match (owned, n.harness()) {
                (true, Some(h)) => {
                    let shape = if lock(&self.panes).has_live(&id) {
                        NodeShape::Interactive
                    } else {
                        NodeShape::Headless
                    };
                    abort_for(marion_harness::adapter::harness_spec(h), shape)
                }
                _ => AbortVerb::None {
                    note: "a node this supervisor does not drive has no abort it can send",
                },
            };
            let intent = CancelRequested {
                agent_id: id.clone(),
                was: n.state,
                by: node_by.clone(),
                verb: verb.kind().to_string(),
                grace: Millis(Duration::from_millis(u64::from(verb.grace_ms()))),
            };
            let deferred = (owned && self.owned_pid(&id).is_none()).then(|| intent.clone());
            let journaled = match deferred {
                Some(_) => Ok(()),
                None => self.journal_append(RecordKind::CancelRequested(intent)),
            };
            if let Err(e) = journaled {
                if owned {
                    self.unclaim_cancel(&id);
                }
                return Ok((members, Err(journal_failure_before_signal_in(VERB, e))));
            }
            self.inboxes.cancel(&id);
            members.push(Member {
                pid: if owned { None } else { n.pid },
                agent_id: id,
                depth: n.depth().unwrap_or(0),
                owned,
                verb,
                by: node_by,
                deferred,
            });
        }
        Ok((members, Ok(())))
    }

    /// Step 2 and 3: abort each level, deepest first, kill its stragglers, confirm every node, and
    /// sweep the groups of the ones that ended on their own.
    fn abort_bottom_up(&self, members: &[Member]) -> Result<Vec<CancelledNode>, RpcError> {
        let watch: Vec<AgentId> = members.iter().map(|m| m.agent_id.clone()).collect();
        let mut depths: Vec<u32> = members.iter().map(|m| m.depth).collect();
        depths.sort_unstable_by(|a, b| b.cmp(a));
        depths.dedup();
        let mut ended = Vec::with_capacity(members.len());
        for depth in depths {
            let level: Vec<&Member> = members.iter().filter(|m| m.depth == depth).collect();
            let unlaunched = self.await_launches(&level, LAUNCH_BOUND)?;
            let mut grace = 0u32;
            let mut aborted = Vec::new();
            for m in &level {
                if unlaunched.contains(&m.agent_id) {
                    continue;
                }
                if m.owned && self.inboxes.abort(&m.agent_id) {
                    grace = grace.max(m.verb.grace_ms());
                    aborted.push(m.agent_id.clone());
                }
            }
            let deadline = Instant::now() + Duration::from_millis(u64::from(grace));
            let left = self.wait_processes_gone(&aborted, deadline, &watch);
            for m in &level {
                let launched = !unlaunched.contains(&m.agent_id);
                let sent = aborted.contains(&m.agent_id);
                let straggler = launched && (!sent || left.contains(&m.agent_id));
                let forced = straggler && self.kill_straggler(m)?;
                self.confirm(m, launched, sent, forced)?;
                ended.push(CancelledNode {
                    agent_id: m.agent_id.clone(),
                    forced,
                });
            }
            let ids: Vec<AgentId> = level.iter().map(|m| m.agent_id.clone()).collect();
            self.wait_finished(&ids, Instant::now() + FINISH_BOUND);
        }
        Ok(ended)
    }

    /// **The level's nodes claimed before their process existed**, waited for until each launch has
    /// a process or has ended without one, and only then given their `CancelRequested`. Answers
    /// the ones that never started a process: there is nothing of theirs to abort or kill. Bounded
    /// by `bound`, the launch's own; a launch still coming up past it is handed to its own start,
    /// which kills the process as it appears ([`RegistryHandle::abandon_launch`]), and reported.
    fn await_launches(&self, level: &[&Member], bound: Duration) -> Result<Vec<AgentId>, RpcError> {
        let mut unlaunched = Vec::new();
        for m in level {
            let Some(intent) = &m.deferred else {
                continue;
            };
            let launched = self.await_launched(&m.agent_id, Instant::now() + bound)
                || match self.abandon_launch(&m.agent_id, intent) {
                    Launch::Started => true,
                    Launch::Ended => false,
                    Launch::Abandoned => {
                        return Err(RpcError::internal(format!(
                            "marion claimed the cancel of `{}` while its launch was coming up, and \
                             the launch neither started a process nor ended within its own bound; \
                             its process is killed, and the cancel journaled, the moment it starts \
                             (§6.7)",
                            m.agent_id.0
                        )));
                    }
                };
            if let Err(e) = self.journal_append(RecordKind::CancelRequested(intent.clone())) {
                self.unclaim_cancel(&m.agent_id);
                return Err(journal_failure_before_signal_in(VERB, e));
            }
            if !launched {
                unlaunched.push(m.agent_id.clone());
            }
        }
        Ok(unlaunched)
    }

    /// Kill a node still running at the end of its level's grace. `false` — nothing signalled —
    /// for an owned node whose process ended at the last moment: its pid may already be reaped.
    fn kill_straggler(&self, m: &Member) -> Result<bool, RpcError> {
        if m.owned && self.process_gone(&m.agent_id) {
            return Ok(false);
        }
        let pid = if m.owned {
            self.owned_pid(&m.agent_id)
        } else {
            m.pid
        }
        .expect("the freeze refused an unowned member with no pid, and waited for an owned one's");
        if !self.runtime.kill_process_tree_and_wait(pid) {
            return Err(RpcError::internal(format!(
                "marion journaled the cancel of `{}` and signalled its per-node process tree when \
                 its grace ended, but could not observe its PID dead; the intent remains \
                 unconfirmed and the supervisor will not exit (§6.7, §5.7)",
                m.agent_id.0
            )));
        }
        Ok(true)
    }

    /// The cancel's confirmation: `KillConfirmed`, with a signal only where marion sent one. A node
    /// something else already ended (a KillTree racing the cancel) is left to that record; a node
    /// that ended on its own has whatever remains in its group swept. `sent` is whether the node's
    /// lane took its abort ([`crate::inbox::Inboxes::abort`]): a row's verb its lane did not send
    /// gave the node no grace, and the description says so rather than claim one ran out.
    fn confirm(
        &self,
        m: &Member,
        launched: bool,
        sent: bool,
        forced: bool,
    ) -> Result<(), RpcError> {
        if !forced && let Some(pgid) = self.owned_pgid(&m.agent_id) {
            self.runtime.sweep_group(pgid);
        }
        self.live.refresh();
        let exited = self.live.read(|r| {
            r.tree()
                .get(&m.agent_id)
                .is_some_and(|n| n.state.is_exited())
        });
        if exited {
            return Ok(());
        }
        let who = m.by.describe();
        let description = match (forced, sent, m.verb) {
            _ if !launched => format!(
                "marion's cancel (by {who}) reached the node while its launch was coming up, and \
                 the launch ended without starting a process"
            ),
            (false, true, v) => format!(
                "the node ended on marion's cancel (by {who}) within its {} ms {} grace",
                v.grace_ms(),
                v.kind()
            ),
            (false, false, _) => format!(
                "the node's process ended on its own as marion's cancel (by {who}) reached it, \
                 before any abort or signal"
            ),
            (true, _, AbortVerb::None { .. }) => format!(
                "marion sent SIGKILL for a cancel by {who}: the node's row has no abort verb on \
                 this shape"
            ),
            (true, false, v) => format!(
                "marion sent SIGKILL for a cancel by {who}: the node's row has a {} verb on this \
                 shape but its lane took none, so no grace was given",
                v.kind()
            ),
            (true, true, v) => format!(
                "marion sent SIGKILL for a cancel by {who}: the node was still running when its \
                 {} ms {} grace ended",
                v.grace_ms(),
                v.kind()
            ),
        };
        self.journal_append(RecordKind::KillConfirmed(KillConfirmed {
            agent_id: m.agent_id.clone(),
            exit: ProcessExit {
                code: None,
                signal: forced.then_some(9),
                description,
            },
        }))
        .map_err(|e| journal_failure_after_signal_in(VERB, e))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, Weak};

    use marion_core::contract::ExitStatus;
    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::journal::{
        Exited, JournalRecord, SpawnIntent, Spawned, StateChanged, WriterId, decode, encode,
    };
    use marion_core::node::NodeState;
    use marion_core::proto::{Call, FailureKind, MethodResult, SpawnCaller};
    use marion_testsupport::{append, scratch};

    use super::super::{NodeOutcome, QuitRuntime};
    use super::*;
    use crate::inbox::DeliveryPort;
    use crate::registry::{LiveRegistry, Registry};
    use crate::serve::{ConnId, Handle};

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    /// A node's thread, as the cancel sees it: its process ends, then its thread finishes.
    fn thread_ends(handle: &RegistryHandle, agent: &AgentId) {
        handle.process_ended(agent);
        handle.mark_finished(agent, NodeOutcome::Root(Ok(())));
    }

    /// Records every kill, and ends the killed node's thread as a real one would once its process
    /// is gone — so an escalation or a straggler kill is observed by the same condvar a real exit
    /// notifies.
    #[derive(Default)]
    struct Runtime {
        handle: Mutex<Weak<RegistryHandle>>,
        by_pid: Mutex<HashMap<i32, AgentId>>,
        killed: Mutex<Vec<AgentId>>,
    }

    impl QuitRuntime for Runtime {
        fn kill_process_tree_and_wait(&self, pid: i32) -> bool {
            let agent = lock(&self.by_pid).get(&pid).cloned();
            if let Some(agent) = agent {
                lock(&self.killed).push(agent.clone());
                if let Some(h) = lock(&self.handle).upgrade()
                    && h.owned_running(&agent).is_some()
                {
                    thread_ends(&h, &agent);
                }
            }
            true
        }
    }

    /// A driver whose harness honours its abort: the process ends `after` the verb.
    struct Honours {
        handle: Weak<RegistryHandle>,
        agent: AgentId,
        after: Duration,
        aborted: Arc<Mutex<Vec<AgentId>>>,
    }

    impl DeliveryPort for Honours {
        fn wake(&self) {}
        fn abort(&self) -> bool {
            lock(&self.aborted).push(self.agent.clone());
            let (handle, agent, after) = (self.handle.clone(), self.agent.clone(), self.after);
            std::thread::spawn(move || {
                std::thread::sleep(after);
                if let Some(h) = handle.upgrade() {
                    thread_ends(&h, &agent);
                }
            });
            true
        }
    }

    /// A driver that takes the abort and whose harness ignores it.
    struct Ignores;

    impl DeliveryPort for Ignores {
        fn wake(&self) {}
        fn abort(&self) -> bool {
            true
        }
    }

    struct Fx {
        _dir: marion_testsupport::Scratch,
        path: std::path::PathBuf,
        handle: Arc<RegistryHandle>,
        runtime: Arc<Runtime>,
        seq: std::sync::atomic::AtomicU64,
        aborted: Arc<Mutex<Vec<AgentId>>>,
    }

    impl Fx {
        fn new(tag: &str) -> Self {
            let dir = scratch(tag);
            let path = dir.join("journal.jsonl");
            let live = Arc::new(LiveRegistry::follow(Registry::boot_path(&path).unwrap()));
            let runtime = Arc::new(Runtime::default());
            let handle = RegistryHandle::with_runtime(live, runtime.clone());
            *lock(&runtime.handle) = Arc::downgrade(&handle);
            Fx {
                _dir: dir,
                path,
                handle,
                runtime,
                seq: std::sync::atomic::AtomicU64::new(0),
                aborted: Arc::default(),
            }
        }

        /// A new supervisor over the same journal, as after a restart: every node the journal
        /// left live is an orphan.
        fn reboot(self) -> Self {
            let live = Arc::new(LiveRegistry::follow(
                Registry::boot_path(&self.path).unwrap(),
            ));
            let runtime = Arc::new(Runtime::default());
            let handle = RegistryHandle::with_runtime(live, runtime.clone());
            *lock(&runtime.handle) = Arc::downgrade(&handle);
            Fx {
                _dir: self._dir,
                path: self.path,
                handle,
                runtime,
                seq: std::sync::atomic::AtomicU64::new(
                    self.seq.load(std::sync::atomic::Ordering::Relaxed),
                ),
                aborted: Arc::default(),
            }
        }

        /// The relaunch's process exists: its `Spawned`, as the launch journals it at
        /// `command.spawn()`, then the pid on its entry.
        fn relaunched(&self, agent: &str, pid: i32) {
            self.write(RecordKind::Spawned(Spawned {
                repo_config_held: Default::default(),
                agent_id: id(agent),
                harness_version: "test".into(),
                model: None,
                pid: Some(pid),
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }));
            self.handle.live.refresh();
            lock(&self.runtime.by_pid).insert(pid, id(agent));
            self.handle.mark_started(&id(agent), pid);
        }

        fn write(&self, kind: RecordKind) {
            let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let line = encode(&JournalRecord {
                writer: WriterId("w".into()),
                seq,
                ts: SystemTime::from_unix_millis(1_000 + seq),
                mono_ns: seq,
                provenance: marion_core::ir::Provenance::marion(),
                src_seq: None,
                kind,
            })
            .unwrap();
            append(&self.path, &line);
        }

        /// A node journaled running under `harness` with `pid`; `owned` claims it, as this
        /// supervisor's `agent/spawn` does, and returns its token.
        fn running(
            &self,
            agent: &str,
            parent: Option<(&str, u32)>,
            harness: Harness,
            pid: i32,
            owned: bool,
        ) -> String {
            self.write(RecordKind::SpawnIntent(SpawnIntent {
                budget: None,
                agent_id: id(agent),
                parent_id: parent.map(|(p, _)| id(p)),
                agent_type: format!("type-{agent}"),
                harness,
                depth: parent.map_or(0, |(_, d)| d),
                task_id: None,
                timeout_secs: None,
                verification: vec![],
                race: None,
                review_of: None,
                workflow: None,
                trust: Default::default(),
            }));
            self.write(RecordKind::Spawned(Spawned {
                repo_config_held: Default::default(),
                agent_id: id(agent),
                harness_version: "test".into(),
                model: None,
                pid: Some(pid),
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }));
            self.write(RecordKind::StateChanged(StateChanged {
                agent_id: id(agent),
                state: NodeState::Running,
                reason: None,
            }));
            self.handle.live.refresh();
            lock(&self.runtime.by_pid).insert(pid, id(agent));
            if !owned {
                return String::new();
            }
            let token = self
                .handle
                .claim(&id(agent), None, "/repo".into())
                .expose()
                .to_string();
            self.handle.mark_started(&id(agent), pid);
            token
        }

        fn honours(&self, agent: &str, after: Duration) {
            let port = Arc::new(Honours {
                handle: Arc::downgrade(&self.handle),
                agent: id(agent),
                after,
                aborted: self.aborted.clone(),
            });
            assert!(self.handle.inboxes.attach_port(&id(agent), port));
        }

        fn cancel(
            &self,
            target: &str,
            caller: Option<(&str, &str)>,
        ) -> Result<NodeCancelResult, RpcError> {
            let out = crate::serve::sink(ConnId(7));
            let call = Call::NodeCancel(NodeCancelParams {
                agent_id: id(target),
                caller: caller.map(|(agent, token)| SpawnCaller {
                    agent_id: id(agent),
                    node_token: token.into(),
                }),
            });
            self.handle.hello_as_operator(ConnId(7));
            match self.handle.call(ConnId(7), &call, &out)? {
                MethodResult::NodeCancel(r) => Ok(r),
                other => panic!("wrong result: {}", other.method().as_str()),
            }
        }

        /// `(record kind, agent)` for every cancel and kill record, in journal order.
        fn trail(&self) -> Vec<(&'static str, String)> {
            std::fs::read(&self.path)
                .unwrap()
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .filter_map(decode)
                .filter_map(|r| match r.kind {
                    RecordKind::CancelRequested(c) => Some(("CancelRequested", c.agent_id.0)),
                    RecordKind::KillIntent(k) => Some(("KillIntent", k.agent_id.0)),
                    RecordKind::KillConfirmed(k) => Some(("KillConfirmed", k.agent_id.0)),
                    RecordKind::Exited(e) => Some(("Exited", e.agent_id.0)),
                    RecordKind::BudgetCrossed(b) => Some(("BudgetCrossed", b.agent_id.0)),
                    _ => None,
                })
                .collect()
        }

        fn node(&self, agent: &str) -> marion_core::registry::ReplayedNode {
            crate::journal::read_path(&self.path)
                .unwrap()
                .get(&id(agent))
                .unwrap()
                .clone()
        }
    }

    fn ended(r: &NodeCancelResult) -> Vec<(String, bool)> {
        r.nodes
            .iter()
            .map(|n| (n.agent_id.0.clone(), n.forced))
            .collect()
    }

    /// **A node whose harness honours its abort is never signalled**: the intent is journaled, the
    /// driver gets the abort, the process ends inside the grace, and the confirmation carries no
    /// signal — `Cancelled`, attributed to the operator, not forced.
    #[test]
    fn a_node_that_honours_its_abort_ends_cancelled_without_a_signal() {
        let fx = Fx::new("cancel-graceful");
        fx.running("root", None, Harness::Pi, 101, true);
        fx.honours("root", Duration::from_millis(50));

        let r = fx
            .cancel("root", None)
            .expect("a running node is cancelled");
        assert_eq!(r.state, NodeState::Exited(ExitStatus::Cancelled));
        assert_eq!(ended(&r), [("root".to_string(), false)]);
        assert!(lock(&fx.runtime.killed).is_empty(), "nothing was signalled");
        assert_eq!(*lock(&fx.aborted), [id("root")]);
        assert_eq!(
            fx.trail(),
            [
                ("CancelRequested", "root".to_string()),
                ("KillConfirmed", "root".to_string())
            ]
        );
        let node = fx.node("root");
        assert_eq!(node.exit.as_ref().unwrap().signal, None);
        assert_eq!(
            node.cancel,
            Some(marion_core::registry::CancelView {
                by: CancelBy::Operator,
                forced: false,
                verb: "channel".into(),
            })
        );
    }

    /// **A spent budget cancels its owner and the subtree below it, attributed to the budget** —
    /// the record of the line comes first, then the cancel the way `node/cancel` runs it. A warn
    /// line journals the crossing and cancels nothing.
    #[test]
    fn a_spent_budget_cancels_its_owners_subtree_and_says_it_was_the_budget() {
        use marion_core::budget::{BudgetLevel, BudgetScope};
        let fx = Fx::new("cancel-budget");
        fx.running("root", None, Harness::Pi, 101, true);
        fx.running("child", Some(("root", 1)), Harness::Pi, 102, true);
        fx.honours("root", Duration::from_millis(20));
        fx.honours("child", Duration::from_millis(20));
        let crossing = |level| crate::budget::Crossing {
            owner: id("root"),
            scope: BudgetScope::Tree,
            level,
            spent: 1_050,
            limit: 1_000,
        };

        fx.handle.budget_crossed(crossing(BudgetLevel::Warn));
        assert_eq!(fx.trail(), [("BudgetCrossed", "root".to_string())]);

        fx.handle.budget_crossed(crossing(BudgetLevel::Stop));
        let trail = fx.trail();
        assert_eq!(trail[1], ("BudgetCrossed", "root".to_string()), "{trail:?}");
        assert!(
            trail[2..].contains(&("CancelRequested", "child".to_string()))
                && trail[2..].contains(&("CancelRequested", "root".to_string())),
            "{trail:?}"
        );
        assert_eq!(
            fx.node("child").cancel.map(|c| c.by),
            Some(CancelBy::Cascade { from: id("root") })
        );
        assert_eq!(
            fx.node("root").cancel.map(|c| c.by),
            Some(CancelBy::Budget {
                owner: id("root"),
                scope: BudgetScope::Tree,
                spent: 1_050,
                limit: 1_000,
            })
        );
    }

    /// **A row with no abort verb is killed at once**, with the intent still durable first — the
    /// cancel's verb is `none` and its confirmation carries marion's signal.
    #[test]
    fn a_node_whose_row_has_no_abort_is_killed_at_once() {
        let fx = Fx::new("cancel-no-verb");
        fx.running("root", None, Harness::ClaudeCode, 101, true);

        let started = Instant::now();
        let r = fx.cancel("root", None).expect("cancelled");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "no grace waited"
        );
        assert_eq!(ended(&r), [("root".to_string(), true)]);
        assert_eq!(*lock(&fx.runtime.killed), [id("root")]);
        let node = fx.node("root");
        assert_eq!(node.exit.as_ref().unwrap().signal, Some(9));
        assert!(node.cancel.unwrap().forced);
        let verb = std::fs::read(&fx.path)
            .unwrap()
            .split(|b| *b == b'\n')
            .filter_map(decode)
            .find_map(|r| match r.kind {
                RecordKind::CancelRequested(c) => Some(c.verb),
                _ => None,
            });
        assert_eq!(verb.as_deref(), Some("none"));
    }

    /// A driver whose lane has no abort to send: the port's default.
    struct NoVerb;

    impl DeliveryPort for NoVerb {
        fn wake(&self) {}
    }

    /// **A row whose verb its lane does not send is killed at once, and said so**: the journal does
    /// not claim a grace ran out that never ran — the row's `channel` verb is named, and that the
    /// lane took none.
    #[test]
    fn a_verb_the_lane_does_not_send_is_killed_at_once_and_says_so() {
        let fx = Fx::new("cancel-no-lane");
        fx.running("root", None, Harness::Pi, 101, true);
        assert!(fx.handle.inboxes.attach_port(&id("root"), Arc::new(NoVerb)));

        let started = Instant::now();
        let r = fx.cancel("root", None).expect("cancelled");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the row's 5 s grace was not waited: nothing was sent to close in it"
        );
        assert_eq!(ended(&r), [("root".to_string(), true)]);
        let exit = fx.node("root").exit.unwrap();
        assert_eq!(exit.signal, Some(9));
        assert!(
            exit.description.contains("channel verb") && exit.description.contains("took none"),
            "{}",
            exit.description
        );
        assert!(
            !exit.description.contains("grace ended"),
            "no grace ran: {}",
            exit.description
        );
    }

    /// **Every row's abort verbs, pinned, and every verb one its lane sends.** A headless
    /// `Channel` verb needs a driver whose port takes the cancel — ACP, app-server, or the duplex
    /// loop on a dialect with an abort frame — and an interactive `Keys` verb needs the paste
    /// injector, which only a `TerminalPaste` row has. A row whose verb its lane cannot send would
    /// journal a grace that never ran; a change to a row's verb changes this table, beside the
    /// measurement that justifies it.
    #[test]
    fn every_rows_abort_verb_is_pinned_and_sent_by_its_lane() {
        use crate::duplex::{Dialect, LaunchPath, launch_path};
        let table = [
            // stream-json's `interrupt` awaits probe P-cancel-1.
            (Harness::ClaudeCode, "none", "none"),
            // S36 P7's `turn/interrupt`; the TUI footer's `esc to interrupt`.
            (Harness::Codex, "channel", "keys"),
            (Harness::OpenCode, "none", "none"),
            (Harness::Copilot, "none", "none"),
            (Harness::Goose, "none", "none"),
            (Harness::Cline, "none", "none"),
            (Harness::Qwen, "none", "none"),
            (Harness::Antigravity, "none", "none"),
            // S34's JSONL `abort`.
            (Harness::Pi, "channel", "none"),
            (Harness::Hermes, "none", "none"),
            // `session/cancel`, the protocol's own.
            (Harness::Acp, "channel", "none"),
        ];
        assert_eq!(table.len(), Harness::ALL.len(), "every row is pinned");
        for (h, headless, interactive) in table {
            let row = marion_harness::adapter::harness_spec(h);
            assert_eq!(
                (row.abort.headless.kind(), row.abort.interactive.kind()),
                (headless, interactive),
                "{h}"
            );
            if let AbortVerb::Channel { .. } = row.abort.headless {
                let sends = match launch_path(&row.surfaces.execution()) {
                    Some(LaunchPath::Acp | LaunchPath::AppServer) => true,
                    Some(LaunchPath::Duplex) => Dialect::of(row).abort().is_some(),
                    _ => false,
                };
                assert!(sends, "{h}: a channel verb no lane of this row sends");
            }
            if let AbortVerb::Keys { .. } = row.abort.interactive {
                assert!(
                    crate::paste::PasteParams::for_row(row).is_some_and(|p| p.abort.is_some()),
                    "{h}: keys no paste injector types"
                );
            }
        }
    }

    /// **Freeze top-down, abort bottom-up**: every intent is written before any confirmation, the
    /// intents in tree order and the confirmations deepest first, each descendant attributed to
    /// the cascade of the named node's cancel. A node this supervisor does not own is killed at
    /// its level; one that ignores its abort is killed when the level's grace ends.
    #[test]
    fn a_cancel_freezes_top_down_and_ends_the_tree_bottom_up() {
        let fx = Fx::new("cancel-cascade");
        fx.running("root", None, Harness::Pi, 101, true);
        fx.running("child", Some(("root", 1)), Harness::Pi, 102, true);
        fx.running("grand", Some(("child", 2)), Harness::ClaudeCode, 103, false);
        fx.honours("root", Duration::from_millis(20));
        fx.honours("child", Duration::from_millis(20));

        let r = fx.cancel("root", None).expect("cancelled");
        assert_eq!(
            ended(&r),
            [
                ("grand".to_string(), true),
                ("child".to_string(), false),
                ("root".to_string(), false)
            ]
        );
        assert_eq!(
            fx.trail(),
            [
                ("CancelRequested", "root".to_string()),
                ("CancelRequested", "child".to_string()),
                ("CancelRequested", "grand".to_string()),
                ("KillConfirmed", "grand".to_string()),
                ("KillConfirmed", "child".to_string()),
                ("KillConfirmed", "root".to_string()),
            ]
        );
        assert_eq!(
            fx.node("grand").cancel.unwrap().by,
            CancelBy::Cascade { from: id("root") }
        );
        assert_eq!(
            *lock(&fx.aborted),
            [id("child"), id("root")],
            "deepest first"
        );
    }

    /// **A second cancel is refused, and `node/kill` is the escalation**: while a node ignores its
    /// abort, the kill ends it at once rather than after its grace, and the cancel keeps its
    /// attribution and writes the one confirmation, forced.
    #[test]
    fn a_kill_during_a_cancel_escalates_it() {
        let fx = Fx::new("cancel-escalate");
        fx.running("root", None, Harness::Pi, 101, true);
        assert!(
            fx.handle
                .inboxes
                .attach_port(&id("root"), Arc::new(Ignores))
        );
        let started = Instant::now();
        let r = std::thread::scope(|s| {
            let cancel = s.spawn(|| fx.cancel("root", None));
            while !fx.handle.cancelling(&id("root")) {
                std::thread::yield_now();
            }
            let again = fx.cancel("root", None).expect_err("already cancelling");
            assert_eq!(again.kind(), Some(FailureKind::Conflict), "{again}");
            let out = crate::serve::sink(ConnId(8));
            fx.handle.hello_as_operator(ConnId(8));
            fx.handle
                .call(
                    ConnId(8),
                    &Call::NodeKill(marion_core::proto::params::NodeKillParams {
                        agent_id: id("root"),
                    }),
                    &out,
                )
                .expect("the kill escalates");
            cancel.join().unwrap().expect("cancelled")
        });
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "the 5 s grace was not waited out"
        );
        assert_eq!(ended(&r), [("root".to_string(), true)]);
        assert_eq!(
            fx.trail(),
            [
                ("CancelRequested", "root".to_string()),
                ("KillConfirmed", "root".to_string())
            ],
            "no KillIntent: the cancel owns the confirmation"
        );
        assert_eq!(fx.node("root").cancel.unwrap().by, CancelBy::Operator);
    }

    /// **A cancelling node spawns nothing**: the one check `agent/spawn` makes for a child and for
    /// a race's seats refuses it for as long as the cancel runs.
    #[test]
    fn a_cancelling_node_may_not_spawn_a_child_or_a_race() {
        let fx = Fx::new("cancel-spawn");
        let token = fx.running("root", None, Harness::Pi, 101, true);
        let caller = SpawnCaller {
            agent_id: id("root"),
            node_token: token.into(),
        };
        fx.handle
            .refuse_if_cancelling(&caller)
            .expect("a running node may spawn");
        assert!(
            fx.handle
                .inboxes
                .attach_port(&id("root"), Arc::new(Ignores))
        );
        std::thread::scope(|s| {
            let cancel = s.spawn(|| fx.cancel("root", None));
            while !fx.handle.cancelling(&id("root")) {
                std::thread::yield_now();
            }
            let e = fx
                .handle
                .refuse_if_cancelling(&caller)
                .expect_err("a cancelling node may not spawn");
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
            // End the cancel now rather than after its grace.
            let out = crate::serve::sink(ConnId(8));
            fx.handle.hello_as_operator(ConnId(8));
            fx.handle
                .call(
                    ConnId(8),
                    &Call::NodeKill(marion_core::proto::params::NodeKillParams {
                        agent_id: id("root"),
                    }),
                    &out,
                )
                .expect("the kill escalates");
            cancel.join().unwrap().expect("cancelled");
        });
    }

    /// **A queued steer is dropped by the cancel and a later one refused**; a cancelling node may
    /// not spawn.
    #[test]
    fn a_cancel_drops_the_queued_steer_and_refuses_later_ones() {
        let fx = Fx::new("cancel-steer");
        fx.running("root", None, Harness::Pi, 101, true);
        let delivery = marion_harness::spec::delivery_for(
            marion_harness::adapter::harness_spec(Harness::Pi),
            NodeShape::Headless,
        );
        let queued = fx
            .handle
            .inboxes
            .enqueue(
                &id("root"),
                delivery,
                crate::inbox::Source::Operator,
                "later".into(),
            )
            .unwrap();
        fx.honours("root", Duration::from_millis(20));
        fx.cancel("root", None).expect("cancelled");
        let dropped = std::fs::read(&fx.path)
            .unwrap()
            .split(|b| *b == b'\n')
            .filter_map(decode)
            .find_map(|r| match r.kind {
                RecordKind::MessageDropped(d) => Some((d.message_id, d.reason)),
                _ => None,
            });
        assert_eq!(
            dropped,
            Some((queued, crate::inbox::CANCELLED_BEFORE_DELIVERY.to_string()))
        );
    }

    /// **A node may cancel only what is below it** (§5.4): its child, yes; its parent, itself or a
    /// forged token, one refusal that says nothing about which.
    #[test]
    fn a_node_cancels_its_descendants_and_nothing_else() {
        let fx = Fx::new("cancel-principal");
        let root = fx.running("root", None, Harness::Pi, 101, true);
        let child = fx.running("child", Some(("root", 1)), Harness::Pi, 102, true);
        fx.honours("root", Duration::from_millis(20));
        fx.honours("child", Duration::from_millis(20));

        for (target, caller) in [
            ("root", ("child", child.as_str())),
            ("child", ("child", child.as_str())),
            ("child", ("root", "forged")),
        ] {
            let e = fx
                .cancel(target, Some(caller))
                .expect_err("not a strict descendant, or not authentic");
            assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
        }
        assert!(fx.trail().is_empty(), "nothing was journaled");

        let r = fx
            .cancel("child", Some(("root", root.as_str())))
            .expect("a parent cancels its child");
        assert_eq!(ended(&r), [("child".to_string(), false)]);
        assert_eq!(
            fx.node("child").cancel.unwrap().by,
            CancelBy::Node { caller: id("root") }
        );
        assert_eq!(
            fx.node("root").state,
            NodeState::Running,
            "the caller runs on"
        );
    }

    /// An orphan, as a restarted supervisor sees it: journaled running under pid 101, its
    /// process the lost supervisor's.
    fn orphan(tag: &str) -> Fx {
        let fx = Fx::new(tag);
        fx.running("root", None, Harness::Pi, 101, false);
        fx.reboot()
    }

    /// Cancel `root` while `during` runs: the cancel has claimed the node and is waiting for its
    /// launch, having signalled nothing, when `during` is called.
    fn cancel_during(fx: &Fx, during: impl FnOnce()) -> Result<NodeCancelResult, RpcError> {
        std::thread::scope(|s| {
            let canceller = s.spawn(|| fx.cancel("root", None));
            assert!(marion_testsupport::until(|| fx
                .handle
                .cancelling(&id("root"))));
            assert!(lock(&fx.runtime.killed).is_empty(), "nothing signalled yet");
            during();
            canceller.join().unwrap()
        })
    }

    /// **A cancel that lands while a resume relaunches the node waits for the relaunch's process
    /// and ends it**: the cancel is not refused for the journal still naming the orphan, nor
    /// signals the orphan's pid; once the launch publishes its process the cancel kills it by the
    /// normal tree kill, and the node's one life ends `Cancelled`, by the operator, with its intent
    /// journaled after the relaunch's `Spawned`. A second resume meanwhile is refused.
    #[test]
    fn a_cancel_during_a_relaunch_waits_for_its_process_and_ends_it() {
        let fx = orphan("cancel-relaunching");
        let claim = fx.handle.claim_relaunch(&id("root"), "/repo".into());
        assert!(claim.is_some(), "an orphan is claimed for its relaunch");
        assert!(
            fx.handle
                .claim_relaunch(&id("root"), "/repo".into())
                .is_none(),
            "a second resume finds the claim"
        );
        let started = Instant::now();
        let r = cancel_during(&fx, || {
            // The launch claims the node for itself, then its process starts.
            let _token = fx.handle.claim(&id("root"), None, "/repo".into());
            fx.relaunched("root", 202);
        })
        .expect("the cancel ends the relaunch");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "woken, not timed out"
        );
        drop(claim);
        assert_eq!(ended(&r), [("root".to_string(), true)]);
        assert_eq!(
            *lock(&fx.runtime.killed),
            [id("root")],
            "killed by the relaunch's pid"
        );
        assert_eq!(
            fx.handle.ended_by(&id("root")),
            Some(CancelBy::Operator),
            "the launch's own claim kept the cancel, so its thread records no exit"
        );
        assert_eq!(
            fx.trail(),
            [
                ("CancelRequested", "root".to_string()),
                ("KillConfirmed", "root".to_string())
            ]
        );
        let node = fx.node("root");
        assert_eq!(node.state, NodeState::Exited(ExitStatus::Cancelled));
        assert_eq!(node.spawn_generation, 2);
        assert_eq!(node.cancel.map(|c| c.by), Some(CancelBy::Operator));
    }

    /// **A cancel of a node whose resume never relaunches it ends it with no signal**: the resume
    /// gives its claim up (it saw the cancel, or failed before launching), and the cancel confirms
    /// the node's end — `Cancelled`, nothing killed, no process started.
    #[test]
    fn a_cancel_of_a_resume_that_never_relaunches_ends_it_without_a_signal() {
        let fx = orphan("cancel-resume-gives-up");
        let claim = fx.handle.claim_relaunch(&id("root"), "/repo".into());
        let r = cancel_during(&fx, || drop(claim)).expect("the cancel confirms the end");
        assert_eq!(ended(&r), [("root".to_string(), false)]);
        assert!(lock(&fx.runtime.killed).is_empty(), "nothing was signalled");
        assert_eq!(
            fx.handle.owned_running(&id("root")),
            None,
            "the claim is gone"
        );
        assert_eq!(
            fx.trail(),
            [
                ("CancelRequested", "root".to_string()),
                ("KillConfirmed", "root".to_string())
            ]
        );
        let node = fx.node("root");
        assert_eq!(node.state, NodeState::Exited(ExitStatus::Cancelled));
        assert_eq!(node.spawn_generation, 1, "no second life");
        assert!(
            node.exit
                .unwrap()
                .description
                .contains("without starting a process"),
            "says no process was started"
        );
    }

    /// **A launch the cancel stopped waiting for is killed the moment its process starts.** Past
    /// the launch's own bound the cancel gives up and reports it, but the node stays claimed for
    /// the cancel: when the process appears, its start kills it by the normal tree kill and
    /// journals the cancel's intent and confirmation, so the node's thread writes no exit of its
    /// own and nothing is left running unaccounted.
    #[test]
    fn a_launch_past_the_cancels_wait_is_killed_as_its_process_starts() {
        let fx = orphan("cancel-late-launch");
        let claim = fx.handle.claim_relaunch(&id("root"), "/repo".into());
        let by = CancelBy::Operator;
        assert!(fx.handle.claim_cancel(&id("root"), &by));
        let member = Member {
            agent_id: id("root"),
            depth: 0,
            pid: None,
            owned: true,
            verb: AbortVerb::None { note: "test" },
            by: by.clone(),
            deferred: Some(CancelRequested {
                agent_id: id("root"),
                was: NodeState::Running,
                by,
                verb: "none".into(),
                grace: Millis(Duration::ZERO),
            }),
        };
        let e = fx
            .handle
            .await_launches(&[&member], Duration::from_millis(50))
            .expect_err("the launch is still coming up");
        assert!(e.to_string().contains("the moment it starts"), "{e}");
        assert!(
            fx.trail().is_empty(),
            "nothing journaled while it has no process"
        );
        assert!(lock(&fx.runtime.killed).is_empty());

        // Late: the launch claims the node for itself, then its process starts.
        let _token = fx.handle.claim(&id("root"), None, "/repo".into());
        fx.relaunched("root", 202);
        drop(claim);
        assert_eq!(
            *lock(&fx.runtime.killed),
            [id("root")],
            "killed as it started"
        );
        assert_eq!(
            fx.trail(),
            [
                ("CancelRequested", "root".to_string()),
                ("KillConfirmed", "root".to_string())
            ]
        );
        assert_eq!(
            fx.handle.ended_by(&id("root")),
            Some(CancelBy::Operator),
            "its thread records no exit of its own"
        );
        let node = fx.node("root");
        assert_eq!(node.state, NodeState::Exited(ExitStatus::Cancelled));
        assert_eq!(node.exit.unwrap().signal, Some(9));
        assert_eq!(node.cancel.map(|c| c.by), Some(CancelBy::Operator));
    }

    /// **A cancel left without a confirmation on a node that had already exited is inert.** A
    /// resume of an exited node claimed for a cancel that then never relaunches journals the
    /// cancel's intent and no confirmation (the node is already ended). Replay keeps the earlier
    /// end, no cancel is pending in memory or after a restart, the node can be resumed again, and
    /// its next life carries no cancel from the last one.
    #[test]
    fn a_dangling_cancel_on_an_exited_node_is_inert_on_replay_and_resume() {
        let fx = Fx::new("cancel-dangling");
        fx.running("root", None, Harness::Pi, 101, false);
        fx.write(RecordKind::Exited(Exited {
            agent_id: id("root"),
            status: ExitStatus::Failed,
            exit: ProcessExit {
                code: Some(1),
                signal: None,
                description: "failed".into(),
            },
        }));
        let fx = fx.reboot();
        let claim = fx.handle.claim_relaunch(&id("root"), "/repo".into());
        let r = cancel_during(&fx, || drop(claim)).expect("the cancel is answered");
        assert_eq!(ended(&r), [("root".to_string(), false)]);
        assert_eq!(
            fx.trail(),
            [
                ("Exited", "root".to_string()),
                ("CancelRequested", "root".to_string())
            ],
            "the intent, and no confirmation"
        );
        assert!(!fx.handle.cancelling(&id("root")));

        let fx = fx.reboot();
        let node = fx.node("root");
        assert_eq!(
            node.state,
            NodeState::Exited(ExitStatus::Failed),
            "its own end kept"
        );
        assert_eq!(node.cancel, None, "no cancel attributed to an ended node");
        assert!(
            !fx.handle.cancelling(&id("root")),
            "nothing pending after a restart"
        );
        let e = fx.cancel("root", None).expect_err("an ended node");
        assert_eq!(e.kind(), Some(FailureKind::Refused), "{e}");
        let claim = fx.handle.claim_relaunch(&id("root"), "/repo".into());
        assert!(claim.is_some(), "it can be resumed again");
        let _token = fx.handle.claim(&id("root"), None, "/repo".into());
        fx.relaunched("root", 202);
        let node = fx.node("root");
        assert_eq!(node.spawn_generation, 2);
        assert_eq!(node.cancel, None, "the new life carries no cancel");
        assert!(!node.state.is_exited());
    }

    /// **A relaunch that fails while a cancel waits ends the node all the same**: the launch claimed
    /// the node and ended before its process, and the cancel confirms its end with no signal.
    #[test]
    fn a_relaunch_that_fails_under_a_pending_cancel_still_ends_cancelled() {
        let fx = orphan("cancel-relaunch-fails");
        let claim = fx.handle.claim_relaunch(&id("root"), "/repo".into());
        let r = cancel_during(&fx, || {
            let _token = fx.handle.claim(&id("root"), None, "/repo".into());
            fx.handle.mark_finished(
                &id("root"),
                NodeOutcome::Root(Err("the version probe failed".into())),
            );
        })
        .expect("the cancel confirms the end");
        drop(claim);
        assert_eq!(ended(&r), [("root".to_string(), false)]);
        assert!(lock(&fx.runtime.killed).is_empty(), "nothing was signalled");
        assert_eq!(fx.handle.owned_running(&id("root")), Some(false));
        assert_eq!(
            fx.node("root").state,
            NodeState::Exited(ExitStatus::Cancelled)
        );
    }

    /// **The refusals come before any side effect**: an unknown node, an ended one, and a subtree
    /// with a node still spawning are refused with nothing journaled or signalled.
    #[test]
    fn a_cancel_is_refused_before_any_side_effect() {
        let fx = Fx::new("cancel-refused");
        let e = fx.cancel("nobody", None).expect_err("no such node");
        assert_eq!(e.kind(), Some(FailureKind::NotFound), "{e}");

        fx.running("root", None, Harness::Pi, 101, true);
        fx.write(RecordKind::SpawnIntent(SpawnIntent {
            budget: None,
            agent_id: id("spawning"),
            parent_id: Some(id("root")),
            agent_type: "type-spawning".into(),
            harness: Harness::Pi,
            depth: 1,
            task_id: None,
            timeout_secs: None,
            verification: vec![],
            race: None,
            review_of: None,
            workflow: None,
            trust: Default::default(),
        }));
        fx.handle.live.refresh();
        let e = fx.cancel("root", None).expect_err("a node without a pid");
        assert_eq!(e.kind(), Some(FailureKind::Conflict), "{e}");
        assert!(fx.trail().is_empty());
        assert!(lock(&fx.runtime.killed).is_empty());
        assert!(
            !fx.handle.cancelling(&id("root")),
            "the claim was not taken"
        );
    }

    /// **A cancel that loses the race to the node's own exit takes nothing**: the node's thread saw
    /// its process end first, so the cancel claims no end, journals and signals nothing, and the
    /// thread's own record is the one exit the node gets.
    #[test]
    fn a_cancel_racing_the_nodes_own_exit_leaves_the_exit_to_its_thread() {
        let fx = Fx::new("cancel-lost-race");
        fx.running("root", None, Harness::Pi, 101, true);
        fx.honours("root", Duration::from_millis(50));
        assert!(
            !fx.handle.process_ended(&id("root")),
            "no one claimed the end, so the thread records it"
        );

        let e = fx
            .cancel("root", None)
            .expect_err("its process already ended");
        assert_eq!(e.kind(), Some(FailureKind::Conflict), "{e}");
        assert!(fx.trail().is_empty(), "the cancel journaled nothing");
        assert!(lock(&fx.runtime.killed).is_empty(), "nothing was signalled");
        assert!(lock(&fx.aborted).is_empty(), "no abort was sent");
        assert_eq!(fx.handle.ended_by(&id("root")), None);
    }

    /// **A cancel whose intent fails to journal hands the node's end back**: nothing is signalled,
    /// and when the process ends its thread records its own exit, instead of leaving it to a
    /// cancel that will never confirm it.
    #[test]
    fn a_cancel_whose_intent_fails_to_journal_gives_the_end_back() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fx::new("cancel-journal-fails");
        fx.running("root", None, Harness::Pi, 101, true);
        fx.honours("root", Duration::from_millis(50));
        let writable = |mode| {
            std::fs::set_permissions(&fx.path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        writable(0o444);

        let e = fx.cancel("root", None);
        writable(0o644);
        let e = e.expect_err("the intent could not be journaled");
        assert!(fx.trail().is_empty(), "nothing reached the journal");
        assert!(lock(&fx.runtime.killed).is_empty(), "nothing was signalled");
        assert!(lock(&fx.aborted).is_empty(), "no abort was sent: {e}");
        assert!(
            !fx.handle.cancelling(&id("root")),
            "the claim was handed back"
        );
        assert!(
            !fx.handle.process_ended(&id("root")),
            "the thread records its own exit"
        );
    }
}
