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

use std::collections::BTreeMap;

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
}

/// Where a harness states its session id: the string at `path` in each unit of `at`. The first
/// unit that carries one is the node's session; the supervisor journals it once.
#[derive(Debug)]
pub struct SessionId {
    pub at: Where,
    pub path: &'static str,
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

impl Verdict {
    fn read(&self, unit: &Value) -> CallOutcome {
        let refused = |words_at: &[&str], fallback: &str| {
            CallOutcome::Refused(words(unit, words_at).unwrap_or_else(|| fallback.to_string()))
        };
        match self {
            Verdict::Status {
                path,
                ok,
                pending,
                words: w,
            } => match text(unit, path) {
                Some(s) if s == *ok => CallOutcome::Answered,
                Some(s) if pending.contains(&s.as_str()) => CallOutcome::Unknown,
                None => CallOutcome::Unknown,
                Some(status) => CallOutcome::Refused(match words(unit, w) {
                    Some(e) => format!("{status}: {e}"),
                    None => status,
                }),
            },
            Verdict::Terminal {
                path,
                ok,
                err,
                words: w,
                fallback,
            } => match text(unit, path) {
                Some(s) if s == *ok => CallOutcome::Answered,
                Some(s) if s == *err => refused(w, fallback),
                _ => CallOutcome::Unknown,
            },
            Verdict::Success {
                path,
                words: w,
                fallback,
            } => match unit.pointer(path).and_then(Value::as_bool) {
                Some(true) => CallOutcome::Answered,
                Some(false) => refused(w, fallback),
                None => CallOutcome::Unknown,
            },
            Verdict::ErrorFlag {
                path,
                words: w,
                fallback,
            } => match unit.pointer(path).and_then(Value::as_bool) {
                Some(true) => refused(w, fallback),
                _ => CallOutcome::Answered,
            },
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
        // Stream order, every rule per frame: the first claim the stream makes is the one recorded.
        failure: frames
            .iter()
            .find_map(|frame| g.failures.iter().find_map(|f| f.claim(frame))),
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
