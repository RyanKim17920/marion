//! **One node judging another's ended work, read-only** — the supervisor half of
//! `marion_core::review`: which node may be reviewed, what the reviewer is shown, and how its
//! report becomes findings on its own contract.
//!
//! A review is an ordinary child launch with three differences, all decided here and applied by
//! `run::run_spawn_watched`: the reviewer is placed under the node it reviews, its worktree is cut
//! at that node's committed work, and it runs **read-only** — `write` dropped from its tools, the
//! row's `ReadOnly` switch rendered, and an empty writable scope so any change that gets through is
//! recorded on its contract as a scope violation. marion, not the reviewer, decides what its
//! findings mean ([`marion_core::review::decide`]).
//!
//! Phase 2 (a gate that re-prompts the author on a blocking verdict, bounded by
//! `ReviewSpec::max_rounds`) hooks in at [`verdict`]: it would run one reviewer per round and fold
//! each round's [`Verdict`] into the *reviewed* node's `Completion::review`. Nothing here writes
//! that field yet.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use marion_core::contract::{AgentId, Oid, TaskContract};
use marion_core::harness::Harness;
use marion_core::review::{self, ReviewSpec, Verdict};

/// What a reviewer's launch knows about the node it reviews, read off that node's contract once,
/// when the review is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub agent_id: AgentId,
    /// The commit the reviewer's worktree is cut at: the reviewed work as marion landed it on its
    /// branch, or `None` where nothing was landed (a shared-cwd node), which cuts at HEAD.
    pub commit: Option<Oid>,
    /// The paths the reviewed change touched — what a finding must name to be grounded.
    pub changed_paths: Vec<PathBuf>,
}

/// The built-in agent types a review falls back to when none is named, in order of preference.
/// Data, so a new vendor's CLI is a new entry and nothing else. Only those whose row refuses a
/// write ([`marion_harness::spec::ReadOnly::blocks_writes`]) are ever picked from it.
const REVIEWERS: &[&str] = &["codex", "claude"];

/// What a reviewer on a row that cannot refuse a write is told, and its contract records.
pub const UNGUARDED: &str = "this harness cannot be made read-only; any write is recorded as a scope violation, not blocked";

/// [`UNGUARDED`] for a harness whose row's read-only strategy records a write rather than
/// refusing it — read off the row, never off the harness's name.
pub fn unguarded(harness: Harness) -> Option<&'static str> {
    (!marion_harness::adapter::harness_spec(harness)
        .read_only
        .blocks_writes())
    .then_some(UNGUARDED)
}

/// **Who reviews when nobody said**: of the [`REVIEWERS`] whose row refuses a write, the first
/// from a different model family than the reviewed node's ([`review::model_family`]) — a second
/// opinion from the same model is the weakest one — else the first of them, where the node's
/// family is unknown or every candidate shares it. Unknown is never taken for different.
pub fn default_reviewer(harness: Harness, model: Option<&str>) -> &'static str {
    let vendor_of = |h: Harness| marion_harness::adapter::harness_spec(h).vendor;
    let theirs = review::model_family(vendor_of(harness), model);
    let guarded = || {
        REVIEWERS.iter().copied().filter_map(|name| {
            marion_core::agent_type::builtin(name)
                .filter(|t| unguarded(t.harness).is_none())
                .map(|t| (name, t))
        })
    };
    guarded()
        .find(|(_, t)| {
            let ours = review::model_family(vendor_of(t.harness), t.model.as_deref());
            theirs.is_some() && ours.is_some() && ours != theirs
        })
        .or_else(|| guarded().next())
        .map(|(name, _)| name)
        .expect("at least one default reviewer refuses writes; the sweep pins it")
}

/// Why a review was refused, in words an operator or a parent model can act on.
pub fn refusal(agent_id: &AgentId, why: &str) -> String {
    format!("marion cannot review node {}: {why}", agent_id.0)
}

/// The node `contract` describes, as a review target — or why it cannot be reviewed. A node is
/// reviewable once it has **ended** (its contract carries a completion) and **changed something**
/// (a diff or a changed path to judge); a reviewer of nothing would report on nothing.
pub fn target(agent_id: &AgentId, contract: &TaskContract) -> Result<Target, String> {
    let Some(c) = contract.completion.as_ref() else {
        return Err(refusal(
            agent_id,
            "it has not ended yet; wait for it to finish, then ask again",
        ));
    };
    if c.changed_paths.is_empty() && c.diff.is_none() {
        return Err(refusal(
            agent_id,
            "it changed no files, so there is nothing to review",
        ));
    }
    Ok(Target {
        agent_id: agent_id.clone(),
        commit: c.commit.clone(),
        changed_paths: c.changed_paths.clone(),
    })
}

/// The reply a reviewer is asked for — the JSON report [`review::parse`] reads first.
const REPLY_SHAPE: &str = r#"{"verdict": "allow" | "block", "summary": "<one paragraph>", "findings": [{"severity": "critical" | "high" | "medium" | "low", "file": "<a path from the changed files>", "line": <number, optional>, "claim": "<what is wrong>", "evidence": "<what shows it>", "recommendation": "<what to do>"}]}"#;

/// The task a reviewer receives: the reviewed node's own task, what it said it did, how its
/// verification went, which files it changed and its diff, then the reply shape.
///
/// Every part is read from the reviewed node's contract, whose strings marion already bounded as
/// it wrote them (the narrative and diff are `Capped`), so the prompt is bounded by construction.
pub fn prompt(target: &Target, contract: &TaskContract) -> String {
    let mut p = format!(
        "Review the work of marion node {}. You are read-only: do not modify, create or delete \
         any file. Your working directory is a checkout of that work{}.\n\n## Its task\n{}\n",
        target.agent_id.0,
        target
            .commit
            .as_ref()
            .map(|c| format!(" at commit {}", c.0))
            .unwrap_or_default(),
        contract.instructions.value,
    );
    if !contract.acceptance_criteria.is_empty() {
        p.push_str("\n## Its acceptance criteria\n");
        for c in &contract.acceptance_criteria {
            let _ = writeln!(p, "- {}", c.value);
        }
    }
    if let Some(c) = contract.completion.as_ref() {
        let _ = write!(p, "\n## How it ended\nstatus: {:?}\n", c.status);
        if let Some(n) = &c.narrative {
            let _ = writeln!(p, "its report: {}", n.value);
        }
        if !c.evidence.is_empty() {
            p.push_str("\n## Its verification\n");
            for e in &c.evidence {
                let exit = e
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".into());
                let _ = writeln!(
                    p,
                    "- `{} {}` exited {exit}{}",
                    e.command.program,
                    e.command.args.join(" "),
                    if e.timed_out { " (timed out)" } else { "" },
                );
            }
        }
        p.push_str("\n## Changed files\n");
        for path in &c.changed_paths {
            let _ = writeln!(p, "- {}", path.display());
        }
        if let Some(d) = &c.diff {
            let _ = write!(p, "\n## Diff\n```diff\n{}\n```\n", d.value);
        }
    }
    let _ = write!(
        p,
        "\n## Your reply\nPut your review in the `narrative` of your report, as exactly this \
         JSON and nothing else:\n{REPLY_SHAPE}\nName a file only from the changed files above; a \
         finding about any other file is recorded but cannot block."
    );
    p
}

/// A reviewer's report read into marion's verdict: parsed ([`review::parse`]) and decided against
/// the reviewed change ([`review::decide`]) at the default policy's threshold. `None` where the
/// reviewer never reported; an unreadable report is the parse error, never a block.
pub fn verdict(
    narrative: Option<&str>,
    target: &Target,
) -> Option<Result<Verdict, review::Unparseable>> {
    let parsed = review::parse(narrative?);
    Some(parsed.map(|p| review::decide(p, &target.changed_paths, ReviewSpec::default().block_on)))
}

/// **`marion review` as the operator**: ask the supervisor at `socket` for a review of `target`
/// (no `caller`, the operator's own `repo`), wait for the reviewer to end, and return its contract.
/// `agent_type` empty lets the supervisor choose ([`default_reviewer`]).
///
/// The same `agent/spawn` a parent model sends through its bridge, so the two client forms share
/// one spawn path; this adds only the wait a terminal command needs.
pub fn request(
    sock: &crate::socket::SocketPaths,
    project: &marion_core::paths::ProjectDir,
    target: &AgentId,
    agent_type: &str,
    model: Option<String>,
    repo: &Path,
    bound: Duration,
) -> Result<(AgentId, TaskContract), String> {
    let spawned = crate::courier::spawn(
        sock,
        marion_core::proto::params::AgentSpawnParams {
            wider_children: None,
            budget_tokens: None,
            review_of: Some(target.clone()),
            notify_parent: false,
            candidates: vec![],
            race: None,
            agent_type: agent_type.to_string(),
            prompt: String::new(),
            native_launch: None,
            caller: None,
            repo: Some(repo.to_path_buf()),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec![],
            timeout_secs: Some(bound.as_secs()),
            model,
            no_change_record: None,
            pane: None,
            isolation: None,
            allow_concurrent_writes: None,
            profile: None,
        },
    )
    .map_err(|e| e.to_string())?;
    let reviewer = spawned.agent_id;
    let task = spawned
        .task_id
        .ok_or_else(|| "the supervisor started a reviewer with no contract".to_string())?;
    match crate::courier::await_contract(sock, project, &reviewer, Some(&task), bound)
        .map_err(|e| e.to_string())?
    {
        crate::courier::Delivered::StillRunning => Err(format!(
            "reviewer {} is still running after {} s; `marion ls` shows it when it ends",
            reviewer.0,
            bound.as_secs()
        )),
        _ => {
            let path = project.agent(&reviewer).contract(&task);
            std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
                .map(|c| (reviewer, c))
                .map_err(|e| format!("reading the reviewer's contract at {}: {e}", path.display()))
        }
    }
}

/// A finished reviewer's contract as the lines `marion review` prints: who reviewed, marion's
/// decision and count, then one line per finding, most severe and grounded first.
pub fn summary_lines(reviewer: &AgentId, contract: &TaskContract) -> Vec<String> {
    let short = crate::tree::short_id(&reviewer.0);
    let Some(c) = contract.completion.as_ref() else {
        return vec![format!(
            "review {short}: the reviewer's contract has no completion"
        )];
    };
    let warning = unguarded(contract.child.harness).map(|w| format!("review {short}: {w}"));
    let Some(f) = c.findings.as_ref() else {
        return warning
            .into_iter()
            .chain([format!(
                "review {short}: no findings could be read ({})",
                c.exit.description
            )])
            .collect();
    };
    let total = f.findings.len() + f.findings_omitted;
    let mut lines = vec![format!(
        "review {short}: {total} finding{}{}",
        if total == 1 { "" } else { "s" },
        if f.summary.is_empty() {
            String::new()
        } else {
            format!(" — {}", f.summary)
        }
    )];
    for finding in &f.findings {
        let at = finding
            .line
            .map(|l| format!("{}:{l}", finding.file))
            .unwrap_or_else(|| finding.file.clone());
        lines.push(format!(
            "  {:?} {at}{}: {}",
            finding.severity,
            if finding.grounded {
                ""
            } else {
                " (outside the change)"
            },
            finding.claim
        ));
    }
    if f.findings_omitted > 0 {
        lines.push(format!("  … {} more in the contract", f.findings_omitted));
    }
    lines.extend(warning);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::review::{Decision, Severity};

    fn target() -> Target {
        Target {
            agent_id: AgentId("019f-child".into()),
            commit: Some(Oid("abc123".into())),
            changed_paths: vec![PathBuf::from("src/a.rs")],
        }
    }

    /// **A default reviewer can always refuse a write**, whatever the reviewed node ran — decided
    /// by the rows' read-only strategies, so a row that turns scope-only drops out on its own.
    #[test]
    fn a_default_reviewer_is_never_one_that_cannot_refuse_a_write() {
        for h in Harness::ALL {
            for model in [
                None,
                Some("openai/gpt-5"),
                Some("anthropic/claude-sonnet-4-5"),
            ] {
                let name = default_reviewer(h, model);
                let t = marion_core::agent_type::builtin(name).expect("a built-in reviewer");
                assert_eq!(unguarded(t.harness), None, "{h} {model:?} picked {name}");
            }
        }
        for h in Harness::ALL {
            assert_eq!(
                unguarded(h).is_some(),
                !marion_harness::adapter::harness_spec(h)
                    .read_only
                    .blocks_writes(),
                "{h}: the warning follows the row"
            );
        }
    }

    #[test]
    fn an_unnamed_reviewer_comes_from_another_model_family() {
        assert_eq!(default_reviewer(Harness::ClaudeCode, None), "codex");
        assert_eq!(default_reviewer(Harness::Codex, None), "claude");
        assert_eq!(
            default_reviewer(Harness::OpenCode, Some("openai/gpt-5")),
            "claude"
        );
        assert_eq!(
            default_reviewer(Harness::OpenCode, None),
            "codex",
            "an unknown family is not taken for a different one: the first candidate"
        );
    }

    #[test]
    fn a_grounded_high_finding_blocks_and_an_absent_report_is_no_verdict() {
        let reply =
            r#"{"verdict":"allow","findings":[{"severity":"high","file":"src/a.rs","claim":"x"}]}"#;
        let v = verdict(Some(reply), &target()).unwrap().unwrap();
        assert_eq!(v.decision, Decision::Block);
        assert_eq!(v.findings.findings[0].severity, Severity::High);
        assert!(v.findings.findings[0].grounded);
        assert!(verdict(None, &target()).is_none());
        assert!(
            verdict(Some("looks fine to me"), &target())
                .unwrap()
                .is_err()
        );
    }
}
