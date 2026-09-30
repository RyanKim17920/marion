//! **The handles this bridge has handed out** — what a `spawn { background: true }` returns and
//! what a `wait` resolves against.
//!
//! # What this used to be, and what §11 item 28 step 5 took out of it
//!
//! This module was ~700 lines and it ran children. A backgrounded `spawn` started a thread inside
//! the bridge process, that thread called `run::run_spawn`, and this table held the `JoinHandle`,
//! the outcome channel and the deadline a `wait` blocked on. The bridge is a process the *harness*
//! starts and owns — s16 measured what that means (SIGINT, SIGTERM 100 ms later, SIGKILL ~450 ms
//! after that, pid-targeted, and **no EOF at all**) — so a backgrounded child's lifetime was
//! bounded by a process marion does not control, and §11 item 30 recorded the six journal shapes a
//! kill could leave behind.
//!
//! Step 5 moved the child to the supervisor ([`crate::courier`]). Everything that existed to *own*
//! a child went with it:
//!
//! * **the thread and its `JoinHandle`** — the node's thread is the supervisor's now;
//! * **the outcome channel and `WAIT_GRACE`'s `recv_timeout`** — a `wait` reads the node's own
//!   stream through `node/attach` and is bounded by the socket's read timeout;
//! * **`join_all` and §5.7's EOF hold** — there is nothing left in this process for an exiting
//!   bridge to hold *for*. The hold was correct while a child lived here and is now a hold over
//!   nothing; a bridge that leaves while its children run is the whole point of the move, and
//!   `tests/background_spawn.rs` asserts it rather than the reverse;
//! * **`live_children`** — §6.1 step 2's concurrency gate reads
//!   `handler::RegistryHandle::live_children_of`, which counts the caller's non-terminal children
//!   out of the registry. That is exact where this table was merely local: this one is per bridge
//!   *process*, empty after a restart, and cannot see a sibling started through another bridge.
//!   The paragraph that used to stand here argued the opposite — that the table was authoritative
//!   *by construction*, because `Spawned` was journaled only after a child had finished, so a
//!   journal read could not see a child between "thread started" and "process observed". Step 1
//!   inverted that premise: `SpawnIntent` is journaled before every side effect and `Spawned` at
//!   `command.spawn()`, so a child is in the journal from the first instant it exists at all.
//!
//! # Why anything is left
//!
//! §5.4 addresses a `wait` by the `task_id` the handle carried, and the supervisor's fifteen
//! methods do not include a lookup from a task id to a node — [`marion_core::proto::Method::ALL`] is
//! pinned at fifteen and step 5 deliberately adds none. So the one fact this process must remember
//! is the pairing the supervisor told it exactly once, in `agent/spawn`'s answer: **which node this
//! handle is about**. That is what is left, plus the two bookkeeping facts that let a second `wait`
//! be answered honestly.
//!
//! It stays per bridge process and not a `static`, for the reason it always did: a process-wide
//! table would make one unit test's answers depend on which other tests had run.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use marion_core::contract::{AgentId, TaskId};

/// One handle this bridge handed out.
struct Handed {
    /// What the caller holds. `wait` addresses a child by this.
    task_id: TaskId,
    /// **The contract file this run will be filed under, or `None` for a root.**
    ///
    /// Separate from [`Self::task_id`] because for a *root* the two are not the same thing. §9
    /// gives a root no `TaskContract`, so `agent/spawn` answers it with no `task_id` at all — and a
    /// handle still has to be *something*, or a top-level `spawn { background: true }` could hand
    /// nothing back and its `wait` could never name what it was waiting for. So the handle is
    /// minted from the node's own id, which the supervisor did name, and this field records the one
    /// thing that is genuinely absent: the file. A single field would have to encode "the handle"
    /// and "the file" in one string and would send `wait` looking for `contracts/<agent-id>.json`,
    /// a path nothing writes.
    contract: Option<TaskId>,
    /// **The node the id is about** — the pairing only `agent/spawn`'s answer carries, and the
    /// reason this table still exists.
    agent_id: AgentId,
    /// The resolved agent type name, so a `wait`'s answer can name what it is talking about without
    /// the caller having kept the request.
    agent_type: String,
    /// **How long a `wait` on this child may block**: the child's own effective wall clock plus a
    /// grace for everything `run_spawn` does around the run. Stored per child rather than derived
    /// at `wait` time because a `wait` frame carries no request and must not invent a bound of its
    /// own — this is the node's own clock, kept.
    wait_bound: Duration,
    /// Whether a watcher is already announcing this node's end to the parent ([`crate::mcp`]'s
    /// push). Set by [`Background::unwatched`], exactly once per handle, so two replies cannot
    /// start two watchers and two announcements for one child.
    watched: bool,
    /// What the `wait` that collected this child actually got, once one has.
    ///
    /// Recorded rather than recomputed because the answer to a *second* `wait` depends on it: a
    /// child that produced a `TaskContract` has a file on disk to point the caller at, and one that
    /// produced an error has nothing at all. Saying "it is on disk" about the second is the
    /// false-receipt shape this codebase keeps deleting.
    collected: Option<Collected>,
    /// How many `wait`s on this handle are blocked right now ([`Background::waiting`]). Each will
    /// hand its caller this node's end, so a push announcing it meanwhile is the same news twice.
    waits: usize,
}

/// What an earlier `wait` walked away with, remembered only so a later one can be told the truth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Collected {
    /// A `TaskContract`. The supervisor persisted it before the node's closing bookend, so it
    /// really is on disk under that child's agent directory and a caller that lost it can go and
    /// read it.
    Contract,
    /// **A root's terminal status.** §9 gives a root no `TaskContract`, so a second `wait` must not
    /// be pointed at a contracts directory — but it must also not be told "nothing was produced",
    /// which is [`Self::NoContract`]'s sentence and is false: the node ran, and its own
    /// `events.jsonl` is on disk and is the record. Its own variant rather than either neighbour,
    /// because both neighbours would be a wrong sentence about a run that went fine.
    Ended,
    /// A refusal, an abort, a contract marion could not read. There is no file and nothing to
    /// re-read; the caller was told everything there is.
    NoContract,
}

/// The handles one bridge process is holding.
#[derive(Default)]
pub struct Background {
    handed: Mutex<Vec<Handed>>,
    races: Mutex<Vec<HandedRace>>,
}

/// A race this bridge started: its handle is the race's id, and a `wait` on it is a wait for the
/// decision rather than for one node.
struct HandedRace {
    race_id: marion_core::race::RaceId,
    wait_bound: Duration,
    collected: bool,
    /// A watcher is announcing this race's decision ([`Background::unwatched_races`]).
    watched: bool,
    /// `wait`s on this race blocked right now ([`Background::race_waiting`]).
    waits: usize,
}

/// What a `wait` found for a race handle.
pub enum RaceWait {
    Pending { bound: Duration },
    AlreadyCollected,
}

/// What a backgrounded `spawn` hands back — enough to `wait` on, and nothing that requires a
/// monitor to interpret (§7.6's worked example: *"nothing is ever handed back that requires a
/// monitor to interpret"*).
pub struct Started {
    pub task_id: TaskId,
    pub agent_type: String,
    /// **Whether the `wait` this handle promises will return a contract.** A child's will; a root's
    /// will return the node's terminal status instead, because §9 gives it no contract. Carried
    /// here so [`crate::bridge::background_result`] can say which — a handle whose sentence
    /// promises a "completed task contract" for a node that can never produce one is the
    /// false-receipt shape, one call earlier than usual.
    pub has_contract: bool,
}

/// One handle a watcher announces the end of: the same pairing a `wait` reads, so the watcher
/// blocks on [`crate::courier::await_contract`] exactly as a `wait` would, on the node's own clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchTarget {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub agent_type: String,
    pub bound: Duration,
    /// See [`Handed::contract`]: `None` is a root, whose end is its own bookend.
    pub contract: Option<TaskId>,
}

/// What a `wait` found in this table. The blocking is [`crate::courier::await_contract`]'s.
pub enum Wait {
    /// This handle names a node, and nobody has collected it yet.
    Pending {
        agent_id: AgentId,
        agent_type: String,
        bound: Duration,
        /// See [`Handed::contract`]: `None` is a root, whose wait ends at the node's own bookend.
        contract: Option<TaskId>,
    },
    /// **This bridge process's table has no row with that id**, which is narrower than "you may not
    /// wait on that".
    ///
    /// §5.4 permits `wait` against *descendants*, and a descendant is not the same set as a child:
    /// an ancestor waiting on a **grandchild** is inside §5.4 and outside this table, because the
    /// grandchild's handle was issued by its own parent's bridge instance. It is also not
    /// restart-durable — the table holds no journal, so a bridge that died and was restarted
    /// answers `Unknown` to a handle it really did issue and really has lost.
    Unknown,
    /// A `wait` on a child an earlier `wait` already collected, and **what that earlier `wait`
    /// got**. Distinguished from [`Wait::Unknown`] on purpose: "you already have this" and "no such
    /// child" are different mistakes with different fixes.
    AlreadyCollected(Collected),
}

impl Background {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Handed>> {
        // Poisoning is recovered from rather than propagated, for `spawn::repo_write_guard`'s
        // reason: one poisoned lock must not make every later `spawn` and `wait` in this process
        // fail. A panic cannot leave a vector half-updated across a lock boundary.
        self.handed.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record the pairing `agent/spawn` just answered with, and hand back what the caller is told.
    pub fn hand_out(
        &self,
        task_id: TaskId,
        contract: Option<TaskId>,
        agent_id: AgentId,
        agent_type: String,
        wait_bound: Duration,
    ) -> Started {
        let started = Started {
            task_id: task_id.clone(),
            agent_type: agent_type.clone(),
            has_contract: contract.is_some(),
        };
        self.lock().push(Handed {
            task_id,
            contract,
            agent_id,
            agent_type,
            wait_bound,
            watched: false,
            collected: None,
            waits: 0,
        });
        started
    }

    /// Record the pairing for a **blocking** `spawn`, whose outcome is about to be delivered in
    /// its own reply: `wait` and `status` then resolve its `task_id` like any handle's, and it is
    /// born watched so no push ever announces an end the caller was already handed. Measured live
    /// (2026-09-22): without the row, `wait` on a blocking child's `task_id` answered "no record".
    pub fn delivered_inline(
        &self,
        task_id: TaskId,
        contract: Option<TaskId>,
        agent_id: AgentId,
        agent_type: String,
        wait_bound: Duration,
    ) {
        self.lock().push(Handed {
            task_id,
            contract,
            agent_id,
            agent_type,
            wait_bound,
            watched: true,
            collected: None,
            waits: 0,
        });
    }

    /// **Every handle no watcher is announcing yet — and from now on, each of them is.**
    ///
    /// The sweep is called after each reply has been written, so the handle's own reply always
    /// precedes its announcement on the pipe. A collected handle is skipped: its parent already
    /// holds the outcome, and an announcement after a delivery is the same news twice.
    pub fn unwatched(&self) -> Vec<WatchTarget> {
        self.lock()
            .iter_mut()
            .filter(|h| !h.watched && h.collected.is_none())
            .map(|h| {
                h.watched = true;
                WatchTarget {
                    task_id: h.task_id.clone(),
                    agent_id: h.agent_id.clone(),
                    agent_type: h.agent_type.clone(),
                    bound: h.wait_bound,
                    contract: h.contract.clone(),
                }
            })
            .collect()
    }

    /// Which node a handle is about, or why it cannot be resolved here.
    pub fn resolve(&self, task_id: &str) -> Wait {
        let handed = self.lock();
        let Some(h) = handed.iter().find(|h| h.task_id.0 == task_id) else {
            return Wait::Unknown;
        };
        match h.collected {
            Some(what) => Wait::AlreadyCollected(what),
            None => Wait::Pending {
                agent_id: h.agent_id.clone(),
                agent_type: h.agent_type.clone(),
                bound: h.wait_bound,
                contract: h.contract.clone(),
            },
        }
    }

    /// **Which node a handle is about, whatever an earlier `wait` did with it** — what `status`
    /// resolves against.
    ///
    /// Deliberately *not* [`Self::resolve`], and the difference is §5.4's own. `wait` is a delivery
    /// and an outcome is delivered once, so a collected handle is an error to wait on again.
    /// `status` is a **read**, permitted against a target in *"any"* state, *"terminal included"* —
    /// so a handle whose contract was already collected still names a node the supervisor can be
    /// asked about, and refusing there would make `status` useless at exactly the moment a caller
    /// wants it. This returns the pairing and says nothing about collection, because collection is
    /// a fact about the *handle* and `status` is a question about the *node*.
    pub fn node_of(&self, task_id: &str) -> Option<(AgentId, String)> {
        self.lock()
            .iter()
            .find(|h| h.task_id.0 == task_id)
            .map(|h| (h.agent_id.clone(), h.agent_type.clone()))
    }

    /// **The handle an `id` names**: a `task_id` as itself, else the handle whose node
    /// [`crate::tree::resolve_node`] picks out of this table — a whole agent id, the short id a
    /// tree row shows, or a unique start of one. `Ok(None)` when it names no handle here; `Err`
    /// naming every candidate when it could be more than one.
    pub fn task_of(&self, id: &str) -> Result<Option<String>, String> {
        let handed = self.lock();
        if handed.iter().any(|h| h.task_id.0 == id) {
            return Ok(Some(id.to_string()));
        }
        let agent = crate::tree::resolve_node(id, handed.iter().map(|h| h.agent_id.0.as_str()))?;
        Ok(handed
            .iter()
            .find(|h| h.agent_id == agent)
            .map(|h| h.task_id.0.clone()))
    }

    /// **A `wait` on `task_id` is blocked until the guard drops.** While one is, the node's end is
    /// on its way to the caller through it, so [`Self::announceable`] says no.
    pub fn waiting(&self, task_id: &str) -> Waiting<'_> {
        if let Some(h) = self.lock().iter_mut().find(|h| h.task_id.0 == task_id) {
            h.waits += 1;
        }
        Waiting {
            bg: self,
            task_id: task_id.to_string(),
        }
    }

    /// Whether a push announcing `task_id`'s end would be news: no `wait` has collected it and
    /// none is blocked on it. `false` for a handle this bridge never handed out.
    pub fn announceable(&self, task_id: &str) -> bool {
        self.lock()
            .iter()
            .find(|h| h.task_id.0 == task_id)
            .is_some_and(|h| h.collected.is_none() && h.waits == 0)
    }

    /// Record a race's handle. The race's own seats are not rows here: the race is what the caller
    /// waits on, and each seat's end is the supervisor's to weigh.
    pub fn hand_out_race(&self, race_id: marion_core::race::RaceId, wait_bound: Duration) {
        self.races
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(HandedRace {
                race_id,
                wait_bound,
                collected: false,
                watched: false,
                waits: 0,
            });
    }

    /// **Every race no watcher is announcing yet — and from now on, each of them is**:
    /// [`Self::unwatched`] for races, with the race's own wait bound.
    pub fn unwatched_races(&self) -> Vec<(marion_core::race::RaceId, Duration)> {
        self.races
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .filter(|r| !r.watched && !r.collected)
            .map(|r| {
                r.watched = true;
                (r.race_id.clone(), r.wait_bound)
            })
            .collect()
    }

    /// [`Self::announceable`] for a race: no `wait` collected its decision and none is blocked on
    /// it.
    pub fn race_announceable(&self, handle: &str) -> bool {
        self.races
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|r| r.race_id.0 == handle)
            .is_some_and(|r| !r.collected && r.waits == 0)
    }

    /// A `wait` on a race is starting, until the guard drops ([`Self::waiting`] for a race).
    pub fn race_waiting(&self, handle: &str) -> RaceWaiting<'_> {
        self.race_waits(handle, |w| *w += 1);
        RaceWaiting {
            bg: self,
            handle: handle.to_string(),
        }
    }

    fn race_waits(&self, handle: &str, f: impl FnOnce(&mut usize)) {
        if let Some(r) = self
            .races
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|r| r.race_id.0 == handle)
        {
            f(&mut r.waits);
        }
    }

    /// The race a handle names, if it names one this bridge started.
    pub fn race(&self, handle: &str) -> Option<(marion_core::race::RaceId, RaceWait)> {
        let races = self.races.lock().unwrap_or_else(|e| e.into_inner());
        let r = races.iter().find(|r| r.race_id.0 == handle)?;
        let wait = if r.collected {
            RaceWait::AlreadyCollected
        } else {
            RaceWait::Pending {
                bound: r.wait_bound,
            }
        };
        Some((r.race_id.clone(), wait))
    }

    /// A `wait` got this race's decision.
    pub fn race_collected(&self, handle: &str) {
        if let Some(r) = self
            .races
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|r| r.race_id.0 == handle)
        {
            r.collected = true;
        }
    }

    /// Remember *what* a `wait` collected, not merely that something was. See [`Collected`].
    ///
    /// **Only a `wait` that reached the node's terminal state collects.** One that expired against
    /// its bound leaves the handle exactly as collectable as it found it, because the child really
    /// is still running and its contract really will be written.
    pub fn collected(&self, task_id: &str, what: Collected) {
        if let Some(h) = self.lock().iter_mut().find(|h| h.task_id.0 == task_id) {
            h.collected = Some(what);
        }
    }
}

/// A `wait` in flight on one race, from [`Background::race_waiting`] until it drops.
pub struct RaceWaiting<'a> {
    bg: &'a Background,
    handle: String,
}

impl Drop for RaceWaiting<'_> {
    fn drop(&mut self) {
        self.bg
            .race_waits(&self.handle, |w| *w = w.saturating_sub(1));
    }
}

/// A `wait` in flight on one handle, from [`Background::waiting`] until it drops.
pub struct Waiting<'a> {
    bg: &'a Background,
    task_id: String,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if let Some(h) = self
            .bg
            .lock()
            .iter_mut()
            .find(|h| h.task_id.0 == self.task_id)
        {
            h.waits = h.waits.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A race is watched once and announced only as news**: the first sweep hands it out and
    /// later ones do not, a blocked `wait` or a collected decision silences the push, and the
    /// guard's drop restores it.
    #[test]
    fn a_race_is_watched_once_and_announced_only_while_nobody_waits_on_it() {
        let bg = Background::new();
        let race = marion_core::race::RaceId("r-1".into());
        bg.hand_out_race(race.clone(), Duration::from_secs(5));
        assert_eq!(
            bg.unwatched_races(),
            vec![(race.clone(), Duration::from_secs(5))]
        );
        assert!(bg.unwatched_races().is_empty(), "one watcher per race");
        assert!(bg.race_announceable("r-1"));
        {
            let _waiting = bg.race_waiting("r-1");
            assert!(!bg.race_announceable("r-1"), "a blocked wait returns it");
        }
        assert!(bg.race_announceable("r-1"));
        bg.race_collected("r-1");
        assert!(!bg.race_announceable("r-1"), "a wait already returned it");
        assert!(
            !bg.race_announceable("r-2"),
            "a race this bridge never started"
        );
    }

    /// **A race handle is its race's id**, pending until a `wait` collects it, and never a node
    /// row: `resolve` and `node_of` know nothing of it.
    #[test]
    fn a_race_handle_resolves_to_its_race_until_collected() {
        let bg = Background::new();
        let race = marion_core::race::RaceId("r-1".into());
        bg.hand_out_race(race.clone(), Duration::from_secs(9));
        assert!(matches!(
            bg.race("r-1"),
            Some((r, RaceWait::Pending { bound })) if r == race && bound == Duration::from_secs(9)
        ));
        assert!(bg.race("r-2").is_none());
        assert!(matches!(bg.resolve("r-1"), Wait::Unknown));
        assert!(bg.node_of("r-1").is_none());
        bg.race_collected("r-1");
        assert!(matches!(
            bg.race("r-1"),
            Some((_, RaceWait::AlreadyCollected))
        ));
    }

    fn hand(bg: &Background, task: &str) {
        bg.hand_out(
            TaskId(task.into()),
            Some(TaskId(task.into())),
            AgentId(format!("node-for-{task}")),
            "codex-impl".into(),
            Duration::from_secs(30),
        );
    }

    /// **A push announces only news**: not an end a `wait` already collected, and not one a `wait`
    /// in flight is about to return — that `wait` hands its caller the same end, and a push
    /// beside it is a second turn for one fact. Once the `wait` gives up (its bound), the end is
    /// news again.
    #[test]
    fn an_end_is_announced_only_while_no_wait_has_or_is_taking_it() {
        let bg = Background::new();
        hand(&bg, "task-1");
        assert!(bg.announceable("task-1"));
        {
            let _waiting = bg.waiting("task-1");
            assert!(!bg.announceable("task-1"), "a wait in flight returns it");
        }
        assert!(bg.announceable("task-1"), "the wait left without it");
        bg.collected("task-1", Collected::Contract);
        assert!(!bg.announceable("task-1"), "already collected");
        assert!(!bg.announceable("no-such-task"));
    }

    /// **A handle resolves to the node it is about**, which is the one fact this table exists for:
    /// `wait` is addressed by `task_id` and the supervisor is addressed by `agent_id`, and the
    /// pairing is told to this process exactly once.
    #[test]
    fn a_handle_resolves_to_the_node_the_supervisor_named() {
        let bg = Background::new();
        hand(&bg, "task-1");
        let Wait::Pending {
            agent_id,
            agent_type,
            ..
        } = bg.resolve("task-1")
        else {
            panic!("a handle this bridge handed out resolves");
        };
        assert_eq!(agent_id, AgentId("node-for-task-1".into()));
        assert_eq!(
            agent_type, "codex-impl",
            "a `wait` frame carries no agent type, so the answer's name comes from here"
        );
    }

    /// **A blocking spawn's `task_id` resolves too.** Measured live (2026-09-22): a parent that got
    /// a blocking child's contract called `wait` with that contract's `task_id` and was told "this
    /// supervisor has no record of task_id", though the contract was on disk. The row exists for
    /// `wait` and `status`; it is born watched, because its outcome was already delivered inline
    /// and a push announcing it would be the same news twice.
    #[test]
    fn a_blocking_spawns_task_id_resolves_and_is_never_announced() {
        let bg = Background::new();
        bg.delivered_inline(
            TaskId("task-b".into()),
            Some(TaskId("task-b".into())),
            AgentId("node-b".into()),
            "codex-impl".into(),
            Duration::from_secs(30),
        );
        assert!(matches!(bg.resolve("task-b"), Wait::Pending { .. }));
        assert_eq!(
            bg.node_of("task-b").map(|(a, _)| a),
            Some(AgentId("node-b".into()))
        );
        assert!(bg.unwatched().is_empty(), "delivered inline, never pushed");
    }

    /// **A root's handle resolves to its node and says there is no contract file** — the one thing
    /// that distinguishes it from a child's, and the thing a `wait` must not get wrong.
    ///
    /// §9 gives a root no `TaskContract`, so `agent/spawn` answers a root with no `task_id`. The
    /// handle is minted from the node's own id instead; what is *absent* is the file, and that
    /// absence is recorded here rather than re-derived at `wait` time. A row that carried
    /// `Some(handle)` here would send `wait` to read `contracts/<agent-id>.json` — a path nothing
    /// writes — and a successful root would come back as `NoContract`, which says marion lost
    /// something.
    #[test]
    fn a_roots_handle_names_its_node_and_no_contract_file() {
        let bg = Background::new();
        let node = AgentId("019f-root".into());
        let started = bg.hand_out(
            TaskId(node.0.clone()),
            None,
            node.clone(),
            "codex".into(),
            Duration::from_secs(30),
        );
        assert!(
            !started.has_contract,
            "the sentence handed to the caller must not promise a contract this node cannot write"
        );
        let Wait::Pending {
            agent_id, contract, ..
        } = bg.resolve(&started.task_id.0)
        else {
            panic!("a root's handle resolves like any other");
        };
        assert_eq!(agent_id, node, "and it names the node the supervisor named");
        assert_eq!(
            contract, None,
            "and reports no contract file, so `wait` ends at the node's own bookend rather than \
             reading a path nothing writes"
        );
        assert!(
            bg.node_of(&started.task_id.0).is_some(),
            "and `status` resolves it too — a root is as readable as any other node"
        );
    }

    /// A `wait` for a handle nobody issued here is `Unknown`, not a panic and not a blocking read
    /// against a node id this process invented.
    #[test]
    fn waiting_on_an_id_this_bridge_never_handed_out_is_refused_rather_than_awaited() {
        assert!(matches!(
            Background::new().resolve("task-nobody-started"),
            Wait::Unknown
        ));
    }

    /// **A second `wait` is told what the first one actually got**, so the bridge never promises a
    /// file that was never written — and it is still distinguishable from a handle this process
    /// never issued, because the two have different fixes.
    #[test]
    fn a_second_wait_is_told_what_the_first_one_got_and_is_not_an_unknown_handle() {
        let bg = Background::new();
        hand(&bg, "task-1");
        hand(&bg, "task-2");
        bg.collected("task-1", Collected::Contract);
        bg.collected("task-2", Collected::NoContract);
        assert!(matches!(
            bg.resolve("task-1"),
            Wait::AlreadyCollected(Collected::Contract)
        ));
        assert!(
            matches!(
                bg.resolve("task-2"),
                Wait::AlreadyCollected(Collected::NoContract)
            ),
            "a spawn that failed has nothing on disk, and the second wait must be able to say so"
        );
        assert!(matches!(bg.resolve("task-3"), Wait::Unknown));
    }

    /// **A handle is handed to a watcher exactly once, and a `wait` after the watcher's push
    /// still finds it collectable.** The push is an announcement, not a delivery: it marks
    /// nothing collected, so the parent's `wait` returns the same contract the push carried
    /// rather than "you already have this" about a document it was only told about.
    #[test]
    fn a_handle_is_watched_once_and_a_wait_after_the_push_still_collects() {
        let bg = Background::new();
        hand(&bg, "task-1");
        hand(&bg, "task-2");
        let first: Vec<String> = bg.unwatched().into_iter().map(|t| t.task_id.0).collect();
        assert_eq!(first, ["task-1", "task-2"]);
        assert!(
            bg.unwatched().is_empty(),
            "a second sweep hands out nothing: one watcher per handle"
        );
        hand(&bg, "task-3");
        let later: Vec<String> = bg.unwatched().into_iter().map(|t| t.task_id.0).collect();
        assert_eq!(
            later,
            ["task-3"],
            "and a handle handed out later is watched from then on"
        );
        // What the watcher does to the table on a push: nothing.
        assert!(matches!(bg.resolve("task-1"), Wait::Pending { .. }));
        bg.collected("task-1", Collected::Contract);
        assert!(
            bg.unwatched().is_empty(),
            "a collected handle is never handed to a watcher"
        );
    }

    /// The target carries everything `courier::await_contract` needs, read off the row rather
    /// than re-derived: the node, its own clock, and whether there is a contract file to read.
    #[test]
    fn a_watch_target_is_the_rows_own_pairing() {
        let bg = Background::new();
        hand(&bg, "task-1");
        let t = bg.unwatched().pop().unwrap();
        assert_eq!(t.agent_id, AgentId("node-for-task-1".into()));
        assert_eq!(t.agent_type, "codex-impl");
        assert_eq!(t.bound, Duration::from_secs(30));
        assert_eq!(t.contract, Some(TaskId("task-1".into())));
    }

    /// **An expired `wait` leaves the handle exactly as collectable as it found it.**
    ///
    /// The bound is on how long marion holds a *caller's turn*, never on the node: the child keeps
    /// its own wall clock and its contract will still be written. A `StillRunning` answer that
    /// marked the row collected would turn "ask again later" into "you have already been told".
    #[test]
    fn a_wait_that_expired_did_not_collect_anything() {
        let bg = Background::new();
        hand(&bg, "task-slow");
        // What `main` does on the expiry path: nothing at all.
        assert!(matches!(bg.resolve("task-slow"), Wait::Pending { .. }));
    }

    /// **An `id` names a handle by its `task_id` or by its node** — the whole agent id or any
    /// unique start of it — and names nothing when this table has no such node.
    #[test]
    fn an_id_names_a_handle_by_its_task_or_its_node() {
        let bg = Background::new();
        hand(&bg, "task-a");
        hand(&bg, "task-b");
        assert_eq!(bg.task_of("task-a"), Ok(Some("task-a".into())));
        assert_eq!(bg.task_of("node-for-task-b"), Ok(Some("task-b".into())));
        assert_eq!(bg.task_of("node-for-task-a"), Ok(Some("task-a".into())));
        assert_eq!(bg.task_of("grandchild"), Ok(None));
        let both = bg.task_of("node-for").expect_err("two nodes start with it");
        assert!(both.contains("node-for-task-a") && both.contains("node-for-task-b"));
    }
}
