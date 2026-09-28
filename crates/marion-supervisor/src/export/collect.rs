//! **Reading a report off disk**: the journal once, each node's contract, each node's stream.
//!
//! The same readers the live views use, so a report and a watching operator see one tree: the
//! journal through [`marion_core::registry::replay`], the target through
//! [`crate::tree::resolve_id`] (a whole id or the short id a tree row shows), each node's steers
//! through [`crate::node_detail::messages_by_node`], its task through the contract `node/get`
//! reads, and its activity through [`crate::activity::page`] — the very lines the Watch tab shows.
//!
//! Nothing here is cleaned: [`super::export`] scrubs the whole [`Report`] at one point after this.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use marion_core::contract::{AgentId, ExitStatus, FailureCause, TaskContract};
use marion_core::encoding::SystemTime;
use marion_core::harness::Harness;
use marion_core::node::ReapState;
use marion_core::paths::ProjectDir;
use marion_core::proto::params::ActivityCursor;
use marion_core::proto::result::{ActionKind, ActionLine};
use marion_core::registry::{Replay, ReplayedNode};

use super::model::{
    CheckLine, ExportOpts, HEAD_ACTIONS, NodeReport, Report, Timeline, TimelineMode,
};
use crate::rollup::{Own, Totals};
use crate::tree::short_id;

/// The most lines of a failing check's output a report keeps: the end, where the error is.
const CHECK_OUTPUT_LINES: usize = 12;
/// How many of a merged run's arguments are named before `…`.
const MERGED_ARGS: usize = 2;

/// A report as read, and the credential ids its endpoint nodes ran on — what the scrubber needs to
/// look up the keys to remove. Ids only; never a key.
#[derive(Debug, Clone)]
pub struct Collected {
    pub report: Report,
    pub credentials: Vec<String>,
}

/// The report for the tree under `target` — a whole id or a short id — in the project whose files
/// are under `project`, for the repository `repo`, stamped `now`.
pub fn collect(
    project: &ProjectDir,
    repo: &Path,
    target: &str,
    opts: &ExportOpts,
    now: SystemTime,
) -> Result<Collected, String> {
    let path = project.journal();
    let bytes = std::fs::read(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!(
            "no journal at {}: nothing has run in this repository under this state dir (pass the \
             --repo / --state-dir the run used)",
            path.display()
        ),
        _ => format!("reading {}: {e}", path.display()),
    })?;
    let replay = marion_core::registry::replay(&bytes);
    let known = replay
        .nodes()
        .iter()
        .filter(|n| n.intent.is_some())
        .map(|n| n.agent_id.0.as_str());
    let id = crate::tree::resolve_id(target, known)?;
    let root = replay
        .get(&id)
        .filter(|n| n.intent.is_some())
        .ok_or_else(|| format!("no node `{target}` in this project's journal"))?;

    let mut messages = crate::node_detail::messages_by_node(&bytes);
    let mut nodes = Vec::new();
    let mut tree = Vec::new();
    let mut owns = Vec::new();
    let mut credentials = Vec::new();
    let mut walk = Walk {
        project,
        replay: &replay,
        opts,
        messages: &mut messages,
        nodes: &mut nodes,
        tree: &mut tree,
        owns: &mut owns,
        credentials: &mut credentials,
        seen: HashSet::new(),
    };
    walk.visit(root, None, 0, "", "");
    credentials.sort();
    credentials.dedup();
    let target = nodes.first().map(|n| n.label.clone()).unwrap_or_default();
    Ok(Collected {
        report: Report {
            target,
            generated_at: now,
            version: env!("CARGO_PKG_VERSION").to_string(),
            project: repo.display().to_string(),
            tree,
            totals: Totals::sum(&owns),
            nodes,
        },
        credentials,
    })
}

/// The depth-first walk's state: what it has read so far.
struct Walk<'a> {
    project: &'a ProjectDir,
    replay: &'a Replay,
    opts: &'a ExportOpts,
    messages: &'a mut HashMap<AgentId, Vec<marion_core::proto::result::MessageLine>>,
    nodes: &'a mut Vec<NodeReport>,
    tree: &'a mut Vec<String>,
    owns: &'a mut Vec<Own>,
    credentials: &'a mut Vec<String>,
    /// A journal is foreign data: a parent loop must end the walk, not recurse forever.
    seen: HashSet<AgentId>,
}

impl Walk<'_> {
    /// `node` and everything under it. `lead` is this row's tree prefix (`├── `), `indent` what
    /// its children's rows start with.
    fn visit(
        &mut self,
        node: &ReplayedNode,
        parent: Option<&str>,
        depth: usize,
        lead: &str,
        indent: &str,
    ) {
        if !self.seen.insert(node.agent_id.clone()) {
            return;
        }
        let (report, own) = self.read(node, parent, depth);
        self.tree.push(format!("{lead}{}", tree_row(&report)));
        let short = report.short.clone();
        self.nodes.push(report);
        self.owns.push(own);
        let children = self.replay.children(&node.agent_id);
        for (i, child) in children.iter().enumerate() {
            let last = i + 1 == children.len();
            let (branch, next) = if last {
                ("└── ", "    ")
            } else {
                ("├── ", "│   ")
            };
            self.visit(
                child,
                Some(&short),
                depth + 1,
                &format!("{indent}{branch}"),
                &format!("{indent}{next}"),
            );
        }
    }

    /// One node's section of the report, and what it adds to the totals.
    fn read(
        &mut self,
        node: &ReplayedNode,
        parent: Option<&str>,
        depth: usize,
    ) -> (NodeReport, Own) {
        let intent = node
            .intent
            .as_ref()
            .expect("the walk visits only nodes with an intent");
        let dir = self.project.agent(&node.agent_id);
        let contract = node
            .contracts
            .last()
            .map(|c| &c.task_id)
            .or(intent.task_id.as_ref())
            .and_then(|t| std::fs::read(dir.contract(t)).ok())
            .and_then(|b| serde_json::from_slice::<TaskContract>(&b).ok());
        let completion = contract.as_ref().and_then(|c| c.completion.as_ref());

        // A child's task is its contract's. A root has none; its kept prompt is the operator's own
        // words, and a shared report carries them only when asked to.
        let root_prompt = contract.is_none() && intent.parent_id.is_none();
        let (task, task_withheld) = match &contract {
            Some(c) => (Some(crate::node_detail::task_sent(c)), false),
            None if root_prompt => match std::fs::read_to_string(dir.prompt()) {
                Ok(p) if self.opts.include_prompt => (
                    Some(crate::node_detail::task_of(&p, Vec::new(), Vec::new())),
                    false,
                ),
                Ok(_) => (None, true),
                Err(_) => (None, false),
            },
            None => (None, false),
        };

        self.credentials.extend(node.credential.clone());
        if let Some(c) = &contract {
            self.credentials.extend(c.child.credential.clone());
            for f in &c.child.credential_failover {
                self.credentials.extend([f.from.clone(), f.to.clone()]);
            }
        }

        let (started, ended) = crate::handler::clock(node);
        let short = short_id(&node.agent_id.0).to_string();
        let label = crate::handler::summarize(node, false)
            .map(|s| crate::tree::label_of(&s))
            .unwrap_or_else(|_| format!("{} {short}", intent.agent_type));
        let usage = node.usage.or(completion.and_then(|c| c.usage));
        let changed = completion.map(|c| c.changed_paths.len() + c.changed_paths_omitted);
        let report = NodeReport {
            short,
            label,
            parent: parent.map(str::to_string),
            depth,
            harness: intent.harness.cli_name().to_string(),
            agent_type: intent.agent_type.clone(),
            model: node.model.clone(),
            status: crate::tree::state_label(node.state, node.reap_state),
            task,
            task_withheld,
            steers: self.messages.remove(&node.agent_id).unwrap_or_default(),
            timeline: timeline(&dir.events(), intent.harness, started, self.opts.timeline),
            checks: completion
                .map(|c| c.evidence.iter().map(check_line).collect())
                .unwrap_or_default(),
            review: completion.and_then(|c| c.review.as_ref()).map(review_line),
            narrative: completion.and_then(|c| c.narrative.as_ref().map(|n| n.value.clone())),
            synthesized: completion.is_some_and(|c| c.narrative_synthesized),
            branch: completion.and_then(|c| c.branch.clone()),
            commit: completion.and_then(|c| c.commit.as_ref().map(|o| o.0.clone())),
            diff: contract.as_ref().and_then(crate::node_detail::landed_diff),
            changed_paths: completion
                .map(|c| {
                    c.changed_paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            changed_omitted: completion.map_or(0, |c| c.changed_paths_omitted),
            full_diff: completion
                .filter(|_| self.opts.full_diff)
                .and_then(|c| c.diff.as_ref())
                .map(|d| {
                    let mut text = d.value.clone();
                    if d.truncated {
                        text.push_str(&format!(
                            "\n… (cut at the contract's cap; {} bytes in all)",
                            d.original_bytes
                        ));
                    }
                    text
                }),
            usage,
            turns: node.turns.len(),
            started,
            ended,
            failure: failure(node, contract.as_ref()),
        };
        let own = Own {
            tokens: usage.map(|u| u.total()),
            changed: changed.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            live: !node.state.is_exited() && node.reap_state == ReapState::Live,
            started,
            ended,
        };
        (report, own)
    }
}

/// A node as its tree row: label, state, harness, model, tokens.
fn tree_row(n: &NodeReport) -> String {
    let mut row = format!("{} · {} · {}", n.label, n.status, n.harness);
    if let Some(m) = &n.model {
        row.push_str(&format!(" ({m})"));
    }
    if let Some(u) = n.usage {
        row.push_str(&format!(" · {} tokens", tokens(u.total())));
    }
    row
}

/// A token count as a person reads it: `812`, `12.4k`, `1.3M`.
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Why a node did not end well, where anything recorded says: the harness's own classified words
/// first, then marion's account of the exit, then an aborted spawn's reason.
fn failure(node: &ReplayedNode, contract: Option<&TaskContract>) -> Option<String> {
    if let Some(c) = contract.and_then(|c| c.completion.as_ref()) {
        if let Some(cause) = &c.failure_cause {
            return Some(match cause {
                FailureCause::UsageLimit { line, .. } => format!("usage limit: {line}"),
                FailureCause::RateLimit { line } => format!("rate limit: {line}"),
                FailureCause::Auth { line } => format!("login refused: {line}"),
                FailureCause::Outage { line } => format!("vendor outage: {line}"),
            });
        }
        if c.status != ExitStatus::Ok {
            return Some(c.exit.description.clone());
        }
        return None;
    }
    if let Some(reason) = &node.spawn_aborted {
        return Some(format!("never started: {reason}"));
    }
    match (node.exit_status(), &node.exit) {
        (Some(s), Some(exit)) if s != ExitStatus::Ok => Some(exit.description.clone()),
        _ => None,
    }
}

fn check_line(o: &marion_core::contract::CommandOutcome) -> CheckLine {
    let failed = o.timed_out || o.exit_code != Some(0);
    let output = failed
        .then(|| {
            let text = if o.stderr.value.trim().is_empty() {
                &o.stdout.value
            } else {
                &o.stderr.value
            };
            let lines: Vec<&str> = text.trim_end().lines().collect();
            lines[lines.len().saturating_sub(CHECK_OUTPUT_LINES)..].join("\n")
        })
        .filter(|t| !t.is_empty());
    CheckLine {
        command: std::iter::once(&o.command.program)
            .chain(&o.command.args)
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        exit: o.exit_code,
        ms: u64::try_from(o.duration.0.as_millis()).unwrap_or(u64::MAX),
        timed_out: o.timed_out,
        output,
    }
}

/// A review in one line: how it ended, then each round's decision.
fn review_line(r: &marion_core::review::ReviewRecord) -> String {
    use marion_core::review::{Decision, ReviewOutcome};
    let outcome = match &r.outcome {
        ReviewOutcome::Allowed => "allowed".to_string(),
        ReviewOutcome::Blocked => "blocked".to_string(),
        ReviewOutcome::Skipped { reason } => format!("skipped ({reason})"),
        ReviewOutcome::Errored { reason } => format!("errored ({reason})"),
    };
    if r.rounds.is_empty() {
        return outcome;
    }
    let rounds: Vec<String> = r
        .rounds
        .iter()
        .map(|round| {
            let decision = match round.verdict.decision {
                Decision::Allow => "allow",
                Decision::Block => "block",
            };
            match round.verdict.blocking {
                0 => format!("round {} {decision}", round.round),
                n => format!("round {} {decision} ({n} blocking)", round.round),
            }
        })
        .collect();
    format!("{outcome} · {}", rounds.join(", "))
}

/// What the node whose stream is `events` did, paged through whole with [`crate::activity::page`],
/// runs of one verb merged, timed from `start`, and cut to `mode`.
fn timeline(
    events: &Path,
    harness: Harness,
    start: Option<SystemTime>,
    mode: TimelineMode,
) -> Timeline {
    let mut lines = Vec::new();
    let mut cursor = 0;
    loop {
        let page = crate::activity::page(events, harness, ActivityCursor::From(cursor));
        if let Some(why) = page.unread {
            return Timeline {
                unread: Some(why),
                ..Timeline::default()
            };
        }
        lines.extend(page.lines);
        if page.next <= cursor {
            break;
        }
        cursor = page.next;
    }
    let mut merged = merge_runs(lines);
    for l in &mut merged {
        l.at = relative(&l.at, start);
    }
    match mode {
        TimelineMode::Condensed(tail) if merged.len() > HEAD_ACTIONS + tail => {
            let tail_lines = merged.split_off(merged.len() - tail);
            let elided = merged.len() - HEAD_ACTIONS;
            merged.truncate(HEAD_ACTIONS);
            Timeline {
                head: merged,
                elided,
                tail: tail_lines,
                unread: None,
            }
        }
        _ => Timeline {
            head: merged,
            ..Timeline::default()
        },
    }
}

/// Consecutive calls of one verb as one line — `Read a.rs, b.rs … ×7` — timed at the first. A
/// line of words the node said is never merged.
fn merge_runs(lines: Vec<ActionLine>) -> Vec<ActionLine> {
    // A command's verb is its program (`$ rg`), so `cargo test` is never folded into a run of
    // `rg`s; any other call's is its first word.
    fn verb(l: &ActionLine) -> Option<(&str, &str)> {
        if l.kind != ActionKind::Call {
            return None;
        }
        let words = if l.text.starts_with("$ ") { 2 } else { 1 };
        let cut = l
            .text
            .match_indices(' ')
            .nth(words - 1)
            .map_or(l.text.len(), |(i, _)| i);
        Some((&l.text[..cut], l.text[cut..].trim_start()))
    }
    let mut out: Vec<ActionLine> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let mut j = i + 1;
        if let Some((v, _)) = verb(&lines[i]) {
            while j < lines.len() && verb(&lines[j]).is_some_and(|(w, _)| w == v) {
                j += 1;
            }
        }
        let run = &lines[i..j];
        if run.len() == 1 {
            out.push(run[0].clone());
        } else {
            let v = verb(&run[0]).map_or("", |(v, _)| v);
            let args: Vec<&str> = run
                .iter()
                .filter_map(|l| verb(l).map(|(_, a)| a))
                .filter(|a| !a.is_empty())
                .collect();
            let mut text = v.to_string();
            if !args.is_empty() {
                text.push(' ');
                text.push_str(&args[..args.len().min(MERGED_ARGS)].join(", "));
                if args.len() > MERGED_ARGS {
                    text.push_str(" …");
                }
            }
            text.push_str(&format!(" ×{}", run.len()));
            out.push(ActionLine {
                at: run[0].at.clone(),
                kind: ActionKind::Call,
                text,
            });
        }
        i = j;
    }
    out
}

/// An RFC3339 time as the time since `start` — `+03:12`, `+1:02:09` — or its time of day where
/// there is no start to count from.
fn relative(at: &str, start: Option<SystemTime>) -> String {
    let parsed = serde_json::from_value::<SystemTime>(serde_json::Value::String(at.to_string()));
    let since = match (parsed, start) {
        (Ok(t), Some(s)) => t.0.duration_since(s.0).unwrap_or_default(),
        _ => return at.get(11..19).unwrap_or(at).to_string(),
    };
    let secs = since.as_secs();
    match secs / 3600 {
        0 => format!("+{:02}:{:02}", secs / 60, secs % 60),
        h => format!("+{h}:{:02}:{:02}", secs / 60 % 60, secs % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(at: &str, text: &str) -> ActionLine {
        ActionLine {
            at: at.into(),
            kind: ActionKind::Call,
            text: text.into(),
        }
    }

    /// **A run of one verb reads as one line**, naming its first arguments and how many there
    /// were; a said line and a change of verb each break the run.
    #[test]
    fn consecutive_calls_of_one_verb_merge_into_one_line() {
        let said = ActionLine {
            kind: ActionKind::Said,
            ..call("t4", "looking")
        };
        let lines = vec![
            call("t0", "Read a.rs"),
            call("t1", "Read b.rs"),
            call("t2", "Read c.rs"),
            call("t3", "$ cargo test"),
            call("t3b", "$ cargo clippy"),
            call("t3c", "$ rg x"),
            said.clone(),
            call("t5", "ping"),
            call("t6", "ping"),
            call("t7", "Read d.rs"),
        ];
        let got: Vec<(String, String)> = merge_runs(lines)
            .into_iter()
            .map(|l| (l.at, l.text))
            .collect();
        let want: Vec<(String, String)> = [
            ("t0", "Read a.rs, b.rs … ×3"),
            ("t3", "$ cargo test, clippy ×2"),
            ("t3c", "$ rg x"),
            ("t4", "looking"),
            ("t5", "ping ×2"),
            ("t7", "Read d.rs"),
        ]
        .iter()
        .map(|(a, t)| (a.to_string(), t.to_string()))
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn times_read_as_the_time_since_the_node_started() {
        let start = Some(SystemTime::from_unix_millis(1_790_000_000_000));
        let at = |ms: u64| {
            crate::activity::rfc3339(SystemTime::from_unix_millis(1_790_000_000_000 + ms))
        };
        assert_eq!(relative(&at(0), start), "+00:00");
        assert_eq!(relative(&at(192_000), start), "+03:12");
        assert_eq!(relative(&at(3_729_000), start), "+1:02:09");
        assert_eq!(
            relative(&at(5_000), None),
            at(5_000)[11..19],
            "no start: the time of day"
        );
        let before = crate::activity::rfc3339(SystemTime::from_unix_millis(1_789_999_999_000));
        assert_eq!(relative(&before, start), "+00:00", "never negative");
    }

    #[test]
    fn token_counts_read_short() {
        assert_eq!(tokens(812), "812");
        assert_eq!(tokens(12_400), "12.4k");
        assert_eq!(tokens(1_300_000), "1.3M");
    }
}
