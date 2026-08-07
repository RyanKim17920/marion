//! `marion-tui` — attaching a terminal to one node (§5.3, §10).
//!
//! # One pane, deliberately
//!
//! M3's acceptance criteria (§9) mention a *pane* three times and a tree zero times. This crate
//! therefore builds exactly one thing: `marion attach <agent-id>`, where the whole terminal is one
//! node's grid. **No layout engine, no splits, no focus model, no tree list.** §2's diagram and
//! §5.6 do describe a tree with panes and queues, and that is real work — it is just work that
//! moves no M3 criterion, so it is deferred rather than half-built here.
//!
//! # The pieces, and the seam each one sits on
//!
//! * [`cast`] — reading `pty.cast`. Its whole job is that a cut lands on a **record boundary**.
//! * [`sticky`] — the six-sequence scan that makes a tail replayable at all. This is the part a
//!   reader gets wrong: a bare tail of a Claude Code session renders alt-screen content on the
//!   main screen, because `?1049h` is said once and never repeated.
//! * [`replay`] — `preamble ++ tail`, and the measured argument for where to cut.
//! * [`keys`] — keystrokes back to the node. A **filter**, not an encoder: marion's terminal is
//!   already in raw mode, so the bytes it produces are the ones the node expects. The one thing
//!   worth doing is reserving a way out.
//! * [`mouse`] — SGR-1006 encoding, gated on the modes the child actually enabled. The only place
//!   in this crate that encodes anything, because a click is the one input with no source bytes.
//! * [`redraw`] — when to paint: the DECSET 2026 bracket **edge**, and none of the three things
//!   that look like it and are not.
//! * [`guard`] — marion's **own** terminal: raw mode, the alternate screen, and getting out of
//!   both on a panic as well as on a drop.
//! * [`view`] — the full-area pane, which is a delegation and deliberately nothing more.
//!
//! # What renders
//!
//! Nothing in this crate. `marion_term`'s `impl Widget for &Term` is the render path, and it is
//! the *same* one `marion-term`'s L4.5 `TestBackend` driver uses. A second renderer here would be
//! a second thing to keep correct, and the snapshot suite would stop covering what ships.
//! [`view::Pane`] is a newtype over that widget, and
//! `a_pane_is_the_marion_term_widget_and_not_a_second_renderer` is what keeps it one.

pub mod backend;
pub mod cast;
pub mod guard;
pub mod keys;
pub mod mouse;
pub mod redraw;
pub mod replay;
pub mod sticky;
pub mod view;

pub use backend::ScreenBackend;
pub use cast::{Cast, CastError, Payload, Record};
pub use guard::Screen;
pub use keys::{Action, Keys};
pub use mouse::{Button, Kind as MouseKind, Mods, MouseEvent};
pub use redraw::Redraw;
pub use replay::{Plan, Policy, Step};
pub use sticky::Sticky;
pub use view::Pane;

/// The widest terminal this client budgets memory for.
///
/// §5.3's captures resize to 140 columns and a maximized terminal on a wide display is wider
/// still, so the memory bound below is computed at a width no realistic attach exceeds. Using the
/// *widest* plausible width rather than a typical one is what makes [`MAX_SCROLLBACK`]'s figure a
/// ceiling instead of an average.
pub const BUDGET_COLUMNS: usize = 200;

/// The memory a client may hold in retained scrollback for one node, at [`BUDGET_COLUMNS`].
///
/// 128 MiB. This is the number actually being chosen — [`MAX_SCROLLBACK`] is derived from it —
/// because "how many rows" is not a quantity anyone has an intuition about and "how much of the
/// operator's RAM" is. A marion client is a foreground tool on a developer's laptop that may hold
/// several of these at once, so a per-node budget in the hundreds of megabytes is not affordable
/// however many hours of history it buys.
pub const SCROLLBACK_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// Scrollback the client retains per node.
///
/// **This is a bound `marion-term` does not impose for us, and M3 criterion C2 is the reason it
/// matters.** C2 is specifically a *retention* criterion — "scrollback retained across at least
/// one resize, proving `CSI 3J` interception" — so an unbounded buffer could fail C2 by the
/// opposite of the mechanism C2 tests: not by dropping history to a `CSI 3J`, but by growing
/// without limit until the client is OOM-killed and loses all of it.
///
/// *(A nuance the increment brief states more strongly than the code supports, recorded rather
/// than quietly worked around: `alacritty_terminal`'s history is **not** effectively unbounded as
/// marion uses it. `Config::scrolling_history` is a hard cap — the grid rotates rows out past it —
/// and `marion_term::Options` already defaults it to 10 000. So the OOM hazard is real in
/// principle but already mitigated by an inherited default. What this constant does is make the
/// number a **client decision with a written derivation** rather than a default nobody chose, and
/// `the_scrollback_cap_actually_bounds_a_grid` proves the mechanism holds instead of assuming it.)*
///
/// # The derivation, in the direction that has an answer
///
/// A retained row costs `columns × marion_term::CELL_BYTES`, and `CELL_BYTES` is taken from
/// `size_of::<Cell>()` rather than written down — **measured 24 on `alacritty_terminal` 0.26**,
/// which is the ~3 kB a row the S18 spike reports at 140 columns and 4.7 kB at [`BUDGET_COLUMNS`].
/// So the row count is not the free variable; the memory is. Fixing
/// [`SCROLLBACK_BUDGET_BYTES`] at 128 MiB and dividing gives ~27 900 rows, and this is rounded
/// **down** to a round number:
///
/// **20 000 rows** — ~64 MiB at 200 columns, ~45 MiB at the 140 columns §5.3's captures actually
/// resize to.
///
/// The other end of the trade is how much history that buys, and the corpus answers it: the
/// densest committed capture scrolls 94 rows into history in 34 s of heavy `/status` traffic
/// (`codex-cli-0.146.0-14row-heavy-history-insert`; §5.3 measures 121 rows retained on the 0.145.0
/// capture). Call it 3 rows a second *sustained*, which is far above what a human-driven session
/// produces. 20 000 rows is then **~1.9 hours** of continuous dense output before the oldest line
/// is dropped — about 11× the "recorded 10-minute manual session" C1 asks for, and longer than any
/// committed capture by two orders of magnitude.
///
/// It is also deliberately **twice** `marion-term`'s 10 000 default, so this is visibly a client
/// widening the bound on purpose and not a constant that happens to agree.
///
/// Finiteness is the load-bearing property; the figure is a trade. Move it by moving
/// [`SCROLLBACK_BUDGET_BYTES`] — that is the number with a reason — and let this follow.
pub const MAX_SCROLLBACK: usize = 20_000;

/// The grid options a marion client attaches with.
///
/// `suppress_erase_saved` stays on — that *is* C2's `CSI 3J` interception — and the scrollback
/// bound is [`MAX_SCROLLBACK`] rather than `marion-term`'s 10 000 default, because a client
/// watching a long codex session is exactly the case the default was not chosen for.
pub fn grid_options() -> marion_term::Options {
    marion_term::Options {
        suppress_erase_saved: true,
        scrolling_history: MAX_SCROLLBACK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_term::{Options, Size, Term};

    /// **Mutation: make `MAX_SCROLLBACK` unbounded.**
    ///
    /// The assertion is the **memory** the cap implies, not the row count, and one assertion
    /// therefore kills both forms of the mutation. `usize::MAX` dies on the `checked_mul` — an
    /// unbounded row count has no representable memory cost, which is the arithmetic saying the
    /// same thing the doc does. A merely *enormous* figure — one large enough that the client OOMs
    /// before the cap ever engages, which a bare `< usize::MAX` check would wave through — dies on
    /// the budget comparison.
    ///
    /// *(There is deliberately no `assert!(MAX_SCROLLBACK < usize::MAX)`. Both operands are
    /// constants, so it is a claim the compiler folds away rather than a test; clippy's
    /// `assertions_on_constants` says so, and it is right. The `checked_mul` is the same guarantee
    /// obtained from a computation that can actually fail.)*
    #[test]
    fn the_scrollback_bound_is_finite_and_within_the_declared_memory_budget() {
        let row_bytes = BUDGET_COLUMNS * marion_term::CELL_BYTES;
        let worst_case = MAX_SCROLLBACK
            .checked_mul(row_bytes)
            .expect("a bound whose memory cost overflows usize is not a bound");
        assert!(
            worst_case <= SCROLLBACK_BUDGET_BYTES,
            "{MAX_SCROLLBACK} rows x {row_bytes} B/row = {worst_case} B exceeds the \
             {SCROLLBACK_BUDGET_BYTES} B budget MAX_SCROLLBACK is derived from"
        );

        // And the derivation is not vacuously satisfied by a tiny number: the bound must still be
        // a deliberate widening of the emulator's default, or this constant earns nothing.
        assert!(
            MAX_SCROLLBACK > Options::default().scrolling_history,
            "a client bound below marion-term's own default is not a client decision"
        );
    }

    /// The row cost the doc's arithmetic rests on, re-derived by the suite.
    ///
    /// A dependency bump that grew `Cell` would otherwise leave the doc quietly wrong while the
    /// budget assertion above silently tightened.
    #[test]
    fn a_retained_row_costs_what_the_derivation_says_it_does() {
        assert_eq!(
            marion_term::CELL_BYTES,
            24,
            "alacritty_terminal 0.26's Cell"
        );
        // ~3 kB at the 140 columns §5.3's captures resize to, which is S18's figure.
        assert_eq!(140 * marion_term::CELL_BYTES, 3_360);
    }

    /// **The mechanism, measured rather than assumed.**
    ///
    /// Everything above is arithmetic about a cap that is only worth stating if
    /// `scrolling_history` is actually enforced. It is not marion's code that enforces it, so this
    /// drives a real grid past a small cap and checks the history stops growing. Small on purpose:
    /// proving the *mechanism* needs 50 rows, and proving it at [`MAX_SCROLLBACK`] would spend
    /// 64 MiB and several seconds to learn the same fact.
    #[test]
    fn the_scrollback_cap_actually_bounds_a_grid() {
        const CAP: usize = 50;
        let mut term = Term::with_options(
            Size::new(20, 5),
            Options {
                suppress_erase_saved: true,
                scrolling_history: CAP,
            },
        );
        // Twenty times the cap in distinct, non-blank lines, so nothing is dropped as empty.
        for i in 0..1_000 {
            term.advance(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(
            term.history_size(),
            CAP,
            "the grid retained more than its configured history — the cap does not hold, so \
             MAX_SCROLLBACK bounds nothing"
        );
        // The rows kept are the *newest*, which is the half that makes a cap useful rather than
        // merely safe: a cap that discarded the recent end would bound memory and lose the screen.
        //
        // Stated as a relation rather than as two literals, because the newest retained line is
        // not line 999 — the last five lines are still in the five-row *viewport*, which is not
        // history. Asserting "999" here is the mistake this comment exists to stop being made
        // again; what matters is that the retained window is contiguous, `CAP` long, and does not
        // start at the beginning of the session.
        let n = |l: &str| l.trim_start_matches("line ").parse::<usize>().expect(l);
        let kept = term.scrollback_lines();
        let (oldest, newest) = (n(&kept[0]), n(kept.last().expect("CAP rows")));
        assert_eq!(
            newest - oldest + 1,
            CAP,
            "the retained window is not contiguous: {oldest}..={newest}"
        );
        assert!(
            oldest > 0,
            "nothing was evicted, so this run never reached the cap and proves nothing"
        );
    }

    #[test]
    fn a_client_grid_intercepts_erase_saved() {
        // C2 in one line: turning this off is how a resize takes the transcript with it.
        assert!(grid_options().suppress_erase_saved);
        assert_eq!(grid_options().scrolling_history, MAX_SCROLLBACK);
    }
}
