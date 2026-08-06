//! **`spawn { background: true }`** — the bridge's table of children that are running while their
//! caller has the turn back, and the `wait` that resolves one.
//!
//! # Why this module exists at all, and where it deliberately stops
//!
//! §5.4 declares `background` and §11 item 23 recorded it as refused by name (`77557e3`) because
//! dropping it made marion perform a different verb: *"a caller asking for a handle waited out the
//! child's entire synchronous run and received a completed `TaskContract` with `isError: false`."*
//! This module is the refusal being lifted.
//!
//! **The child runs on a thread inside the bridge process** (`marion-supervisor mcp`), which the
//! *harness* started as a stdio MCP server — marion is not its parent. That placement is a
//! deliberate choice between two, and the other one is not built:
//!
//! * **(A) here, in the bridge.** Reachable today, because `run::run_spawn` already owns a child
//!   end to end inside this process and writes that child's `events.jsonl` (§7.3.3 says so in as
//!   many words: the bridge's process *"is the only process that ever has a child's frames"*).
//!   Its limit is that a child's lifetime is bounded by the **bridge's**, which is bounded by the
//!   parent harness's — and **s16 measured exactly how**, which is worse than it was assumed to
//!   be. See "What this costs" below.
//! * **(B) over the socket, in the detached `marion-supervisor serve`**, which §2 and §5.7 say owns
//!   every node's lifecycle. `Method::AgentSpawn` exists in `marion-proto` and `handler.rs` answers
//!   it `Unimplemented`. Only (B) can implement §7.5's *"the descendants outlive the parent"*.
//!
//! **(A) is enough for what backgrounding is needed for and not enough for what it eventually
//! means**, and the distinction is worth stating precisely because it is easy to get backwards.
//! §9's M2 criterion 4(ii) — *"the new client receives events emitted after it attached"*, which
//! §9 calls *"the load-bearing half… the only assertion in §9 that a replay-only implementation
//! fails"* — needs a node emitting events **while no client exists**. The client is `marion run` or
//! a TUI; it is **not** the root. A backgrounded child writing into its `events.jsonl` while the
//! root is still mid-turn and the client has detached satisfies it exactly. What (A) cannot do is
//! outlive the root.
//!
//! # What this costs, measured rather than assumed (s16, 2026-08-06)
//!
//! The obvious mitigation for (A) — *"hold the bridge open at stdin EOF while a child is
//! outstanding"* — was designed before it was measured, and the measurement says it does not
//! apply. Against a real headless `claude` **2.1.222**, four runs agreeing on every discriminating
//! reading (`tests/fixtures/s16/`):
//!
//! * **there is no EOF.** `stdin_eof` never appears in any harness run, although the probe records
//!   one in the no-harness control. The harness sends **SIGINT, then SIGTERM 100 ms later** (100 /
//!   100 / 101 / 100 ms — a fixed timer, not a race), then a third signal the probe could not
//!   catch despite holding handlers on 29 signals: **SIGKILL by elimination**, ~430–475 ms after
//!   the SIGTERM.
//! * **the signals are pid-targeted at the server, not sent to its group.** A run with `claude` as
//!   its own group leader — marion's own `setpgid` arrangement, so `killpg` was fully available —
//!   is identical.
//! * **a grandchild survives untouched**: zero signals, still heartbeating 58 heartbeats later,
//!   `ppid: 1`, alive at the end of the watch.
//!
//! So [`Background::join_all`] **never runs under a real Claude Code harness**, and a bridge that
//! ignored SIGTERM would buy ~450 ms and die anyway. The consequence is the one that matters:
//! **a backgrounded child whose parent harness exits becomes an untracked live process** —
//! reparented to pid 1, its wall clock unenforced because the enforcer was in the bridge, its
//! `SpawnIntent` journaled with no resolution, and its worktree left behind. That is §11 item 18's
//! runaway shape and it is what §9's M2 criterion *"no untracked live process"* forbids.
//!
//! **This is a known, recorded hole and not a solved problem**: §11 item 27 states it, what closes
//! it ((B), or a pid-carrying kill path the bridge can run inside the measured ~450 ms grace), and
//! what would change the measurement. The hold below is kept because it is correct for a client
//! that *does* close stdin — `marion run`, the tests, a future TUI — and because a hold that is
//! right for the clients marion controls is worth having even when it is unreachable from the one
//! it does not.
//!
//! # The one thing this table is authoritative about
//!
//! **Every child of a node is spawned through that node's own bridge instance**, so this table is
//! the complete set of that node's children by construction. That is what makes
//! [`Background::live_children`] a sound input to §6.1 step 2's concurrency gate without a journal
//! read — and a journal read would be wrong in the dangerous direction anyway, since `Spawned` is
//! journaled only once marion has *observed* a process (`run.rs`), so a child between "thread
//! started" and "process observed" is invisible to it. A gate that cannot see a child it is
//! supposed to be counting is the S7 lesson restated: a check which cannot see a survivor is worse
//! than none.
//!
//! # Why a `JoinHandle` rather than a channel or a poll
//!
//! `wait` must return the child's `TaskContract`, which is `run_spawn`'s return value, and joining
//! is the one mechanism that cannot lose it: a channel would need a receiver alive at exactly the
//! right moment, and a poll would need a bound of its own. Joining is already bounded — the child's
//! own `timeout_secs` bounds `run_spawn`, so no `wait` needs a second clock and none is offered.
//! That is deliberate: a bound whose expiry is not the node's own would put a `wait` and its
//! contract into disagreement about whether the run had ended.

use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;

use marion_core::contract::{TaskContract, TaskId};

use crate::run::{self, Caller, Env, SpawnRequest};
use crate::spawn::SpawnError;

/// One backgrounded child: what the caller was told about it, and how to collect it.
struct Child {
    /// What the caller holds. `wait` addresses a child by this.
    task_id: TaskId,
    /// The resolved agent type name, so a refusal or a `wait` can name what it is talking about
    /// without the caller having kept the request.
    agent_type: String,
    /// `None` once collected. Collecting twice is a caller error, not a panic — see [`Wait`].
    join: Option<JoinHandle<Result<TaskContract, SpawnError>>>,
}

/// The bridge's live children.
///
/// One per bridge process, created in `main::run_bridge` and borrowed by every `spawn` and `wait`
/// it serves. Not a `static`: a `static` would be shared by the unit tests in this file, which
/// would make each test's live count depend on which other tests had run.
#[derive(Default)]
pub struct Background {
    children: Mutex<Vec<Child>>,
}

/// What a backgrounded `spawn` hands back — enough to `wait` on, and nothing that requires a
/// monitor to interpret (§7.6's worked example: *"nothing is ever handed back that requires a
/// monitor to interpret"*).
pub struct Started {
    pub task_id: TaskId,
    pub agent_type: String,
}

/// The three ways a `wait` can end, kept as a type so the bridge answers each in its own sentence
/// rather than flattening two of them into "error".
pub enum Wait {
    /// The child reached a terminal state. Its contract is the same value a synchronous `spawn`
    /// would have returned — the same `run_spawn` produced both.
    Finished(Box<Result<TaskContract, SpawnError>>),
    /// No child of this caller carries that id.
    ///
    /// §5.4 permits `wait` against *descendants only*, and this table holds exactly this node's
    /// children, so "unknown here" and "not yours" are the same answer — which is why the refusal
    /// says so rather than implying marion looked further than it did.
    Unknown,
    /// A `wait` on a child that has already been collected by an earlier `wait`.
    ///
    /// Distinguished from [`Wait::Unknown`] on purpose: "you already have this contract" and "no
    /// such child" are different mistakes with different fixes, and answering both with the same
    /// sentence is how a caller learns to retry the one that will never succeed.
    AlreadyCollected,
}

impl Background {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Child>> {
        // Poisoning is recovered from rather than propagated, for `spawn::repo_write_guard`'s
        // reason: a panic in one child's thread must not make every later `spawn` and `wait` in
        // this process fail. The vector's invariant is a vector's — a panic cannot leave it
        // half-updated across a lock boundary.
        self.children.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// **The caller's live, uncollected children** — §6.1 step 2's third argument.
    ///
    /// "Live" is *not yet collected by a `wait`*, which is deliberately coarser than *the process
    /// is still running*: a child that has finished but whose contract nobody has taken is still
    /// occupying a slot, because the alternative is a caller that can start an unbounded number of
    /// children so long as it never collects any of them. §3.1 bounds concurrency to bound the
    /// machine, and an uncollected finished child costs a thread and a worktree until it is taken.
    pub fn live_children(&self) -> u32 {
        // Saturating rather than `as`: a count that wrapped to 0 would silently *open* the gate,
        // which is the direction that costs something.
        u32::try_from(self.lock().len()).unwrap_or(u32::MAX)
    }

    /// Start a child and return **while it runs**.
    ///
    /// Everything the thread needs is owned: `Env`, `SpawnRequest` and `Caller` are cloned rather
    /// than borrowed, which is why `run` derives `Clone` on the first two. The `'static` bound is
    /// the compiler enforcing the actual requirement — this work outlives the JSON-RPC frame that
    /// asked for it.
    ///
    /// **The gate is not evaluated here, and the bridge evaluates it before calling this.** §6.1
    /// step 2 is `marion_core::agent_type::check_spawn_gates`, and `main::handle_tool_call` runs it
    /// on the background path so a refusal arrives in the frame that asked for it rather than as a
    /// handle to a child that was never going to start — see that call site for the argument.
    /// `run_spawn` runs it too, being a public entry point with callers that do not come through
    /// the bridge. Two call sites of one pure function, never two implementations.
    ///
    /// A spawn refused *inside* the thread for some other reason — an unknown agent type, a scope
    /// error, a harness that will not compile — is registered like any other child and its `Err`
    /// reaches the caller through `wait`. That is not a leak: it is the only way the refusal can
    /// be read at all, and a refusal that vanished before anyone could see it is the
    /// accept-and-ignore shape one level up.
    pub fn start(&self, env: Env, req: SpawnRequest, task_id: TaskId, caller: Caller) -> Started {
        let agent_type = req.agent_type.clone();
        let started = Started {
            task_id: task_id.clone(),
            agent_type: agent_type.clone(),
        };
        let id = task_id.clone();
        let join = std::thread::spawn(move || run::run_spawn(&env, &req, &id, &caller));
        self.lock().push(Child {
            task_id,
            agent_type,
            join: Some(join),
        });
        started
    }

    /// Collect one child, blocking until it reaches a terminal state.
    ///
    /// Bounded by the child's own `timeout_secs` and by nothing else — see the module docs.
    ///
    /// A thread that **panicked** is reported as a `SpawnError::Panicked` rather than re-raised
    /// into the bridge's own loop. Re-raising would take down the bridge, and with it every
    /// *other* live child of this caller, over one child's fault: a node's failure must not be
    /// contagious to its siblings.
    pub fn wait(&self, task_id: &str) -> Wait {
        let join = {
            let mut children = self.lock();
            let Some(child) = children.iter_mut().find(|c| c.task_id.0 == task_id) else {
                return Wait::Unknown;
            };
            let agent_type = child.agent_type.clone();
            match child.join.take() {
                None => return Wait::AlreadyCollected,
                Some(j) => (j, agent_type),
            }
        };
        // The lock is **released before the join**, which is the whole reason for the block above:
        // holding it across a join would make one `wait` block every other `spawn` and `wait` this
        // bridge serves, turning a concurrency feature into a serialization one.
        let (handle, agent_type) = join;
        Wait::Finished(Box::new(match handle.join() {
            Ok(outcome) => outcome,
            Err(_) => Err(SpawnError::Panicked(agent_type)),
        }))
    }

    /// **Wait for every uncollected child before letting this process go**, and the reason it is
    /// not optional.
    ///
    /// §5.7's exit rules say a supervisor MUST NOT exit while *"any `spawn` is outstanding"*.
    /// Exiting instead would kill each child's process group mid-run and leave its `SpawnIntent`
    /// journaled with no resolution — a node §7.2 would later mark `Orphaned`, asserting marion
    /// *lost* a process it in fact chose to abandon.
    ///
    /// **This does not run under a real Claude Code harness**, and that is measured, not assumed:
    /// s16 found no EOF at all — SIGINT, SIGTERM 100 ms later, SIGKILL ~450 ms after that, all
    /// pid-targeted at the server. See the module docs. It is correct for a client that closes
    /// stdin and leaves, which is every client marion itself writes, and it is not a claim about
    /// the one marion does not.
    pub fn join_all(&self) {
        loop {
            let Some(mut child) = self.lock().pop() else {
                return;
            };
            // Same reason as `wait`: never join under the lock.
            if let Some(j) = child.join.take() {
                let _ = j.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `wait` for a child nobody started is `Unknown`, not a panic and not a hang.
    ///
    /// §5.4 permits `wait` against descendants only, and this table *is* the descendant set, so the
    /// two questions collapse into one lookup. The mistake this guards is answering an unknown id
    /// by blocking on nothing.
    #[test]
    fn waiting_on_an_id_this_bridge_never_started_is_refused_rather_than_awaited() {
        let bg = Background::new();
        assert!(matches!(bg.wait("task-nobody-started"), Wait::Unknown));
        assert_eq!(bg.live_children(), 0);
    }

    /// Collecting twice says so, instead of repeating the first answer or blocking forever.
    ///
    /// The second `wait` cannot re-join a consumed `JoinHandle`; the choice is what to say about
    /// it. `AlreadyCollected` and `Unknown` are kept apart because the fixes differ — one caller
    /// already has the contract, the other is asking about a node that does not exist.
    #[test]
    fn a_second_wait_on_a_collected_child_is_distinguishable_from_an_unknown_one() {
        let bg = Background::new();
        // A child whose "run" is a thread that returns immediately: this test is about the table's
        // bookkeeping, not about a real spawn, and a real one would need a harness binary.
        bg.lock().push(Child {
            task_id: TaskId("task-collect-twice".into()),
            agent_type: "codex-impl".into(),
            join: Some(std::thread::spawn(|| {
                Err(SpawnError::UnknownAgentType("stub".into()))
            })),
        });
        assert_eq!(bg.live_children(), 1);
        assert!(matches!(bg.wait("task-collect-twice"), Wait::Finished(_)));
        assert!(matches!(
            bg.wait("task-collect-twice"),
            Wait::AlreadyCollected
        ));
        assert!(matches!(bg.wait("task-nope"), Wait::Unknown));
    }

    /// A collected child still holds its slot, and `join_all` is what empties the table.
    ///
    /// This pins the coarse definition of "live" that [`Background::live_children`] argues for: an
    /// uncollected finished child occupies a concurrency slot. Were `wait` to remove the entry, a
    /// caller could start `max_concurrent_children` children, collect them, and start that many
    /// again without bound — which is the gate not binding, arrived at from the other side.
    #[test]
    fn a_child_holds_its_concurrency_slot_until_the_table_is_emptied() {
        let bg = Background::new();
        for i in 0..3 {
            bg.lock().push(Child {
                task_id: TaskId(format!("task-slot-{i}")),
                agent_type: "codex-impl".into(),
                join: Some(std::thread::spawn(|| {
                    Err(SpawnError::UnknownAgentType("stub".into()))
                })),
            });
        }
        assert_eq!(bg.live_children(), 3);
        assert!(matches!(bg.wait("task-slot-1"), Wait::Finished(_)));
        assert_eq!(
            bg.live_children(),
            3,
            "collecting a contract does not free the slot; only leaving the process does"
        );
        bg.join_all();
        assert_eq!(bg.live_children(), 0);
    }

    /// A panicking child is a `SpawnError`, not a bridge that dies with it.
    ///
    /// The sibling argument in [`Background::wait`]'s docs, as a test: one child's fault must not
    /// be contagious. Without the `Err(_)` arm this is an unwind through the bridge's JSON-RPC
    /// loop, taking every other live child's thread with the process.
    #[test]
    fn a_child_thread_that_panics_is_reported_rather_than_propagated() {
        let bg = Background::new();
        bg.lock().push(Child {
            task_id: TaskId("task-panic".into()),
            agent_type: "codex-impl".into(),
            join: Some(std::thread::spawn(|| panic!("the child's thread died"))),
        });
        let Wait::Finished(outcome) = bg.wait("task-panic") else {
            panic!("a started child is collectable");
        };
        let Err(e) = *outcome else {
            panic!("a panicking thread cannot have produced a contract");
        };
        assert!(
            e.to_string().contains("codex-impl") && e.to_string().contains("panicked"),
            "the refusal names the child and what happened to it: {e}"
        );
    }

    /// `join_all` on an empty table returns, rather than blocking on nothing.
    ///
    /// The loop pops under a lock it then releases; a `while let` over an iterator would have held
    /// it across the join. This is the degenerate case that would catch a `loop {}` written without
    /// its exit.
    #[test]
    fn joining_no_children_terminates() {
        Background::new().join_all();
    }
}
