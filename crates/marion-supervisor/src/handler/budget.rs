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
        parent: Option<&AgentId>,
    ) -> Option<marion_core::budget::Budget> {
        let typed = match parent {
            // The table the parent's tree started with, as the spawn itself resolves the type —
            // never the repository's live file, which a node can edit (see
            // `crate::types_snapshot`).
            Some(parent) => self.spawn_env.as_ref().and_then(|env| {
                let parent_type = self.live.read(|r| {
                    r.tree()
                        .get(parent)
                        .and_then(|n| n.intent.as_ref())
                        .map(|i| i.agent_type.clone())
                })?;
                crate::types_snapshot::for_caller(&env.project_dir, parent, repo, &parent_type)
                    .and_then(|s| s.resolve(agent_type))
                    .ok()
                    .flatten()
                    .and_then(|t| t.budget)
            }),
            // The operator's own node reads the operator's own checkout, as a root does.
            None => crate::run::agent_types(repo)
                .ok()
                .and_then(|t| t.resolve(agent_type))
                .and_then(|t| t.budget),
        };
        // The operator's own node has no tree above it to clamp to.
        let above = parent.and_then(|p| self.budgets.remaining_tree(p));
        marion_core::budget::resolve(typed, asked, above)
    }

    /// **What is left of the nearest wall clock at or above `agent`**: each node's recorded bound
    /// from its latest launch, less the time since — a child's always, a root's where its path has
    /// one ([`crate::root::wall_clocked`]) — the least of them. `None` where none on the way up
    /// has a clock.
    pub(super) fn remaining_wall_secs(&self, agent: &AgentId) -> Option<u64> {
        self.live.refresh();
        let chain = self.live.read(|r| {
            let mut chain = Vec::new();
            let mut at = Some(agent.clone());
            while let Some(id) = at {
                let Some(n) = r.tree().get(&id) else { break };
                let clock = n
                    .intent
                    .as_ref()
                    .and_then(|i| i.timeout_secs)
                    .zip(n.spawned_ts);
                chain.push((id.clone(), n.parent_id().cloned(), n.harness(), clock));
                at = n
                    .parent_id()
                    .cloned()
                    .filter(|p| p != &id && chain.len() < 64);
            }
            chain
        });
        chain
            .into_iter()
            .filter_map(|(id, parent, harness, clock)| {
                let (bound, started) = clock?;
                let timed = parent.is_some()
                    || harness.is_some_and(|h| {
                        crate::root::wall_clocked(h, super::lock(&self.panes).has_live(&id))
                    });
                let elapsed = started.0.elapsed().unwrap_or_default().as_secs();
                timed.then(|| bound.saturating_sub(elapsed))
            })
            .min()
    }

    /// **A child's wall clock, never longer than what is left above it**: `asked`, clamped to
    /// [`Self::remaining_wall_secs`] of `parent`. Refused where nothing is left — a child that
    /// could only be killed at once.
    pub(super) fn child_wall_secs(
        &self,
        asked: u64,
        parent: &AgentId,
    ) -> Result<u64, marion_core::proto::RpcError> {
        match self.remaining_wall_secs(parent) {
            Some(0) => Err(marion_core::proto::RpcError::refused(
                "timeout_secs",
                "the calling node's wall clock — or its root's — is spent, so a child of it could \
                 only be killed as it started. Nothing was spawned.",
                "§3.1",
            )),
            Some(left) => Ok(asked.min(left)),
            None => Ok(asked),
        }
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
    use marion_core::journal::RecordKind;

    /// A node journaled as launched just now, under `parent`, on `harness`, with `timeout_secs`.
    fn launched(
        path: &std::path::Path,
        agent: &str,
        parent: Option<&str>,
        harness: marion_core::harness::Harness,
        timeout_secs: u64,
    ) {
        use marion_core::journal::{SpawnIntent, Spawned};
        crate::journal::append_at(
            path,
            RecordKind::SpawnIntent(SpawnIntent {
                agent_id: AgentId(agent.into()),
                parent_id: parent.map(|p| AgentId(p.into())),
                agent_type: "t".into(),
                harness,
                depth: u32::from(parent.is_some()),
                task_id: None,
                timeout_secs: Some(timeout_secs),
                verification: vec![],
                review_of: None,
                budget: None,
                race: None,
                workflow: None,
            }),
        )
        .unwrap();
        crate::journal::append_at(
            path,
            RecordKind::Spawned(Spawned {
                agent_id: AgentId(agent.into()),
                harness_version: "test".into(),
                model: None,
                pid: None,
                start_id: None,
                provider: None,
                route: None,
                credential: None,
            }),
        )
        .unwrap();
    }

    /// **A child never outlives the clock above it**: its bound is clamped to what its parent has
    /// left, a spent parent refuses it, and a root whose path has no wall clock (claude's duplex
    /// root, §9's `Blocked`-only budget) clamps nothing while one that has (codex's launch-only
    /// root) does.
    #[test]
    fn a_childs_wall_clock_is_clamped_to_what_is_left_above_it() {
        use marion_core::harness::Harness;
        let dir = marion_testsupport::scratch("budget-wall");
        let path = dir.join("journal.jsonl");
        let live = Arc::new(LiveRegistry::follow(Registry::boot_path(&path).unwrap()));
        launched(&path, "duplex-root", None, Harness::ClaudeCode, 0);
        launched(&path, "child", Some("duplex-root"), Harness::Codex, 100);
        launched(&path, "spent", Some("duplex-root"), Harness::Codex, 0);
        launched(&path, "timed-root", None, Harness::Codex, 50);
        let h = RegistryHandle::new(live);
        let id = |s: &str| AgentId(s.into());

        assert_eq!(h.child_wall_secs(900, &id("duplex-root")).unwrap(), 900);
        let clamped = h.child_wall_secs(900, &id("child")).unwrap();
        assert!((98..=100).contains(&clamped), "{clamped}");
        assert_eq!(h.child_wall_secs(30, &id("child")).unwrap(), 30);
        assert!(h.child_wall_secs(900, &id("spent")).is_err());
        let under_root = h.child_wall_secs(900, &id("timed-root")).unwrap();
        assert!((48..=50).contains(&under_root), "{under_root}");
    }

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
        let b = h
            .child_budget(&dir, "codex", Some(5_000), Some(&root))
            .unwrap();
        assert_eq!(b.tree_tokens, Some(300), "5000 asked, 300 left above");
        assert_eq!(
            h.child_budget(&dir, "codex", None, Some(&root))
                .unwrap()
                .tree_tokens,
            Some(300),
            "an unasked child inherits the remainder"
        );
        assert_eq!(
            h.child_budget(&dir, "codex", None, Some(&AgentId("elsewhere".into()))),
            None
        );
    }
}
