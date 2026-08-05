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
    /// §6.1 step 2's depth and concurrency gates, refused.
    ///
    /// **A refusal, never a clamp and never a queue**, and the wrapped error names the bound *and*
    /// the value that broke it — §3.1 is explicit that a `spawn` past `max_depth` "is refused with
    /// a spawn error, never silently clamped", and that excess `spawn`s are "refused, not queued".
    /// Carried as its own variant rather than flattened into a string so the caller that reads the
    /// tool result gets a sentence naming what it may not do, and so a test can match the shape
    /// rather than a message.
    #[error("spawn refused (§6.1 step 2): {0}")]
    Gate(#[from] marion_core::agent_type::SpawnGateError),
    /// §5.4's `background`, which the tool schema declares and M1 does not implement.
    ///
    /// **Refused, not ignored.** §5.4 spells the field `"background": false // M1: must be false
    /// (§9)`, and accept-and-ignore is the §12 silent-failure shape this codebase keeps finding and
    /// killing — `default_tools_approval_mode`, `trust: true`, `--permission-prompt-tool stdio`,
    /// `--verbose`. The caller asked for a handle and would instead get a finished contract, with
    /// nothing anywhere saying the request was dropped: a parent backgrounding four children to run
    /// them concurrently gets four serialized ones and no way to tell. Real backgrounding is M2
    /// (`LIVE_CHILDREN_OF_A_SYNCHRONOUS_CALLER` in `run` records what it would change), so the
    /// honest M1 answer is a sentence naming the field, not a different verb performed quietly.
    #[error(
        "spawn refused: `background: true` is declared in marion's tool schema but not implemented \
         — §5.4 requires `background: false` in M1, and backgrounding lands in M2 with §7.6's \
         descendant gating. Omit the field or pass `false` to spawn synchronously; the contract is \
         returned when the child reaches a terminal state."
    )]
    BackgroundUnimplemented,
    /// §5.4's `isolation`, for every value but the one marion performs.
    ///
    /// **`run_spawn` calls `make_worktree` unconditionally** and builds `Workspace::Worktree`;
    /// `Workspace::SharedCwd` is constructed nowhere outside `marion_core`'s own definition, there
    /// is no `Remote` variant at all, and `AgentType` carries no `isolation` key for a `spawn` to
    /// override. So the field selected nothing: `shared-cwd` and `remote` both got a worktree.
    ///
    /// Refused rather than ignored because **the two directions are not symmetrical and neither is
    /// harmless**. `shared-cwd → worktree` is *more* containment than was asked for, but it puts
    /// the child's writes in a tree the caller never named and §6.6 says marion "never auto-merges"
    /// — so the caller's edits are not where it expects them, and the §6.6 write-conflict refusal
    /// it was relying on to name a holder never runs. `remote → worktree` is the dangerous one: a
    /// request to run somewhere else, silently served by running on the operator's own machine.
    /// The contract does say `Worktree`, so a caller reading it carefully could tell — but "the
    /// artifact contradicts your request and nothing points at the contradiction" is the §12 shape,
    /// not an excuse for it.
    ///
    /// `worktree` and absence are **not** refused: that is what marion does, so accepting it is
    /// the honest answer rather than a lucky one.
    #[error(
        "spawn refused: `isolation: {0:?}` is declared in marion's tool schema but not implemented \
         — marion creates a git worktree for every child (§6.6), and `shared-cwd` and `remote` \
         have no code path. Omit the field or pass `\"worktree\"`; a spawn that silently ran \
         somewhere other than where it was asked to would be worse than this refusal."
    )]
    IsolationUnimplemented(String),
    /// §5.4's `verification`, which is **accepted, dropped, and then contradicted in the artifact**.
    ///
    /// The worst of the family, because the lie is durable. `spawn`'s schema declares it, nothing
    /// reads it, and `build_contract` hardcodes `verification: vec![]` — so the contract, whose
    /// whole purpose §6.7 states as *"knowing exactly what came back"*, records that no
    /// verification was requested. A caller that asked for `cargo test` and one that asked for
    /// nothing get **byte-identical** evidence, and the one that asked has no way to tell its
    /// commands never ran. `MILESTONES.md` already lists the *execution* gap ("`verification`
    /// never executes, so every contract's `evidence` is always empty"); what was never written
    /// down is that marion goes on **accepting the parameter** while that is true.
    ///
    /// An empty or absent list is not refused — it asks for nothing, which is what marion does.
    #[error(
        "spawn refused: `verification` is declared in marion's tool schema but not implemented — \
         the commands never run and the contract's `verification` and `evidence` are written empty \
         (MILESTONES.md), so accepting them would return a contract that reads as \"verified, \
         nothing to report\" when the truth is \"never ran\". Omit the field and verify the child's \
         work yourself; §6.7's contract carries its diff and changed paths."
    )]
    VerificationUnimplemented,
    #[error("invalid writable scope: {0}")]
    Scope(#[from] marion_core::scope::ScopeError),
    #[error("compiling the child's launch: {0}")]
    Harness(#[from] marion_harness::HarnessError),
    /// §6.1 step 8's gate, failed on a **child**. Carried through rather than flattened so the
    /// cause survives into the tool result the parent reads: the alternative to a loud refusal here
    /// is a child that took its turn without marion's tools, reported nothing, and exited 0 — the
    /// §12 silent-failure shape, indistinguishable from a run that simply had nothing to say.
    #[error("driving the child over its control plane: {0}")]
    Duplex(#[from] crate::duplex::DuplexError),
    /// §3.4's third control transport, which is not a launch path on either axis.
    #[error(
        "{0} drives its node through a terminal, and marion MUST NOT give a headless node a pty \
         on stdin (§5.2). There is no child launch path for that surface, so the spawn is refused \
         rather than pushed down one that does not fit it."
    )]
    UnsupportedChildSurface(marion_core::harness::Harness),
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

/// A scratch `GIT_INDEX_FILE`, seeded from a commit and removed on the way out.
///
/// It lives in the system temp dir and **never inside the workspace**: an index file written under
/// the worktree would itself show up as an untracked file, so the diff would report the instrument
/// that produced it.
struct ScratchIndex(PathBuf);

impl Drop for ScratchIndex {
    fn drop(&mut self) {
        // Ignored: a leftover scratch index costs a few hundred bytes in the temp dir and must not
        // turn a successful run into a failed one.
        let _ = std::fs::remove_file(&self.0);
    }
}

impl ScratchIndex {
    /// `GIT_INDEX_FILE=<tmp>` plus `git read-tree <base>`, §6.7's own recipe.
    fn seeded(wt: &Path, base: &Oid) -> Result<Self, SpawnError> {
        let path = std::env::temp_dir().join(format!(
            "marion-diff-index-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        // A stale file from a run that died before `Drop` would be read as the index's *contents*,
        // so it is removed rather than reused: pids recycle.
        let _ = std::fs::remove_file(&path);
        let me = Self(path);
        git_indexed(wt, &me, &["read-tree", &base.0])?;
        Ok(me)
    }
}

/// `git` in `wt` with `GIT_INDEX_FILE` pointed at the scratch index.
///
/// Every index-mutating call in `diff_text` goes through here. The workspace's own index is never
/// named, which is the property §6.7 states twice: a bare `git add -N` in the workspace permanently
/// changes what `git status`, `git diff`, `git stash` and `git commit -a` do for the user — on
/// files marion was only ever reading — and since `isolation` defaults to `shared-cwd`, that
/// workspace is by default the user's own checkout.
fn git_indexed(wt: &Path, index: &ScratchIndex, args: &[&str]) -> Result<String, SpawnError> {
    let out = SysCommand::new("git")
        .current_dir(wt)
        .env("GIT_INDEX_FILE", &index.0)
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

/// The patch for everything the child did, as one `git diff <base_commit>` (§6.7).
///
/// **One diff against the base, not two diffs concatenated.** The previous shape — `git diff <base>
/// HEAD` followed by `git diff HEAD` — could not see an untracked file at all: with nothing
/// committed the first term is empty and the second compares the index to the worktree, where a
/// file git does not track is simply absent. A child that *created* a file therefore produced an
/// empty diff, and since `run_spawn` drops an empty one, §6.7's audit record carried
/// `changed_paths: ["the/file"]` with no bytes anywhere — and `cleanup` then removed the worktree
/// holding the only copy. Measured, and now pinned by `tests/worktree_reap.rs`.
///
/// The intent-to-add pass is what puts those bytes in the patch, and it is the reason the scratch
/// index exists: `git add -N` records "this path is about to be tracked" so `git diff` will emit it
/// as a creation, and doing that in the workspace's own index would alter what the *user's* `git
/// status` and `git commit -a` do.
///
/// Untracked paths are enumerated with the same `ls-files --others --exclude-standard` call
/// [`changed_paths`] uses, so the two derive their subject from one dialect rather than two: a path
/// that reaches `changed_paths` is a path whose content reaches the diff.
pub fn diff_text(wt: &Path, base: &Oid) -> Result<String, SpawnError> {
    let index = ScratchIndex::seeded(wt, base)?;
    let untracked: Vec<String> = git(wt, &["ls-files", "--others", "--exclude-standard"])?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if !untracked.is_empty() {
        // `--` first: a path that happens to look like a revision is still a path. `git add` with an
        // empty pathspec is an error, hence the guard above rather than an unconditional call.
        let mut args = vec!["add", "-N", "--"];
        args.extend(untracked.iter().map(String::as_str));
        git_indexed(wt, &index, &args)?;
    }
    // `--no-renames`, matching `changed_paths`: a rename rendered as a rename carries no content,
    // and these two must describe the same run.
    git_indexed(wt, &index, &["diff", "--no-renames", &base.0])
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
    /// The commits the child named in its `report`, carried through to
    /// `Completion::result_commits` unchanged.
    ///
    /// `Oid` here and `String` on [`marion_harness::StreamOutcome`] is the whole of the conversion:
    /// **wrapping is not validating**, and the newtype must not be read as marion having checked
    /// anything. See `Completion::result_commits`.
    pub result_commits: Vec<Oid>,
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
            result_commits: stream.result_commits.into_iter().map(Oid).collect(),
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
        // **The child's, not marion's.** §6.7 calls this the one field the child owns outright, and
        // it was hardcoded empty here — so a child that committed its work and reported the oids
        // had them dropped in transit, and the contract then asserted it had committed nothing.
        result_commits: outcome.result_commits.clone(),
        changed_paths: changed,
        acceptance_criteria_omitted: 0,
        changed_paths_omitted: 0,
        result_commits_omitted: 0,
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
        // Provisional, like `child` above and for the same reason: `run_spawn` overwrites it from
        // the **adapter**, which is the only thing that knows what constraint was compiled.
        //
        // This used to be the final value — `["apply_patch", "shell"]`, hardcoded, on every child
        // of every harness. It was wrong on all four. On three it named tools those harnesses have
        // never had; on codex, where it looks plausible, it is the per-tool echo §3.1 forbids in as
        // many words (*"echoing marion's own vocabulary there would make the field claim a
        // constraint that never existed"*), since codex has no allowlist to check a call against
        // and its real constraint is `sandbox:workspace-write`. §6.7 makes this the audit record,
        // so a constant here understated some children and invented permissions for others.
        allowed_tools: vec![],
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

    /// **The child's own field reaches the contract, and marion adds nothing to it.**
    ///
    /// §6.7 calls `result_commits` the one field the child owns outright, and `build_contract`
    /// hardcoded `vec![]` — so a child that committed its work and reported the oids produced a
    /// contract asserting it had committed nothing. That is a *wrong answer*, not a gap: empty is
    /// how a reader learns nothing was committed, and `worktree_reap.rs` reads it exactly that way.
    /// It also matters more than it looks, because the commits are real — `git worktree remove`
    /// leaves `marion/<task_id>` alive holding them, so the contract was denying durable work that
    /// existed.
    ///
    /// Order is asserted too: these are the child's words in the child's sequence, and a set would
    /// lose the ordering a `git cherry-pick` sequence depends on.
    #[test]
    fn the_commits_a_child_reported_are_the_commits_the_contract_records() {
        let commits = vec![Oid("b".repeat(40)), Oid("c".repeat(40))];
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
                narrative: Some("committed twice".into()),
                result_commits: commits.clone(),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            None,
            vec![],
        );
        let comp = c.completion.unwrap();
        assert_eq!(
            comp.result_commits, commits,
            "verbatim and in order — marion neither validates nor reorders what the child owns"
        );
        assert_eq!(
            comp.result_commits_omitted, 0,
            "nothing was elided, and the counter must say so rather than being left to a default"
        );
    }

    /// The other side of the same claim: a child that names no commits still gets an empty list,
    /// which is a statement rather than an absence. Pinned so the threading above cannot drift into
    /// inventing one — the failure mode this repo has hit twice with `child.model`.
    #[test]
    fn a_child_that_named_no_commits_records_none() {
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
                narrative: Some("did not commit".into()),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            None,
            vec![],
        );
        assert!(c.completion.unwrap().result_commits.is_empty());
    }

    /// The seam between the two layers: `StreamOutcome` carries the child's strings, `ChildOutcome`
    /// carries `Oid`s, and **the conversion is a wrap and nothing else**. A future filter here —
    /// dropping a malformed or unreachable oid — would make the contract imply a check marion never
    /// performed, so the non-oid below is carried through deliberately.
    #[test]
    fn from_stream_wraps_the_childs_commits_without_validating_them() {
        let stream = marion_harness::StreamOutcome {
            narrative: Some("x".into()),
            result_commits: vec!["not-an-oid".into(), "d".repeat(40)],
            ..Default::default()
        };
        let out = ChildOutcome::from_stream(stream, ChildExit::default(), String::new());
        assert_eq!(
            out.result_commits,
            vec![Oid("not-an-oid".into()), Oid("d".repeat(40))],
            "wrapping is not validating: §6.7 gives the child this field outright, and a silent \
             filter would be marion asserting a check it did not run"
        );
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
