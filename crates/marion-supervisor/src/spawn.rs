//! The blocking `spawn` path (design §6.1, §6.7, §9).
//!
//! M1's shape: marion creates a worktree, writes the child's configuration, starts `codex exec`,
//! reads its JSONL until the process ends, then derives the contract from **git** and from the
//! child's `report` call. `spawn` blocks for the whole run and returns the completed contract.

use std::path::{Path, PathBuf};
use std::process::Command as SysCommand;

use marion_core::contract::*;
use marion_core::encoding::{Duration, SystemTime};
use marion_core::scope::Scope;
use marion_harness::{ChildExit, StreamOutcome};

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("git {0} failed: {1}")]
    Git(&'static str, String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown agent type {0}")]
    UnknownAgentType(String),
    #[error("invalid writable scope: {0}")]
    Scope(#[from] marion_core::scope::ScopeError),
    #[error("compiling the child's launch: {0}")]
    Harness(#[from] marion_harness::HarnessError),
}

fn git(repo: &Path, args: &[&str]) -> Result<String, SpawnError> {
    let out = SysCommand::new("git")
        .current_dir(repo)
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(SpawnError::Git(
            "command",
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn now() -> SystemTime {
    SystemTime(std::time::SystemTime::now())
}

/// Create the child's worktree at `base_commit`.
pub fn make_worktree(repo: &Path, path: &Path, branch: &str) -> Result<Oid, SpawnError> {
    let head = git(repo, &["rev-parse", "HEAD"])?.trim().to_string();
    git(
        repo,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            &path.to_string_lossy(),
            &head,
        ],
    )?;
    Ok(Oid(head))
}

/// `changed_paths` in three independent terms, so no term's meaning depends on index state:
/// committed work, uncommitted tracked work, and untracked files.
///
/// The intent-to-add pass runs against a **scratch index**, never the workspace's own, so
/// deriving a diff cannot disturb what the user sees in their own repo.
pub fn changed_paths(wt: &Path, base: &Oid) -> Result<Vec<PathBuf>, SpawnError> {
    let mut set: Vec<PathBuf> = Vec::new();
    let mut push = |s: &str| {
        for l in s.lines().filter(|l| !l.trim().is_empty()) {
            let p = PathBuf::from(l.trim());
            if !set.contains(&p) {
                set.push(p);
            }
        }
    };
    push(&git(
        wt,
        &["diff", "--name-only", "--no-renames", &base.0, "HEAD"],
    )?);
    push(&git(wt, &["diff", "--name-only", "--no-renames", "HEAD"])?);
    let untracked = git(wt, &["ls-files", "--others", "--exclude-standard"])?;
    push(&untracked);
    Ok(set)
}

pub fn diff_text(wt: &Path, base: &Oid) -> Result<String, SpawnError> {
    // Committed plus uncommitted, without touching the workspace index.
    let a = git(wt, &["diff", "--no-renames", &base.0, "HEAD"])?;
    let b = git(wt, &["diff", "--no-renames", "HEAD"])?;
    Ok(format!("{a}{b}"))
}

/// What marion knows about a finished child: what its stream said, plus what marion observed of
/// the process.
///
/// The stream half is no longer parsed here. Which events a harness emits is exactly what differs
/// between the four, so reading them is behind the adapter seam
/// ([`marion_harness::HarnessAdapter::parse_stream`]) and this struct is where the two halves are
/// joined — `from_stream` below is the join, so no caller can assemble half of one.
#[derive(Debug, Default)]
pub struct ChildOutcome {
    pub narrative: Option<String>,
    pub file_change_paths: Vec<PathBuf>,
    /// The child's stream said the run failed. See [`marion_harness::StreamOutcome::failure`]: on
    /// gemini this is the *only* signal, because an auth failure exits 0 (S12).
    pub failure: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub stderr: String,
}

impl ChildOutcome {
    /// Join what the harness's stream said with what marion observed of the process.
    pub fn from_stream(stream: StreamOutcome, exit: ChildExit, stderr: String) -> Self {
        Self {
            narrative: stream.narrative,
            file_change_paths: stream.file_change_paths,
            failure: stream.failure,
            exit_code: exit.code,
            signal: exit.signal,
            timed_out: exit.timed_out,
            stderr,
        }
    }
}

/// Assemble the contract at the child's terminal transition.
#[allow(clippy::too_many_arguments)]
pub fn build_contract(
    task_id: TaskId,
    requester: AgentId,
    repo: RepoIdentity,
    base: Oid,
    workspace: Workspace,
    instructions: &str,
    criteria: &[String],
    ceiling: &[Glob],
    requested: &[Glob],
    timeout: Duration,
    spawned: SystemTime,
    outcome: &ChildOutcome,
    changed: Vec<PathBuf>,
    diff: Option<String>,
    evidence: Vec<CommandOutcome>,
) -> TaskContract {
    let scope = Scope::new(ceiling, requested).ok();
    let violations = scope
        .as_ref()
        .map(|s| s.violations(&changed))
        .unwrap_or_default();
    // A child that never reported is Unreported even if everything else looks clean — the status
    // is never silently promoted from a final message.
    let status = if outcome.timed_out {
        ExitStatus::TimedOut
    } else if outcome.failure.is_some() {
        // Ahead of the `Unreported` arm on purpose: a child whose stream said *why* it failed has
        // told marion more than "no report arrived", and on gemini this is the only arm that fires
        // at all — its auth failure exits 0, so the code-based arm below would call it clean (S12).
        ExitStatus::Failed
    } else if outcome.narrative.is_none() {
        ExitStatus::Unreported
    } else if outcome.exit_code.unwrap_or(0) != 0 {
        ExitStatus::Failed
    } else {
        ExitStatus::Ok
    };
    let description = if outcome.timed_out {
        "child exceeded its timeout and its process group was killed".into()
    } else if let Some(signal) = outcome.signal {
        format!("child terminated by signal {signal}")
    } else if let Some(code) = outcome.exit_code {
        format!("child exited with code {code}")
    } else {
        "child exit status was unavailable".into()
    };
    // The harness's own words about the failure, kept verbatim beside marion's exit numbers. S13
    // measured an opencode failure arriving with an **empty stderr** and its whole description
    // in-stream, so without this the description would read "child exited with code 1" and nothing
    // else.
    let description = match &outcome.failure {
        Some(f) => format!("{description}; the child's stream reported: {f}"),
        None => description,
    };
    let stderr = outcome.stderr.trim();
    let description = if stderr.is_empty() {
        description
    } else {
        let preview: String = stderr.chars().take(512).collect();
        format!("{description}; stderr: {preview}")
    };
    let completion = Completion {
        status,
        died_before_gate: false,
        reported_early: false,
        held_to_timeout: false,
        live_descendants_at_report: vec![],
        narrative: outcome.narrative.as_deref().map(Capped::whole),
        narrative_synthesized: false,
        result_commits: vec![],
        changed_paths: changed,
        acceptance_criteria_omitted: 0,
        changed_paths_omitted: 0,
        scope_violations_omitted: 0,
        scope_enforced: scope.is_some(),
        scope_violations: violations,
        diff: diff.map(Capped::whole),
        evidence,
        evidence_omitted: 0,
        exit: ProcessExit {
            code: outcome.exit_code,
            signal: outcome.signal,
            description,
        },
    };
    TaskContract {
        task_id,
        requester,
        // Provisional, all three fields: `run_spawn` overwrites them from the **adapter** and from
        // the **compiled invocation**, which are the only things that know what actually ran.
        child: ChildRef {
            harness: marion_core::Harness::Codex,
            version: "unknown".into(),
            model: None,
        },
        repo,
        base_commit: base,
        workspace,
        instructions: Capped::whole(instructions),
        acceptance_criteria: criteria.iter().map(Capped::whole).collect(),
        allowed_tools: vec!["apply_patch".into(), "shell".into()],
        scope_ceiling: ceiling.to_vec(),
        scope_requested: requested.to_vec(),
        timeout,
        verification: vec![],
        timestamps: TaskTimestamps {
            spawned,
            first_output: None,
            reported: outcome.narrative.as_ref().map(|_| now()),
            exited: Some(now()),
        },
        completion: Some(completion),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::harness::Harness;
    use marion_harness::adapter_for;

    /// **The pre-move implementation, preserved verbatim.**
    ///
    /// This is `parse_child_stream` exactly as it stood before codex's stream parsing moved behind
    /// the seam — same `str::lines` framing, same `serde_json::from_str` per line, same two match
    /// arms in the same order. It exists solely so the claim "moved, not rewritten" can be
    /// *checked* rather than asserted in prose, which is the standard this seam's Phase 1 set with
    /// `the_codex_adapter_compiles_exactly_what_the_free_function_did`.
    ///
    /// It must never be edited to make a test pass. If the moved parser diverges from it, the
    /// divergence is the finding.
    fn parse_child_stream_before_the_move(s: &str) -> (Option<String>, Vec<PathBuf>) {
        let mut narrative = None;
        let mut file_change_paths = Vec::new();
        for line in s.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let item = &v["item"];
            match item["type"].as_str() {
                Some("mcp_tool_call") if item["server"] == "marion" && item["tool"] == "report" => {
                    if let Some(n) = item["arguments"]["narrative"].as_str() {
                        narrative = Some(n.to_string());
                    }
                }
                Some("file_change") => {
                    if let Some(cs) = item["changes"].as_array() {
                        for c in cs {
                            if let Some(p) = c["path"].as_str() {
                                file_change_paths.push(PathBuf::from(p));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        (narrative, file_change_paths)
    }

    /// Streams the two implementations are compared over. Every one is either a measured shape or
    /// a boundary the move could plausibly have shifted: framing (CRLF, no trailing newline,
    /// blank and non-JSON lines), the two evidence arms, a near-miss on each match condition, and
    /// a repeat that has to keep last-write-wins semantics.
    fn codex_corpus() -> Vec<String> {
        // Verbatim shape from tests/fixtures/s6/exec-mcp-report.stream.jsonl.
        let report = r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"marion","tool":"report","arguments":{"narrative":"did the work"},"status":"completed"}}"#;
        let change = r#"{"type":"item.completed","item":{"id":"item_0","type":"file_change","changes":[{"path":"/wt/a.rs","kind":"update"},{"path":"/wt/b.rs","kind":"add"}],"status":"completed"}}"#;
        let other_server = r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"other","tool":"report","arguments":{"narrative":"nope"}}}"#;
        let other_tool = r#"{"item":{"type":"mcp_tool_call","server":"marion","tool":"status","arguments":{"narrative":"nope"}}}"#;
        let no_narrative =
            r#"{"item":{"type":"mcp_tool_call","server":"marion","tool":"report","arguments":{}}}"#;
        let second = r#"{"item":{"type":"mcp_tool_call","server":"marion","tool":"report","arguments":{"narrative":"and again"}}}"#;
        vec![
            String::new(),
            "\n\n".to_string(),
            format!("{report}\n"),
            report.to_string(), // no trailing newline
            format!("{report}\r\n{change}\r\n"),
            format!("{change}\n{report}\n{second}\n"),
            format!("{other_server}\n{other_tool}\n{no_narrative}\n"),
            format!("not json at all\n{report}\n[]\n\"a string\"\n42\n"),
            format!("{report}\n{{\"item\":{{\"type\":\"half-writt"),
            format!("{change}\n{change}\n"),
        ]
    }

    /// The move's whole claim, stated as a test: routing codex's stream through the adapter changes
    /// nothing about what marion reads out of it.
    #[test]
    fn the_codex_adapter_parses_exactly_what_the_pre_move_function_did() {
        for s in codex_corpus() {
            let before = parse_child_stream_before_the_move(&s);
            let after = adapter_for(Harness::Codex)
                .unwrap()
                .parse_stream(&s, ChildExit::default());
            assert_eq!(
                (after.narrative.clone(), after.file_change_paths.clone()),
                before,
                "the moved parser diverged on:\n{s}"
            );
            assert_eq!(
                after.failure, None,
                "codex makes no failure claim of its own; adding one would change every status"
            );
        }
    }

    #[test]
    fn a_report_is_read_from_the_mcp_tool_call_item() {
        // Verbatim shape from tests/fixtures/s6/exec-mcp-report.stream.jsonl.
        let s = r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"marion","tool":"report","arguments":{"narrative":"did the work"},"status":"completed"}}"#;
        assert_eq!(
            marion_harness::codex::parse_stream(s).narrative.as_deref(),
            Some("did the work")
        );
    }

    #[test]
    fn file_changes_are_collected_as_corroboration() {
        let s = r#"{"type":"item.completed","item":{"id":"item_0","type":"file_change","changes":[{"path":"/wt/a.rs","kind":"update"}],"status":"completed"}}"#;
        assert_eq!(
            marion_harness::codex::parse_stream(s).file_change_paths,
            vec![PathBuf::from("/wt/a.rs")]
        );
    }

    #[test]
    fn a_tool_call_from_another_server_is_not_a_report() {
        let s = r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"other","tool":"report","arguments":{"narrative":"nope"}}}"#;
        assert!(marion_harness::codex::parse_stream(s).narrative.is_none());
    }

    /// A child whose stream said it failed is `Failed`, not `Unreported` — and the harness's own
    /// words survive into the audit record, which on opencode is the only place they exist at all
    /// (S13: exit 1 with an empty stderr).
    #[test]
    fn a_stream_reported_failure_outranks_the_silence_it_arrives_with() {
        let c = build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            RepoIdentity {
                git_common_dir: "/r/.git".into(),
                head_branch: None,
            },
            Oid("a".repeat(40)),
            Workspace::Worktree {
                path: "/wt".into(),
                branch: "b".into(),
            },
            "do it",
            &[],
            &[Glob("**".into())],
            &[Glob("**".into())],
            Duration::from_secs(900),
            now(),
            &ChildOutcome {
                failure: Some("APIError: bad request".into()),
                // Exit 0, as gemini's measured auth failure did: the code alone would say clean.
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            None,
            vec![],
        );
        let comp = c.completion.unwrap();
        assert_eq!(comp.status, ExitStatus::Failed);
        assert!(comp.exit.description.contains("APIError: bad request"));
    }

    /// The other half: a timeout still outranks everything, so marion's own attributed kill is
    /// never relabelled by something the child said on its way out.
    #[test]
    fn a_timeout_still_outranks_a_stream_reported_failure() {
        let c = build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            RepoIdentity {
                git_common_dir: "/r/.git".into(),
                head_branch: None,
            },
            Oid("a".repeat(40)),
            Workspace::Worktree {
                path: "/wt".into(),
                branch: "b".into(),
            },
            "do it",
            &[],
            &[Glob("**".into())],
            &[Glob("**".into())],
            Duration::from_secs(900),
            now(),
            &ChildOutcome {
                failure: Some("APIError".into()),
                timed_out: true,
                ..ChildOutcome::default()
            },
            vec![],
            None,
            vec![],
        );
        assert_eq!(c.completion.unwrap().status, ExitStatus::TimedOut);
    }

    #[test]
    fn a_silent_child_is_unreported_not_ok() {
        let c = build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            RepoIdentity {
                git_common_dir: "/r/.git".into(),
                head_branch: None,
            },
            Oid("a".repeat(40)),
            Workspace::Worktree {
                path: "/wt".into(),
                branch: "b".into(),
            },
            "do it",
            &["passes".to_string()],
            &[Glob("**".into())],
            &[Glob("src/**".into())],
            Duration::from_secs(900),
            now(),
            &ChildOutcome {
                narrative: None,
                file_change_paths: vec![],
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            None,
            vec![],
        );
        assert_eq!(c.completion.unwrap().status, ExitStatus::Unreported);
    }

    #[test]
    fn an_out_of_scope_write_is_recorded_with_scope_enforced_true() {
        let c = build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            RepoIdentity {
                git_common_dir: "/r/.git".into(),
                head_branch: None,
            },
            Oid("a".repeat(40)),
            Workspace::Worktree {
                path: "/wt".into(),
                branch: "b".into(),
            },
            "do it",
            &["passes".to_string()],
            &[Glob("**".into())],
            &[Glob("src/**".into())],
            Duration::from_secs(900),
            now(),
            &ChildOutcome {
                narrative: Some("done".into()),
                file_change_paths: vec![],
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![PathBuf::from("src/a.rs"), PathBuf::from("outside/b.txt")],
            None,
            vec![],
        );
        let comp = c.completion.unwrap();
        assert!(comp.scope_enforced, "false would mean the check never ran");
        assert_eq!(comp.scope_violations, vec![PathBuf::from("outside/b.txt")]);
        assert_eq!(
            comp.status,
            ExitStatus::Ok,
            "detective, not preventive: the run still succeeded"
        );
    }
}
