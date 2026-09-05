//! The stream grammar: **one reader for every harness's output stream, driven by a row of data.**
//!
//! §6.1 step 9 reads a child's own stream for three things — the `report` it made, what became of
//! each call to marion, and whether the stream itself claimed failure — and five harnesses put
//! those three things in five places with five spellings. Until this module each harness had its
//! own ~70-line reader that walked frames, matched a type, found a name, paired a result and
//! folded a `StreamOutcome`; they differed in *where* and *what*, never in *how*. So the how is
//! [`parse_stream`] and [`marion_calls`], once, and the where and what are a [`StreamGrammar`]
//! row per harness beside its launch row.
//!
//! # What is data and what is code
//!
//! A row names pointers and values: which frame is a call, where its name and arguments sit, how
//! a result is paired back, what spells success. Those are facts about a captured stream
//! (`tests/fixtures/s6`, `s9`, `s12`, `s13`, `s24`), transcribed. The vocabulary is **closed and
//! enumerated** — [`Verdict`] has one variant per verdict shape the five streams were measured to
//! carry, [`Pairing`] one per way a result reaches its call — and a sixth harness that needs a new
//! shape adds a variant here, visibly, rather than a reader.
//!
//! ACP is the one harness whose reader stays code (`crate::acp::parse_stream`): its arguments
//! nest differently per *agent* ([`crate::acp::ToolSpelling::arguments`]), its terminal frame
//! carries no name, and a shim's startup diagnostic wears another shim's prefix. Those are three
//! agent-level facts, not a frame shape, and a row cannot hold them honestly.

use crate::stream::{MarionCall, StreamOutcome};

/// How one harness's stream shows marion's verbs being called, answered and failed.
#[derive(Debug)]
pub struct StreamGrammar {
    /// The unit that is a call to some tool.
    pub call: Where,
    /// Where the tool's name sits in a call unit, and how marion's verb is read out of it.
    pub name: Name,
    /// Where the call's arguments object sits in a call unit.
    pub args: &'static str,
    /// How the call's verdict reaches it.
    pub pairing: Pairing,
    /// What a refused `report` means for the run.
    pub refused_report: OnRefusedReport,
    /// The stream's own failure claims, in order. **The first claim wins**; a refused `report`
    /// under [`OnRefusedReport::Fail`] / [`OnRefusedReport::FailWithoutNarrative`] overrides it,
    /// because the most specific claim about marion's own call is the one worth recording.
    pub failures: &'static [Failure],
    /// Where the harness announces the files it changed, if it does. Corroboration only.
    pub file_changes: Option<PathList>,
}

/// A set of JSON units inside a stream: frames of a shape, or elements of an array in them.
///
/// Values are compared as text so that a boolean or a number can be matched (`("/is_error",
/// "true")`, `("/exitCode", "0")`).
#[derive(Debug)]
pub struct Where {
    /// `(pointer, value)` pairs the **frame** must satisfy.
    pub frame: &'static [(&'static str, &'static str)],
    /// `None`: the frame is the unit. `Some(ptr)`: each element of the array at `ptr` is a unit.
    pub each: Option<&'static str>,
    /// `(pointer, value)` pairs the **unit** must satisfy.
    pub unit: &'static [(&'static str, &'static str)],
}

/// Where marion's verb is read from.
#[derive(Debug)]
pub enum Name {
    /// A string in this harness's spelling of marion's tools; the verb is what follows the
    /// spelling's prefix, and a name without that prefix is not marion's.
    Prefixed(&'static str),
    /// The bare verb — the harness names server and tool as two fields, and [`Where`] already
    /// selected marion's server.
    Verb(&'static str),
}

/// How a call's verdict reaches it.
#[derive(Debug)]
pub enum Pairing {
    /// The verdict is on the call unit itself. With an `id`, a later unit for the same id
    /// **revises** the earlier one in place (codex's `item.started` / `item.completed`); without
    /// one, every unit is its own call (opencode's terminal-only `tool_use`).
    SameUnit {
        id: Option<&'static str>,
        verdict: Verdict,
    },
    /// A separate result unit, paired back by id. A call whose result never arrived is
    /// `CallOutcome::Unknown` — a run killed mid-call leaves exactly that trace.
    Separate {
        call_id: &'static str,
        result: Where,
        result_id: &'static str,
        verdict: Verdict,
    },
}

/// The shapes a verdict was measured to take. Every variant reads its refusal's words from
/// `words` — the first pointer that holds a non-empty string, or an array of text blocks joined.
#[derive(Debug)]
pub enum Verdict {
    /// A status string: `ok` is `CallOutcome::Answered`; a `pending` value or a missing status
    /// is `CallOutcome::Unknown`; **anything else is a refusal** spelled `"<status>: <words>"`,
    /// or `"<status>"` alone — a stream saying something unmeasured must not be read as consent.
    Status {
        path: &'static str,
        ok: &'static str,
        pending: &'static [&'static str],
        words: &'static [&'static str],
    },
    /// A terminal state on the call unit: `ok` is answered, `err` is a refusal with `words` (or
    /// `fallback`), and anything else is unknown — this harness emits only terminal states, so a
    /// third spelling is one it has not been measured emitting.
    Terminal {
        path: &'static str,
        ok: &'static str,
        err: &'static str,
        words: &'static [&'static str],
        fallback: &'static str,
    },
    /// A boolean: `true` is answered, `false` a refusal with `words` (or `fallback`), missing is
    /// unknown.
    Success {
        path: &'static str,
        words: &'static [&'static str],
        fallback: &'static str,
    },
    /// An error flag: `true` is a refusal with `words` (or `fallback`); **missing or false is
    /// answered** — on this shape an absent key is the harness saying the call was fine, and
    /// reading it as a refusal would red-line every working run.
    ErrorFlag {
        path: &'static str,
        words: &'static [&'static str],
        fallback: &'static str,
    },
}

/// What a refused `report` means for the run's [`StreamOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnRefusedReport {
    /// Recorded on the call and nowhere else: the stream's own failure frames decide.
    Record,
    /// The run failed, in the words of the refusal; the narrative the call carried is kept.
    Fail,
    /// The run failed, and the refused call is not a report: its arguments are not read.
    FailWithoutNarrative,
}

/// One kind of failure claim a stream can make.
#[derive(Debug)]
pub enum Failure {
    /// A unit of this shape is a failure claim; its message is the first non-empty of `words`,
    /// else `fallback`.
    Frame {
        at: Where,
        words: &'static [&'static str],
        fallback: &'static str,
    },
    /// A unit of this shape whose value at `path` is present and not `ok` is a failure claim; its
    /// message is the first non-empty of `words`, else `label` followed by the value.
    NotOk {
        at: Where,
        path: &'static str,
        ok: &'static str,
        words: &'static [&'static str],
        label: &'static str,
    },
}

/// Where a harness lists the paths it changed: the array at `list` in each unit of `at`, each
/// element's path at `path`.
#[derive(Debug)]
pub struct PathList {
    pub at: Where,
    pub list: &'static str,
    pub path: &'static str,
}

/// Every call to one of marion's verbs the stream shows, with what came of each — in
/// **marion's** vocabulary. `prefix` is this harness's spelling of marion's tools with the verb
/// left off (`marion_tool_name("")`).
pub fn marion_calls(g: &StreamGrammar, stdout: &str, prefix: &str) -> Vec<MarionCall> {
    let _ = (g, stdout, prefix);
    todo!("step 6: the grammar engine")
}

/// §6.1 step 9's fold over the stream: the `report`, the commits, the failure claim.
pub fn parse_stream(g: &StreamGrammar, stdout: &str, prefix: &str) -> StreamOutcome {
    let _ = (g, stdout, prefix);
    todo!("step 6: the grammar engine")
}
