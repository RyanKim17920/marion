//! **Turn delivery by continuation** — the lane for a `LaunchOnly` node, whose one turn rides argv
//! and which has no channel after it (`marion_harness::spec::TurnDelivery::Continuation`).
//!
//! The next turn of such a node is a **relaunch of the same node under its observed session**: the
//! row's resume spelling with the message as the prompt, as a new generation of the same id — the
//! second `Spawned` replay already folds for `node/resume`, written by the same confirmation. This
//! module is the part that decides, at each stop of the process, whether there is a next turn;
//! `run.rs` launches it.
//!
//! # The boundary, in order
//!
//! 1. **A death is the end.** A process that did not stop on its own — killed, crashed, out of
//!    clock — takes no further turn; what waits is dropped when the node closes.
//! 2. **A message already waiting is the next turn** — one queued while the process ran.
//! 3. **Otherwise the §7.6 gate runs**, and a hold is a wait for a message: one that arrives while
//!    the node is held for its descendants (a child's end, a parent's steer) ends the hold and is
//!    the next turn — §7.6 step 2's re-prompt (`descendant_gate::gate_or_woken`).
//! 4. **Once the gate settles, the inbox is taken or sealed in one step**, and while it is held
//!    open for a message still owed ([`TurnSource::held`]) the driver waits for it on the node's
//!    clock rather than seal — **unless the node reported early**: §7.6 accepts a staged report
//!    with live descendants as a deliberate conclusion at once, so nothing owed keeps it waiting,
//!    and the child's end finds the node ended (its close settles the debt).
//!
//! A message that cannot be a turn — the node's stream never named a session, or its clock is
//! spent — is dropped with that reason and the boundary is taken again.

use std::time::{Duration, Instant};

use crate::descendant_gate::{Gated, Stop, Waited};
use crate::inbox::{Latch, Message, TurnSource};
use crate::spawn::ChildOutcome;

/// How long one wait for an owed message lasts before the driver looks again.
const OWED_POLL: Duration = Duration::from_millis(100);

/// A node's inbox with the port its driver blocks on attached.
pub(crate) struct Turns {
    source: std::sync::Arc<dyn TurnSource>,
    latch: std::sync::Arc<Latch>,
}

impl Turns {
    pub(crate) fn attach(source: std::sync::Arc<dyn TurnSource>) -> Self {
        let latch = std::sync::Arc::new(Latch::default());
        source.attach_port(latch.clone());
        Turns { source, latch }
    }

    /// Wait up to `bound` for a message and take it.
    fn wait_take(&self, bound: Duration) -> Option<Message> {
        if self.latch.wait_until(Some(Instant::now() + bound)) {
            self.source.take_next()
        } else {
            None
        }
    }

    pub(crate) fn delivered(&self, id: &str, via: &str) {
        self.source.delivered(id, via);
    }

    pub(crate) fn dropped(&self, id: &str, reason: &str) {
        self.source.dropped(id, reason);
    }
}

/// What the boundary decided.
#[derive(Debug)]
pub(crate) enum Turn {
    /// Relaunch the node under `session` with `message` as its prompt.
    Next { message: Message, session: String },
    /// The node has taken its last turn; the gate's verdict for its contract.
    Last(Gated),
}

/// The gate as the boundary calls it: over the node's outcome so far, spending the hold's pauses in
/// the wait it is handed.
pub(crate) type Gate<'a> =
    dyn FnMut(&ChildOutcome, &mut dyn FnMut(Duration) -> Option<Message>) -> Waited<Message> + 'a;

/// **One stop of the node's process, decided.** See the module docs for the order.
///
/// `session` is what the node's stream named, `deadline` is the end of the node's own wall clock,
/// and `gate` is §7.6's gate over the outcome so far. `turns` is `None` for a node with no inbox,
/// which takes exactly the turn it was launched with.
pub(crate) fn boundary(
    turns: Option<&Turns>,
    outcome: &ChildOutcome,
    session: Option<&str>,
    deadline: Instant,
    gate: &mut Gate<'_>,
) -> Turn {
    let Some(turns) = turns.filter(|_| Stop::of(outcome) != Stop::Involuntary) else {
        let mut slept = |d: Duration| {
            std::thread::sleep(d);
            None
        };
        return Turn::Last(settled(gate(outcome, &mut slept)));
    };
    loop {
        let message = match turns.source.take_next() {
            Some(m) => m,
            None => match gate(outcome, &mut |d| turns.wait_take(d)) {
                Waited::Woken(m) => m,
                Waited::Settled(gated) => {
                    let next = if gated.reported_early {
                        turns.source.take_next()
                    } else {
                        take_or_seal(turns, deadline)
                    };
                    match next {
                        Some(m) => m,
                        None => return Turn::Last(gated),
                    }
                }
            },
        };
        match (session, deadline.saturating_duration_since(Instant::now())) {
            (None, _) => turns.dropped(
                &message.id,
                "the node's stream never named a harness session, so there is none to relaunch it \
                 under and no next turn can carry this message",
            ),
            (Some(_), left) if left.is_zero() => turns.dropped(
                &message.id,
                "the node's wall clock is spent, so no next turn can carry this message",
            ),
            (Some(s), _) => {
                return Turn::Next {
                    message,
                    session: s.to_string(),
                };
            }
        }
    }
}

/// A gate that cannot be woken has settled.
fn settled(w: Waited<Message>) -> Gated {
    match w {
        Waited::Settled(g) => g,
        // Unreachable: the wait handed to it never returns a message.
        Waited::Woken(_) => Gated::default(),
    }
}

/// Take the next message or seal the inbox; while a message is still owed, wait for it on the
/// node's clock instead of sealing.
fn take_or_seal(turns: &Turns, deadline: Instant) -> Option<Message> {
    loop {
        if let Some(m) = turns.source.take_or_seal() {
            return Some(m);
        }
        if !turns.source.held() {
            return None;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        if let Some(m) = turns.wait_take(left.min(OWED_POLL)) {
            return Some(m);
        }
    }
}

/// **The node's outcome after one more generation.** The last generation's process facts (exit,
/// signal, timeout, the stream's failure claim) are the node's; the **report** is the last one any
/// generation made, since a steered turn that says nothing more has not withdrawn it. Paths the
/// harness announced accumulate, and each generation's stderr is kept in order.
pub(crate) fn fold(earlier: ChildOutcome, later: ChildOutcome) -> ChildOutcome {
    let reported = later.narrative.is_some();
    let mut file_change_paths = earlier.file_change_paths;
    for p in later.file_change_paths {
        if !file_change_paths.contains(&p) {
            file_change_paths.push(p);
        }
    }
    ChildOutcome {
        narrative: if reported {
            later.narrative
        } else {
            earlier.narrative
        },
        result_commits: if reported {
            later.result_commits
        } else {
            earlier.result_commits
        },
        file_change_paths,
        failure: later.failure,
        exit_code: later.exit_code,
        signal: later.signal,
        timed_out: later.timed_out,
        stderr: match (earlier.stderr.is_empty(), later.stderr.is_empty()) {
            (_, true) => earlier.stderr,
            (true, false) => later.stderr,
            (false, false) => format!("{}\n{}", earlier.stderr, later.stderr),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::{BoundInbox, Inboxes, Source};
    use marion_core::contract::AgentId;
    use marion_core::journal::RecordKind;
    use marion_harness::spec::TurnDelivery;
    use std::sync::{Arc, Mutex};

    const CONT: TurnDelivery = TurnDelivery::Continuation { note: "t" };

    struct Fx {
        inboxes: Arc<Inboxes>,
        log: Arc<Mutex<Vec<RecordKind>>>,
        agent: AgentId,
    }

    fn fx() -> Fx {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l2 = Arc::clone(&log);
        let inboxes = Arc::new(Inboxes::new(Box::new(move |k| {
            l2.lock().unwrap().push(k);
            Ok(())
        })));
        let agent = AgentId("n".into());
        inboxes.open(&agent);
        Fx {
            inboxes,
            log,
            agent,
        }
    }

    impl Fx {
        fn steer(&self, text: &str) -> String {
            self.inboxes
                .enqueue(&self.agent, CONT, Source::Operator, text.into())
                .unwrap()
        }
        fn turns(&self) -> Turns {
            Turns::attach(Arc::new(BoundInbox::new(
                Arc::clone(&self.inboxes),
                self.agent.clone(),
            )))
        }
        fn dropped(&self) -> Vec<(String, String)> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .filter_map(|k| match k {
                    RecordKind::MessageDropped(d) => Some((d.message_id.clone(), d.reason.clone())),
                    _ => None,
                })
                .collect()
        }
    }

    fn stopped() -> ChildOutcome {
        ChildOutcome {
            exit_code: Some(0),
            ..ChildOutcome::default()
        }
    }

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// A gate that admits at once, counting how often it ran.
    fn admitting(
        runs: &mut usize,
    ) -> impl FnMut(&ChildOutcome, &mut dyn FnMut(Duration) -> Option<Message>) -> Waited<Message> + '_
    {
        move |_, _| {
            *runs += 1;
            Waited::Settled(Gated::default())
        }
    }

    /// **A message queued while the process ran is its next turn**, taken before any gate, under
    /// the session its stream named.
    #[test]
    fn a_message_waiting_at_the_stop_is_the_next_turn_under_the_observed_session() {
        let fx = fx();
        let id = fx.steer("use the v2 API");
        let turns = fx.turns();
        let mut runs = 0;
        let t = boundary(
            Some(&turns),
            &stopped(),
            Some("thread-1"),
            later(),
            &mut admitting(&mut runs),
        );
        let Turn::Next { message, session } = t else {
            panic!("a waiting message is a next turn: {t:?}")
        };
        assert_eq!((message.id, session.as_str()), (id, "thread-1"));
        assert_eq!(runs, 0, "a waiting message needs no gate");
    }

    /// **No session, no continuation**: the message is dropped by name, and the boundary goes on
    /// to seal the node's last turn.
    #[test]
    fn with_no_observed_session_a_message_is_dropped_with_the_reason() {
        let fx = fx();
        let id = fx.steer("x");
        let turns = fx.turns();
        let mut runs = 0;
        let t = boundary(
            Some(&turns),
            &stopped(),
            None,
            later(),
            &mut admitting(&mut runs),
        );
        assert!(matches!(t, Turn::Last(_)), "{t:?}");
        let dropped = fx.dropped();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].0, id);
        assert!(dropped[0].1.contains("session"), "{}", dropped[0].1);
        assert!(
            fx.inboxes
                .enqueue(&fx.agent, CONT, Source::Operator, "late".into())
                .is_err(),
            "the last turn sealed the inbox"
        );
    }

    /// A spent clock drops the message too: a continuation would be a turn outside the bound.
    #[test]
    fn with_the_clock_spent_a_message_is_dropped_with_the_reason() {
        let fx = fx();
        fx.steer("x");
        let turns = fx.turns();
        let mut runs = 0;
        let t = boundary(
            Some(&turns),
            &stopped(),
            Some("s"),
            Instant::now(),
            &mut admitting(&mut runs),
        );
        assert!(matches!(t, Turn::Last(_)));
        assert!(
            fx.dropped()[0].1.contains("wall clock"),
            "{:?}",
            fx.dropped()
        );
    }

    /// **A death takes no next turn** — not even one already waiting, which the node's close drops.
    #[test]
    fn a_process_that_died_takes_no_further_turn() {
        let fx = fx();
        fx.steer("x");
        let turns = fx.turns();
        let mut runs = 0;
        let died = ChildOutcome {
            signal: Some(9),
            ..ChildOutcome::default()
        };
        let t = boundary(
            Some(&turns),
            &died,
            Some("s"),
            later(),
            &mut admitting(&mut runs),
        );
        assert!(matches!(t, Turn::Last(_)));
        assert_eq!(runs, 1, "the gate still records the death");
        assert_eq!(
            fx.inboxes.queued(&fx.agent),
            1,
            "left for the close to drop by name"
        );
    }

    /// **The gate's hold waits on the inbox**: a steer queued while the node is held wakes the
    /// wait the gate was handed, and is the next turn.
    #[test]
    fn a_message_during_the_gate_s_hold_is_the_next_turn() {
        let fx = fx();
        let turns = fx.turns();
        let inboxes = Arc::clone(&fx.inboxes);
        let agent = fx.agent.clone();
        let mut gate = |_: &ChildOutcome, wait: &mut dyn FnMut(Duration) -> Option<Message>| {
            let (inboxes, agent) = (Arc::clone(&inboxes), agent.clone());
            let sender = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                inboxes.enqueue(&agent, CONT, Source::Operator, "during the hold".into())
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if let Some(m) = wait(Duration::from_millis(100)) {
                    sender.join().unwrap().unwrap();
                    return Waited::Woken(m);
                }
            }
            panic!("the steer never woke the hold");
        };
        let t = boundary(Some(&turns), &stopped(), Some("s"), later(), &mut gate);
        let Turn::Next { message, .. } = t else {
            panic!("{t:?}")
        };
        assert_eq!(message.text, "during the hold");
    }

    /// **An inbox held open for an owed message is waited on, not sealed**: the driver waits for
    /// the message, then takes it.
    #[test]
    fn an_owed_message_is_waited_for_rather_than_sealed_past() {
        let fx = fx();
        assert!(
            fx.inboxes.owe(&fx.agent),
            "a background child's end is owed"
        );
        let turns = fx.turns();
        let (inboxes, agent) = (Arc::clone(&fx.inboxes), fx.agent.clone());
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            inboxes.announce(&agent, CONT, Source::Operator, "owed".into())
        });
        let mut runs = 0;
        let t = boundary(
            Some(&turns),
            &stopped(),
            Some("s"),
            later(),
            &mut admitting(&mut runs),
        );
        let id = sender.join().unwrap().unwrap();
        let Turn::Next { message, .. } = t else {
            panic!("{t:?}")
        };
        assert_eq!(message.id, id);
    }

    /// **An owed message is waited for only on the node's clock**: with the clock spent the hold
    /// ends and the node takes its last turn, its debt still open for the close to settle.
    #[test]
    fn an_owed_message_is_not_waited_for_past_the_wall_clock() {
        let fx = fx();
        fx.inboxes.owe(&fx.agent);
        let turns = fx.turns();
        let mut runs = 0;
        let started = Instant::now();
        let t = boundary(
            Some(&turns),
            &stopped(),
            Some("s"),
            Instant::now() + Duration::from_millis(150),
            &mut admitting(&mut runs),
        );
        assert!(matches!(t, Turn::Last(_)), "{t:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// **A node that reported early is not held for what it is owed**: §7.6 accepts the report at
    /// once, so the boundary ends the node without waiting on the clock.
    #[test]
    fn a_node_that_reported_early_is_not_held_for_an_owed_message() {
        let fx = fx();
        fx.inboxes.owe(&fx.agent);
        let turns = fx.turns();
        let mut early = |_: &ChildOutcome, _: &mut dyn FnMut(Duration) -> Option<Message>| {
            Waited::Settled(Gated {
                reported_early: true,
                ..Gated::default()
            })
        };
        let started = Instant::now();
        let t = boundary(Some(&turns), &stopped(), Some("s"), later(), &mut early);
        let Turn::Last(gated) = t else {
            panic!("{t:?}")
        };
        assert!(gated.reported_early);
        assert!(started.elapsed() < Duration::from_secs(5), "not held");
    }

    /// Nothing waiting and nothing owed: the last turn, sealed.
    #[test]
    fn an_empty_unheld_inbox_is_the_last_turn() {
        let fx = fx();
        let turns = fx.turns();
        let mut runs = 0;
        let t = boundary(
            Some(&turns),
            &stopped(),
            Some("s"),
            later(),
            &mut admitting(&mut runs),
        );
        assert!(matches!(t, Turn::Last(_)));
        assert!(
            fx.inboxes
                .enqueue(&fx.agent, CONT, Source::Operator, "late".into())
                .is_err()
        );
    }

    /// **The report is the last one any generation made**; the process facts are the last
    /// generation's.
    #[test]
    fn the_folded_outcome_keeps_the_last_report_and_the_last_exit() {
        let reported = ChildOutcome {
            narrative: Some("first".into()),
            exit_code: Some(0),
            stderr: "a".into(),
            ..ChildOutcome::default()
        };
        let quiet = ChildOutcome {
            exit_code: Some(0),
            timed_out: true,
            stderr: "b".into(),
            ..ChildOutcome::default()
        };
        let f = fold(reported, quiet);
        assert_eq!(f.narrative.as_deref(), Some("first"));
        assert!(f.timed_out);
        assert_eq!(f.stderr, "a\nb");
        let first = ChildOutcome {
            narrative: Some("first".into()),
            ..ChildOutcome::default()
        };
        let again = ChildOutcome {
            narrative: Some("second".into()),
            ..ChildOutcome::default()
        };
        assert_eq!(fold(first, again).narrative.as_deref(), Some("second"));
    }
}
