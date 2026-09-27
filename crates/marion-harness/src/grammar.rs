//! The stream grammar: **one reader for every harness's output stream, driven by a row of data.**
//!
//! §6.1 step 9 reads a child's own stream for three things — the `report` it made, what became of
//! each call to marion, and whether the stream itself claimed failure — and five harnesses put
//! those three things in five places with five spellings. Until this module each harness had its
//! own ~70-line reader that walked frames, matched a type, found a name, paired a result and
//! folded a `StreamOutcome`; they differed in *where* and *what*, never in *how*. So the how is
//! [`parse_stream`] and [`marion_calls`], once, and the where and what are a [`StreamGrammar`]
//! row per harness beside its launch row. A row also says where the harness states the tokens the
//! run spent ([`UsageRule`], read by [`usage`]) — the same pointers-over-units data, one reader.
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

use std::collections::BTreeMap;

use marion_core::TokenUsage;
use serde_json::Value;

use crate::stream::{CallOutcome, MarionCall, StreamOutcome, json_frames, report_commits};

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
    /// Where the harness names its own session — the id its `resume` grammar
    /// ([`crate::spec::HarnessSpec::resume`]) takes back. `None` where no frame was measured
    /// carrying one, and a resume of such a node is refused rather than guessed. Read by
    /// [`session_id`], once per frame, by whoever owns the node's stream.
    pub session: Option<SessionId>,
    /// Where the harness states the tokens the run spent — read by [`usage`]. `None` where no
    /// frame was measured carrying a token count, and the row says why beside it.
    pub usage: Option<UsageRule>,
    /// Where the harness shows what a node is doing — **every** tool it calls, not only marion's,
    /// and the words it writes — read by [`recent_activity`] for a peek at a running node. `None`
    /// where no frame was measured carrying either.
    pub activity: Option<ActivityRule>,
}

/// Where a harness shows a node's own activity: the units that are a call to any tool, and the
/// units that carry the model's text. A list of each because one harness spells several kinds of
/// work as several unit shapes (codex's `mcp_tool_call`, `command_execution`, `file_change` items).
#[derive(Debug)]
pub struct ActivityRule {
    pub calls: &'static [ToolUnit],
    pub text: &'static [TextUnit],
}

/// A unit that is a call to some tool: the tool's name at `name`, what it was given at `args`.
/// With an `id`, a later unit for the same id is the same call seen again (codex's `item.started`
/// then `item.completed`) and is not counted twice.
#[derive(Debug)]
pub struct ToolUnit {
    pub at: Where,
    pub name: &'static str,
    pub args: &'static str,
    pub id: Option<&'static str>,
    /// What the unit's arguments are, so a reader can word the call without knowing the harness.
    pub shape: CallShape,
}

/// What a call unit's arguments hold. Most harnesses spell every kind of work as a named tool with
/// an argument object; some give a shell command or a file change a unit shape of its own, whose
/// "name" is only the shape's tag (codex's `command_execution`, `file_change`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallShape {
    /// A named tool and its argument object: the name is the verb.
    Tool,
    /// The arguments are the shell command it ran (a string, or an argv array).
    Command,
    /// The arguments list the files it changed (strings, or objects with a `path`).
    Files,
}

/// A unit carrying the model's words: the string at `path`. `joins` where the harness streams one
/// message as consecutive deltas (gemini's `"delta":true`), so adjacent units are one message
/// rather than each replacing the last.
#[derive(Debug)]
pub struct TextUnit {
    pub at: Where,
    pub path: &'static str,
    pub joins: bool,
}

/// One tool call as [`recent_activity`] read it: its name as the harness spelled it, and its
/// arguments whole — shortening is the renderer's decision, not the reader's.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub args: Value,
    /// Its unit's [`CallShape`].
    pub shape: CallShape,
}

/// What a stream shows a node doing most recently: its last calls, oldest first, and the last line
/// of text it wrote.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecentActivity {
    pub calls: Vec<ToolCall>,
    pub text: Option<String>,
}

/// Where a harness states its session id: the string at `path` in each unit of `at`. The first
/// unit that carries one is the node's session; the supervisor journals it once.
#[derive(Debug)]
pub struct SessionId {
    pub at: Where,
    pub path: &'static str,
    /// A resumed launch was **measured** naming the session it resumed in this same unit, so a
    /// different id is a fresh session the harness started instead — agy 1.2.8 answers an unknown
    /// `--conversation` that way, at exit 0 (s32). `false` where that was not measured, and no
    /// resume is checked: a harness that mints a new id per resumed turn would otherwise fail
    /// every resume it ever ran.
    pub resumes_in_place: bool,
}

/// A set of JSON units inside a stream: frames of a shape, or elements of an array in them.
///
/// Values are compared as text so that a boolean or a number can be matched (`("/is_error",
/// "true")`, `("/exitCode", "0")`).
#[derive(Debug)]
pub struct Where {
    /// What the **frame** must satisfy.
    pub frame: &'static [Cond],
    /// `None`: the frame is the unit. `Some(ptr)`: each element of the array at `ptr` is a unit.
    pub each: Option<&'static str>,
    /// What the **unit** must satisfy.
    pub unit: &'static [Cond],
}

/// One condition on a JSON value.
#[derive(Debug)]
pub enum Cond {
    /// The text at the pointer equals the value.
    Eq(&'static str, &'static str),
    /// Something sits at the pointer, whatever it is — gemini's untyped `{"error":{…}}` body.
    Has(&'static str),
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
    /// [`Self::Terminal`], except that `ok` **answers only when the unit also carries `output`**.
    /// agy 1.2.8 marks a call it auto-denied headless either `ERROR` with a message or `DONE` with
    /// no output at all (s32), so the state alone is not an answer: `ok` without `output` is a
    /// refusal spelled `silent`.
    TerminalWithOutput {
        path: &'static str,
        ok: &'static str,
        err: &'static str,
        output: &'static str,
        words: &'static [&'static str],
        fallback: &'static str,
        silent: &'static str,
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

/// Where a harness states the tokens a run spent, and how its units add up to the run.
///
/// Pointers into each unit of `at`, read as unsigned integers; a counter the rule does not name,
/// or a unit does not carry, reads as zero. The reading is normalised to [`TokenUsage`]'s
/// convention — `input` is uncached input — so a consumer sums runs of different harnesses
/// without knowing which harness counted how.
#[derive(Debug)]
pub struct UsageRule {
    /// The units that carry usage counters.
    pub at: Where,
    pub input: &'static str,
    pub output: &'static str,
    pub cache_read: Option<&'static str>,
    pub cache_write: Option<&'static str>,
    /// A counter the harness reports **beside** `output` although the model generated it — its
    /// reasoning tokens — which the reader adds into output, so that output counts every generated
    /// token as codex's and claude's counters already do. `None` where `output` includes them or
    /// no split was measured. opencode's `tokens.reasoning` (s36: `output` 43 + `reasoning` 7 of
    /// the provider's 50 completion tokens).
    pub reasoning: Option<&'static str>,
    /// The harness's `input` already counts its cache reads (codex's `input_tokens` does), so the
    /// reader subtracts `cache_read` from it. Cache *writes* are not subtracted: no harness was
    /// measured folding them into input with a non-zero write count to prove it.
    pub input_includes_cache: bool,
    pub fold: UsageFold,
}

/// How a stream's usage units make up the run's usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageFold {
    /// The final unit is already the whole run (a terminal `result` frame).
    Last,
    /// Each unit is one turn's or step's spend, and the run is their sum.
    Sum,
}

/// A counter at `ptr` as an unsigned integer, zero when absent or not one.
fn counter(unit: &Value, ptr: Option<&str>) -> u64 {
    ptr.and_then(|p| text(unit, p))
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

/// The tokens `frames` say the run spent under `rule`. `None` when **no unit matched** — a stream
/// that never reached its usage frame made no claim about spend, and zero would be one. A unit of
/// zeros is `Some` of zero: a run that reported spending nothing did report.
pub fn usage(rule: &UsageRule, frames: &[Value]) -> Option<TokenUsage> {
    fold_usage(rule, &usage_units(rule, frames))
}

/// Every usage unit in `frames` under `rule`, oldest first, each as the counts it states. Units
/// are read frame by frame, so a caller reading a stream in pieces may read each piece's units and
/// append them: the run's usage is then [`fold_usage`] over the whole list.
pub fn usage_units(rule: &UsageRule, frames: &[Value]) -> Vec<TokenUsage> {
    units(frames, &rule.at)
        .into_iter()
        .map(|unit| {
            let cache_read = counter(unit, rule.cache_read);
            let input = counter(unit, Some(rule.input));
            TokenUsage {
                input: if rule.input_includes_cache {
                    input.saturating_sub(cache_read)
                } else {
                    input
                },
                output: counter(unit, Some(rule.output))
                    .saturating_add(counter(unit, rule.reasoning)),
                cache_read,
                cache_write: counter(unit, rule.cache_write),
            }
        })
        .collect()
}

/// The run's usage from its units under the row's fold. `None` for no units.
pub fn fold_usage(rule: &UsageRule, units: &[TokenUsage]) -> Option<TokenUsage> {
    match rule.fold {
        UsageFold::Last => units.last().copied(),
        UsageFold::Sum => units.iter().copied().reduce(|a, b| a + b),
    }
}

/// Each turn's (or step's) total spend, oldest first, where the row's units are turns — a `Sum`
/// row. Empty for a `Last` row, whose units are running totals rather than turns.
pub fn turns(rule: &UsageRule, units: &[TokenUsage]) -> Vec<u64> {
    match rule.fold {
        UsageFold::Sum => units.iter().map(TokenUsage::total).collect(),
        UsageFold::Last => Vec::new(),
    }
}

/// One call as the grammar read it: the verb, its verdict, and the arguments it carried.
struct Call {
    verb: String,
    outcome: CallOutcome,
    args: Value,
}

/// The value at `ptr` as text: a string as itself, a boolean or number spelled out, anything else
/// `None`. What [`Where`] compares and what a [`Verdict`] reads.
fn text(v: &Value, ptr: &str) -> Option<String> {
    match v.pointer(ptr)? {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The words a unit carries: the first pointer holding a non-empty string, or a non-empty array of
/// text blocks joined with a space (Claude Code's `tool_result.content`).
///
/// Harness error frames nest their message differently (opencode `error.data.message`, gemini
/// `error.message` at two depths), and a reader that guessed one shape would record an empty
/// failure string for the others — which reads as "no failure" downstream.
fn words(v: &Value, ptrs: &[&str]) -> Option<String> {
    ptrs.iter().find_map(|p| match v.pointer(p)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
            (!texts.is_empty()).then(|| texts.join(" "))
        }
        _ => None,
    })
}

fn matches(v: &Value, conds: &[Cond]) -> bool {
    conds.iter().all(|c| match c {
        Cond::Eq(ptr, want) => text(v, ptr).as_deref() == Some(*want),
        Cond::Has(ptr) => v.pointer(ptr).is_some(),
    })
}

/// The units of `w` in `frames`, in stream order.
fn units<'a>(frames: &'a [Value], w: &Where) -> Vec<&'a Value> {
    let mut out = Vec::new();
    for frame in frames.iter().filter(|f| matches(f, w.frame)) {
        match w.each {
            None => out.push(frame),
            Some(ptr) => out.extend(
                frame
                    .pointer(ptr)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            ),
        }
    }
    out.retain(|u| matches(u, w.unit));
    out
}

/// The refusal a unit spells: the first non-empty of `words_at`, else the row's `fallback` —
/// never an empty string, which reads as "no refusal" downstream.
fn refused(unit: &Value, words_at: &[&str], fallback: &str) -> CallOutcome {
    CallOutcome::Refused(words(unit, words_at).unwrap_or_else(|| fallback.to_string()))
}

/// [`Verdict::Status`]: `ok` is answered, a `pending` or missing status is unknown, and
/// **anything else is a refusal** spelled `"<status>: <words>"`, or `"<status>"` alone.
fn read_status(
    unit: &Value,
    path: &str,
    ok: &str,
    pending: &[&str],
    words_at: &[&str],
) -> CallOutcome {
    match text(unit, path) {
        Some(s) if s == ok => CallOutcome::Answered,
        Some(s) if pending.contains(&s.as_str()) => CallOutcome::Unknown,
        None => CallOutcome::Unknown,
        Some(status) => CallOutcome::Refused(match words(unit, words_at) {
            Some(e) => format!("{status}: {e}"),
            None => status,
        }),
    }
}

/// [`Verdict::Terminal`]: `ok` is answered, `err` a refusal, and a third spelling is one this
/// harness has not been measured emitting.
fn read_terminal(
    unit: &Value,
    path: &str,
    ok: &str,
    err: &str,
    words_at: &[&str],
    fallback: &str,
) -> CallOutcome {
    match text(unit, path) {
        Some(s) if s == ok => CallOutcome::Answered,
        Some(s) if s == err => refused(unit, words_at, fallback),
        _ => CallOutcome::Unknown,
    }
}

/// [`Verdict::TerminalWithOutput`]: [`read_terminal`], with an `ok` that carries no `output`
/// read as the `silent` refusal.
fn read_terminal_with_output(unit: &Value, v: &TerminalRead<'_>) -> CallOutcome {
    match text(unit, v.path) {
        Some(s) if s == v.ok && unit.pointer(v.output).is_some_and(|o| !o.is_null()) => {
            CallOutcome::Answered
        }
        Some(s) if s == v.ok => CallOutcome::Refused(v.silent.to_string()),
        Some(s) if s == v.err => refused(unit, v.words, v.fallback),
        _ => CallOutcome::Unknown,
    }
}

/// [`Verdict::TerminalWithOutput`]'s fields, borrowed, so its reader takes one argument.
struct TerminalRead<'a> {
    path: &'a str,
    ok: &'a str,
    err: &'a str,
    output: &'a str,
    words: &'a [&'a str],
    fallback: &'a str,
    silent: &'a str,
}

/// [`Verdict::Success`]: `true` is answered, `false` a refusal, missing unknown.
fn read_success(unit: &Value, path: &str, words_at: &[&str], fallback: &str) -> CallOutcome {
    match unit.pointer(path).and_then(Value::as_bool) {
        Some(true) => CallOutcome::Answered,
        Some(false) => refused(unit, words_at, fallback),
        None => CallOutcome::Unknown,
    }
}

/// [`Verdict::ErrorFlag`]: `true` is a refusal; **missing or false is answered**.
fn read_error_flag(unit: &Value, path: &str, words_at: &[&str], fallback: &str) -> CallOutcome {
    match unit.pointer(path).and_then(Value::as_bool) {
        Some(true) => refused(unit, words_at, fallback),
        _ => CallOutcome::Answered,
    }
}

impl Verdict {
    /// The verdict this shape reads off one call unit. One reader per variant.
    fn read(&self, unit: &Value) -> CallOutcome {
        match self {
            Verdict::Status {
                path,
                ok,
                pending,
                words: w,
            } => read_status(unit, path, ok, pending, w),
            Verdict::Terminal {
                path,
                ok,
                err,
                words: w,
                fallback,
            } => read_terminal(unit, path, ok, err, w, fallback),
            Verdict::TerminalWithOutput {
                path,
                ok,
                err,
                output,
                words: w,
                fallback,
                silent,
            } => read_terminal_with_output(
                unit,
                &TerminalRead {
                    path,
                    ok,
                    err,
                    output,
                    words: w,
                    fallback,
                    silent,
                },
            ),
            Verdict::Success {
                path,
                words: w,
                fallback,
            } => read_success(unit, path, w, fallback),
            Verdict::ErrorFlag {
                path,
                words: w,
                fallback,
            } => read_error_flag(unit, path, w, fallback),
        }
    }
}

/// Every call to marion the stream shows, in the order the node made them, each with its verdict
/// and arguments. The one walk both public readers are built on.
fn calls(g: &StreamGrammar, frames: &[Value], prefix: &str) -> Vec<Call> {
    let verb_of = |unit: &Value| -> Option<String> {
        match g.name {
            Name::Prefixed(ptr) => unit
                .pointer(ptr)?
                .as_str()?
                .strip_prefix(prefix)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
            Name::Verb(ptr) => unit.pointer(ptr)?.as_str().map(str::to_string),
        }
    };
    let args_of = |unit: &Value| unit.pointer(g.args).cloned().unwrap_or(Value::Null);
    let call_units = units(frames, &g.call);
    match &g.pairing {
        // Insertion-ordered by first sighting, so the calls come back in the order the node made
        // them while a later unit for the same id revises the outcome in place.
        Pairing::SameUnit { id, verdict } => {
            let mut order: Vec<String> = Vec::new();
            let mut by_key: BTreeMap<String, Call> = BTreeMap::new();
            for unit in call_units {
                let Some(verb) = verb_of(unit) else { continue };
                // A unit with no id is its own call: nothing can revise it, and dropping it would
                // lose a reached verb over a missing field.
                let key = id
                    .and_then(|ptr| text(unit, ptr))
                    .unwrap_or_else(|| format!("{}-{}", verb, order.len()));
                let call = Call {
                    verb,
                    outcome: verdict.read(unit),
                    args: args_of(unit),
                };
                if by_key.insert(key.clone(), call).is_none() {
                    order.push(key);
                }
            }
            order
                .into_iter()
                .filter_map(|k| by_key.remove(&k))
                .collect()
        }
        // Results first, because a stream is read once and the results trail the calls; the last
        // result for an id wins.
        Pairing::Separate {
            call_id,
            result,
            result_id,
            verdict,
        } => {
            let mut results: BTreeMap<String, CallOutcome> = BTreeMap::new();
            for unit in units(frames, result) {
                if let Some(id) = text(unit, result_id) {
                    results.insert(id, verdict.read(unit));
                }
            }
            call_units
                .into_iter()
                .filter_map(|unit| {
                    let verb = verb_of(unit)?;
                    let outcome = text(unit, call_id)
                        .and_then(|id| results.get(&id).cloned())
                        .unwrap_or(CallOutcome::Unknown);
                    Some(Call {
                        verb,
                        outcome,
                        args: args_of(unit),
                    })
                })
                .collect()
        }
    }
}

/// The session id `frame` carries under this row's [`StreamGrammar::session`], if it is the unit
/// that carries one. Per frame rather than per stream because the owner of a live node reads its
/// frames as they arrive and journals the id on **first sighting**, while the process is still
/// running — a stream read whole after exit would name the session only of a node that has already
/// gone. `None` on a row with no measured session unit, and on every frame that is not it.
pub fn session_id(g: &StreamGrammar, frame: &Value) -> Option<String> {
    let s = g.session.as_ref()?;
    units(std::slice::from_ref(frame), &s.at)
        .into_iter()
        .find_map(|u| text(u, s.path).filter(|id| !id.trim().is_empty()))
}

/// The last `max_calls` tool calls `frames` show, oldest first, and the last non-empty line of the
/// last text the model wrote — read in stream order under `rule`, the same pointers-over-units
/// walk [`usage`] and [`session_id`] take, so no harness is named here.
///
/// Within one frame the rule's text units are read before its call units: a frame that carries
/// both (Claude Code's `assistant` content) says the words that led to the call.
pub fn recent_activity(rule: &ActivityRule, frames: &[Value], max_calls: usize) -> RecentActivity {
    let items = activity_stream(rule, frames);
    let said = items.iter().rev().find_map(|i| match &i.item {
        Activity::Said(t) => Some(t.clone()),
        Activity::Call(_) => None,
    });
    let mut calls: Vec<ToolCall> = items
        .into_iter()
        .filter_map(|i| match i.item {
            Activity::Call(c) => Some(c),
            Activity::Said(_) => None,
        })
        .collect();
    let keep = calls.len().saturating_sub(max_calls);
    RecentActivity {
        calls: calls.split_off(keep),
        text: said.and_then(|t| {
            t.lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string)
        }),
    }
}

/// One thing a stream shows a node doing, in the order it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Activity {
    /// A tool call, counted once however many frames repeat it (a start and its completion).
    Call(ToolCall),
    /// Text it wrote: consecutive joining units (deltas) are one item.
    Said(String),
}

/// An [`Activity`] and the index of the frame it was read from, so a caller can time it.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityItem {
    pub frame: usize,
    pub item: Activity,
}

/// **Everything** a stream shows a node doing under `rule`, oldest first — the whole sequence that
/// [`recent_activity`] keeps only the end of. A call whose id was already seen in `frames` is not
/// repeated; a text unit that joins (a delta) extends the text item before it, unless a call came
/// between them.
pub fn activity_stream(rule: &ActivityRule, frames: &[Value]) -> Vec<ActivityItem> {
    let mut items: Vec<ActivityItem> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Whether the last thing read was a joining text unit, so the next one continues it.
    let mut joining = false;
    for (index, frame) in frames.iter().enumerate() {
        let one = std::slice::from_ref(frame);
        for t in rule.text {
            for unit in units(one, &t.at) {
                let Some(s) = unit.pointer(t.path).and_then(Value::as_str) else {
                    continue;
                };
                if s.trim().is_empty() {
                    continue;
                }
                match (items.last_mut(), t.joins && joining) {
                    (
                        Some(ActivityItem {
                            item: Activity::Said(held),
                            ..
                        }),
                        true,
                    ) => held.push_str(s),
                    _ => items.push(ActivityItem {
                        frame: index,
                        item: Activity::Said(s.to_string()),
                    }),
                }
                joining = t.joins;
            }
        }
        for c in rule.calls {
            for unit in units(one, &c.at) {
                let Some(name) = unit
                    .pointer(c.name)
                    .and_then(Value::as_str)
                    .filter(|n| !n.trim().is_empty())
                else {
                    continue;
                };
                if let Some(id) = c.id.and_then(|p| text(unit, p))
                    && !seen.insert(id)
                {
                    continue;
                }
                items.push(ActivityItem {
                    frame: index,
                    item: Activity::Call(ToolCall {
                        name: name.to_string(),
                        args: unit.pointer(c.args).cloned().unwrap_or(Value::Null),
                        shape: c.shape,
                    }),
                });
                joining = false;
            }
        }
    }
    items
}

/// Why a launch that resumed `resumed` did not: the stream's first session unit names another
/// session, on a row whose [`SessionId::resumes_in_place`] was measured. `None` on every other row,
/// and on a stream that named no session at all — that is no claim either way.
pub fn resume_refusal(g: &StreamGrammar, stdout: &str, resumed: &str) -> Option<String> {
    g.session.as_ref().filter(|s| s.resumes_in_place)?;
    let started = json_frames(stdout)
        .iter()
        .find_map(|frame| session_id(g, frame))?;
    (started != resumed).then(|| {
        format!(
            "asked to resume session {resumed}, the harness started session {started} instead: \
             the resumed session was not found, and the run is a fresh one with none of its history"
        )
    })
}

/// Every call to one of marion's verbs the stream shows, with what came of each — in
/// **marion's** vocabulary. `prefix` is this harness's spelling of marion's tools with the verb
/// left off (`marion_tool_name("")`).
pub fn marion_calls(g: &StreamGrammar, stdout: &str, prefix: &str) -> Vec<MarionCall> {
    calls(g, &json_frames(stdout), prefix)
        .into_iter()
        .map(|c| MarionCall {
            verb: c.verb,
            outcome: c.outcome,
        })
        .collect()
}

/// §6.1 step 9's fold over the stream: the `report`, the commits, the failure claim.
///
/// The narrative stays conditional — `None` is load-bearing, it is what `build_contract` turns
/// into `Unreported` — while commits are read whenever a report is seen, since an absent list and
/// an empty one are the same claim. `file_change_paths` is corroboration only: git is the
/// authority for `changed_paths`.
pub fn parse_stream(g: &StreamGrammar, stdout: &str, prefix: &str) -> StreamOutcome {
    let frames = json_frames(stdout);
    let mut out = StreamOutcome {
        failure: first_failure_claim(g, &frames),
        ..StreamOutcome::default()
    };
    for call in calls(g, &frames, prefix)
        .into_iter()
        .filter(|c| c.verb == "report")
    {
        let refused = matches!(call.outcome, CallOutcome::Refused(_));
        if !(refused && g.refused_report == OnRefusedReport::FailWithoutNarrative) {
            if let Some(n) = call.args["narrative"].as_str() {
                out.narrative = Some(n.to_string());
            }
            out.result_commits = report_commits(&call.args);
        }
        if let (
            CallOutcome::Refused(why),
            OnRefusedReport::Fail | OnRefusedReport::FailWithoutNarrative,
        ) = (&call.outcome, g.refused_report)
        {
            out.failure = Some(format!(
                "the child's {prefix}report call ended in error: {why}"
            ));
        }
    }
    if let Some(list) = &g.file_changes {
        for unit in units(&frames, &list.at) {
            for c in unit
                .pointer(list.list)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(p) = c.pointer(list.path).and_then(Value::as_str) {
                    out.file_change_paths.push(p.into());
                }
            }
        }
    }
    out
}

/// The stream's **own** failure claims alone, without [`parse_stream`]'s refused-`report` rule —
/// what a root is judged by. That rule is about a node that has a contract; a root has none (§9),
/// and its refused `report` is §5.4 turning away a call, not the run failing.
pub fn stream_failure(g: &StreamGrammar, stdout: &str) -> Option<String> {
    first_failure_claim(g, &json_frames(stdout))
}

/// Stream order, every rule per frame: the first claim the stream makes is the one recorded.
fn first_failure_claim(g: &StreamGrammar, frames: &[Value]) -> Option<String> {
    frames
        .iter()
        .find_map(|frame| g.failures.iter().find_map(|f| f.claim(frame)))
}

impl Failure {
    /// The failure this frame claims under this rule, in the stream's own words.
    fn claim(&self, frame: &Value) -> Option<String> {
        let at = match self {
            Failure::Frame { at, .. } | Failure::NotOk { at, .. } => at,
        };
        units(std::slice::from_ref(frame), at)
            .into_iter()
            .find_map(|u| match self {
                Failure::Frame {
                    words: w, fallback, ..
                } => Some(words(u, w).unwrap_or_else(|| fallback.to_string())),
                Failure::NotOk {
                    path,
                    ok,
                    words: w,
                    label,
                    ..
                } => match text(u, path) {
                    Some(v) if v != *ok => {
                        Some(words(u, w).unwrap_or_else(|| format!("{label}{v}")))
                    }
                    _ => None,
                },
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rule over a codex-shaped frame: a `done` frame's `usage`, input counted with its cache.
    const RULE: UsageRule = UsageRule {
        at: Where {
            frame: &[Cond::Eq("/type", "done")],
            each: None,
            unit: &[],
        },
        input: "/usage/in",
        output: "/usage/out",
        cache_read: Some("/usage/cached"),
        cache_write: None,
        reasoning: None,
        input_includes_cache: false,
        fold: UsageFold::Last,
    };

    fn done(input: u64, output: u64, cached: u64) -> Value {
        serde_json::json!({"type": "done", "usage": {"in": input, "out": output, "cached": cached}})
    }

    fn tokens(input: u64, output: u64, cache_read: u64) -> TokenUsage {
        TokenUsage {
            input,
            output,
            cache_read,
            cache_write: 0,
        }
    }

    #[test]
    fn a_last_rule_takes_the_final_unit_and_a_sum_rule_adds_every_unit() {
        let frames = [
            done(10, 1, 2),
            serde_json::json!({"type": "other", "usage": {"in": 999}}),
            done(30, 3, 4),
        ];
        // Last: the harness's terminal frame is already a whole-run total.
        assert_eq!(usage(&RULE, &frames), Some(tokens(30, 3, 4)));
        // Sum: each unit is one step's spend, and the run is all of them.
        let sum = UsageRule {
            fold: UsageFold::Sum,
            ..RULE
        };
        assert_eq!(usage(&sum, &frames), Some(tokens(40, 4, 6)));
    }

    /// **Per-turn spend is the units themselves, on a row whose units are turns.** A `Sum` row's
    /// units are each one turn's or step's spend, which is what a sparkline draws; a `Last` row's
    /// units are running totals, so it has no per-turn series to offer and says so with none.
    #[test]
    fn per_turn_spend_is_read_off_a_sum_row_and_never_invented_for_a_last_row() {
        let frames = [done(10, 1, 2), done(30, 3, 4)];
        let sum = UsageRule {
            fold: UsageFold::Sum,
            ..RULE
        };
        let units = usage_units(&sum, &frames);
        assert_eq!(units, vec![tokens(10, 1, 2), tokens(30, 3, 4)]);
        assert_eq!(fold_usage(&sum, &units), usage(&sum, &frames));
        assert_eq!(turns(&sum, &units), vec![13, 37]);
        assert_eq!(
            turns(&RULE, &usage_units(&RULE, &frames)),
            Vec::<u64>::new()
        );
        assert_eq!(fold_usage(&sum, &[]), None, "no unit is no claim, not zero");
    }

    #[test]
    fn input_that_counts_its_cache_is_normalised_to_uncached_input() {
        let rule = UsageRule {
            reasoning: None,
            input_includes_cache: true,
            ..RULE
        };
        // codex's measured shape: 14997 prompt tokens of which 11008 were cache reads.
        assert_eq!(
            usage(&rule, &[done(14997, 5, 11008)]),
            Some(tokens(3989, 5, 11008))
        );
        // A cache count larger than the input it is said to be part of is a harness's bad
        // arithmetic, not a negative number of tokens.
        assert_eq!(usage(&rule, &[done(3, 1, 7)]), Some(tokens(0, 1, 7)));
    }

    #[test]
    fn no_matching_unit_is_no_usage_but_a_zero_unit_is_a_zero_usage() {
        // A stream that never reached its usage frame said nothing about spend; zero would be a
        // claim it did not make.
        assert_eq!(usage(&RULE, &[]), None);
        assert_eq!(usage(&RULE, &[serde_json::json!({"type": "other"})]), None);
        // A canned provider that reports zeros did report: the run spent nothing.
        assert_eq!(usage(&RULE, &[done(0, 0, 0)]), Some(TokenUsage::default()));
    }

    #[test]
    fn a_counter_that_is_absent_or_unmeasured_reads_as_zero() {
        // No cache counters in the rule, none in the frame, and a numeric string where a number
        // was expected: each reads as what the harness can be said to have reported.
        let rule = UsageRule {
            cache_read: None,
            ..RULE
        };
        let frame = serde_json::json!({"type": "done", "usage": {"in": "12", "cached": 5}});
        assert_eq!(
            usage(&rule, std::slice::from_ref(&frame)),
            Some(tokens(12, 0, 0))
        );
        let write = UsageRule {
            cache_write: Some("/usage/written"),
            ..RULE
        };
        assert_eq!(usage(&write, &[frame]), Some(tokens(12, 0, 5)));
    }

    /// A rule over a made-up stream: `call` frames with an id, `say` frames whole, `delta`
    /// frames streamed.
    const ACTIVITY: ActivityRule = ActivityRule {
        calls: &[ToolUnit {
            at: Where {
                frame: &[Cond::Eq("/type", "call")],
                each: None,
                unit: &[],
            },
            name: "/name",
            args: "/args",
            id: Some("/id"),
            shape: CallShape::Tool,
        }],
        text: &[
            TextUnit {
                at: Where {
                    frame: &[Cond::Eq("/type", "say")],
                    each: None,
                    unit: &[],
                },
                path: "/text",
                joins: false,
            },
            TextUnit {
                at: Where {
                    frame: &[Cond::Eq("/type", "delta")],
                    each: None,
                    unit: &[],
                },
                path: "/text",
                joins: true,
            },
        ],
    };

    fn call(id: &str, name: &str) -> Value {
        serde_json::json!({"type": "call", "id": id, "name": name, "args": {"n": id}})
    }

    #[test]
    fn only_the_last_calls_are_kept_oldest_first_and_a_revision_is_not_a_second_call() {
        let frames: Vec<Value> = (1..=7)
            .map(|i| call(&i.to_string(), &format!("t{i}")))
            .chain([call("7", "t7")])
            .collect();
        let a = recent_activity(&ACTIVITY, &frames, 3);
        let names: Vec<&str> = a.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["t5", "t6", "t7"]);
        assert_eq!(a.calls[2].args, serde_json::json!({"n": "7"}));
        assert_eq!(a.text, None, "no text unit, no line — never an empty one");
    }

    #[test]
    fn the_text_is_the_last_line_of_the_last_message_and_deltas_join_into_one() {
        let say = |t: &str| serde_json::json!({"type": "say", "text": t});
        let delta = |t: &str| serde_json::json!({"type": "delta", "text": t});
        // A whole message replaces the last; its last non-empty line is the one kept.
        let a = recent_activity(&ACTIVITY, &[say("first"), say("plan:\nstep two\n\n")], 5);
        assert_eq!(a.text.as_deref(), Some("step two"));
        // Deltas are one message until something else is said between them.
        let a = recent_activity(&ACTIVITY, &[delta("wri"), delta("ting tests")], 5);
        assert_eq!(a.text.as_deref(), Some("writing tests"));
        let a = recent_activity(
            &ACTIVITY,
            &[delta("old"), call("1", "t"), delta("new "), delta("words")],
            5,
        );
        assert_eq!(a.text.as_deref(), Some("new words"));
        // A blank message is not words, and does not erase the last ones.
        let a = recent_activity(&ACTIVITY, &[say("kept"), say("   ")], 5);
        assert_eq!(a.text.as_deref(), Some("kept"));
    }

    /// The whole stream, in order, each item with the frame it came from: every call once, and
    /// deltas joined into one message until a call comes between them.
    #[test]
    fn the_activity_stream_is_every_call_and_message_in_order() {
        let say = |t: &str| serde_json::json!({"type": "say", "text": t});
        let delta = |t: &str| serde_json::json!({"type": "delta", "text": t});
        let frames = [
            say("planning"),
            call("1", "read"),
            call("1", "read"),
            delta("edit"),
            delta("ing"),
            call("2", "write"),
            say("   "),
            delta("done"),
        ];
        let got: Vec<(usize, String)> = activity_stream(&ACTIVITY, &frames)
            .into_iter()
            .map(|i| match i.item {
                Activity::Call(c) => (i.frame, format!("call {}", c.name)),
                Activity::Said(t) => (i.frame, format!("said {t}")),
            })
            .collect();
        assert_eq!(
            got,
            [
                (0, "said planning".to_string()),
                (1, "call read".to_string()),
                (3, "said editing".to_string()),
                (5, "call write".to_string()),
                (7, "said done".to_string()),
            ]
        );
    }

    #[test]
    fn an_error_message_is_read_from_whichever_shape_carries_it() {
        let v: Value = serde_json::json!({"error": {"data": {"message": "bad request"}}});
        assert_eq!(
            words(&v, &["/error/message", "/error/data/message"]).as_deref(),
            Some("bad request")
        );
        // An empty string is not a message: it would read as "a failure with nothing to say".
        let v: Value = serde_json::json!({"error": {"message": "  ", "name": "APIError"}});
        assert_eq!(
            words(&v, &["/error/message", "/error/name"]).as_deref(),
            Some("APIError")
        );
        // Claude Code's `tool_result.content`: an array of typed blocks, or a bare string.
        let v: Value = serde_json::json!({"content": [{"type": "text", "text": "no"}, {"type": "text", "text": "way"}]});
        assert_eq!(words(&v, &["/content"]).as_deref(), Some("no way"));
        let v: Value = serde_json::json!({"content": "refused"});
        assert_eq!(words(&v, &["/content"]).as_deref(), Some("refused"));
        let v: Value = serde_json::json!({"content": []});
        assert_eq!(words(&v, &["/content"]), None);
    }
}
