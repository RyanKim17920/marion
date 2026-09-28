//! **What a report says**, as plain data: read by [`super::collect`], cleaned by
//! [`Report::scrub`], drawn by the renderers.
//!
//! Every field is text or a number a renderer prints as it is, so the renderers decide layout and
//! nothing else. Tokens only, never a price (see `home::mod`).

use marion_core::contract::TokenUsage;
use marion_core::encoding::SystemTime;
use marion_core::proto::result::{ActionLine, DiffStat, MessageLine, TaskSent};
use serde::{Deserialize, Serialize};

use super::scrub::Scrubber;
use crate::rollup::Totals;

/// Which file a report is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Html,
}

impl Format {
    /// The format `path`'s extension names — `.html`/`.htm` is HTML, anything else Markdown.
    pub fn of_path(path: &std::path::Path) -> Format {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("html" | "htm") => Format::Html,
            _ => Format::Markdown,
        }
    }
}

/// How much of each node's activity a report shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineMode {
    /// Every action, runs of one verb merged.
    All,
    /// The first [`HEAD_ACTIONS`] and the last `n`, runs of one verb merged, the middle counted.
    Condensed(usize),
}

/// How many of a condensed timeline's first actions are kept: how the node started.
pub const HEAD_ACTIONS: usize = 5;
/// How many of its last actions are kept by default: how it ended.
pub const TAIL_ACTIONS: usize = 30;

/// What the operator chose to include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportOpts {
    /// The root's own prompt — the operator's words, withheld unless asked for.
    pub include_prompt: bool,
    /// Each landed child's full diff, beside its stat.
    pub full_diff: bool,
    pub timeline: TimelineMode,
}

impl Default for ExportOpts {
    fn default() -> Self {
        ExportOpts {
            include_prompt: false,
            full_diff: false,
            timeline: TimelineMode::Condensed(TAIL_ACTIONS),
        }
    }
}

/// One delegation tree, as a report shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The node the report was asked for, as its tree row names it.
    pub target: String,
    pub generated_at: SystemTime,
    /// The marion that wrote it.
    pub version: String,
    /// The repository, as a path (`~` once scrubbed).
    pub project: String,
    /// The tree, one row per node, drawn with box-drawing characters.
    pub tree: Vec<String>,
    /// Depth-first, the target first.
    pub nodes: Vec<NodeReport>,
    pub totals: Totals,
}

/// One node of the tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeReport {
    /// The id a tree row shows ([`crate::tree::short_id`]).
    pub short: String,
    /// [`crate::tree::label_of`]: its name, else its agent type and short id.
    pub label: String,
    /// The parent's short id; `None` for the target.
    pub parent: Option<String>,
    /// Below the target: 0 for the target itself.
    pub depth: usize,
    /// The harness's command name (`claude`, `codex`).
    pub harness: String,
    pub agent_type: String,
    pub model: Option<String>,
    /// [`crate::tree::state_label`]'s word, as the journal last recorded it.
    pub status: String,
    /// What it was asked, as it received it.
    pub task: Option<TaskSent>,
    /// A root's prompt that was left out because `--include-prompt` was not given.
    pub task_withheld: bool,
    /// The messages queued for it — who, when, how long, what came of each; never the text.
    pub steers: Vec<MessageLine>,
    pub timeline: Timeline,
    /// Its verification commands, as they ran.
    pub checks: Vec<CheckLine>,
    /// How its review ended, in one line, when its agent type asked for one.
    pub review: Option<String>,
    /// What it reported in its own words.
    pub narrative: Option<String>,
    /// Whether marion wrote [`Self::narrative`] because the node reported none.
    pub synthesized: bool,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub diff: Option<DiffStat>,
    pub changed_paths: Vec<String>,
    /// Changed paths the contract's cap left out.
    pub changed_omitted: usize,
    /// Only with `--full-diff`.
    pub full_diff: Option<String>,
    pub usage: Option<TokenUsage>,
    /// How many turns its runs recorded a spend for.
    pub turns: usize,
    pub started: Option<SystemTime>,
    pub ended: Option<SystemTime>,
    /// Why it did not end well, where anything says.
    pub failure: Option<String>,
}

/// What a node did, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timeline {
    /// The actions shown first; every one under [`TimelineMode::All`]. Each `at` is the time since
    /// the node started (`+03:12`).
    pub head: Vec<ActionLine>,
    /// How many actions between `head` and `tail` a condensed timeline left out.
    pub elided: usize,
    pub tail: Vec<ActionLine>,
    /// Why nothing is shown, when marion cannot read this harness's stream.
    pub unread: Option<String>,
}

/// One verification command, as it ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckLine {
    pub command: String,
    /// `None` when it was signalled.
    pub exit: Option<i32>,
    pub ms: u64,
    pub timed_out: bool,
    /// The end of what a failing check wrote — stderr, else stdout.
    pub output: Option<String>,
}

impl Report {
    /// **The one scrub point.** Every string the report holds — whatever field it is in, including
    /// one added after this was written — passes through `scrubber`: the report is walked as the
    /// JSON it serializes to, every string cleaned, and read back. A field cannot be forgotten
    /// here by a later change, because nothing here names a field.
    pub fn scrub(self, scrubber: &Scrubber) -> Result<Report, String> {
        let mut value =
            serde_json::to_value(&self).map_err(|e| format!("reading the report to scrub: {e}"))?;
        clean_strings(&mut value, scrubber);
        // Reached only if cleaning changed a timestamp so it no longer parses; the report is then
        // refused rather than written half-cleaned.
        serde_json::from_value(value).map_err(|e| format!("a scrubbed report no longer reads: {e}"))
    }
}

fn clean_strings(v: &mut serde_json::Value, scrubber: &Scrubber) {
    use serde_json::Value;
    match v {
        Value::String(s) => *s = scrubber.clean(s),
        Value::Array(items) => items.iter_mut().for_each(|i| clean_strings(i, scrubber)),
        Value::Object(map) => map.values_mut().for_each(|i| clean_strings(i, scrubber)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
