//! **Token budgets, enforced** ([`marion_core::budget`]): every node's spend, kept current as its
//! stream reports it, added to its own total and to every ancestor's tree total, and checked against
//! each budget on the way.
//!
//! Driven by [`crate::spending::Spending::publish`] — the moment a node's figure moves, never on a
//! timer — so an idle supervisor does no budget work at all. A node's figure is its current run's,
//! so each observation is folded in as the delta since the last one; a resumed node is registered
//! again with what the journal recorded for its earlier runs, and its new run counts from zero on
//! top of that.
//!
//! Each line of each budget is crossed at most once: the book remembers what it reported.
//! [`BudgetBook::observe`] returns the crossings for the supervisor to act on — journal them, warn
//! the owner, cancel its subtree — off the reader thread that observed them.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use marion_core::budget::{Budget, BudgetLevel, BudgetScope, crossed};
use marion_core::contract::AgentId;

/// One line of one node's budget, newly crossed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crossing {
    /// Whose budget: the node itself for [`BudgetScope::Node`], the ancestor (or the node) whose
    /// subtree spent it for [`BudgetScope::Tree`].
    pub owner: AgentId,
    pub scope: BudgetScope,
    pub level: BudgetLevel,
    pub spent: u64,
    pub limit: u64,
}

#[derive(Debug, Default)]
struct Entry {
    parent: Option<AgentId>,
    budget: Option<Budget>,
    /// Every token this node has spent: what the journal recorded when it was registered, plus its
    /// current run so far.
    own: u64,
    /// The current run's figure at the last observation.
    run_seen: u64,
    /// What the journal had recorded for the node at its last registration: a figure that grew
    /// means a run ended and a new one (a resume) counts from zero.
    recorded: u64,
    /// This node's and every descendant's spend.
    tree: u64,
    /// The lines already reported, so each is reported once.
    fired: HashSet<(BudgetScope, BudgetLevel)>,
}

/// Every registered node's spend and budget. Bounded by the nodes this supervisor knows.
#[derive(Debug, Default)]
pub struct BudgetBook {
    entries: Mutex<HashMap<AgentId, Entry>>,
}

impl BudgetBook {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<AgentId, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `id` is live: under `parent`, with `budget`, having already spent `recorded` in runs the
    /// journal holds. Registered again — each generation of a run, or a resume — it keeps what it
    /// counted; only a larger `recorded` (a run ended, its figure journaled) starts a new run.
    pub fn register(
        &self,
        id: &AgentId,
        parent: Option<&AgentId>,
        budget: Option<Budget>,
        recorded: u64,
    ) {
        let mut entries = self.lock();
        let entry = entries.entry(id.clone()).or_default();
        entry.parent = parent.cloned();
        entry.budget = budget.filter(|b| !b.is_unlimited());
        if recorded <= entry.recorded {
            return;
        }
        entry.recorded = recorded;
        entry.run_seen = 0;
        // What the journal adds to what this book already counted, for the node and every tree
        // above it.
        let added = recorded.saturating_sub(entry.own);
        entry.own += added;
        entry.tree = entry.tree.saturating_add(added);
        let mut up = parent.cloned();
        while let Some(a) = up {
            let Some(e) = entries.get_mut(&a) else { break };
            e.tree = e.tree.saturating_add(added);
            up = e.parent.clone();
        }
    }

    /// `id`'s current run now stands at `run_total` tokens: fold the change in and return every
    /// line it newly crossed, the node's own first, then each tree from the node upwards.
    pub fn observe(&self, id: &AgentId, run_total: u64) -> Vec<Crossing> {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(id) else {
            return Vec::new();
        };
        let delta = run_total.saturating_sub(entry.run_seen);
        entry.run_seen = run_total;
        if delta == 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let before = entry.own;
        entry.own = entry.own.saturating_add(delta);
        check(id, entry, BudgetScope::Node, before, entry.own, &mut out);
        let mut at = Some(id.clone());
        while let Some(a) = at {
            let Some(e) = entries.get_mut(&a) else { break };
            let before = e.tree;
            e.tree = e.tree.saturating_add(delta);
            let after = e.tree;
            check(&a, e, BudgetScope::Tree, before, after, &mut out);
            at = e.parent.clone();
        }
        out
    }

    /// What is left of the nearest tree budget at or above `id` — the most a new child of `id` may
    /// be given — or `None` where no node on the way up has a tree limit.
    pub fn remaining_tree(&self, id: &AgentId) -> Option<u64> {
        let entries = self.lock();
        let mut left: Option<u64> = None;
        let mut at = Some(id.clone());
        while let Some(a) = at {
            let Some(e) = entries.get(&a) else { break };
            if let Some(limit) = e.budget.and_then(|b| b.tree_tokens) {
                let here = limit.saturating_sub(e.tree);
                left = Some(left.map_or(here, |l| l.min(here)));
            }
            at = e.parent.clone();
        }
        left
    }
}

/// Report `scope`'s line on `entry` if a spend from `before` to `after` newly crossed it.
fn check(
    owner: &AgentId,
    entry: &mut Entry,
    scope: BudgetScope,
    before: u64,
    after: u64,
    out: &mut Vec<Crossing>,
) {
    let Some(budget) = entry.budget else { return };
    let Some(limit) = budget.limit(scope) else {
        return;
    };
    let Some(level) = crossed(limit, budget.warn_pct, before, after) else {
        return;
    };
    if entry.fired.insert((scope, level)) {
        out.push(Crossing {
            owner: owner.clone(),
            scope,
            level,
            spent: after,
            limit,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AgentId {
        AgentId(s.into())
    }

    fn tokens(node: Option<u64>, tree: Option<u64>) -> Option<Budget> {
        Some(Budget {
            tokens: node,
            tree_tokens: tree,
            ..Budget::default()
        })
    }

    #[test]
    fn a_nodes_own_budget_warns_once_and_stops_once() {
        let book = BudgetBook::default();
        book.register(&id("a"), None, tokens(Some(100), None), 0);
        assert!(book.observe(&id("a"), 50).is_empty());
        let warn = book.observe(&id("a"), 85);
        assert_eq!(warn.len(), 1);
        assert_eq!(
            (warn[0].scope, warn[0].level, warn[0].spent),
            (BudgetScope::Node, BudgetLevel::Warn, 85)
        );
        assert!(book.observe(&id("a"), 90).is_empty(), "warned once");
        let stop = book.observe(&id("a"), 120);
        assert_eq!(stop[0].level, BudgetLevel::Stop);
        assert!(book.observe(&id("a"), 200).is_empty(), "stopped once");
    }

    /// **A grandchild's spend trips its grandparent's tree budget**, and the crossing names the
    /// grandparent as its owner.
    #[test]
    fn a_grandchilds_spend_counts_against_every_ancestors_tree_budget() {
        let book = BudgetBook::default();
        book.register(&id("root"), None, tokens(None, Some(1_000)), 0);
        book.register(&id("child"), Some(&id("root")), None, 0);
        book.register(&id("grand"), Some(&id("child")), None, 0);
        book.observe(&id("child"), 600);
        let crossed = book.observe(&id("grand"), 500);
        assert_eq!(
            crossed,
            vec![Crossing {
                owner: id("root"),
                scope: BudgetScope::Tree,
                level: BudgetLevel::Stop,
                spent: 1_100,
                limit: 1_000,
            }]
        );
    }

    /// **A resumed node counts on from what the journal recorded**: its new run starts at zero and
    /// is added, never subtracted from the figure before it.
    #[test]
    fn a_run_that_starts_over_adds_to_what_the_node_had_spent() {
        let book = BudgetBook::default();
        book.register(&id("a"), None, tokens(Some(1_000), None), 0);
        book.observe(&id("a"), 700);
        book.register(&id("a"), None, tokens(Some(1_000), None), 700);
        let warn = book.observe(&id("a"), 100);
        assert_eq!(
            (warn[0].level, warn[0].spent),
            (BudgetLevel::Warn, 800),
            "700 before the resume and 100 after"
        );
        let stop = book.observe(&id("a"), 350);
        assert_eq!((stop[0].level, stop[0].spent), (BudgetLevel::Stop, 1_050));
    }

    #[test]
    fn what_a_child_may_be_given_is_the_least_any_ancestor_has_left() {
        let book = BudgetBook::default();
        book.register(&id("root"), None, tokens(None, Some(1_000)), 0);
        book.register(&id("child"), Some(&id("root")), tokens(None, Some(800)), 0);
        book.observe(&id("child"), 300);
        assert_eq!(book.remaining_tree(&id("child")), Some(500));
        book.register(&id("side"), Some(&id("root")), None, 0);
        book.observe(&id("side"), 400);
        assert_eq!(
            book.remaining_tree(&id("child")),
            Some(300),
            "the root's is less now"
        );
        assert_eq!(BudgetBook::default().remaining_tree(&id("x")), None);
    }
}
