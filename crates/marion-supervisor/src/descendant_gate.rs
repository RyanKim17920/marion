//! **§7.6's descendant-gated completion** — the rule principle 11 calls non-negotiable, at the one
//! moment it can run.
//!
//! > A node's `Exited` is **held** while any descendant is non-terminal — *unless* the node reported
//! > early or the hold bound expired.
//!
//! The rule's premise is that a stop is not a result: both failures §7.6 records were an agent that
//! stopped while its own children were still running, returning a status message in the slot a
//! result belonged in. marion can gate this where no single harness can, because the registry owns
//! the whole tree — including children running in other harnesses — so the check is a subtree scan
//! and never a heuristic.
//!
//! # What runs, and when
//!
//! `run_spawn` calls [`gate`] after the child's process has stopped and **before** anything terminal
//! is written: no `Exited`, no contract file, no closing bookend. That ordering is the whole
//! mechanism. The gate reads the live descendant set off the registry the supervisor already holds
//! (through [`crate::run::SpawnObserver::live_descendants`] — no new state, no second table), then:
//!
//! * **no live descendant** → admit. Nothing to gate.
//! * **a staged `report`** ([`Stop::Concluded`]) → admit, `reported_early: true`, the live set
//!   recorded in `live_descendants_at_report`. A report is a deliberate conclusion; the node has
//!   chosen (§7.6 step 1), and the flag is what keeps its exit legal under L1.
//! * **an orderly stop with no report** ([`Stop::Voluntary`]) → **hold**. The node is journaled
//!   `Blocked(Descendants)` and the gate polls the registry until every descendant is terminal or
//!   the node's own bound expires (step 3). Released → the ordinary `Unreported` follows; expired →
//!   `held_to_timeout: true`, `Unreported`, and the still-running descendants **outlive** it
//!   (§7.5: killing them would destroy work to tidy up bookkeeping).
//! * **a death** ([`Stop::Involuntary`]) → admit, `died_before_gate: true`. The node never got the
//!   chance to choose, so L1 exempts it; holding a corpse would pin an ancestor open for nothing.
//!
//! The bound is the node's own `timeout`, never its descendants' — §7.6 is explicit that unbounded
//! holding "made a slow grandchild able to pin an ancestor open forever".
//!
//! # What this module does not do
//!
//! Steps 2 and 4 — the `Stop`-hook re-prompt and the grace turn — need a mechanism per harness and
//! are not here. A hookless node that stops voluntarily is **not** exempt for want of one (§7.6:
//! "skipping steps 2–4 for want of a mechanism never skips the gate"); it goes straight to the
//! hold, which is exactly what [`Verdict::Hold`] is. An owner that holds no registry
//! ([`crate::run::Unwatched`]) cannot see the tree and the gate leaves the two flags unset — the
//! supervisor is the only owner of a child since §11 item 28, so that path is fixtures and
//! `marion run`'s own root, which `root.rs` gates on its own terms.

use std::time::{Duration, Instant};

use marion_core::contract::{AgentId, ExitStatus, TaskContract};
use marion_core::journal::{RecordKind, StateChanged};
use marion_core::node::{BlockReason, NodeState};
use marion_core::paths::ProjectDir;
use marion_core::registry::Replay;

use crate::run::SpawnObserver;
use crate::spawn::ChildOutcome;

/// How often the hold re-reads the registry. The registry is a follower of the journal and the
/// grandchild's `Exited` lands as a file append, so this is a latency floor on release, not a
/// correctness parameter.
const HOLD_POLL: Duration = Duration::from_millis(100);

/// §7.6's gating set, stated totally: a descendant is **live** iff it is not `Exited(_)`, its
/// reap state is not one marion will observe no further transition of (`Orphaned`, `ReapedIdle`),
/// and its spawn was not aborted — a node whose fate marion decided is never one to wait on.
///
/// The whole subtree, not the direct children: a live grandchild holds the node exactly as a live
/// child does (§7.6 step 1). Breadth-first over `parent_id`, which `SpawnIntent` fixes at birth, so
/// the walk terminates on any journal marion wrote.
pub fn live_descendants(tree: &Replay, node: &AgentId) -> Vec<AgentId> {
    let mut frontier = vec![node.clone()];
    let mut live = Vec::new();
    while let Some(parent) = frontier.pop() {
        for n in tree.children(&parent) {
            frontier.push(n.agent_id.clone());
            let terminal = n.state.is_exited()
                || n.reap_state.is_terminal_for_gating()
                || n.spawn_aborted.is_some();
            if !terminal {
                live.push(n.agent_id.clone());
            }
        }
    }
    live
}

/// How the node stopped, in the terms §7.6 gates on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// A `report` was staged: the node concluded on purpose. Its exit code no longer matters to the
    /// gate — "a node that reports and then dies non-zero has `died_before_gate: false`".
    Concluded,
    /// An orderly end of turn with nothing reported — the `codex exec` shape, and the one the rule
    /// exists for. Not exempt: it reaches the gate.
    Voluntary,
    /// The process died before it could choose: a signal, a non-zero abort, a stream that said the
    /// run failed, or marion's own timeout kill. L1 exempts these — the node never got the chance.
    Involuntary,
}

impl Stop {
    pub fn of(outcome: &ChildOutcome) -> Self {
        if outcome.narrative.is_some() {
            Stop::Concluded
        } else if outcome.timed_out
            || outcome.signal.is_some()
            || outcome.failure.is_some()
            || outcome.exit_code != Some(0)
        {
            Stop::Involuntary
        } else {
            Stop::Voluntary
        }
    }
}

/// The gate's decision about a stop, given the live set at that instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to gate, or nothing that may be gated (an involuntary death).
    Admit,
    /// A deliberate early conclusion; the set is what `live_descendants_at_report` records.
    ReportedEarly(Vec<AgentId>),
    /// A voluntary, unreported stop with live descendants: hold until they are terminal or the
    /// node's bound expires.
    Hold(Vec<AgentId>),
}

pub fn evaluate(stop: Stop, live: Vec<AgentId>) -> Verdict {
    if live.is_empty() {
        return Verdict::Admit;
    }
    match stop {
        Stop::Concluded => Verdict::ReportedEarly(live),
        Stop::Voluntary => Verdict::Hold(live),
        Stop::Involuntary => Verdict::Admit,
    }
}

/// How a hold ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    /// Every descendant reached a terminal state inside the bound.
    Released,
    /// The bound expired first; these descendants were still live and outlive the node.
    Expired(Vec<AgentId>),
}

/// Poll `live` until it is empty or `deadline` passes. The set is re-read on every pass and the
/// **final** reading is what an expiry reports, so the contract names what was live at the
/// moment the flag was set (§7.6: "records the set at whichever moment set the flag").
pub fn hold(mut live: impl FnMut() -> Vec<AgentId>, deadline: Instant, poll: Duration) -> Held {
    loop {
        let now = live();
        if now.is_empty() {
            return Held::Released;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Held::Expired(now);
        }
        std::thread::sleep(poll.min(remaining));
    }
}

/// What the gate decided, ready to be written onto the contract's `Completion`.
///
/// A value rather than a mutation of the contract at the gate, because the contract does not exist
/// yet when the gate runs — `Completion` is assembled once, at the terminal transition, after the
/// hold (§7.6 step 1: "nothing was written yet").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gated {
    pub died_before_gate: bool,
    pub reported_early: bool,
    pub held_to_timeout: bool,
    pub live_descendants_at_report: Vec<AgentId>,
    /// The rule, in marion's words, for `ProcessExit.description` — so a reader of the contract
    /// alone can tell a held exit from an ordinary one.
    pub note: Option<String>,
}

impl Gated {
    pub fn apply(&self, contract: &mut TaskContract) {
        let Some(c) = contract.completion.as_mut() else {
            return;
        };
        c.died_before_gate = self.died_before_gate;
        c.reported_early = self.reported_early;
        c.held_to_timeout = self.held_to_timeout;
        c.live_descendants_at_report = self.live_descendants_at_report.clone();
        // §7.6 step 3: a task node whose hold bound expires becomes `Exited{Unreported}`. Stated
        // rather than relied on — the derivation in `build_contract` lands there too for an
        // unreported stop, but the rule is this module's and two derivations of one status are two
        // chances to disagree.
        if self.held_to_timeout {
            c.status = ExitStatus::Unreported;
        }
        if let Some(note) = &self.note {
            c.exit.description = format!("{}; {note}", c.exit.description);
        }
    }
}

fn names(ids: &[AgentId]) -> String {
    ids.iter()
        .map(|a| a.0.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// **The gate, run at the node's terminal transition.** See the module docs for the four outcomes.
///
/// `spawned` and `bound` are the node's own clock: the hold runs on whatever of the bound the turn
/// left, never on a fresh budget, so a node's contract can never claim a `timeout` it outlived.
pub fn gate(
    observer: &dyn SpawnObserver,
    agent_id: &AgentId,
    project: &ProjectDir,
    outcome: &ChildOutcome,
    spawned: std::time::SystemTime,
    bound: Duration,
) -> Gated {
    let stop = Stop::of(outcome);
    let mut gated = Gated {
        // A fact about the stop, not about the tree: recorded whether or not the tree is readable.
        // A timeout is marion's own terminal (`TimedOut`) and exempt on its own account, so it is
        // not also written up as a death before the gate.
        died_before_gate: stop == Stop::Involuntary && !outcome.timed_out,
        ..Gated::default()
    };
    let Some(live) = observer.live_descendants(agent_id) else {
        return gated;
    };
    match evaluate(stop, live) {
        Verdict::Admit => {}
        Verdict::ReportedEarly(live) => {
            gated.note = Some(format!(
                "reported early under §7.6 descendant gating: {} descendant(s) were live at the \
                 report ({})",
                live.len(),
                names(&live)
            ));
            gated.reported_early = true;
            gated.live_descendants_at_report = live;
        }
        Verdict::Hold(live) => {
            // §7.6 step 3: the hold is a state the tree can see, not a private sleep.
            crate::journal::record(
                project,
                RecordKind::StateChanged(StateChanged {
                    agent_id: agent_id.clone(),
                    state: NodeState::Blocked(BlockReason::Descendants),
                }),
            );
            let remaining = bound.saturating_sub(spawned.elapsed().unwrap_or_default());
            let entered = format!(
                "held under §7.6 descendant gating: the node stopped without a report while {} \
                 descendant(s) were live ({})",
                live.len(),
                names(&live)
            );
            match hold(
                || observer.live_descendants(agent_id).unwrap_or_default(),
                Instant::now() + remaining,
                HOLD_POLL,
            ) {
                Held::Released => {
                    gated.note = Some(format!(
                        "{entered}; the hold ended when every descendant was terminal"
                    ));
                }
                Held::Expired(still_live) => {
                    gated.note = Some(format!(
                        "{entered}; the node's own bound of {}s expired first (held_to_timeout), \
                         and its {} still-running descendant(s) outlive it ({})",
                        bound.as_secs(),
                        still_live.len(),
                        names(&still_live)
                    ));
                    gated.held_to_timeout = true;
                    gated.live_descendants_at_report = still_live;
                }
            }
        }
    }
    gated
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::contract::{
        AgentId, ExitStatus, Glob, Oid, ProcessExit, RepoIdentity, TaskId, Workspace,
    };
    use marion_core::encoding::SystemTime;
    use marion_core::harness::Harness;
    use marion_core::ir::Provenance;
    use marion_core::journal::{
        Exited, JournalRecord, RecordKind, SpawnAborted, SpawnIntent, Spawned, WriterId, encode,
    };
    use marion_core::registry::replay;

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    fn record(seq: u64, kind: RecordKind) -> JournalRecord {
        JournalRecord {
            writer: WriterId("w".into()),
            seq,
            ts: SystemTime::from_unix_millis(1_785_625_628_619),
            mono_ns: seq,
            provenance: Provenance::marion(),
            src_seq: None,
            kind,
        }
    }

    fn intent(agent: &str, parent: Option<&str>, depth: u32) -> RecordKind {
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: id(agent),
            parent_id: parent.map(id),
            agent_type: "codex-impl".into(),
            harness: Harness::Codex,
            depth,
            task_id: Some(TaskId(format!("task-{agent}"))),
            timeout_secs: None,
        })
    }

    fn spawned(agent: &str) -> RecordKind {
        RecordKind::Spawned(Spawned {
            agent_id: id(agent),
            harness_version: "0".into(),
            model: None,
            pid: Some(4242),
            start_id: None,
        })
    }

    fn exited(agent: &str) -> RecordKind {
        RecordKind::Exited(Exited {
            agent_id: id(agent),
            status: ExitStatus::Ok,
            exit: ProcessExit {
                code: Some(0),
                signal: None,
                description: "done".into(),
            },
        })
    }

    fn tree_of(kinds: Vec<RecordKind>) -> Replay {
        let bytes: Vec<u8> = kinds
            .into_iter()
            .enumerate()
            .flat_map(|(seq, k)| encode(&record(seq as u64, k)).unwrap())
            .collect();
        replay(&bytes)
    }

    /// A root, a child, and under the child: a live grandchild, an exited one, an orphaned one and
    /// an aborted intent — plus a live great-grandchild under the live grandchild.
    fn fixture() -> Replay {
        let mut t = tree_of(vec![
            intent("root", None, 0),
            spawned("root"),
            intent("child", Some("root"), 1),
            spawned("child"),
            intent("live", Some("child"), 2),
            spawned("live"),
            intent("done", Some("child"), 2),
            spawned("done"),
            exited("done"),
            intent("lost", Some("child"), 2),
            spawned("lost"),
            intent("never", Some("child"), 2),
            RecordKind::SpawnAborted(SpawnAborted {
                agent_id: id("never"),
                reason: "refused".into(),
            }),
            intent("deep", Some("live"), 3),
            spawned("deep"),
        ]);
        assert!(t.mark_orphaned(&id("lost")));
        t
    }

    fn sorted(mut v: Vec<AgentId>) -> Vec<AgentId> {
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    #[test]
    fn live_descendants_scans_the_whole_subtree_and_counts_only_the_gating_live_set() {
        let t = fixture();
        assert_eq!(
            sorted(live_descendants(&t, &id("child"))),
            vec![id("deep"), id("live")],
            "an exited, an orphaned and an aborted node are all terminal for gating; a live \
             great-grandchild is not"
        );
        assert_eq!(
            sorted(live_descendants(&t, &id("root"))),
            vec![id("child"), id("deep"), id("live")],
            "the root's set includes the child and everything live under it"
        );
        assert!(live_descendants(&t, &id("live")).contains(&id("deep")));
        assert!(live_descendants(&t, &id("deep")).is_empty());
        assert!(live_descendants(&t, &id("done")).is_empty());
    }

    #[test]
    fn a_stop_is_classified_by_whether_the_node_got_to_choose() {
        let reported = ChildOutcome {
            narrative: Some("done".into()),
            exit_code: Some(1),
            ..ChildOutcome::default()
        };
        assert_eq!(
            Stop::of(&reported),
            Stop::Concluded,
            "a report is a conclusion whatever the exit code"
        );
        let orderly = ChildOutcome {
            exit_code: Some(0),
            ..ChildOutcome::default()
        };
        assert_eq!(Stop::of(&orderly), Stop::Voluntary);
        for died in [
            ChildOutcome {
                exit_code: Some(2),
                ..ChildOutcome::default()
            },
            ChildOutcome {
                signal: Some(11),
                ..ChildOutcome::default()
            },
            ChildOutcome {
                exit_code: Some(0),
                failure: Some("provider refused".into()),
                ..ChildOutcome::default()
            },
            ChildOutcome {
                timed_out: true,
                ..ChildOutcome::default()
            },
        ] {
            assert_eq!(Stop::of(&died), Stop::Involuntary, "{died:?}");
        }
    }

    #[test]
    fn the_verdict_admits_holds_or_marks_early_per_the_rule() {
        let live = vec![id("g")];
        assert_eq!(evaluate(Stop::Voluntary, vec![]), Verdict::Admit);
        assert_eq!(evaluate(Stop::Concluded, vec![]), Verdict::Admit);
        assert_eq!(
            evaluate(Stop::Concluded, live.clone()),
            Verdict::ReportedEarly(live.clone())
        );
        assert_eq!(
            evaluate(Stop::Voluntary, live.clone()),
            Verdict::Hold(live.clone())
        );
        assert_eq!(
            evaluate(Stop::Involuntary, live),
            Verdict::Admit,
            "a node that died before it could choose is L1-exempt and is not held"
        );
    }

    #[test]
    fn a_hold_releases_when_the_set_empties_and_expires_with_the_set_it_last_saw() {
        let mut readings = vec![vec![], vec![id("g")], vec![id("g")]];
        let released = hold(
            || readings.pop().unwrap_or_default(),
            Instant::now() + Duration::from_secs(30),
            Duration::from_millis(1),
        );
        assert_eq!(released, Held::Released);
        assert!(
            readings.is_empty(),
            "every reading was consumed before release"
        );

        let expired = hold(
            || vec![id("g")],
            Instant::now() + Duration::from_millis(30),
            Duration::from_millis(5),
        );
        assert_eq!(expired, Held::Expired(vec![id("g")]));
    }

    fn contract(outcome: &ChildOutcome) -> TaskContract {
        crate::spawn::build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            RepoIdentity {
                git_common_dir: None,
                head_branch: None,
            },
            None::<Oid>,
            Workspace::Worktree {
                path: "/wt".into(),
                branch: "b".into(),
            },
            "do it",
            &[],
            &[Glob("**".into())],
            &[Glob("**".into())],
            marion_core::encoding::Duration(Duration::from_secs(900)),
            SystemTime(std::time::SystemTime::now()),
            outcome,
            None,
            None,
            vec![],
        )
    }

    #[test]
    fn an_expired_hold_lands_unreported_with_the_flag_the_set_and_the_rule_named() {
        let mut c = contract(&ChildOutcome {
            exit_code: Some(0),
            ..ChildOutcome::default()
        });
        Gated {
            held_to_timeout: true,
            live_descendants_at_report: vec![id("g")],
            note: Some("held under §7.6 descendant gating: test".into()),
            ..Gated::default()
        }
        .apply(&mut c);
        let comp = c.completion.unwrap();
        assert_eq!(comp.status, ExitStatus::Unreported);
        assert!(comp.held_to_timeout);
        assert!(!comp.reported_early);
        assert_eq!(comp.live_descendants_at_report, vec![id("g")]);
        assert!(
            comp.exit.description.contains("§7.6 descendant gating"),
            "{}",
            comp.exit.description
        );
        assert!(
            comp.exit
                .description
                .starts_with("child exited with code 0"),
            "the rule is appended to marion's exit numbers, never in place of them: {}",
            comp.exit.description
        );
    }

    #[test]
    fn an_early_report_keeps_its_status_and_names_the_live_set() {
        let mut c = contract(&ChildOutcome {
            narrative: Some("done for now".into()),
            exit_code: Some(0),
            ..ChildOutcome::default()
        });
        Gated {
            reported_early: true,
            live_descendants_at_report: vec![id("g")],
            note: None,
            ..Gated::default()
        }
        .apply(&mut c);
        let comp = c.completion.unwrap();
        assert_eq!(comp.status, ExitStatus::Ok);
        assert!(comp.reported_early);
        assert!(!comp.held_to_timeout);
        assert_eq!(comp.live_descendants_at_report, vec![id("g")]);
    }

    /// An owner that cannot see the tree leaves the gate's two flags unset and holds nothing — and
    /// still records the one fact that is about the stop rather than the tree.
    #[test]
    fn an_owner_without_a_registry_gates_nothing_but_still_records_a_death() {
        let project = ProjectDir::new(
            &std::env::temp_dir().join("marion-descendant-gate-unwatched"),
            std::path::Path::new("/nowhere"),
        );
        let died = ChildOutcome {
            signal: Some(9),
            ..ChildOutcome::default()
        };
        let g = gate(
            &crate::run::Unwatched,
            &id("n"),
            &project,
            &died,
            std::time::SystemTime::now(),
            Duration::from_secs(10),
        );
        assert!(g.died_before_gate);
        assert!(!g.reported_early && !g.held_to_timeout && g.note.is_none());
        let timed_out = ChildOutcome {
            timed_out: true,
            ..ChildOutcome::default()
        };
        let g = gate(
            &crate::run::Unwatched,
            &id("n"),
            &project,
            &timed_out,
            std::time::SystemTime::now(),
            Duration::from_secs(10),
        );
        assert!(
            !g.died_before_gate,
            "a timeout is marion's own exempt terminal, not a death before the gate"
        );
    }
}
