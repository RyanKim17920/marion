//! **What a subtree adds up to**: the tokens its nodes claimed, the files they changed, how many
//! there are and how many still run, and the wall time from the first start to the last end.
//!
//! Pure arithmetic over facts a caller has already read — no journal, no file, no screen — so the
//! export's totals and a tree view's per-row subtree figures are one computation. Tokens only,
//! never a price (see `home::mod`).
//!
//! **No claim is not zero.** A node whose harness stated no usage adds nothing to [`Totals::tokens`]
//! and is not counted in [`Totals::claimed`], so a reader can tell "184k over 3 of 4 nodes" from
//! "184k over all 4". Changed files are summed across nodes, so a file two nodes both touched — a
//! child's work merged into its parent's — is counted twice: the figure is labelled a sum (`Σ`),
//! not a count of distinct files.

use std::collections::HashMap;

use marion_core::contract::AgentId;
use marion_core::encoding::SystemTime;
use marion_core::proto::NodeSummary;

/// The facts one node contributes, as its reader found them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Own {
    /// Every token its runs spent, where its harness said; `None` is no claim.
    pub tokens: Option<u64>,
    /// How many paths its landed work changed, where marion recorded it.
    pub changed: Option<u32>,
    /// Whether it has not reached a terminal state.
    pub live: bool,
    pub started: Option<SystemTime>,
    pub ended: Option<SystemTime>,
}

impl Own {
    /// What a view's summary of the node says: live while neither exited nor reaped, and no end
    /// while live.
    pub fn of_summary(n: &NodeSummary) -> Own {
        let live = !n.state.is_exited() && !n.reap_state.is_terminal_for_gating();
        Own {
            tokens: n.tokens,
            changed: n.changed,
            live,
            started: n.started_at,
            ended: if live { None } else { n.ended_at },
        }
    }
}

/// A subtree's figures: every node's [`Own`] added.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Totals {
    /// Tokens claimed, summed, saturating: the counters are foreign data.
    pub tokens: u64,
    /// How many nodes claimed any tokens.
    pub claimed: u32,
    /// Changed paths, summed across nodes (see the module doc for the double count).
    pub changed: u32,
    pub nodes: u32,
    pub live: u32,
    pub first_start: Option<SystemTime>,
    pub last_end: Option<SystemTime>,
}

impl Totals {
    /// One node's figures on their own.
    pub fn of(own: &Own) -> Totals {
        Totals {
            tokens: own.tokens.unwrap_or(0),
            claimed: u32::from(own.tokens.is_some()),
            changed: own.changed.unwrap_or(0),
            nodes: 1,
            live: u32::from(own.live),
            first_start: own.started,
            last_end: own.ended,
        }
    }

    /// `self` with `other` added: counts summed, the earlier start and the later end kept.
    pub fn add(&mut self, other: &Totals) {
        self.tokens = self.tokens.saturating_add(other.tokens);
        self.claimed = self.claimed.saturating_add(other.claimed);
        self.changed = self.changed.saturating_add(other.changed);
        self.nodes = self.nodes.saturating_add(other.nodes);
        self.live = self.live.saturating_add(other.live);
        self.first_start = earlier(self.first_start, other.first_start);
        self.last_end = later(self.last_end, other.last_end);
    }

    /// Every node of `owns` added.
    pub fn sum<'a>(owns: impl IntoIterator<Item = &'a Own>) -> Totals {
        let mut t = Totals::default();
        for own in owns {
            t.add(&Totals::of(own));
        }
        t
    }

    /// First start to last end, once nothing runs: while a node is live the subtree has no end
    /// yet, and the last recorded end would understate it.
    pub fn wall(&self) -> Option<std::time::Duration> {
        if self.live > 0 {
            return None;
        }
        self.last_end?.0.duration_since(self.first_start?.0).ok()
    }

    /// First start to last end, or to `now` while any node runs — what a live view shows.
    pub fn wall_to(&self, now: std::time::SystemTime) -> Option<std::time::Duration> {
        let start = self.first_start?.0;
        let end = if self.live > 0 { now } else { self.last_end?.0 };
        end.duration_since(start).ok()
    }
}

fn earlier(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.0 < a.0 { b } else { a }),
        (a, b) => a.or(b),
    }
}

fn later(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.0 > a.0 { b } else { a }),
        (a, b) => a.or(b),
    }
}

/// Every node's subtree totals.
#[derive(Debug, Clone, Default)]
pub struct Rollup {
    subtree: HashMap<AgentId, Totals>,
    children: HashMap<AgentId, usize>,
}

impl Rollup {
    /// One pass: each node's own figures, then each added to its parent's, deepest first. A node
    /// whose parent is not in `nodes` heads its own subtree; a parent chain that loops is cut
    /// where it would repeat.
    pub fn build(nodes: &[NodeSummary]) -> Rollup {
        let index: HashMap<&AgentId, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (&n.agent_id, i))
            .collect();
        let parent_of = |i: usize| -> Option<usize> {
            nodes[i]
                .parent_id
                .as_ref()
                .and_then(|p| index.get(p).copied())
        };
        // Depth by walking up, bounded by the node count so a loop cannot run for ever.
        let depth: Vec<usize> = (0..nodes.len())
            .map(|i| {
                let mut d = 0;
                let mut at = parent_of(i);
                while let Some(p) = at {
                    d += 1;
                    if d > nodes.len() {
                        break;
                    }
                    at = parent_of(p);
                }
                d
            })
            .collect();
        let mut order: Vec<usize> = (0..nodes.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(depth[i]));
        let mut totals: Vec<Totals> = nodes
            .iter()
            .map(|n| Totals::of(&Own::of_summary(n)))
            .collect();
        let mut children: HashMap<AgentId, usize> = HashMap::new();
        for &i in &order {
            if let Some(p) = parent_of(i).filter(|&p| depth[p] < depth[i]) {
                let t = totals[i];
                totals[p].add(&t);
                *children.entry(nodes[p].agent_id.clone()).or_default() += 1;
            }
        }
        Rollup {
            subtree: nodes
                .iter()
                .zip(totals)
                .map(|(n, t)| (n.agent_id.clone(), t))
                .collect(),
            children,
        }
    }

    /// `id`'s subtree, itself included.
    pub fn get(&self, id: &AgentId) -> Option<&Totals> {
        self.subtree.get(id)
    }

    /// Whether `id` has any child — a leaf's subtree is only itself, and says nothing new.
    pub fn has_children(&self, id: &AgentId) -> bool {
        self.children.get(id).is_some_and(|c| *c > 0)
    }

    /// The whole forest: every root's subtree, added.
    pub fn forest(&self, nodes: &[NodeSummary]) -> Totals {
        let ids: std::collections::HashSet<&AgentId> = nodes.iter().map(|n| &n.agent_id).collect();
        let mut all = Totals::default();
        for n in nodes
            .iter()
            .filter(|n| n.parent_id.as_ref().is_none_or(|p| !ids.contains(p)))
        {
            if let Some(t) = self.get(&n.agent_id) {
                all.add(t);
            }
        }
        all
    }
}

/// **A node's tree budget against its subtree's spend**: `(spent, limit, over the warn line)`,
/// where the node carries a tree limit and some node of its subtree reported a figure.
pub fn budget_of(node: &NodeSummary, totals: &Totals) -> Option<(u64, u64, bool)> {
    let budget = node.budget?;
    let limit = budget.tree_tokens?;
    let warn_at = limit.saturating_mul(u64::from(budget.warn_pct)) / 100;
    Some((totals.tokens, limit, totals.tokens >= warn_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(s: u64) -> Option<SystemTime> {
        Some(SystemTime::from_unix_millis(1_790_000_000_000 + s * 1000))
    }

    /// **A silent node adds nothing and is not counted as claiming**, the earliest start and the
    /// latest end bound the wall time, and changed files are summed.
    #[test]
    fn totals_sum_claims_and_bound_the_wall_time() {
        let owns = [
            Own {
                tokens: Some(1000),
                changed: Some(3),
                started: at(0),
                ended: at(300),
                ..Own::default()
            },
            Own {
                tokens: None,
                changed: None,
                started: at(10),
                ended: at(700),
                ..Own::default()
            },
            Own {
                tokens: Some(24),
                changed: Some(2),
                started: at(20),
                ended: at(100),
                ..Own::default()
            },
        ];
        let t = Totals::sum(&owns);
        assert_eq!(
            (t.tokens, t.claimed, t.changed, t.nodes, t.live),
            (1024, 2, 5, 3, 0)
        );
        assert_eq!((t.first_start, t.last_end), (at(0), at(700)));
        assert_eq!(t.wall(), Some(Duration::from_secs(700)));
    }

    /// While any node runs there is no wall time; an empty subtree is all zeros and no times; the
    /// token sum saturates rather than wrapping.
    #[test]
    fn a_live_subtree_has_no_wall_time_and_sums_saturate() {
        let live = Own {
            live: true,
            started: at(0),
            ..Own::default()
        };
        let done = Own {
            tokens: Some(u64::MAX),
            started: at(1),
            ended: at(2),
            ..Own::default()
        };
        let t = Totals::sum(&[live, done, done]);
        assert_eq!((t.live, t.wall()), (1, None));
        assert_eq!(t.tokens, u64::MAX);
        assert_eq!(Totals::sum(&[]), Totals::default());
        assert_eq!(Totals::default().wall(), None);
    }

    /// Adding is order-free: a subtree summed child-first or parent-first is the same figure, which
    /// is what lets a caller fold a subtree in whatever order it walks it.
    #[test]
    fn adding_is_order_free() {
        let a = Own {
            tokens: Some(5),
            changed: Some(1),
            started: at(9),
            ended: at(12),
            ..Own::default()
        };
        let b = Own {
            live: true,
            started: at(3),
            ..Own::default()
        };
        let c = Own {
            tokens: Some(7),
            ended: at(40),
            ..Own::default()
        };
        assert_eq!(Totals::sum(&[a, b, c]), Totals::sum(&[c, b, a]));
        let mut left = Totals::of(&a);
        left.add(&Totals::sum(&[b, c]));
        assert_eq!(left, Totals::sum(&[a, b, c]));
    }

    mod summaries {
        use super::*;
        use marion_core::contract::ExitStatus;
        use marion_core::harness::Harness;
        use marion_core::node::{NodeState, ReapState};

        fn node(id: &str, parent: Option<&str>, tokens: Option<u64>, changed: u32) -> NodeSummary {
            NodeSummary {
                agent_id: AgentId(id.into()),
                parent_id: parent.map(|p| AgentId(p.into())),
                name: None,
                agent_type: "codex".into(),
                harness: Harness::Codex,
                harness_version: None,
                depth: 0,
                state: NodeState::Running,
                reap_state: ReapState::Live,
                timeout: marion_core::encoding::Duration::from_secs(900),
                pane: false,
                started_at: None,
                ended_at: None,
                tokens,
                attention: None,
                review_of: None,
                review: None,
                cancel: None,
                changed: Some(changed),
                budget: None,
                endpoint: None,
                race: None,
                widened: vec![],
            }
        }

        fn at(s: u64) -> Option<SystemTime> {
            Some(SystemTime::from_unix_millis(s * 1000))
        }

        /// **A subtree is itself and every descendant**, however the list is ordered — a child listed
        /// before its parent is still added to it — and a forest adds only its roots' subtrees.
        #[test]
        fn a_subtree_adds_every_descendant_in_any_order() {
            let mut grand = node("grand", Some("child"), Some(100), 1);
            grand.state = NodeState::Exited(ExitStatus::Ok);
            grand.started_at = at(20);
            grand.ended_at = at(90);
            let mut root = node("root", None, Some(1_000), 2);
            root.started_at = at(10);
            let nodes = vec![
                grand,
                node("child", Some("root"), Some(500), 3),
                root,
                node("other", None, None, 0),
            ];
            let r = Rollup::build(&nodes);
            let t = r.get(&AgentId("root".into())).unwrap();
            assert_eq!((t.tokens, t.changed, t.nodes, t.live), (1_600, 6, 3, 2));
            assert_eq!(t.claimed, 3);
            assert_eq!(t.first_start, at(10));
            assert_eq!(
                t.wall_to(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100)),
                Some(std::time::Duration::from_secs(90)),
                "still running: to now"
            );
            assert!(r.has_children(&AgentId("root".into())));
            assert!(!r.has_children(&AgentId("grand".into())));
            let other = r.get(&AgentId("other".into())).unwrap();
            assert_eq!(other.claimed, 0, "no figure is no claim");
            assert_eq!(r.forest(&nodes).nodes, 4);
            assert_eq!(r.forest(&nodes).tokens, 1_600);
        }

        #[test]
        fn a_parent_loop_is_cut_rather_than_followed() {
            let nodes = vec![
                node("a", Some("b"), Some(1), 0),
                node("b", Some("a"), Some(2), 0),
            ];
            let r = Rollup::build(&nodes);
            assert!(r.get(&AgentId("a".into())).is_some());
        }

        #[test]
        fn a_tree_budget_is_measured_against_the_subtrees_spend() {
            let mut root = node("root", None, Some(700), 0);
            root.budget = Some(marion_core::budget::Budget {
                tree_tokens: Some(1_000),
                ..Default::default()
            });
            let nodes = vec![root.clone(), node("kid", Some("root"), Some(200), 0)];
            let r = Rollup::build(&nodes);
            let t = r.get(&root.agent_id).unwrap();
            assert_eq!(budget_of(&root, t), Some((900, 1_000, true)));
            assert_eq!(
                budget_of(&nodes[1], r.get(&nodes[1].agent_id).unwrap()),
                None
            );
        }
    }
}
