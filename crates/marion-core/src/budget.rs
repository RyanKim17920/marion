//! **Token budgets**: how many tokens a node may spend alone, and how many it and everything below
//! it may spend together — resolved once at spawn, journaled on the node's intent, and enforced by
//! the supervisor as the node's stream reports its spend.
//!
//! Tokens only. A dollar figure would need a price table marion would have to keep true, and a
//! wall-clock budget is the node's own `timeout`, which a child's is clamped under (a child never
//! outlives what is left of its parent's clock).
//!
//! Crossing the warn line tells the owner once; crossing the limit cancels the owner and its
//! subtree the way `node/cancel` does, keeping the work each committed. The overshoot is at most
//! one response per running node plus its grace: a stream reports spend after it is spent.

use serde::{Deserialize, Serialize};

/// The warn line when a budget states none: 80% of the limit.
pub const DEFAULT_WARN_PCT: u8 = 80;

fn default_warn_pct() -> u8 {
    DEFAULT_WARN_PCT
}

/// A node's budget. `None` on either limit is no limit on that scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Tokens this node alone may spend, across every run of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    /// Tokens this node and every node below it may spend together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_tokens: Option<u64>,
    /// The percentage of a limit at which the owner is warned, once; 100 or more warns never.
    #[serde(default = "default_warn_pct")]
    pub warn_pct: u8,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            tokens: None,
            tree_tokens: None,
            warn_pct: DEFAULT_WARN_PCT,
        }
    }
}

impl Budget {
    /// No limit on either scope.
    pub fn is_unlimited(&self) -> bool {
        self.tokens.is_none() && self.tree_tokens.is_none()
    }

    /// The limit on `scope`, if any.
    pub fn limit(&self, scope: BudgetScope) -> Option<u64> {
        match scope {
            BudgetScope::Node => self.tokens,
            BudgetScope::Tree => self.tree_tokens,
        }
    }

    /// This budget with its tree limit narrowed to `remaining` — what the nearest ancestor's tree
    /// budget has left. Narrows only: a child cannot be given more than its ancestors have.
    pub fn clamped_to(self, remaining: Option<u64>) -> Budget {
        let tree_tokens = match (self.tree_tokens, remaining) {
            (Some(own), Some(left)) => Some(own.min(left)),
            (own, left) => own.or(left),
        };
        Budget {
            tree_tokens,
            ..self
        }
    }
}

/// Which of a budget's two limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BudgetScope {
    /// The node's own spend.
    Node,
    /// The node's and its descendants' spend together.
    Tree,
}

impl BudgetScope {
    pub fn word(self) -> &'static str {
        match self {
            BudgetScope::Node => "token",
            BudgetScope::Tree => "tree token",
        }
    }
}

/// How far past a line a spend has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BudgetLevel {
    /// Past the warn line: the owner is told once.
    Warn,
    /// At or past the limit: the owner and its subtree are cancelled.
    Stop,
}

/// The highest level a spend moving from `before` to `after` crosses under `limit`, or `None`
/// where it crosses none. A spend that jumps straight past the limit is a `Stop`, never also a
/// `Warn`.
pub fn crossed(limit: u64, warn_pct: u8, before: u64, after: u64) -> Option<BudgetLevel> {
    let warn_at = limit.saturating_mul(u64::from(warn_pct)) / 100;
    if after >= limit && before < limit {
        Some(BudgetLevel::Stop)
    } else if warn_pct < 100 && after >= warn_at && before < warn_at && after < limit {
        Some(BudgetLevel::Warn)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spend_crosses_each_line_once_and_a_jump_past_the_limit_is_only_a_stop() {
        assert_eq!(crossed(100, 80, 0, 79), None);
        assert_eq!(crossed(100, 80, 0, 80), Some(BudgetLevel::Warn));
        assert_eq!(crossed(100, 80, 80, 90), None, "already warned");
        assert_eq!(crossed(100, 80, 90, 100), Some(BudgetLevel::Stop));
        assert_eq!(crossed(100, 80, 100, 150), None, "already stopped");
        assert_eq!(crossed(100, 80, 10, 500), Some(BudgetLevel::Stop));
        assert_eq!(
            crossed(100, 100, 0, 99),
            None,
            "a warn line at 100% never warns"
        );
    }

    #[test]
    fn a_tree_limit_is_only_ever_narrowed_by_what_an_ancestor_has_left() {
        let b = Budget {
            tree_tokens: Some(1_000),
            ..Budget::default()
        };
        assert_eq!(b.clamped_to(Some(400)).tree_tokens, Some(400));
        assert_eq!(b.clamped_to(Some(5_000)).tree_tokens, Some(1_000));
        assert_eq!(b.clamped_to(None).tree_tokens, Some(1_000));
        assert_eq!(
            Budget::default().clamped_to(Some(300)).tree_tokens,
            Some(300),
            "an unlimited child inherits what its ancestor has left"
        );
    }

    #[test]
    fn a_budget_written_without_a_warn_line_warns_at_eighty_percent() {
        let b: Budget = serde_json::from_str(r#"{"tokens":10}"#).unwrap();
        assert_eq!(b.warn_pct, DEFAULT_WARN_PCT);
        assert!(serde_json::from_str::<Budget>(r#"{"usd":1}"#).is_err());
    }
}
