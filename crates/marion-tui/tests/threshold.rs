//! **The threshold test: how much tail is enough, decided by measurement.**
//!
//! The increment brief set the rule: *if `preamble + min(last 2 MiB, last 120 s)` does not
//! reproduce the same `viewport_lines()` as a full fold on every committed capture, take full
//! replay and eat the burst.*
//!
//! Written literally, that test is **vacuous on this corpus**, and reporting that is the first
//! result rather than a reason to skip it. Every committed capture is 6-44 kB and 12-40 s long
//! (see [`SIZES`]), so both bounds select record 0 and "windowed replay" *is* "full replay",
//! byte for byte. The comparison would pass without executing one line of cut logic and would go
//! on passing if the cut were deleted — the exact shape of test this increment is meant to reject.
//! `the_brief_window_is_vacuous_on_this_corpus` asserts that degeneracy directly, so the fact is
//! pinned rather than buried in a comment.
//!
//! So the real question is asked the way the corpus can answer it: sweep the cut across **every
//! record boundary of every capture** and ask whether `preamble + tail` reproduces the full fold.
//! That is `the_windowed_replay_threshold`, and it is what decides
//! [`Policy::default`](marion_tui::Policy::default).
//!
//! **The sweep went against the window: 337 of 462 cuts diverge**, so the shipped policy is full
//! replay. Two things that shaped the answer, both of them corrections to the brief's framing:
//!
//! 1. The observable had to be **viewport *and* scrollback**, not `viewport_lines()` alone. On
//!    `claude-2.1.220-boot-help-status-resize`, dropping alt-screen tracking leaves the viewport
//!    byte-identical — the corruption is entirely in the history. Judged on the viewport alone,
//!    the preamble's most important field would have looked like dead code. See [`state`].
//! 2. The natural hypothesis that unsafe cuts form a contiguous suffix is **false**, because a
//!    capture's own `CSI 3J` can make a late cut agree again. Recorded on
//!    `the_windowed_replay_threshold` rather than asserted.
//!
//! No process is started here. The committed `.cast` files are the whole input, which is why this
//! file runs in milliseconds.

use std::path::PathBuf;

use marion_term::{Size, Term};
use marion_tui::cast::{Cast, Payload};
use marion_tui::{Plan, Policy, Step, Sticky, grid_options};

const CLAUDE_BOOT_EXIT: &str = "claude-2.1.220-boot-exit";
const CLAUDE_TRUST: &str = "claude-2.1.220-boot-help-status-resize";
const CODEX_145: &str = "codex-cli-0.145.0-boot-status-help-diff-resize";
const CODEX_146_14ROW: &str = "codex-cli-0.146.0-14row-heavy-history-insert";
const CODEX_146_RESIZE: &str = "codex-cli-0.146.0-boot-status-help-resize";

const ALL: &[&str] = &[
    CLAUDE_BOOT_EXIT,
    CLAUDE_TRUST,
    CODEX_145,
    CODEX_146_14ROW,
    CODEX_146_RESIZE,
];

/// The two captures that enter the alternate screen and paint essentially the whole session there.
/// These are the ones a bare tail corrupts.
const CLAUDE: &[&str] = &[CLAUDE_BOOT_EXIT, CLAUDE_TRUST];

/// Measured `o`-payload **UTF-8 bytes** and duration, for the vacuity argument.
///
/// Bytes, not characters: a box-drawing glyph is three bytes and these captures are mostly box
/// drawing, so the two counts differ by ~30%. The byte figure is the one a replay budget is
/// denominated in.
///
/// Cross-check worth keeping: each of these exceeds the matching `.raw.bin` length by exactly the
/// U+FFFD damage `tests/fixtures/s2/NOTES.txt` documents — +9, +24, +9, 0, 0 against 6 093 /
/// 22 879 / 44 031 / 27 532 / 19 373. Two independently recorded facts agreeing is the cheapest
/// evidence available that this loader reads the corpus the way the corpus says it should be read.
const SIZES: &[(&str, usize, f64)] = &[
    (CLAUDE_BOOT_EXIT, 6_102, 12.179),
    (CLAUDE_TRUST, 22_903, 34.814),
    (CODEX_145, 44_040, 39.553),
    (CODEX_146_14ROW, 27_532, 34.031),
    (CODEX_146_RESIZE, 19_373, 36.515),
];

/// The window the increment brief proposed, kept as a named constant so the test that shows it is
/// non-binding does not silently start measuring whatever [`Policy::default`] happens to be.
const BRIEF_WINDOW: Policy = Policy {
    max_bytes: 2 * 1024 * 1024,
    max_age: std::time::Duration::from_secs(120),
};

/// The sweep's measured result, per capture: `(name, safe_cuts, total_cuts)`.
///
/// Pinned as data rather than as a bare "must be zero failures", so that a change in the replay
/// path shows up as a number that moved. Only the shortest capture — 33 records, 12 s, one clean
/// boot-and-exit — is safe at every cut.
const SWEEP: &[(&str, usize, usize)] = &[
    (CLAUDE_BOOT_EXIT, 33, 33),
    (CLAUDE_TRUST, 41, 61),
    (CODEX_145, 15, 155),
    (CODEX_146_14ROW, 14, 111),
    (CODEX_146_RESIZE, 22, 102),
];

/// `.raw.bin` lengths, for the cross-check above.
const RAW_LENS: &[(&str, usize, usize)] = &[
    (CLAUDE_BOOT_EXIT, 6_093, 9),
    (CLAUDE_TRUST, 22_879, 24),
    (CODEX_145, 44_031, 9),
    (CODEX_146_14ROW, 27_532, 0),
    (CODEX_146_RESIZE, 19_373, 0),
];

fn load(name: &str) -> Cast {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/s2")
        .join(format!("{name}.cast"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Cast::parse(&text).expect("a committed capture parses")
}

/// Fold a whole capture from its header size. The reference every cut is compared against.
fn full_fold(cast: &Cast) -> Term {
    let mut term = Term::with_options(
        Size::new(cast.cols as usize, cast.rows as usize),
        grid_options(),
    );
    for rec in &cast.records {
        match &rec.payload {
            Payload::Output(s) => term.advance(s.as_bytes()),
            Payload::Resize { cols, rows } => {
                term.resize(Size::new(*cols as usize, *rows as usize))
            }
            Payload::Input(_) | Payload::Exit(_) => {}
        }
    }
    term
}

/// The observable a replay is judged on: **viewport and retained scrollback together**.
///
/// The brief names `viewport_lines()` alone, and that turned out to be too weak to see the very
/// corruption this file exists to catch. Measured: on `claude-2.1.220-boot-help-status-resize`,
/// dropping `?1049h` from the preamble leaves the viewport **identical** — Claude repaints the
/// whole screen with absolute addressing, so whichever buffer it lands in ends up looking the
/// same. The damage is entirely in the *other* half: painting a full-screen TUI onto the main
/// screen scrolls the user's transcript away into history, and only `scrollback_lines()` shows it.
///
/// That is also precisely the harm the alt screen exists to prevent, and precisely what M3's C2
/// is about. Judging on the viewport alone would have declared the alt-screen tracking dead code.
fn state(term: &Term) -> (Vec<String>, Vec<String>) {
    (term.viewport_lines(), term.scrollback_lines())
}

/// Apply a plan to a fresh grid, exactly as an attaching client would.
fn apply(plan: &Plan) -> Term {
    let mut term = Term::with_options(
        Size::new(plan.cols as usize, plan.rows as usize),
        grid_options(),
    );
    term.advance(&plan.preamble);
    for step in &plan.tail {
        match step {
            Step::Feed(bytes) => term.advance(bytes),
            Step::Resize { cols, rows } => term.resize(Size::new(*cols as usize, *rows as usize)),
        }
    }
    term
}

// -------------------------------------------------------------------------------------------
// The vacuity finding
// -------------------------------------------------------------------------------------------

/// The brief's window selects record 0 on every committed capture, so a test comparing it to a
/// full fold tests nothing. Pinned, so that the day a capture *is* big enough the vacuity claim
/// fails loudly instead of the sweep quietly becoming the only real coverage.
#[test]
fn the_brief_window_is_vacuous_on_this_corpus() {
    let policy = BRIEF_WINDOW;
    for name in ALL {
        let cast = load(name);
        let cut = marion_tui::replay::choose_cut(&cast, policy);
        assert_eq!(
            cut, 0,
            "{name}: the 2 MiB / 120 s window is supposed to be non-binding here"
        );
        let bytes = cast.output_bytes_from(0);
        assert!(
            bytes < policy.max_bytes,
            "{name}: {bytes} bytes is no longer under the 2 MiB cap — re-run the sweep"
        );
        assert!(
            cast.duration() < policy.max_age,
            "{name}: {:?} is no longer under the 120 s cap — re-run the sweep",
            cast.duration()
        );
    }
}

/// The measured sizes the vacuity argument rests on, so the numbers in the docs are re-derived by
/// the suite rather than trusted.
#[test]
fn the_recorded_capture_sizes_are_what_the_docs_claim() {
    for (name, bytes, secs) in SIZES {
        let cast = load(name);
        assert_eq!(cast.output_bytes_from(0), *bytes, "{name}: output bytes");
        let measured = cast.duration().as_secs_f64();
        assert!(
            (measured - secs).abs() < 0.01,
            "{name}: duration {measured} != documented {secs}"
        );
    }
}

// -------------------------------------------------------------------------------------------
// The sweep that actually decides the policy
// -------------------------------------------------------------------------------------------

/// **The threshold, and the measurement that settled it.**
///
/// Sweep the cut across every record boundary of every capture; a cut is *safe* when
/// `preamble + tail` reproduces the full fold's viewport **and** its retained scrollback.
///
/// Result: **125 of 462 cuts are safe, 337 are not** — the window loses, decisively and on every
/// capture but the shortest.
///
/// The cause is the thing a sticky-mode preamble structurally cannot fix. The preamble restores
/// *modes*; it cannot restore *content*. Cut after the screen was painted and the tail carries
/// only the incremental updates that follow, so the grid comes up missing whatever was drawn
/// before the cut and never redrawn — and, for the scrollback half, missing every line that
/// scrolled into history before the cut.
///
/// *(One hypothesis tested and refuted, recorded because it is the natural guess: the unsafe cuts
/// are **not** a contiguous suffix. `codex-cli-0.146.0-boot-status-help-resize` has safe cuts
/// interleaved among unsafe ones, because its resize sequence clears history — so a cut placed
/// just after a `CSI 3J` can agree with a full fold again on a scrollback the full fold had also
/// just lost. An earlier draft asserted contiguity and failed; the assertion is gone rather than
/// weakened, since the conclusion never depended on it.)*
///
/// By the brief's own rule the policy is therefore full replay.
/// [`the_policy_is_full_replay_because_the_sweep_said_so`] is where that conclusion is enforced;
/// this test's job is to keep the *numbers* honest, so that a change which made windowing viable
/// (or made it worse) shows up as a diff rather than as silence.
#[test]
fn the_windowed_replay_threshold() {
    let mut total_safe = 0usize;
    let mut total = 0usize;

    for (name, want_safe, want_total) in SWEEP {
        let cast = load(name);
        let reference = state(&full_fold(&cast));
        let n = cast.records.len();
        let bad: Vec<usize> = (0..=n)
            .filter(|&cut| state(&apply(&Plan::for_cut(&cast, cut))) != reference)
            .collect();
        let safe = n + 1 - bad.len();

        assert_eq!(n + 1, *want_total, "{name}: record count changed");
        assert_eq!(
            safe,
            *want_safe,
            "{name}: {safe} of {} cuts are safe, expected {want_safe} \
             (first divergence {:?}) — the replay policy may need revisiting",
            n + 1,
            bad.first()
        );
        assert!(
            !bad.contains(&0),
            "{name}: a cut at record 0 IS a full fold and must always be safe"
        );
        total_safe += safe;
        total += n + 1;
    }

    assert_eq!(
        (total_safe, total),
        (125, 462),
        "the corpus-wide sweep result"
    );
}

/// The conclusion the sweep forces, enforced where it is actually consumed.
///
/// If someone re-tunes [`Policy::default`] back to a window, this fails and points at the sweep.
#[test]
fn the_policy_is_full_replay_because_the_sweep_said_so() {
    assert_eq!(
        Policy::default(),
        Policy::full(),
        "the sweep found 337 of 462 cuts unsafe, so a windowed default is unsupported"
    );
    // And the property that makes it correct rather than merely conservative: a full replay is a
    // cut at 0, which the sweep measured safe on every capture.
    for name in ALL {
        let cast = load(name);
        assert_eq!(
            marion_tui::replay::choose_cut(&cast, Policy::default()),
            0,
            "{name}"
        );
        assert_eq!(
            state(&apply(&Plan::under(&cast, Policy::default()))),
            state(&full_fold(&cast)),
            "{name}: the shipped policy must reproduce a full fold exactly"
        );
    }
}

/// The `.cast`/`.raw.bin` cross-check: each cast's output exceeds its raw capture by exactly the
/// U+FFFD damage `NOTES.txt` records.
#[test]
fn cast_output_exceeds_raw_by_exactly_the_documented_utf8_damage() {
    for (name, raw_len, delta) in RAW_LENS {
        let cast = load(name);
        assert_eq!(
            cast.output_bytes_from(0),
            raw_len + delta,
            "{name}: cast/raw delta is not NOTES.txt's documented U+FFFD damage"
        );
    }
}

/// The same sweep, but checking retained **scrollback** rather than the viewport.
///
/// The viewport is what the brief names, and it is the weaker of the two questions: M3's C2 is a
/// *retention* criterion, so a replay that painted the right screen while losing the transcript
/// would satisfy the brief's literal test and fail the milestone. Measured separately, and
/// reported rather than asserted equal — a tail legitimately cannot recover history that scrolled
/// away before the cut.
#[test]
fn the_scrollback_cost_of_cutting_is_measured_not_assumed() {
    for name in ALL {
        let cast = load(name);
        let full = full_fold(&cast).scrollback_len();
        let from_zero = apply(&Plan::for_cut(&cast, 0)).scrollback_len();
        assert_eq!(
            full, from_zero,
            "{name}: a cut at record 0 is a full fold and must retain identically"
        );
        // And the shape of the loss as the cut advances: monotone non-increasing is the property
        // that would break if the preamble ever *added* phantom history.
        let mid = apply(&Plan::for_cut(&cast, cast.records.len() / 2)).scrollback_len();
        let end = apply(&Plan::for_cut(&cast, cast.records.len())).scrollback_len();
        assert!(
            mid <= full && end <= mid,
            "{name}: retained history must not grow as the cut moves later \
             (full={full}, mid={mid}, end={end})"
        );
        println!("{name}: scrollback full={full} mid-cut={mid} end-cut={end}");
    }
}

// -------------------------------------------------------------------------------------------
// Required mutation guards
// -------------------------------------------------------------------------------------------

/// Recompute a plan with one field of the sticky state suppressed, to prove that field is
/// load-bearing. This is the mutation, applied in the test rather than to the source, so the
/// suite carries its own evidence.
fn plan_without_alt_tracking(cast: &Cast, cut: usize) -> Plan {
    let mut st = Sticky::initial(cast.cols, cast.rows);
    for rec in &cast.records[..cut] {
        match &rec.payload {
            Payload::Output(s) => st.absorb(s),
            Payload::Resize { cols, rows } => st.resize(*cols, *rows),
            _ => {}
        }
    }
    st.alt_screen = false; // <-- the mutation
    let tail = cast.records[cut..]
        .iter()
        .filter_map(|r| match &r.payload {
            Payload::Output(s) => Some(Step::Feed(s.as_bytes().to_vec())),
            Payload::Resize { cols, rows } => Some(Step::Resize {
                cols: *cols,
                rows: *rows,
            }),
            _ => None,
        })
        .collect();
    Plan {
        cols: st.cols,
        rows: st.rows,
        preamble: st.preamble(),
        cut,
        tail,
    }
}

/// **Mutation: the preamble drops alt-screen tracking.** The two Claude captures must diverge.
///
/// This is the trap the whole `sticky` module exists for. Both Claude captures enter the alternate
/// screen and paint the session there; without `?1049h` in the preamble the tail's absolute-
/// addressed repaint lands on the main screen, and the resulting grid is not the one a full fold
/// produces.
#[test]
fn dropping_alt_screen_from_the_preamble_breaks_both_claude_captures() {
    for name in CLAUDE {
        let cast = load(name);
        let want = state(&full_fold(&cast));

        // Find a cut after the capture has entered the alt screen and is still in it.
        let entered = cast
            .records
            .iter()
            .position(|r| matches!(&r.payload, Payload::Output(s) if s.contains("\u{1b}[?1049h")))
            .unwrap_or_else(|| panic!("{name}: expected a ?1049h somewhere"));
        let cut = entered + 1;

        // With the real preamble the cut is safe...
        assert_eq!(
            state(&apply(&Plan::for_cut(&cast, cut))),
            want,
            "{name}: the intact preamble must reproduce the full fold at record {cut}"
        );
        // ...and without alt-screen tracking it is not.
        assert_ne!(
            state(&apply(&plan_without_alt_tracking(&cast, cut))),
            want,
            "{name}: dropping ?1049 from the preamble must change the rendered grid — \
             if this passes, the preamble's alt-screen tracking is dead code"
        );
    }
}

/// **Mutation: cut on a byte boundary instead of a record boundary.**
///
/// A `.cast` `o` record is one whole `read()`, so a record-index cut can never split an escape
/// sequence. A byte cut can, and the failure is silent: `ESC [ ? 1 0 4 9 h` cut after `ESC [ ? 1`
/// hands the emulator `049h`, which is three printable characters and an `h`, painted as text.
///
/// This test performs the byte cut deliberately and requires it to produce a *different* grid, so
/// the record-boundary guarantee is measured rather than asserted in a comment.
#[test]
fn cutting_mid_escape_on_a_byte_boundary_corrupts_the_replay() {
    let cast = load(CLAUDE_TRUST);
    let want = state(&full_fold(&cast));

    // The record that enters the alternate screen, and where `?1049h` sits inside it.
    let (idx, text) = cast
        .records
        .iter()
        .enumerate()
        .find_map(|(i, r)| match &r.payload {
            Payload::Output(s) if s.contains("\u{1b}[?1049h") => Some((i, s.clone())),
            _ => None,
        })
        .expect("a ?1049h record");
    let esc = text.find("\u{1b}[?1049h").expect("offset");

    // The record-boundary cut, at the record *after* this one: safe, because the preamble
    // synthesizes the mode.
    assert_eq!(
        state(&apply(&Plan::for_cut(&cast, idx + 1))),
        want,
        "the record-boundary cut is the control and must be safe"
    );

    // Now the byte cut: keep the same preamble, but start the tail four bytes into `?1049h`,
    // which is what a byte-offset window lands on when it does not know where records begin.
    let mut plan = Plan::for_cut(&cast, idx);
    let severed = text[esc + 4..].to_owned();
    plan.tail[0] = Step::Feed(severed.clone().into_bytes());
    assert!(
        severed.starts_with("049h"),
        "the mutation must actually sever the sequence, got {:?}",
        &severed[..4.min(severed.len())]
    );
    assert_ne!(
        state(&apply(&plan)),
        want,
        "a byte-boundary cut through an escape sequence must corrupt the grid — \
         if this passes, nothing is protecting the record boundary"
    );
}

/// A cut at the very end replays nothing but the preamble, and must still produce a *valid* grid
/// rather than a panic or an empty one — this is the degenerate case a client hits when it
/// attaches to a node that has been quiet for a long time.
#[test]
fn a_cut_past_the_last_record_is_preamble_only_and_does_not_panic() {
    for name in ALL {
        let cast = load(name);
        let plan = Plan::for_cut(&cast, cast.records.len() + 10);
        assert_eq!(plan.cut, cast.records.len(), "the cut is clamped");
        assert!(plan.tail.is_empty());
        let term = apply(&plan);
        assert_eq!(term.size().cols, plan.cols as usize);
    }
}

/// The scan's one stated assumption, measured: no sequence `Sticky` tracks is split across two
/// `o` records in any committed capture. If a future capture splits one, this fails rather than
/// the preamble silently losing a mode.
#[test]
fn no_tracked_sequence_straddles_a_record_boundary_in_any_capture() {
    for name in ALL {
        let cast = load(name);
        // Folding record-by-record must equal folding the concatenation.
        let mut incremental = Sticky::initial(cast.cols, cast.rows);
        let mut joined = String::new();
        for rec in &cast.records {
            if let Payload::Output(s) = &rec.payload {
                incremental.absorb(s);
                joined.push_str(s);
            }
        }
        let mut whole = Sticky::initial(cast.cols, cast.rows);
        whole.absorb(&joined);
        assert_eq!(
            incremental, whole,
            "{name}: a tracked sequence is split across two `o` records, so the per-record \
             scan loses it — the preamble needs a carry"
        );
    }
}
