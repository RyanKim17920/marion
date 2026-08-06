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
//! **That is the shape when the kill lands during the child's run, and it is not the only one.**
//! SIGKILL can land anywhere in `run_spawn`'s timeline, and because that timeline deliberately
//! separates intent from confirmation and confirmation from persistence, it leaves **six**
//! distinguishable journal states — two of them indistinguishable *from each other* on disk, and
//! one of them reading as "fate unknown" about a child marion had necessarily already reaped.
//! §11 item 30 enumerates all six. The paragraph above is the first entry, not the list.
//!
//! **This is a known, recorded hole and not a solved problem**: §11 item 30 states it, what closes
//! it ((B), or a pid-carrying kill path the bridge can run inside the measured ~450 ms grace), and
//! what would change the measurement. Nothing here recovers any of the six states: a restarted
//! bridge has an empty table, and every production `Spawned` carries `pid: None`, so there is
//! nothing to wait on, signal or reap a survivor with. The hold below is kept because it is
//! correct for a client
//! that *does* close stdin — `marion run`, the tests, a future TUI — and because a hold that is
//! right for the clients marion controls is worth having even when it is unreachable from the one
//! it does not.
//!
//! # What this table used to be authoritative about, and why it no longer is
//!
//! This section used to argue that [`Background::live_children`] was a *sound* input to §6.1 step
//! 2's concurrency gate, on two grounds: every child of a node is spawned through that node's own
//! bridge instance, so the table is that node's complete child set **by construction**; and a
//! journal read would be wrong in the dangerous direction anyway, since `Spawned` was journaled
//! only once marion had observed a process — which, `spawn` being synchronous, meant after the
//! child had *finished*. A child between "thread started" and "process observed" was invisible to
//! the journal, and a gate that cannot see a child it is supposed to count is the S7 lesson
//! restated.
//!
//! **§11 item 28 step 1 inverted the second half, and the first half was always narrower than it
//! read.** `run_spawn` now journals `SpawnIntent` before every side effect and `Spawned` between
//! `command.spawn()` and the child's first byte of stdin, so a child is in the journal from the
//! first instant it exists at all — earlier than it is in this table, not later. The blind window
//! the argument turned on is gone, and it has moved to the other side: **a journal read is now
//! exact where this table is merely local.** This table is per bridge *process*; it is empty after
//! a restart, it cannot see a sibling started through another bridge, and §5.4 permits `wait` on
//! descendants it has never heard of ([`Wait::Unknown`] says so).
//!
//! So `handler::RegistryHandle::live_children_of` counts the caller's non-terminal children out of
//! the registry, and that is the count the supervisor's own `agent/spawn` gates on. **This table
//! stays and stays correct for what it is**: the set of children *this bridge process* started and
//! has not yet handed back, which is what a `wait` resolves against and what `join_all` holds the
//! process open for. It remains the gate's input on the bridge path only because that path has no
//! registry to read — the bridge is a short-lived process the harness started, not a follower of
//! the journal — and §11 item 28 step 5 is what removes the last caller by making the bridge dial
//! the supervisor instead of spawning.
//!
//! # Why a channel and a deadline, rather than a bare `JoinHandle`
//!
//! `wait` must return the child's `TaskContract`, which is `run_spawn`'s return value, and this was
//! a `JoinHandle::join` — chosen because joining is the one mechanism that cannot lose the value.
//! The argument for it ended *"joining is already bounded: the child's own `timeout_secs` bounds
//! `run_spawn`, so no `wait` needs a second clock"*, **and that was false**.
//!
//! `timeout_secs` bounds one thing: the harness invocation, inside `run_bounded`. Everything else
//! `run_spawn` does is outside it — `git worktree add`, writing the agent and config directories,
//! `compile`, `changed_paths` and `diff_text`, persisting the contract, `git worktree remove`, and
//! the `program --version` probe. A `git` command blocked on a repository lock, or a harness that
//! answered its run and then hung on `--version`, left `run_spawn` unable to return; the join had
//! no deadline of its own, so `wait` blocked forever.
//!
//! **And a blocked `wait` is not one stuck caller — it is the whole bridge.** `main::run_bridge` is
//! a single-threaded read/dispatch loop and `handle_tool_call` runs inline in it, so a `wait` that
//! does not return means no subsequent frame is ever *read*: not a sibling's `spawn`, not another
//! `wait`, not a `report`. One child's hang takes every other child's caller with it.
//!
//! So the outcome travels over an `mpsc` channel and `wait` uses `recv_timeout`. The channel gives
//! back exactly what the join gave — a panicking thread drops its sender, which arrives as
//! `Disconnected` and is reported as [`crate::spawn::SpawnError::Panicked`] — and adds the one
//! thing the join could not have: an expiry. The `JoinHandle` is kept, for [`Background::join_all`]
//! alone, because §5.7's exit rule needs the *thread* joined and not merely its answer read.
//!
//! The deadline is the child's own wall clock plus [`WAIT_GRACE`], so it is still derived from the
//! node's contract rather than invented beside it — the original concern, that a `wait` and a
//! contract could disagree about whether a run had ended, is answered by making the grace additive
//! and by [`Wait::StillRunning`] refusing to claim the run *did* end.

use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use marion_core::contract::{TaskContract, TaskId};

use crate::run::{self, Caller, Env, SpawnRequest};
use crate::spawn::SpawnError;

/// **How much longer than the child's own wall clock a `wait` will block**, before answering that
/// the child is still held rather than continuing to block.
///
/// The child's `timeout_secs` bounds only its harness invocation (see the module docs), so a
/// `wait` bounded by that alone would expire on a healthy child that happened to be inside
/// `git worktree remove`. This is the allowance for everything `run_spawn` does around the run:
/// two `git worktree` commands that serialize against every sibling's, the config writes, a
/// bounded `--version` probe, the diff, and the contract write.
///
/// **Deliberately generous, and deliberately finite.** Generous because expiring early is the
/// expensive mistake — it hands the caller a non-answer about a child that was about to finish.
/// Finite because the alternative is what this replaced: a bridge that reads no further frame from
/// anyone, ever, because one child's `git` is blocked on a lock.
///
/// It is not a kill and not a timeout on the *child*: the child keeps running under its own clock,
/// its row keeps its concurrency slot, and its handle stays valid. This bounds only how long marion
/// will hold a caller's turn hostage to it.
const WAIT_GRACE: Duration = Duration::from_secs(120);

/// One backgrounded child: what the caller was told about it, and how to collect it.
struct Child {
    /// What the caller holds. `wait` addresses a child by this.
    task_id: TaskId,
    /// The resolved agent type name, so a refusal or a `wait` can name what it is talking about
    /// without the caller having kept the request.
    agent_type: String,
    /// **How long a `wait` on this child may block**: the child's own effective wall clock plus
    /// [`WAIT_GRACE`]. Stored per child rather than derived at `wait` time because `wait` carries
    /// no request and must not invent a bound of its own — this is the node's own clock, kept.
    wait_bound: Duration,
    /// The child's outcome, once its thread produces one. `None` from the moment a `wait` takes it.
    ///
    /// Taken and — if that `wait` expires — put back, so an expired `wait` leaves the handle
    /// exactly as collectable as it found it.
    outcome: Option<Receiver<Result<TaskContract, SpawnError>>>,
    /// What the `wait` that collected this child actually got. `Some` exactly when [`Self::outcome`]
    /// has been taken *and* that `wait` completed.
    ///
    /// Recorded rather than recomputed because the answer to a *second* `wait` depends on it: a
    /// child that produced a `TaskContract` has a file on disk to point the caller at, and one that
    /// produced a `SpawnError` has nothing at all. Saying "it is on disk" about the second is the
    /// false-receipt shape this codebase keeps deleting.
    collected: Option<Collected>,
    /// Kept for [`Background::join_all`] only. `wait` reads the channel instead — see the module
    /// docs for why the join could not be the thing with the deadline.
    join: Option<JoinHandle<()>>,
}

impl Child {
    /// Whether this row still occupies a §3.1 concurrency slot — see [`Background::live_children`]
    /// for why "collected" and "present in the table" are different questions.
    ///
    /// A child whose outcome a `wait` is currently blocked on is live: nothing has been delivered
    /// and the thread is still running.
    fn is_live(&self) -> bool {
        self.collected.is_none()
    }
}

/// What an earlier `wait` walked away with, remembered only so a later one can be told the truth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Collected {
    /// A `TaskContract`. `run_spawn` persisted it before returning it, so it really is on disk
    /// under the child's agent directory and a caller that lost it can go and read it.
    Contract,
    /// A `SpawnError` — a refused spawn, a harness that would not compile, a panicked thread. There
    /// is no contract, no file, and nothing to re-read; the caller was told everything there is.
    NoContract,
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

/// The four ways a `wait` can end, kept as a type so the bridge answers each in its own sentence
/// rather than flattening any of them into "error".
pub enum Wait {
    /// The child reached a terminal state. Its contract is the same value a synchronous `spawn`
    /// would have returned — the same `run_spawn` produced both.
    Finished(Box<Result<TaskContract, SpawnError>>),
    /// **This bridge process's table has no row with that id**, which is narrower than "you may not
    /// wait on that".
    ///
    /// §5.4 permits `wait` against *descendants*, and a descendant is not the same set as a child:
    /// an ancestor waiting on a **grandchild**, or a permitted peer, is inside §5.4 and outside this
    /// table, because the grandchild was started through its own parent's bridge instance. Those
    /// land here, and the answer they get is honest about which lookup happened rather than
    /// implying marion searched the tree. Closing that gap needs a cross-process registry —
    /// `Method::AgentSpawn` over the socket to the detached `serve`, option (B) in the module docs.
    ///
    /// It is also **not restart-durable**. The table is per bridge process and holds no journal, so
    /// a bridge that died and was restarted answers `Unknown` to a handle it really did issue, and
    /// really has lost. The refusal must not claim otherwise.
    Unknown,
    /// A `wait` on a child that an earlier `wait` already collected, and **what that earlier `wait`
    /// got**.
    ///
    /// Distinguished from [`Wait::Unknown`] on purpose: "you already have this" and "no such child"
    /// are different mistakes with different fixes, and answering both with the same sentence is how
    /// a caller learns to retry the one that will never succeed. The [`Collected`] payload exists
    /// because the follow-up advice differs too — see its variants.
    AlreadyCollected(Collected),
    /// **The child is past the bound marion will block a caller for, and is still running.**
    ///
    /// Not a failure of the child, not a timeout on the child, and emphatically not a terminal
    /// state: the child keeps its own wall clock, keeps its concurrency slot, and keeps a valid
    /// handle. What expired is only marion's willingness to hold this caller's turn — and, because
    /// the bridge dispatches frames on one thread, every other caller's turn behind it.
    StillRunning {
        /// So the answer can name what it is talking about; a `wait` frame carries no agent type.
        agent_type: String,
        /// How long marion blocked before saying so, so the caller can judge whether to wait again.
        waited: Duration,
    },
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
    /// Over-counting is also the safe direction: this table cannot tell "finished" from "running"
    /// without joining, so it counts the case it cannot rule out.
    ///
    /// **A *collected* child is not counted, and getting that wrong made the bound a lifetime
    /// quota.** This was the vector's length, and `wait` never removed an entry — only the
    /// process's own exit did, in `join_all`. So a caller could start `max_concurrent_children`
    /// children, watch every one of them finish, successfully collect every contract, and then be
    /// refused for the rest of the bridge's life: zero running processes, zero unjoined threads,
    /// and §3.1's *concurrency* bound silently reinterpreted as "the maximum number of background
    /// spawns this bridge may ever serve". A collected child holds nothing — its thread is joined,
    /// its worktree is removed by `run_spawn`'s own cleanup, and its contract is in the caller's
    /// hands — so there is nothing left for it to be concurrent with.
    ///
    /// The entry itself stays, because it is what tells a second `wait` [`Wait::AlreadyCollected`]
    /// from [`Wait::Unknown`]. Membership and liveness are different questions about the same row,
    /// and this counts the second one.
    pub fn live_children(&self) -> u32 {
        // Saturating rather than `as`: a count that wrapped to 0 would silently *open* the gate,
        // which is the direction that costs something.
        u32::try_from(self.lock().iter().filter(|c| c.is_live()).count()).unwrap_or(u32::MAX)
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
        // The node's own clock, resolved through the same function `run_spawn` uses, so the bound a
        // `wait` respects is derived from the child's contract rather than guessed beside it.
        // Saturating, because `effective_timeout` already caps the request but the sum with the
        // grace must still be a duration that exists.
        let wait_bound = run::effective_timeout(req.timeout_secs).saturating_add(WAIT_GRACE);
        let id = task_id.clone();
        let (tx, outcome) = channel();
        let join = std::thread::spawn(move || {
            // The send is the *only* thing this thread has to do with its answer, and its failure
            // is ignored on purpose: a receiver dropped before the child finished means the whole
            // `Background` is gone, which happens only as the process leaves. There is nobody left
            // to tell, and panicking here would turn "the bridge is shutting down" into "a child
            // crashed".
            let _ = tx.send(run::run_spawn(&env, &req, &id, &caller));
        });
        self.lock().push(Child {
            task_id,
            agent_type,
            wait_bound,
            outcome: Some(outcome),
            collected: None,
            join: Some(join),
        });
        started
    }

    /// Collect one child, blocking until it reaches a terminal state **or until the bound this
    /// bridge is willing to block for expires**.
    ///
    /// The bound is the child's own effective wall clock plus [`WAIT_GRACE`], stored on the row at
    /// `start`. The module docs argue for its existence; the short version is that `timeout_secs`
    /// bounds only the harness invocation, so an unbounded `wait` could block on `git`, on a
    /// `--version` probe, or on a contract write — and blocking here blocks the bridge's entire
    /// read loop, not just this caller.
    ///
    /// A thread that **panicked** drops its sender, which arrives as `Disconnected` and is reported
    /// as a `SpawnError::Panicked` rather than re-raised into the bridge's own loop. Re-raising
    /// would take down the bridge, and with it every *other* live child of this caller, over one
    /// child's fault: a node's failure must not be contagious to its siblings.
    pub fn wait(&self, task_id: &str) -> Wait {
        let (rx, agent_type, bound) = {
            let mut children = self.lock();
            let Some(child) = children.iter_mut().find(|c| c.task_id.0 == task_id) else {
                return Wait::Unknown;
            };
            let agent_type = child.agent_type.clone();
            let bound = child.wait_bound;
            match child.outcome.take() {
                // `collected` is `None` here only in the window where another thread holds this
                // child's receiver — unreachable through the bridge, whose dispatch is single
                // threaded, and answered conservatively if it ever becomes reachable: `NoContract`
                // promises the caller no file, which is the direction that cannot mislead.
                None => {
                    return Wait::AlreadyCollected(
                        child.collected.unwrap_or(Collected::NoContract),
                    );
                }
                Some(rx) => (rx, agent_type, bound),
            }
        };
        // The lock is **released before the wait**, which is the whole reason for the block above.
        //
        // What that does and does not buy is worth being exact about, because the comment here used
        // to claim more than it delivered. It does *not* let a sibling's `spawn` or `wait` proceed:
        // `main::run_bridge` reads and dispatches frames on one thread and calls `handle_tool_call`
        // inline, so while this call is outstanding no later frame is even read. What it buys is
        // that `join_all` and `live_children` — which run from this same thread's shutdown path and
        // from a `spawn` that has already returned — are never blocked by an in-flight `wait`, and
        // that the table stays usable the day dispatch stops being serial. The bound below, not
        // this release, is what keeps a stuck child from stopping the bridge.
        let outcome = match rx.recv_timeout(bound) {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Disconnected) => Err(SpawnError::Panicked(agent_type)),
            Err(RecvTimeoutError::Timeout) => {
                // The handle is left exactly as collectable as it was found: the receiver goes
                // back, `collected` is untouched, and the row keeps its concurrency slot because
                // the child really is still running.
                let mut children = self.lock();
                if let Some(child) = children.iter_mut().find(|c| c.task_id.0 == task_id) {
                    child.outcome = Some(rx);
                }
                return Wait::StillRunning {
                    agent_type,
                    waited: bound,
                };
            }
        };
        // Remember *what* was collected, not merely that something was: a second `wait` may point
        // the caller at a persisted contract only when there is one. See [`Collected`].
        let mark = if outcome.is_ok() {
            Collected::Contract
        } else {
            Collected::NoContract
        };
        let mut children = self.lock();
        if let Some(child) = children.iter_mut().find(|c| c.task_id.0 == task_id) {
            child.collected = Some(mark);
        }
        drop(children);
        Wait::Finished(Box::new(outcome))
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

    /// A stub child wired **exactly the way [`Background::start`] wires a real one** — same
    /// channel, same `JoinHandle`, same row — differing only in that its body is a closure rather
    /// than `run_spawn`.
    ///
    /// These tests are about the table's bookkeeping, and a real spawn would need a harness binary.
    /// Going through this one constructor rather than building `Child` inline at each site is what
    /// keeps them honest: a field added to `Child` and forgotten here would fail to compile in one
    /// place instead of being silently defaulted in five.
    fn push_stub(
        bg: &Background,
        task_id: &str,
        wait_bound: Duration,
        body: impl FnOnce() -> Result<TaskContract, SpawnError> + Send + 'static,
    ) {
        let (tx, outcome) = channel();
        let join = std::thread::spawn(move || {
            let _ = tx.send(body());
        });
        bg.lock().push(Child {
            task_id: TaskId(task_id.into()),
            agent_type: "codex-impl".into(),
            wait_bound,
            outcome: Some(outcome),
            collected: None,
            join: Some(join),
        });
    }

    /// A bound no passing assertion in this module ever reaches. Every stub above resolves
    /// immediately; reaching this would mean `wait` is not reading the channel at all.
    const AMPLE: Duration = Duration::from_secs(30);

    /// The test's own way out of a `wait` that never returns, so a lost deadline is a **failure**
    /// naming itself rather than a hung suite. Never reached on a passing run.
    const ESCAPE: Duration = Duration::from_secs(10);

    fn refused() -> Result<TaskContract, SpawnError> {
        Err(SpawnError::UnknownAgentType("stub".into()))
    }

    /// A `wait` for a child nobody started is `Unknown`, not a panic and not a hang.
    ///
    /// The mistake this guards is answering an unknown id by blocking on nothing. What the answer
    /// may *not* do is claim the lookup was exhaustive — see [`Wait::Unknown`], which is scoped to
    /// this bridge process's own direct children and is narrower than §5.4's descendant rule.
    #[test]
    fn waiting_on_an_id_this_bridge_never_started_is_refused_rather_than_awaited() {
        let bg = Background::new();
        assert!(matches!(bg.wait("task-nobody-started"), Wait::Unknown));
        assert_eq!(bg.live_children(), 0);
    }

    /// Collecting twice says so, instead of repeating the first answer or blocking forever.
    ///
    /// The second `wait` has no outcome left to read; the choice is what to say about it.
    /// `AlreadyCollected` and `Unknown` are kept apart because the fixes differ — one caller
    /// already has the answer, the other is asking about a node this process cannot see.
    #[test]
    fn a_second_wait_on_a_collected_child_is_distinguishable_from_an_unknown_one() {
        let bg = Background::new();
        push_stub(&bg, "task-collect-twice", AMPLE, refused);
        assert_eq!(bg.live_children(), 1);
        assert!(matches!(bg.wait("task-collect-twice"), Wait::Finished(_)));
        assert!(matches!(
            bg.wait("task-collect-twice"),
            Wait::AlreadyCollected(_)
        ));
        assert!(matches!(bg.wait("task-nope"), Wait::Unknown));
    }

    /// **A second `wait` is told what the first one actually got**, so the bridge never promises a
    /// file that was never written.
    ///
    /// `wait_already_collected` used to end *"the contract is on disk under that child's agent
    /// directory"* unconditionally. For a child that failed inside its thread there is no contract
    /// and no file, so that sentence sent the caller to read a path that does not exist. This pins
    /// the distinction at the layer that knows it: only the `wait` that collected the outcome ever
    /// sees whether it was a `TaskContract`.
    #[test]
    fn a_child_that_produced_no_contract_is_remembered_as_having_produced_none() {
        let bg = Background::new();
        push_stub(&bg, "task-no-contract", AMPLE, refused);
        assert!(matches!(bg.wait("task-no-contract"), Wait::Finished(_)));
        assert!(
            matches!(
                bg.wait("task-no-contract"),
                Wait::AlreadyCollected(Collected::NoContract)
            ),
            "a spawn that failed has nothing on disk, and the second wait must be able to say so"
        );
    }

    /// **A child holds its concurrency slot until it is collected, and not one moment longer.**
    ///
    /// Both halves matter and they pull in opposite directions, which is why they are one test:
    ///
    /// * an **uncollected** child counts even though this table cannot tell whether its process is
    ///   still running. That is [`Background::live_children`]'s coarse definition, and the coarse
    ///   direction is the safe one — the alternative is a caller that starts children without
    ///   bound so long as it never collects any.
    /// * a **collected** child does not count. Its thread is finished, its worktree is gone and its
    ///   outcome is in the caller's hands; there is nothing for it to be concurrent with. This
    ///   half is the regression: while `wait` merely took the join handle and left the row's
    ///   length behind, §3.1's *concurrency* bound was a **lifetime quota** — four children
    ///   started, finished and collected refused the fifth for the whole life of the bridge.
    ///
    /// The row itself survives collection, which the last assertion pins: it is the only thing that
    /// distinguishes a second `wait` from a `wait` on an id nobody ever started.
    #[test]
    fn a_child_holds_its_concurrency_slot_until_it_is_collected_and_no_longer() {
        let bg = Background::new();
        for i in 0..3 {
            push_stub(&bg, &format!("task-slot-{i}"), AMPLE, refused);
        }
        assert_eq!(bg.live_children(), 3);
        assert!(matches!(bg.wait("task-slot-1"), Wait::Finished(_)));
        assert_eq!(
            bg.live_children(),
            2,
            "a collected child holds nothing: its thread is done and its outcome delivered"
        );
        assert!(
            matches!(bg.wait("task-slot-1"), Wait::AlreadyCollected(_)),
            "and it is still *known*, which is what keeps this answer apart from Unknown"
        );
        bg.join_all();
        assert_eq!(bg.live_children(), 0);
    }

    /// **A `wait` on a child that never finishes returns anyway, and says the child is still
    /// running.**
    ///
    /// This is the whole of B3 at the layer that owns it. `wait` was an unconditional
    /// `JoinHandle::join`, bounded — the docs claimed — by the child's own `timeout_secs`. That
    /// bound covers the harness invocation and nothing else: `git`, the config writes, the
    /// `--version` probe and the contract write are all outside it, so a `run_spawn` that hung in
    /// any of them blocked the join forever. And because `main::run_bridge` dispatches on one
    /// thread, forever meant the bridge read no further frame from anyone — not a sibling's
    /// `spawn`, not another `wait`.
    ///
    /// The stub hangs until the test releases it, so this is an *ordering* claim and not a timing
    /// one: the `wait` returns while the child is provably still running, and the child is released
    /// only afterwards. The child's own bound is deliberately tiny because nothing here is
    /// measuring patience.
    ///
    /// **The `wait` runs on its own thread behind [`ESCAPE`].** Without that, a regression to an
    /// unbounded `wait` does not *fail* this test — it hangs it, and a hung test is a worse signal
    /// than a failing one: it reports nothing, blocks the suite, and looks like an infrastructure
    /// problem. This is `run.rs`'s own idiom for the same hazard, and it is the difference between
    /// "the bound is gone" and "the machine is slow".
    ///
    /// It also asserts the two things `StillRunning` must not quietly break: the handle stays
    /// collectable, and the row keeps its concurrency slot — a child marion is still holding must
    /// not free a slot just because a caller stopped waiting for it.
    #[test]
    fn a_wait_on_a_child_that_outlasts_the_bound_returns_and_says_it_is_still_running() {
        let bg = std::sync::Arc::new(Background::new());
        let (release, held) = channel::<()>();
        push_stub(&bg, "task-hangs", Duration::from_millis(50), move || {
            // Stands in for a `git` command blocked on a repository lock, or a harness hung on
            // `--version`: work inside `run_spawn` that the child's own wall clock never bounds.
            let _ = held.recv();
            refused()
        });

        let waiter = std::sync::Arc::clone(&bg);
        let (tx, answered) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(waiter.wait("task-hangs"));
        });
        let answer = answered.recv_timeout(ESCAPE).unwrap_or_else(|_| {
            panic!(
                "`wait` did not return within {ESCAPE:?} for a child that has not finished. An \
                 unbounded wait is the defect: it blocks marion's whole single-threaded dispatch \
                 loop, so every later frame from every caller goes unread."
            )
        });
        let Wait::StillRunning { agent_type, .. } = answer else {
            panic!("a child that has not finished cannot have been collected");
        };
        assert_eq!(
            agent_type, "codex-impl",
            "the answer names the child it is about; a wait frame carries no agent type"
        );
        assert_eq!(
            bg.live_children(),
            1,
            "the child is still running, so it still occupies its slot — a caller must not be able \
             to free slots by giving up on waits"
        );

        // Only now may it finish, and the same handle still resolves it.
        drop(release);
        assert!(
            matches!(bg.wait("task-hangs"), Wait::Finished(_)),
            "an expired wait leaves the handle exactly as collectable as it found it"
        );
        assert_eq!(bg.live_children(), 0);
    }

    /// A panicking child is a `SpawnError`, not a bridge that dies with it.
    ///
    /// The sibling argument in [`Background::wait`]'s docs, as a test: one child's fault must not
    /// be contagious. The panic reaches `wait` as a dropped sender — `RecvTimeoutError::Disconnected`
    /// — and without that arm it would be an unwind through the bridge's JSON-RPC loop, taking
    /// every other live child's thread with the process.
    #[test]
    fn a_child_thread_that_panics_is_reported_rather_than_propagated() {
        let bg = Background::new();
        push_stub(&bg, "task-panic", AMPLE, || {
            panic!("the child's thread died")
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
