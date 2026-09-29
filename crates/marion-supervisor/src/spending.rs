//! **What each running node has spent so far**, published by its event sink as it meters the node's
//! frames — the one path from a usage frame to a figure.
//!
//! A node's stream passes through its [`EventSink`](crate::events::EventSink), which folds each
//! usage unit into the node's [`UsageMeter`](marion_harness::grammar::UsageMeter) as the frame is
//! recorded, and hands the run's figure here the moment it moves: no file is re-read and nothing
//! waits on a timer. When the run ends the same meter's figure is the contract's and the journal's
//! `UsageRecorded`, and from then on the journal is the only source — [`shown`] prefers it for an
//! ended node, so this map holds only runs in progress.
//!
//! A node driven by another process (a `marion run` outside any supervisor) publishes nothing here;
//! its row shows its figure once its record is on the journal. No harness is named here.

use marion_core::contract::{AgentId, TokenUsage, add_usage_claims};
use marion_core::journal::MAX_RECORDED_TURNS;
use marion_core::registry::{Replay, ReplayedNode};
use std::collections::HashMap;
use std::sync::Mutex;

/// What a node spent, as a view shows it: the total, and each turn's spend (the sparkline).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spent {
    /// `None` when nothing claimed any spend — which is not zero.
    pub usage: Option<TokenUsage>,
    /// Oldest first, the latest [`MAX_RECORDED_TURNS`].
    pub turns: Vec<u64>,
}

impl Spent {
    /// What the journal recorded for `node`: every run that has ended, added.
    pub fn recorded(node: &ReplayedNode) -> Spent {
        Spent {
            usage: node.usage,
            turns: node.turns.clone(),
        }
    }
}

/// What a view shows a node as having spent: every run the journal recorded, and while the node
/// runs, the run in progress on top of them — its earlier runs (a resume) are recorded, the
/// current one is not yet. An ended node's figure is the journal's alone.
pub fn shown(exited: bool, recorded: &Spent, live: Option<&Spent>) -> Spent {
    match live.filter(|_| !exited) {
        None => recorded.clone(),
        Some(live) => {
            let mut turns: Vec<u64> = recorded.turns.iter().chain(&live.turns).copied().collect();
            turns.drain(..turns.len().saturating_sub(MAX_RECORDED_TURNS));
            Spent {
                usage: add_usage_claims(recorded.usage, live.usage),
                turns,
            }
        }
    }
}

/// Every run in progress's figure, keyed by agent id.
#[derive(Debug, Default)]
pub struct Spending {
    runs: Mutex<HashMap<AgentId, Spent>>,
    /// Bumped whenever a published figure changes or a run is forgotten: the tree's "anything new
    /// to tell?" asks this beside the registry's generation, since a figure moves without a
    /// journal record (`RegistryHandle::flush`).
    version: std::sync::atomic::AtomicU64,
    /// Notified by the first figure change since the last [`Self::announced`], so the loop that
    /// tells subscribers wakes for it — once per telling, not once per frame. `None`: nobody to
    /// wake (a figure read only on request).
    wake: Option<std::sync::Arc<crate::wake::Signal>>,
    unannounced: std::sync::atomic::AtomicBool,
    /// Where each moved figure is checked against the budgets ([`crate::budget`]), and where the
    /// lines it crosses are sent to be acted on. `None` for a view that enforces nothing.
    budgets: Option<(
        std::sync::Arc<crate::budget::BudgetBook>,
        std::sync::mpsc::Sender<crate::budget::Crossing>,
    )>,
}

impl Spending {
    /// A map whose changes notify `wake`: the supervisor's, whose accept loop pushes figures to
    /// `tree/subscribe`rs and otherwise sleeps.
    pub fn notifying(wake: std::sync::Arc<crate::wake::Signal>) -> Spending {
        Spending {
            wake: Some(wake),
            ..Spending::default()
        }
    }

    /// This map, with its moved figures also checked against `book` and each line crossed sent to
    /// `crossed`.
    pub fn enforcing(
        self,
        book: std::sync::Arc<crate::budget::BudgetBook>,
        crossed: std::sync::mpsc::Sender<crate::budget::Crossing>,
    ) -> Self {
        Spending {
            budgets: Some((book, crossed)),
            ..self
        }
    }

    /// The run in progress of `id` now stands at `spent`. Called by the node's sink, on its reader
    /// thread, each time a frame moves the figure: a short lock, no I/O.
    pub fn publish(&self, id: &AgentId, spent: Spent) {
        use std::sync::atomic::Ordering::SeqCst;
        let total = spent.usage.map_or(0, |u| u.total());
        if self.lock().insert(id.clone(), spent.clone()).as_ref() != Some(&spent) {
            self.version.fetch_add(1, SeqCst);
            if !self.unannounced.swap(true, SeqCst)
                && let Some(wake) = &self.wake
            {
                wake.notify();
            }
            if let Some((book, crossed)) = &self.budgets {
                for c in book.observe(id, total) {
                    // Acted on off this reader thread; a dropped receiver is a supervisor exiting.
                    let _ = crossed.send(c);
                }
            }
        }
    }

    /// The teller is about to read [`Self::version`] and tell what it finds: the next change after
    /// this notifies again. Call **before** reading the version, so a change in between is not
    /// lost.
    pub fn announced(&self) {
        self.unannounced
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many times a figure has changed — a change counter, not a count of anything shown.
    pub fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The run in progress of `id`, if one has published.
    pub fn get(&self, id: &AgentId) -> Option<Spent> {
        self.lock().get(id).cloned()
    }

    /// [`shown`]'s total for `node`, without copying any turns: what a row carries.
    pub fn shown_total(&self, node: &ReplayedNode) -> Option<u64> {
        let exited = node.state.is_exited();
        let live = if exited {
            None
        } else {
            self.lock().get(&node.agent_id).and_then(|s| s.usage)
        };
        add_usage_claims(node.usage, live).map(|u| u.total())
    }

    /// Drop the runs whose node has ended on `tree`: the journal holds their figure now. A run the
    /// tree does not know yet (its intent not yet followed) is kept.
    pub fn forget_ended(&self, tree: &Replay) {
        let mut runs = self.lock();
        if !runs.is_empty() {
            let before = runs.len();
            runs.retain(|id, _| tree.get(id).is_none_or(|n| !n.state.is_exited()));
            if runs.len() != before {
                self.version
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Spent>> {
        self.runs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A figure that moves is checked against the budgets, and only a move is**: the line it
    /// crosses is sent once, and publishing the same figure again sends nothing.
    #[test]
    fn a_published_figure_that_crosses_a_budget_line_sends_the_crossing() {
        let book = std::sync::Arc::new(crate::budget::BudgetBook::default());
        let id = AgentId("n".into());
        book.register(
            &id,
            None,
            Some(marion_core::budget::Budget {
                tokens: Some(100),
                ..Default::default()
            }),
            0,
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let s = Spending::default().enforcing(book, tx);
        let spent = |n| Spent {
            usage: Some(tokens(n)),
            turns: vec![],
        };
        s.publish(&id, spent(50));
        assert!(rx.try_recv().is_err());
        s.publish(&id, spent(120));
        s.publish(&id, spent(120));
        let c = rx.try_recv().expect("the limit crossed");
        assert_eq!(
            (c.level, c.spent),
            (marion_core::budget::BudgetLevel::Stop, 120)
        );
        assert!(rx.try_recv().is_err(), "once");
    }

    fn tokens(input: u64) -> TokenUsage {
        TokenUsage {
            input,
            ..TokenUsage::default()
        }
    }

    /// **A running node shows its recorded runs plus the run in progress; an ended one shows the
    /// journal's figure alone**, whatever a stale live entry says. Turns follow one another in a
    /// window as bounded as the record's, and two silences stay silent.
    #[test]
    fn a_view_adds_the_run_in_progress_to_the_recorded_runs_only_while_the_node_runs() {
        let recorded = Spent {
            usage: Some(tokens(100)),
            turns: vec![100],
        };
        let live = Spent {
            usage: Some(tokens(7)),
            turns: vec![3, 4],
        };
        let running = shown(false, &recorded, Some(&live));
        assert_eq!(running.usage, Some(tokens(107)));
        assert_eq!(running.turns, vec![100, 3, 4]);
        assert_eq!(shown(true, &recorded, Some(&live)), recorded);
        assert_eq!(shown(false, &recorded, None), recorded);
        assert_eq!(shown(false, &Spent::default(), None), Spent::default());

        let long = Spent {
            usage: None,
            turns: vec![1; MAX_RECORDED_TURNS],
        };
        assert_eq!(
            shown(false, &recorded, Some(&long)).turns.len(),
            MAX_RECORDED_TURNS
        );
    }
}
