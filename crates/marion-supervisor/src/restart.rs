//! **§7.2's supervisor-restart marking** — the judgement applied to a replayed tree.
//!
//! `marion_core::registry::replay` is deliberately total, infallible, and never produces
//! `Orphaned`; its module doc says why, and `registry.rs`'s says this is the layer that decides.
//! This is that layer. It reads a booted tree and answers, per node, *what does the journal show
//! marion decided about this node's fate?* — and nothing else.
//!
//! # It does not probe for processes, and that is the design rather than a shortcut
//!
//! §7.2 defines `Orphaned` as *"marion has **no record of deciding this node's fate**: no reap
//! intent, no observed exit"*, and is explicit that the state covers two physically different
//! worlds — *"the process may be gone **or still running with marion no longer attached**"* —
//! because *"both are 'marion does not know', both require the same user resolution"*. The marking
//! is therefore a judgement about the **record**, not a measurement of the world, and it needs no
//! liveness check to be sound. [`marion_core::registry::ReplayedNode::is_unresolved`] already
//! computes its precondition.
//!
//! The one half of §7.2 that genuinely needs to look at a process is the **unconfirmed reap
//! intent**: *"resolved by checking for the process … gone means marion writes the confirmation;
//! still alive means the supervisor died before the kill landed, so marion kills it now."* That
//! check cannot be made today and this module refuses to fake it — see
//! [`Marking::ReapIntentUnresolved`].
//!
//! # There is a pid now, and it still does not license the probe
//!
//! **The premise this section used to rest on is gone.** It said every production writer of
//! `Spawned` recorded `pid: None`, and that this was structural: `run.rs` journalled the record
//! *after* the child had been run to completion and reaped, so any pid captured there would have
//! named a process marion had already observed dead. §11 item 28's step 1 moved that record to the
//! instant the process exists (`run.rs`'s `announce_started`, called between `command.spawn()` and
//! the first byte written to the child's stdin), and it carries a real pid. A child's `Spawned`
//! now names a process that was running when the record was written — **and so does a root's**.
//! This paragraph used to say the root was the exception, on the grounds that `marion run` owned
//! the whole turn in one blocking call; item 28's step 6 ended that, and `root.rs`'s `launch_inner`
//! has had an `on_started` hook writing a real pid ever since. The `pid: None` arm is now only a
//! launch that never reached a process.
//!
//! **And [`Marking::ReapIntentUnresolved`] still refuses §7.2's probe branch, unchanged.** The
//! reason survives the premise it used to be attached to, because it was never really about
//! whether a number was on disk:
//!
//! > **The pid closes the signal-target problem, not the identity problem.**
//!
//! `kill_tree` needs to know *where to send a signal now*, and a pid recorded moments ago by a
//! supervisor that is still running answers that. §7.2's probe needs to know *whether the process
//! this journal is about is the one wearing that pid today*, and a bare pid cannot answer it:
//! a bare pid cannot answer it, because on a restart — the exact moment this module runs, and by
//! construction after a crash of unknown duration — nothing in the number itself distinguishes a
//! surviving node from an unrelated process handed a recycled pid. Probing on a pid alone would
//! turn that ambiguity into a `ReapConfirmed` (*"the process was observed dead"*) or into marion
//! killing a stranger. Both are fabrications; the refusal is not.
//!
//! **The substrate that was missing now exists, and the refusal below is therefore a deferral
//! rather than an impossibility.** `Spawned` carries a [`marion_core::node::StartId`] beside its
//! pid, and [`crate::procid`] compares it, so *"is the process this journal is about still
//! running"* has a definite answer on a platform where the identity can be read. What has **not**
//! been designed is the other half of §7.2's sentence — *"still alive means the supervisor died
//! before the kill landed, so marion kills it now"* — which is marion signalling a process on the
//! strength of a replayed record, at start-up, with no client watching. That wants its own
//! argument, so this module still reports the intent unresolved and `procid` reports the process
//! honestly, which together say strictly more than a fabricated confirmation would.
//!
//! What the pid *does* change here is what a `SpawnIntent` with nothing after it means. It used to
//! cover both "no process was ever started" and "a process is running and marion cannot name it".
//! It now means the first, full stop — §11 item 30's shapes 1 and 2 stop being indistinguishable —
//! and the [`Marking::Orphaned`] arm below is correspondingly narrower and more truthful. The
//! identity half is closed by [`crate::procid`], whose start-time comparison follows
//! `spikes/s15/procid.py`; this module still does not reach for it, for the reason above.
//!
//! # Derived, not journaled
//!
//! No `RecordKind` is added and nothing is appended. Three reasons:
//!
//! * The marking is a **total function of the journal prefix**, so every restart re-derives it
//!   identically. Journaling buys no durability.
//! * Every existing record is an observation or an intent. This is a judgement, and §7.2 hangs it
//!   on restart rather than on an event.
//! * Decisively: a node this pass marks `Orphaned` may still be driven by a **different live
//!   process** — §11 item 28's bridge, §11 item 30's backgrounded child — which can still write a
//!   truthful `Exited` afterwards. A derived marking is superseded the moment that record lands. A
//!   journaled one would contradict it forever.

use marion_core::contract::AgentId;
use marion_core::node::ReapState;
use marion_core::registry::{Replay, ReplayedNode};

/// What restart concluded about one node's fate, from the record alone.
///
/// Three variants and not one, because the three are the classes §7.2 keeps apart and collapsing
/// any two of them loses the distinction that makes the marking honest: a node marion lost, a node
/// marion decided about and could not finish deciding, and a node marion abandoned over a process
/// that may have existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marking {
    /// §7.2's `Live` → `Orphaned`. Recorded live, no exit observed, no reap decided, no abort.
    Orphaned,
    /// A reap intent with no confirmation, **left exactly as the journal has it**.
    ///
    /// §7.2 resolves this by checking for the process and lands on `ReapedIdle` either way. Marion
    /// cannot make that check — there is no pid (see the module doc) — and
    /// `RecordKind::ReapConfirmed` means *"the process was observed dead"*. Writing one here would
    /// fabricate the observation the record exists to carry, so nothing is written and the node
    /// stays `Live` with its intent outstanding.
    ///
    /// **It is never `Orphaned`.** §7.2: *"marion knows what it intended for this process."*
    /// [`ReplayedNode::is_unresolved`] enforces that structurally by requiring
    /// `reap_intent.is_none()`, so the two arms cannot both fire for one node.
    ReapIntentUnresolved,
    /// `SpawnAborted` written **over** a spawn that had got somewhere — the b600d82 correction.
    ///
    /// An abort is a decision, so §7.2's *"a node marion decided the fate of is never `Orphaned`"*
    /// excludes it from the arm above. But `run.rs`'s `AbortOnDrop` is armed across the whole
    /// synchronous child run, so an unwind writes `SpawnAborted` beside a `Spawned` that names a
    /// live pid — and a `Child` dropped rather than waited on is not signalled. The decision on
    /// record is therefore about the *spawn*, not about the process, and that gap is named rather
    /// than folded into either neighbour.
    ///
    /// **Two shapes are not this, and both produce no entry at all.** The plain case —
    /// `SpawnIntent` then `SpawnAborted` and nothing else, which is what a `marion run` whose root
    /// failed to launch journals — because there is no process and there never was one. And the
    /// abort written *after* a terminal record, which is `run.rs`'s own commonest way to reach the
    /// guard (`persist_then_cap` returning while it is still armed): the whole content of this
    /// variant is *"marion cannot say whether a process was still running"*, and beside an observed
    /// exit marion can, so [`classify`] asks that first.
    AbortedOverALiveSpawn,
}

/// One node and what restart concluded about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marked {
    pub agent_id: AgentId,
    pub marking: Marking,
}

/// §7.2's restart pass, as a pure reading: what would be marked, in the tree's own node order.
///
/// Pure and separate from [`apply`] so the judgement can be asserted without a mutable tree, and so
/// the classification has exactly one implementation for both callers.
///
/// Nodes with no entry are the ones §7.2 has nothing to say about on restart: already `Exited(_)`,
/// already `ReapedIdle`, or aborted before a process existed.
pub fn mark(tree: &Replay) -> Vec<Marked> {
    tree.nodes().iter().filter_map(classify).collect()
}

/// The partition, once. Every caller reads this rather than restating the clauses — a second copy
/// is what lets a refusal class drift out of one of them.
///
/// **Terminality is asked before anything else, and the order is the correctness argument.** Both
/// refusal classes below are claims about what marion *could not determine*, and each is read off
/// a field that the journal never retracts: `reap_intent` outlives an unobserved kill, and
/// `spawn_aborted` outlives the guard that wrote it. Asking either first therefore lets a stale
/// field outvote an observation marion actually made, and the result is a refusal that is
/// provably false rather than merely cautious:
///
/// * `run.rs` writes `Spawned` at the instant the process exists and `Exited` from the completion
///   afterwards — then `persist_then_cap`'s `?` can return while `AbortOnDrop` is still armed,
///   appending `SpawnAborted` over both. Reporting `AbortedOverALiveSpawn` there claims the abort
///   may have been written over a running process, when the journal in front of it records the
///   process being observed dead. **This clause got sharper when the pid landed, not weaker:**
///   `Spawned` now genuinely does name a process that was alive, so `spawn_confirmed` is no longer
///   a near-tautology and the `Exited` in front of it is the only thing keeping the abort arm off
///   a node whose death is on the record.
/// * §7.2 resolves an unconfirmed reap intent *by checking for the process*. A terminal record is
///   that check, already made and already journaled. Reporting `ReapIntentUnresolved` over it
///   claims marion cannot tell whether a process it watched die is alive — and, through
///   `handler.rs`'s residency, keeps an already-dead fleet's supervisor alive forever.
///
/// A node whose fate is on the record needs no verdict from restart at all, which is what the
/// early return says.
fn classify(node: &ReplayedNode) -> Option<Marked> {
    if fate_decided(node) {
        return None;
    }
    let marking = if node.reap_intent.is_some() {
        Marking::ReapIntentUnresolved
    } else if node.is_unresolved() {
        Marking::Orphaned
    } else if node.spawn_aborted.is_some() && (node.spawn_confirmed || node.pid.is_some()) {
        Marking::AbortedOverALiveSpawn
    } else {
        return None;
    };
    Some(Marked {
        agent_id: node.agent_id.clone(),
        marking,
    })
}

/// **Whether the journal shows marion deciding this node's fate** — [`classify`]'s early return,
/// as a predicate other modules can ask.
///
/// Exposed rather than restated, for the reason this module gives about its own partition: a second
/// copy of a clause is how the two drift apart. [`crate::procid`] needs exactly this question and
/// asked it the wrong way first — by re-running [`mark`] and treating "no marking" as "decided",
/// which is true before [`apply`] and false after it, because `apply` moves a node to `Orphaned`
/// and `is_unresolved` then stops firing. That made every orphan read as decided the moment the
/// standard restart pass had run, which would have reported a fleet of healthy orphans as leaks.
///
/// This is stable across `apply`: `Orphaned` is precisely marion saying it did **not** decide.
pub fn fate_decided(node: &ReplayedNode) -> bool {
    node.state.is_exited() || node.reap_state == ReapState::ReapedIdle
}

/// Run the pass and **write its `Orphaned` verdicts into the tree**, answering everything it
/// concluded.
///
/// Only [`Marking::Orphaned`] moves a `reap_state`. The other two arms are reports about nodes
/// whose journal reading is already correct and must stay exactly as recorded.
pub fn apply(tree: &mut Replay) -> Vec<Marked> {
    let marks = mark(tree);
    for m in &marks {
        if m.marking == Marking::Orphaned {
            let moved = tree.mark_orphaned(&m.agent_id);
            debug_assert!(moved, "the id came from this tree's own nodes");
        }
    }
    marks
}

/// The nodes §7.2 leaves **resumable** across a restart: reaped idle, and never exited.
///
/// The `!is_exited()` half is not redundant. `session/quit`'s confirmed `KillTree` writes a
/// `KillConfirmed` over an already-`ReapedIdle` node — `handler.rs` says so in as many words,
/// *"confirmed session/quit retired an already ReapedIdle node; no process existed to signal"* —
/// which replays as `Exited(Cancelled)` with `reap_state` still `ReapedIdle`. A node retired that
/// way has been deliberately ended and is not resumable, and keying on `reap_state` alone would
/// offer the operator a resume of a node marion has already retired.
pub fn resumable(tree: &Replay) -> Vec<&ReplayedNode> {
    tree.nodes()
        .iter()
        .filter(|n| n.reap_state == ReapState::ReapedIdle && !n.state.is_exited())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::{ExitStatus, ProcessExit, TaskId};
    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::ir::Provenance;
    use marion_core::journal::{
        Exited, JournalRecord, KillConfirmed, ReapConfirmed, ReapIntent, RecordKind, SpawnAborted,
        SpawnIntent, Spawned, StateChanged, WriterId, encode,
    };
    use marion_core::node::NodeState;
    use marion_core::registry::replay;

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    /// A journal builder that keeps the writer's ordinals gapless, so no test accidentally asserts
    /// over a tree replay flagged a `SeqGap` in.
    #[derive(Default)]
    struct Log(Vec<JournalRecord>);

    impl Log {
        fn push(&mut self, kind: RecordKind) -> &mut Self {
            let seq = self.0.len() as u64;
            self.0.push(JournalRecord {
                writer: WriterId("w".into()),
                seq,
                ts: SystemTime::from_unix_millis(1_785_625_628_619),
                mono_ns: seq,
                provenance: Provenance::marion(),
                src_seq: None,
                kind,
            });
            self
        }

        /// `SpawnIntent` + `Spawned` for one node: the shape every node below starts from.
        fn started(&mut self, agent: &str, pid: Option<i32>) -> &mut Self {
            self.push(RecordKind::SpawnIntent(SpawnIntent {
                agent_id: id(agent),
                parent_id: None,
                agent_type: "claude".into(),
                harness: Harness::ClaudeCode,
                depth: 0,
                task_id: Some(TaskId(format!("t-{agent}"))),
            }));
            self.push(RecordKind::Spawned(Spawned {
                agent_id: id(agent),
                harness_version: "2.1.220".into(),
                model: None,
                pid,
                start_id: None,
            }))
        }

        fn tree(&self) -> Replay {
            let bytes: Vec<u8> = self.0.iter().flat_map(|r| encode(r).unwrap()).collect();
            let t = replay(&bytes);
            assert!(t.gaps.is_empty(), "the fixture journal must be gapless");
            assert!(t.truncation.is_none(), "the fixture journal must be whole");
            t
        }
    }

    fn marking_of(marks: &[Marked], agent: &str) -> Option<Marking> {
        marks
            .iter()
            .find(|m| m.agent_id == id(agent))
            .map(|m| m.marking.clone())
    }

    /// A tree carrying one node of every class §7.2 partitions, so every assertion below is also a
    /// negative control for the others: each node is named, and each is asserted *not* to be what
    /// its neighbours are.
    fn every_class() -> Replay {
        let mut log = Log::default();
        // Recorded live, nothing since. §7.2's Orphaned.
        log.started("lost", None);
        // Ran and exited on the record.
        log.started("exited", None);
        log.push(RecordKind::Exited(Exited {
            agent_id: id("exited"),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "exited cleanly".into(),
            },
        }));
        // Reaped, intent confirmed. Resumable.
        log.started("reaped", Some(4242));
        log.push(RecordKind::ReapIntent(ReapIntent {
            agent_id: id("reaped"),
            reason: "idle memory reclaim".into(),
        }));
        log.push(RecordKind::ReapConfirmed(ReapConfirmed {
            agent_id: id("reaped"),
        }));
        // Reap intent written, supervisor died before the confirmation.
        log.started("reaping", Some(4243));
        log.push(RecordKind::ReapIntent(ReapIntent {
            agent_id: id("reaping"),
            reason: "idle memory reclaim".into(),
        }));
        // b600d82's plain case: the intent was abandoned before a process existed.
        log.push(RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id("nevergot"),
            parent_id: None,
            agent_type: "claude".into(),
            harness: Harness::ClaudeCode,
            depth: 0,
            task_id: None,
        }));
        log.push(RecordKind::SpawnAborted(SpawnAborted {
            agent_id: id("nevergot"),
            reason: "the harness binary was not on PATH".into(),
        }));
        // b600d82's correction: the abort was written over a spawn that had already confirmed.
        log.started("abortedlive", Some(4244));
        log.push(RecordKind::SpawnAborted(SpawnAborted {
            agent_id: id("abortedlive"),
            reason: "an unwind aborted the run after the child existed".into(),
        }));
        log.tree()
    }

    /// **The pass itself**, and the negative control the brief names first: an `Orphaned` node must
    /// be provably distinguishable from one that exited.
    #[test]
    fn a_live_node_is_marked_orphaned_and_an_exited_one_is_not_marked_at_all() {
        let mut tree = every_class();
        let marks = apply(&mut tree);

        assert_eq!(marking_of(&marks, "lost"), Some(Marking::Orphaned));
        assert_eq!(
            tree.get(&id("lost")).unwrap().reap_state,
            ReapState::Orphaned,
            "§7.2: on restart, Live → Orphaned"
        );
        // The distinction, both ways round.
        assert_eq!(
            marking_of(&marks, "exited"),
            None,
            "an exit is a record of a decided fate"
        );
        let exited = tree.get(&id("exited")).unwrap();
        assert_eq!(
            exited.reap_state,
            ReapState::Live,
            "an exited node is not orphaned"
        );
        assert_eq!(exited.state, NodeState::Exited(ExitStatus::Ok));
        let lost = tree.get(&id("lost")).unwrap();
        assert!(
            !lost.state.is_exited(),
            "Orphaned is not an exit (§7.6, node.rs)"
        );
        assert_eq!(
            lost.exit, None,
            "nothing was observed, so no ProcessExit is fabricated"
        );
    }

    /// §7.2's second negative control: a `ReapedIdle` node is provably resumable and provably not
    /// orphaned — and the pass does not touch it.
    #[test]
    fn a_reaped_idle_node_is_resumable_and_is_never_marked_orphaned() {
        let mut tree = every_class();
        let marks = apply(&mut tree);

        assert_eq!(marking_of(&marks, "reaped"), None);
        let reaped = tree.get(&id("reaped")).unwrap();
        assert_eq!(reaped.reap_state, ReapState::ReapedIdle);
        assert_ne!(
            reaped.reap_state,
            ReapState::Orphaned,
            "§7.2: a node marion decided the fate of is never Orphaned"
        );
        assert_eq!(
            resumable(&tree)
                .iter()
                .map(|n| n.agent_id.0.as_str())
                .collect::<Vec<_>>(),
            ["reaped"],
            "the reaped node is the only resumable one; the orphan is not"
        );
        // Resumable means the node's identity survived, not merely that a flag is set.
        assert_eq!(reaped.agent_type(), Some("claude"));
        assert_eq!(reaped.task_id(), Some(&TaskId("t-reaped".into())));
        assert!(reaped.spawn_confirmed);
        assert_eq!(
            reaped.reap_intent, None,
            "the intent was confirmed, not left outstanding"
        );
    }

    /// §7.2's third: a node whose liveness marion cannot determine is distinguishable from both.
    #[test]
    fn an_unconfirmed_reap_intent_is_left_undetermined_and_is_neither_orphaned_nor_reaped() {
        let mut tree = every_class();
        let marks = apply(&mut tree);

        assert_eq!(
            marking_of(&marks, "reaping"),
            Some(Marking::ReapIntentUnresolved)
        );
        let n = tree.get(&id("reaping")).unwrap();
        assert_eq!(
            n.reap_state,
            ReapState::Live,
            "no ReapConfirmed was written, because none was observed"
        );
        assert_ne!(
            n.reap_state,
            ReapState::Orphaned,
            "§7.2: it is never marked Orphaned"
        );
        assert_ne!(
            n.reap_state,
            ReapState::ReapedIdle,
            "that record means observed dead"
        );
        assert_eq!(
            n.reap_intent.as_deref(),
            Some("idle memory reclaim"),
            "the intent stays outstanding, which is what holds §5.7's exit predicate"
        );
        assert!(
            !resumable(&tree).iter().any(|r| r.agent_id == id("reaping")),
            "an undetermined node is not offered as resumable"
        );
    }

    /// The b600d82 partition, both halves, in one test so neither can be silently dropped.
    #[test]
    fn an_abort_before_a_process_marks_nothing_and_an_abort_over_one_is_named() {
        let mut tree = every_class();
        let marks = apply(&mut tree);

        assert_eq!(
            marking_of(&marks, "nevergot"),
            None,
            "no process, and there never was one: nothing to be Orphaned about"
        );
        assert_eq!(
            tree.get(&id("nevergot")).unwrap().reap_state,
            ReapState::Live
        );
        assert_eq!(
            marking_of(&marks, "abortedlive"),
            Some(Marking::AbortedOverALiveSpawn),
            "the abort decided the spawn, not the process"
        );
        assert_ne!(
            tree.get(&id("abortedlive")).unwrap().reap_state,
            ReapState::Orphaned,
            "an abort is a decision, so §7.2 excludes it from Orphaned"
        );
    }

    /// **Every class, stated as one exhaustive partition** — the trap the brief names: a test that
    /// claims to check "every refusal class" and checks one. Every node in the fixture appears here
    /// with its exact verdict, so adding a class without extending this fails.
    #[test]
    fn the_partition_is_total_and_every_node_gets_exactly_one_verdict() {
        let tree = every_class();
        let marks = mark(&tree);
        let got: Vec<(&str, Marking)> = marks
            .iter()
            .map(|m| (m.agent_id.0.as_str(), m.marking.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("lost", Marking::Orphaned),
                ("reaping", Marking::ReapIntentUnresolved),
                ("abortedlive", Marking::AbortedOverALiveSpawn),
            ],
            "in the journal's own node order, and nothing else marked"
        );
        // No node is claimed twice, which is what makes the list above a partition rather than a
        // set of overlapping filters.
        let mut ids: Vec<&str> = marks.iter().map(|m| m.agent_id.0.as_str()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "one verdict per node");
        assert_eq!(tree.nodes().len(), 6, "the fixture covers six classes");
    }

    /// A `ReapedIdle` node that a confirmed `session/quit` KillTree later retired is **not**
    /// resumable, though its `reap_state` still reads `ReapedIdle`.
    #[test]
    fn a_reaped_idle_node_retired_by_a_confirmed_kill_is_no_longer_resumable() {
        let mut log = Log::default();
        log.started("reaped", Some(4242));
        log.push(RecordKind::ReapIntent(ReapIntent {
            agent_id: id("reaped"),
            reason: "idle memory reclaim".into(),
        }));
        log.push(RecordKind::ReapConfirmed(ReapConfirmed {
            agent_id: id("reaped"),
        }));
        let tree = log.tree();
        assert_eq!(resumable(&tree).len(), 1, "resumable until it is retired");

        log.push(RecordKind::KillConfirmed(KillConfirmed {
            agent_id: id("reaped"),
            exit: ProcessExit {
                code: None,
                signal: None,
                description: "confirmed session/quit retired an already ReapedIdle node; no \
                              process existed to signal"
                    .into(),
            },
        }));
        let tree = log.tree();
        let n = tree.get(&id("reaped")).unwrap();
        assert_eq!(
            n.reap_state,
            ReapState::ReapedIdle,
            "the reap is still on the record"
        );
        assert_eq!(n.state, NodeState::Exited(ExitStatus::Cancelled));
        assert!(
            resumable(&tree).is_empty(),
            "a retired node must not be offered as resumable"
        );
    }

    /// The pass is **idempotent**, which is what lets it be derived rather than journaled: a second
    /// restart over the same bytes reaches the same tree, and re-running it changes nothing.
    #[test]
    fn re_running_the_pass_over_an_already_marked_tree_changes_nothing() {
        let mut tree = every_class();
        let first = apply(&mut tree);
        let snapshot = tree.clone();
        let second = apply(&mut tree);
        assert_eq!(
            second,
            first
                .iter()
                .filter(|m| m.marking != Marking::Orphaned)
                .cloned()
                .collect::<Vec<_>>(),
            "an already-Orphaned node is no longer unresolved, so it is not re-marked"
        );
        assert_eq!(tree, snapshot, "and nothing about the tree moved");
    }

    /// A later, truthful `Exited` **supersedes** the marking — the property the module doc leans on
    /// when it argues the verdict is derived rather than journaled.
    #[test]
    fn an_exit_written_after_the_marking_supersedes_it_on_the_next_boot() {
        let mut log = Log::default();
        log.started("lost", None);
        let mut tree = log.tree();
        assert_eq!(apply(&mut tree).len(), 1, "marked Orphaned on this boot");

        // The bridge process that was actually driving it finished and wrote its exit.
        log.push(RecordKind::StateChanged(StateChanged {
            agent_id: id("lost"),
            state: NodeState::Idle,
        }));
        log.push(RecordKind::Exited(Exited {
            agent_id: id("lost"),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "exited cleanly".into(),
            },
        }));
        let mut next_boot = log.tree();
        assert!(
            apply(&mut next_boot).is_empty(),
            "the next restart marks nothing: the journal now records the fate"
        );
        assert_eq!(
            next_boot.get(&id("lost")).unwrap().reap_state,
            ReapState::Live,
            "a journaled Orphaned would have contradicted this exit forever"
        );
    }

    /// **Terminality is asked first, or the auxiliary fields lie.** `run.rs`'s production shape:
    /// the child ran, `Spawned` and `Exited` are on the record, and then `persist_then_cap` failed
    /// — so the still-armed `AbortOnDrop` appended `SpawnAborted` on the way out.
    ///
    /// `AbortedOverALiveSpawn` is a claim that the abort may have been written over a process that
    /// was still running. Here it provably was not: production `Spawned` is written *after* the
    /// child was run and reaped, and the journal carries the observed exit beside it.
    #[test]
    fn an_abort_written_after_an_observed_exit_is_not_an_abort_over_a_live_spawn() {
        let mut log = Log::default();
        log.started("finished", None);
        log.push(RecordKind::Exited(Exited {
            agent_id: id("finished"),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "exited cleanly".into(),
            },
        }));
        // `run.rs`: `persist_then_cap(&agent_dir, &contract)?` returns before
        // `resolution.armed = false`, so the guard fires with both records already written.
        log.push(RecordKind::SpawnAborted(SpawnAborted {
            agent_id: id("finished"),
            reason: "marion left the spawn path before the child reached a terminal record".into(),
        }));
        let mut tree = log.tree();
        let marks = apply(&mut tree);
        assert_eq!(
            marking_of(&marks, "finished"),
            None,
            "the exit is observed on the record; there is no live spawn for the abort to be over",
        );
        assert_eq!(
            tree.get(&id("finished")).unwrap().reap_state,
            ReapState::Live,
            "and it is certainly not orphaned",
        );
    }

    /// The same ordering fault on the other arm: a reap intent whose process was **observed** dead
    /// is not one marion failed to resolve.
    ///
    /// §7.2 resolves an unconfirmed intent by checking for the process and *"gone means marion
    /// writes the confirmation"*. A journaled `Exited` — or, as here, the `KillConfirmed` a
    /// `session/quit` KillTree writes over the node — **is** that observation, already on the
    /// record. Reporting `ReapIntentUnresolved` for it claims marion cannot tell whether a process
    /// it watched die is alive.
    #[test]
    fn a_reap_intent_over_a_node_whose_death_was_observed_is_not_unresolved() {
        let mut log = Log::default();
        log.started("reaping", Some(4243));
        log.push(RecordKind::ReapIntent(ReapIntent {
            agent_id: id("reaping"),
            reason: "session/quit reaped an idle node before detaching busy work".into(),
        }));
        // The signal went out and marion could not observe the death, so no `ReapConfirmed` was
        // written (`handler.rs` returns rather than fabricating one). The operator then confirmed
        // a `session/quit` KillTree, which did observe it.
        log.push(RecordKind::KillConfirmed(KillConfirmed {
            agent_id: id("reaping"),
            exit: ProcessExit {
                code: None,
                signal: Some(9),
                description: "confirmed session/quit killed the node's process tree".into(),
            },
        }));
        let mut tree = log.tree();
        let marks = apply(&mut tree);
        assert_eq!(
            marking_of(&marks, "reaping"),
            None,
            "the death is on the record, so there is nothing left for restart to resolve",
        );
    }

    /// The `ReapedIdle` half of the terminality guard, pinned on its own.
    ///
    /// Defensive rather than measured: `handler.rs`'s reap path only ever selects nodes that are
    /// `Idle`, `Live` and intent-free, so marion writing this exact journal would take a sequence
    /// nothing today produces. It is asserted anyway because `classify` is a **total function over
    /// the journal**, not over the journals marion happens to write — a reader of any journal must
    /// not be told marion abandoned a spawn over a process it confirmed dead itself.
    #[test]
    fn a_reaped_node_is_not_reclassified_by_a_later_abort_record() {
        let mut log = Log::default();
        log.started("reaped", Some(4242));
        log.push(RecordKind::ReapIntent(ReapIntent {
            agent_id: id("reaped"),
            reason: "idle memory reclaim".into(),
        }));
        log.push(RecordKind::ReapConfirmed(ReapConfirmed {
            agent_id: id("reaped"),
        }));
        log.push(RecordKind::SpawnAborted(SpawnAborted {
            agent_id: id("reaped"),
            reason: "an unwind aborted the run".into(),
        }));
        let mut tree = log.tree();
        assert_eq!(
            tree.get(&id("reaped")).unwrap().reap_state,
            ReapState::ReapedIdle
        );
        assert_eq!(
            marking_of(&apply(&mut tree), "reaped"),
            None,
            "marion observed this process dead itself; the abort decides nothing further",
        );
    }

    /// An empty journal — a project that has never run — is not a tree full of orphans.
    #[test]
    fn an_empty_tree_marks_nothing() {
        let mut tree = replay(b"");
        assert!(apply(&mut tree).is_empty());
        assert!(resumable(&tree).is_empty());
    }
}
