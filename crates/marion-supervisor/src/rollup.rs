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

use marion_core::encoding::SystemTime;

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

/// A subtree's figures: every node's [`Own`] added.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
}
