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
//! methods do not include a lookup from a task id to a node — [`marion_proto::Method::ALL`] is
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
    /// What the `wait` that collected this child actually got, once one has.
    ///
    /// Recorded rather than recomputed because the answer to a *second* `wait` depends on it: a
    /// child that produced a `TaskContract` has a file on disk to point the caller at, and one that
    /// produced an error has nothing at all. Saying "it is on disk" about the second is the
    /// false-receipt shape this codebase keeps deleting.
    collected: Option<Collected>,
}

/// What an earlier `wait` walked away with, remembered only so a later one can be told the truth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Collected {
    /// A `TaskContract`. The supervisor persisted it before the node's closing bookend, so it
    /// really is on disk under that child's agent directory and a caller that lost it can go and
    /// read it.
    Contract,
    /// A refusal, an abort, a contract marion could not read. There is no file and nothing to
    /// re-read; the caller was told everything there is.
    NoContract,
}

/// The handles one bridge process is holding.
#[derive(Default)]
pub struct Background {
    handed: Mutex<Vec<Handed>>,
}

/// What a backgrounded `spawn` hands back — enough to `wait` on, and nothing that requires a
/// monitor to interpret (§7.6's worked example: *"nothing is ever handed back that requires a
/// monitor to interpret"*).
pub struct Started {
    pub task_id: TaskId,
    pub agent_type: String,
}

/// What a `wait` found in this table. The blocking is [`crate::courier::await_contract`]'s.
pub enum Wait {
    /// This handle names a node, and nobody has collected it yet.
    Pending {
        agent_id: AgentId,
        agent_type: String,
        bound: Duration,
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
        agent_id: AgentId,
        agent_type: String,
        wait_bound: Duration,
    ) -> Started {
        let started = Started {
            task_id: task_id.clone(),
            agent_type: agent_type.clone(),
        };
        self.lock().push(Handed {
            task_id,
            agent_id,
            agent_type,
            wait_bound,
            collected: None,
        });
        started
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hand(bg: &Background, task: &str) {
        bg.hand_out(
            TaskId(task.into()),
            AgentId(format!("node-for-{task}")),
            "codex-impl".into(),
            Duration::from_secs(30),
        );
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
}
