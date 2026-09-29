//! **Token budgets, acted on** ([`crate::budget`]): each node is registered with the budget its
//! intent recorded when its process starts, and each line its spend crosses is journaled and
//! acted on — the warn line tells the owner once, the limit cancels the owner and everything below
//! it the way `node/cancel` does, keeping the work each committed.
//!
//! The crossings arrive on one enforcer thread, blocked on its channel between them: no timer, and
//! nothing at all for a supervisor whose nodes have no budget.

use std::sync::Weak;
use std::sync::mpsc::Receiver;

use marion_core::budget::BudgetLevel;
use marion_core::contract::AgentId;
use marion_core::journal::{BudgetCrossed, CancelBy, RecordKind};

use super::RegistryHandle;
use super::steer::{AnnouncementRoute, announcement_route};
use crate::budget::Crossing;
use crate::inbox::Source;

impl RegistryHandle {
    /// The enforcer: acts on each crossing in the order they were observed, for as long as the
    /// handle lives. A cancel it starts waits out its graces here, so a second crossing waits
    /// behind it rather than racing it.
    pub(super) fn enforce_budgets(me: Weak<RegistryHandle>, crossed: Receiver<Crossing>) {
        let spawned = std::thread::Builder::new()
            .name("marion-budget".into())
            .spawn(move || {
                for c in crossed {
                    let Some(handle) = me.upgrade() else { break };
                    handle.budget_crossed(c);
                }
            });
        if let Err(e) = spawned {
            eprintln!("marion: no thread to enforce token budgets, so none will be: {e}");
        }
    }

    /// `agent_id`'s process started: count its spend against the budget its intent recorded, under
    /// its parent, on top of what the journal recorded for its earlier runs.
    pub(super) fn register_budget(&self, agent_id: &AgentId) {
        self.live.refresh();
        let facts = self.live.read(|r| {
            r.tree().get(agent_id).map(|n| {
                (
                    n.parent_id().cloned(),
                    n.intent.as_ref().and_then(|i| i.budget),
                    n.usage.map_or(0, |u| u.total()),
                )
            })
        });
        if let Some((parent, budget, recorded)) = facts {
            self.budgets
                .register(agent_id, parent.as_ref(), budget, recorded);
        }
    }

    /// **A new child's budget**: its type's, with the tree limit its spawn asked for, narrowed to
    /// what the nearest tree budget at or above `parent` has left. An agent type the tree cannot
    /// resolve has no budget here; the spawn itself refuses it.
    pub(super) fn child_budget(
        &self,
        repo: &std::path::Path,
        agent_type: &str,
        asked: Option<u64>,
        parent: &AgentId,
    ) -> Option<marion_core::budget::Budget> {
        let typed = crate::run::agent_types(repo)
            .ok()
            .and_then(|t| t.resolve(agent_type))
            .and_then(|t| t.budget);
        marion_core::budget::resolve(typed, asked, self.budgets.remaining_tree(parent))
    }

    /// One line crossed: journal it, then warn the owner or cancel its subtree.
    pub(super) fn budget_crossed(&self, c: Crossing) {
        let record = RecordKind::BudgetCrossed(BudgetCrossed {
            agent_id: c.owner.clone(),
            scope: c.scope,
            level: c.level,
            spent: c.spent,
            limit: c.limit,
        });
        if let Err(e) = self.journal_append(record) {
            eprintln!(
                "marion: `{}` crossed its {} budget and the record was not journaled: {e}",
                c.owner.0,
                c.scope.word()
            );
        }
        match c.level {
            BudgetLevel::Warn => self.warn_of_budget(&c),
            BudgetLevel::Stop => {
                let by = CancelBy::Budget {
                    owner: c.owner.clone(),
                    scope: c.scope,
                    spent: c.spent,
                    limit: c.limit,
                };
                if let Err(e) = self.cancel_tree(&c.owner, by) {
                    eprintln!(
                        "marion: `{}` spent its {} budget ({} of {}) and could not be cancelled: {}",
                        c.owner.0,
                        c.scope.word(),
                        c.spent,
                        c.limit,
                        e.message
                    );
                }
            }
        }
    }

    /// The warn line: marion's words for the owner's next turn, where its row takes one from the
    /// inbox. A row that cannot take a turn is warned by the journal record alone.
    fn warn_of_budget(&self, c: &Crossing) {
        let Some(harness) = self
            .live
            .read(|r| r.tree().get(&c.owner).and_then(|n| n.harness()))
        else {
            return;
        };
        let delivery = self.delivery_of(&c.owner, harness);
        if announcement_route(delivery) != AnnouncementRoute::Inbox {
            return;
        }
        let text = crate::inbox::budget_warning_text(c.scope, c.spent, c.limit);
        let source = Source::BudgetWarning {
            scope: c.scope,
            spent: c.spent,
            limit: c.limit,
        };
        if let Err(r) = self.inboxes.enqueue(&c.owner, delivery, source, text) {
            eprintln!(
                "marion: `{}`'s budget warning was not queued: {}",
                c.owner.0,
                r.sentence()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use marion_core::budget::Budget;

    use super::*;
    use crate::registry::{LiveRegistry, Registry};

    /// **A child is given no more than its tree has left**: its spawn's tree limit is narrowed to
    /// the nearest budgeted ancestor's remainder, and a child of an unbudgeted tree that asks for
    /// nothing has no budget at all.
    #[test]
    fn a_childs_budget_is_narrowed_to_what_its_ancestors_tree_has_left() {
        let dir = marion_testsupport::scratch("budget-child");
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot_path(&dir.join("journal.jsonl")).unwrap(),
        ));
        let h = RegistryHandle::new(live);
        let root = AgentId("root".into());
        let tree = Budget {
            tree_tokens: Some(1_000),
            ..Budget::default()
        };
        h.budgets.register(&root, None, Some(tree), 0);
        h.budgets.observe(&root, 700);
        let b = h.child_budget(&dir, "codex", Some(5_000), &root).unwrap();
        assert_eq!(b.tree_tokens, Some(300), "5000 asked, 300 left above");
        assert_eq!(
            h.child_budget(&dir, "codex", None, &root)
                .unwrap()
                .tree_tokens,
            Some(300),
            "an unasked child inherits the remainder"
        );
        assert_eq!(
            h.child_budget(&dir, "codex", None, &AgentId("elsewhere".into())),
            None
        );
    }
}
