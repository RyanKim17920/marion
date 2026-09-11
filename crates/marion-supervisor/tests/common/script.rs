//! **The canned script for a `claude` root that delegates to a `codex` child.**
//!
//! `client_run.rs`, `node_attach.rs`, `child_events.rs` and `child_stream.rs` each drove the same
//! shape — one root turn calling marion's `spawn` for a `codex-impl` child that adds a marker file
//! under `src/` and reports back — and differed only in the strings that key and mark it. Those
//! strings are [`Delegation`]'s fields, spelled by each suite where it asserts over them.

use marion_provider::{RootScript, RootTurn, Script};
use serde_json::json;

/// What varies between the suites that use this shape. Every field is a suite constant so that a
/// marker read back from a provider log or an event stream is traceable to the file that set it.
pub struct Delegation<'a> {
    /// Keys the root's half of the script, and appears in no child request.
    pub root_marker: &'a str,
    /// The root's closing text after the child reports back.
    pub root_final_text: &'a str,
    /// The codex child's narrative while it works.
    pub child_narrative: &'a str,
    /// The file the child's patch adds, relative to the repo.
    pub child_file: &'a str,
    /// The single line that file contains.
    pub child_file_line: &'a str,
    /// The `narrative` field of the child's report through marion's own verb.
    pub child_final_narrative: &'a str,
    /// The child's own wall clock, through `spawn`'s `timeout_secs`.
    pub child_timeout_secs: u64,
}

pub fn claude_delegates_to_codex(d: Delegation<'_>) -> Script {
    let claude = marion_harness::adapter_for(marion_core::harness::Harness::ClaudeCode)
        .expect("claude has an adapter");
    Script {
        root: Some(RootScript {
            marker: d.root_marker.into(),
            turn: RootTurn {
                tool: claude.marion_tool_name("spawn"),
                args: json!({
                    "agent_type": "codex-impl",
                    "prompt": "Add the marker file under src/ and report back.",
                    "acceptance_criteria": ["a file exists under src/ containing the marker"],
                    "writable_scope": ["src/**"],
                    "timeout_secs": d.child_timeout_secs,
                }),
                final_text: d.root_final_text.into(),
            },
        }),
        // The child is codex: it applies a patch, then reports through marion's own verb.
        child_narrative: d.child_narrative.into(),
        child_patch: format!(
            "*** Begin Patch\n*** Add File: {}\n+{}\n*** End Patch",
            d.child_file, d.child_file_line
        ),
        child_final_text: json!({"narrative": d.child_final_narrative, "result_commits": []})
            .to_string(),
        ..Script::default()
    }
}

/// The two real binaries this script drives, gated **before** anything is spawned.
///
/// These suites are the ones MILESTONES named as driving real harnesses "without going through the
/// gate": a missing or drifted binary surfaced as a timeout downstream rather than by name here.
/// Since the shim, the gate is also what lays the pinned release into the process's `PATH` prefix,
/// so a run that skipped it would spawn whatever an auto-update left first on `PATH` while every
/// gated suite in the same tree spawned the admitted one. See `marion_testsupport::on_path`.
///
/// # Why this returns a bool rather than asserting
///
/// It used to assert, and off a runner it still effectively does: `harness_available` keeps
/// `on_path`'s rule — an absent or drifted binary panics, naming the program and the pinned
/// version — for every machine that has not set `MARION_CI_NO_HARNESSES=1`. The one runner that
/// has says so explicitly, and there the skip is announced by name on the uncaptured stderr and
/// this returns `false`. The caller's contract is one line:
///
/// ```ignore
/// if !common::script::require_claude_and_codex() {
///     return;
/// }
/// ```
///
/// Without that, a suite driving this script could only be kept out of CI by being *named out* of
/// the workflow's list — and a suite that drives a real harness without ever mentioning `on_path`
/// reads, to that list, exactly like a suite that needs nothing. `client_run.rs` was one, and it
/// failed on both runners with five journal invariants rather than with the missing binary.
///
/// **Both programs are probed, never short-circuited**, so a machine missing both is told about
/// both in one run rather than one per fix.
#[must_use]
pub fn require_claude_and_codex() -> bool {
    let mut ready = true;
    for program in ["claude", "codex"] {
        ready &= marion_testsupport::harness_available(program);
    }
    ready
}
