//! Turning a `pty.cast` into the bytes an attaching client feeds its fresh grid.
//!
//! # The shape of the answer
//!
//! A replay is `preamble ++ tail`, where the tail starts at a **record index** (never a byte
//! offset — see [`crate::cast`]) and the preamble is the sticky-mode synthesis of everything
//! before it (see [`crate::sticky`]).
//!
//! # Which cut, and how that was decided
//!
//! The increment brief proposed `min(last 2 MiB, last 120 s)` and asked for a test that takes full
//! replay instead if that window ever fails to reproduce a full fold's `viewport_lines()`.
//!
//! **That test, written literally, is vacuous on this corpus, and saying so is the finding.** The
//! five committed captures are 6-44 kB and 12-40 s long:
//!
//! | capture | bytes | duration |
//! |---|---|---|
//! | `claude-2.1.220-boot-exit` | 6 093 | 12.2 s |
//! | `claude-2.1.220-boot-help-status-resize` | 22 879 | 34.8 s |
//! | `codex-cli-0.145.0-…-diff-resize` | 44 031 | 39.6 s |
//! | `codex-cli-0.146.0-14row-…` | 27 532 | 34.0 s |
//! | `codex-cli-0.146.0-boot-status-help-resize` | 19 373 | 36.5 s |
//!
//! Every one is under 2 MiB *and* under 120 s, so on every one the window selects **record 0** and
//! "windowed replay" and "full replay" are the same bytes. A test comparing them would pass
//! without exercising a single line of cut logic, and would keep passing if the cut were deleted.
//! That is precisely the "test that can only pass by being vacuous" this increment is supposed to
//! reject.
//!
//! So the threshold is measured the way the corpus can actually answer it: [`Plan::for_cut`] is
//! swept across **every record boundary in every capture**, asking whether `preamble + tail`
//! reproduces the full fold's grid. That sweep is
//! `tests/threshold.rs::the_windowed_replay_threshold`.
//!
//! **It went against the window.** 337 of 462 cuts diverge, so [`Policy::default`] is
//! [`Policy::full`] — replay everything and eat the burst, exactly as the brief instructed for
//! that outcome. The burst is small: the largest committed capture is 44 kB and folds in about
//! two milliseconds. The numbers and the reasoning are on [`Policy::default`].
//!
//! [`Policy`] and [`choose_cut`] are kept rather than deleted. They are tested and correct, and
//! they are the mechanism the sweep will re-decide against when a session long enough to make
//! full replay expensive is finally committed. What is not kept is a *default* that measurement
//! does not support.

use std::time::Duration;

use crate::cast::{Cast, Payload};
use crate::sticky::Sticky;

/// How much tail to replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Never replay more than this many bytes of `o` payload.
    pub max_bytes: usize,
    /// Never replay more than this much wall time.
    pub max_age: Duration,
}

impl Default for Policy {
    /// **Full replay. The threshold test decided this, and it decided against the window.**
    ///
    /// `tests/threshold.rs::the_windowed_replay_threshold` sweeps the cut across every record
    /// boundary of every committed capture and compares `preamble + tail` against a full fold, on
    /// **viewport and retained scrollback together**. **337 of 462 cuts do not reproduce the full
    /// grid:**
    ///
    /// | capture | safe cuts |
    /// |---|---|
    /// | `claude-2.1.220-boot-exit` | 33 / 33 |
    /// | `claude-2.1.220-boot-help-status-resize` | 41 / 61 |
    /// | `codex-cli-0.145.0-…-diff-resize` | 15 / 155 |
    /// | `codex-cli-0.146.0-14row-…` | 14 / 111 |
    /// | `codex-cli-0.146.0-boot-status-help-resize` | 22 / 102 |
    ///
    /// Only the shortest capture — one clean boot-and-exit — survives every cut.
    ///
    /// The cause is the thing a sticky-mode preamble structurally cannot fix. The preamble
    /// restores *modes*; it cannot restore *content*. Cut after the screen was painted and the
    /// tail carries only the incremental updates that follow, so the grid comes up missing
    /// whatever was drawn before the cut and never redrawn — and missing every line that scrolled
    /// into history before it. No enlargement of the tracked-sequence set helps, because the
    /// missing information is the pixels themselves.
    ///
    /// This also means the preamble is **necessary but not sufficient**: it is what makes the
    /// safe cuts safe (see `dropping_alt_screen_from_the_preamble_breaks_both_claude_captures`),
    /// and it is not enough on its own to license a window.
    ///
    /// So the brief's own rule applies as written: *"if it does not reproduce the same viewport on
    /// every committed capture, take full replay and eat the burst."* It does not, so we do.
    ///
    /// The burst is affordable, which is why this is not a painful trade: the largest committed
    /// capture is 44 kB and folds in ~2 ms. [`Policy`] and [`choose_cut`] are kept — tested, and
    /// correct — because the *shape* of the answer will matter for a genuinely long session, and
    /// the sweep is the harness that will re-decide it when a capture long enough to need a window
    /// is committed. What is not kept is a window nothing measured supports.
    fn default() -> Self {
        Self::full()
    }
}

impl Policy {
    /// Replay everything, always. What [`Policy::default`] degenerates to on every committed
    /// capture, and what the sweep would have forced had it found a single unsafe cut.
    pub fn full() -> Self {
        Self {
            max_bytes: usize::MAX,
            max_age: Duration::MAX,
        }
    }
}

/// A replay, ready to feed to a grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The size to build the grid at — the size in force **at the cut**, which is the header size
    /// only when nothing resized before it.
    pub cols: u16,
    pub rows: u16,
    /// Synthesized mode-restoring bytes. Empty when the cut is at record 0.
    pub preamble: Vec<u8>,
    /// The first record of the tail.
    pub cut: usize,
    /// `o` bytes and `r` resizes from `cut` onward, in order.
    pub tail: Vec<Step>,
}

/// One thing a replay does to the grid, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Feed(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

impl Plan {
    /// Build a replay that cuts at `cut`, restoring everything before it via the preamble.
    ///
    /// `i` (input) records are **not** replayed. They are what marion typed, not what the harness
    /// painted; the harness's echo of them is already in the `o` stream, and feeding them to the
    /// grid would double every keystroke.
    ///
    /// `x` (exit) is likewise not replayed — it is a status, not screen bytes.
    pub fn for_cut(cast: &Cast, cut: usize) -> Self {
        let cut = cut.min(cast.records.len());
        let mut before = Sticky::initial(cast.cols, cast.rows);
        for rec in &cast.records[..cut] {
            match &rec.payload {
                Payload::Output(s) => before.absorb(s),
                Payload::Resize { cols, rows } => before.resize(*cols, *rows),
                Payload::Input(_) | Payload::Exit(_) => {}
            }
        }
        let tail = cast.records[cut..]
            .iter()
            .filter_map(|r| match &r.payload {
                Payload::Output(s) => Some(Step::Feed(s.as_bytes().to_vec())),
                Payload::Resize { cols, rows } => Some(Step::Resize {
                    cols: *cols,
                    rows: *rows,
                }),
                Payload::Input(_) | Payload::Exit(_) => None,
            })
            .collect();
        Self {
            cols: before.cols,
            rows: before.rows,
            preamble: before.preamble(),
            cut,
            tail,
        }
    }

    /// Build a replay under `policy`, choosing the cut itself.
    pub fn under(cast: &Cast, policy: Policy) -> Self {
        Self::for_cut(cast, choose_cut(cast, policy))
    }
}

/// The first record index that satisfies both bounds.
///
/// Both bounds are **caps** on how much to replay — "at most 2 MiB", "at most 120 s old" — so the
/// window is the *intersection* of the two, which is the **later** of the two candidate cuts.
/// `min(last 2 MiB, last 120 s)` in the brief's phrasing is a minimum over *windows*, and a
/// smaller window is a larger index. Taking the earlier index instead would let a generous byte
/// budget silently defeat a tight age budget, which is the bug this comment exists to stop
/// someone re-introducing: it type-checks, and every test with only one bound set still passes.
pub fn choose_cut(cast: &Cast, policy: Policy) -> usize {
    let n = cast.records.len();
    if n == 0 {
        return 0;
    }
    // Earliest index whose suffix fits in max_bytes.
    let mut by_bytes = n;
    let mut acc = 0usize;
    for i in (0..n).rev() {
        if let Payload::Output(s) = &cast.records[i].payload {
            acc = acc.saturating_add(s.len());
            if acc > policy.max_bytes {
                break;
            }
        }
        by_bytes = i;
    }
    // Earliest index no older than max_age.
    let end = cast.duration();
    let cutoff = end.saturating_sub(policy.max_age);
    let by_age = cast
        .records
        .iter()
        .position(|r| r.at >= cutoff)
        .unwrap_or(n);
    by_bytes.max(by_age)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::Cast;

    fn cast(body: &str) -> Cast {
        Cast::parse(&format!(
            "{{\"version\":3,\"term\":{{\"cols\":80,\"rows\":24}}}}\n{body}"
        ))
        .expect("parse")
    }

    /// `ESC` is a control character, so it is `\u001b` inside a JSON string — a raw `\x1b` makes
    /// the record unparseable, which is how the first draft of these tests silently measured an
    /// empty cast.
    const ALT_ON: &str = "[0,\"o\",\"\\u001b[?1049h\"]\n[0,\"o\",\"hello\"]\n";

    #[test]
    fn a_cut_at_zero_needs_no_preamble() {
        let c = cast(ALT_ON);
        let p = Plan::for_cut(&c, 0);
        assert!(p.preamble.is_empty());
        assert_eq!(p.tail.len(), 2);
    }

    #[test]
    fn a_cut_after_the_alt_screen_switch_carries_it_in_the_preamble() {
        let c = cast(ALT_ON);
        let p = Plan::for_cut(&c, 1);
        assert_eq!(p.preamble, b"\x1b[?1049h".to_vec());
        assert_eq!(p.tail, vec![Step::Feed(b"hello".to_vec())]);
    }

    #[test]
    fn the_plan_is_sized_at_the_resize_in_force_at_the_cut_not_the_header() {
        let c = cast("[0,\"o\",\"a\"]\n[0,\"r\",\"140x45\"]\n[0,\"o\",\"b\"]\n");
        let p = Plan::for_cut(&c, 2);
        assert_eq!(
            (p.cols, p.rows),
            (140, 45),
            "the header's 80x24 is stale by the cut"
        );
    }

    #[test]
    fn input_and_exit_records_are_never_replayed_into_the_grid() {
        // An `i` record is what marion typed; the harness's echo is already in the `o` stream.
        let c = cast("[0,\"i\",\"ls\\r\"]\n[0,\"o\",\"ls\"]\n[0,\"x\",\"exit 0\"]\n");
        let p = Plan::for_cut(&c, 0);
        assert_eq!(
            p.tail,
            vec![Step::Feed(b"ls".to_vec())],
            "only `o` and `r` reach the grid"
        );
    }

    #[test]
    fn a_generous_policy_replays_from_record_zero() {
        let c = cast("[1,\"o\",\"a\"]\n[1,\"o\",\"b\"]\n[1,\"o\",\"c\"]\n");
        assert_eq!(choose_cut(&c, Policy::default()), 0);
        assert_eq!(choose_cut(&c, Policy::full()), 0);
    }

    #[test]
    fn a_byte_bound_cuts_the_tail_and_still_lands_on_a_record_boundary() {
        let c = cast("[0,\"o\",\"aaaa\"]\n[0,\"o\",\"bbbb\"]\n[0,\"o\",\"cccc\"]\n");
        let policy = Policy {
            max_bytes: 5,
            ..Policy::full()
        };
        let cut = choose_cut(&c, policy);
        assert_eq!(cut, 2, "only the last 4-byte record fits under 5 bytes");
        // The load-bearing half: whatever the bound, the tail is whole records.
        let p = Plan::for_cut(&c, cut);
        assert_eq!(p.tail, vec![Step::Feed(b"cccc".to_vec())]);
    }

    #[test]
    fn an_age_bound_cuts_by_absolute_time() {
        let c = cast("[10,\"o\",\"old\"]\n[10,\"o\",\"mid\"]\n[10,\"o\",\"new\"]\n");
        let policy = Policy {
            max_age: Duration::from_secs(15),
            ..Policy::full()
        };
        // duration is 30 s; cutoff 15 s; the first record at or after 15 s is index 1 (20 s).
        assert_eq!(choose_cut(&c, policy), 1);
    }

    /// Both bounds are caps, so the *tighter* one must win — a generous byte budget must not be
    /// able to defeat a tight age budget. Taking `min` instead of `max` passes every test that
    /// sets only one bound, so this is the only test that catches the inversion.
    #[test]
    fn a_generous_bound_cannot_defeat_a_tight_one() {
        let c = cast("[10,\"o\",\"aaaaaaaa\"]\n[10,\"o\",\"b\"]\n[10,\"o\",\"c\"]\n");
        let tight_age = Duration::from_secs(5);

        // Age alone: duration 30 s, cutoff 25 s, so only the last record survives.
        let age_only = choose_cut(
            &c,
            Policy {
                max_age: tight_age,
                ..Policy::full()
            },
        );
        assert_eq!(age_only, 2);

        // Adding an enormous byte budget must not move it back to 0.
        let with_generous_bytes = choose_cut(
            &c,
            Policy {
                max_age: tight_age,
                max_bytes: 1024 * 1024,
            },
        );
        assert_eq!(
            with_generous_bytes, age_only,
            "the tighter cap must still bind"
        );

        // And symmetrically: a generous age must not defeat a tight byte budget.
        let bytes_only = choose_cut(
            &c,
            Policy {
                max_bytes: 2,
                ..Policy::full()
            },
        );
        let with_generous_age = choose_cut(
            &c,
            Policy {
                max_bytes: 2,
                max_age: Duration::from_secs(3600),
            },
        );
        assert_eq!(with_generous_age, bytes_only);
    }

    #[test]
    fn an_empty_cast_plans_an_empty_replay() {
        let c = cast("");
        let p = Plan::under(&c, Policy::default());
        assert_eq!(p.cut, 0);
        assert!(p.preamble.is_empty() && p.tail.is_empty());
        assert_eq!((p.cols, p.rows), (80, 24), "the header size still applies");
    }
}
