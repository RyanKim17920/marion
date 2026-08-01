//! The task contract (design §6.7).
//!
//! Field ownership is three-way — parent, marion, child — and §9 is authoritative.
//! JSON encodings are part of the specification, not serde defaults: see `encoding.rs`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::encoding::{Duration, Millis, SystemTime};

/// A value that may have been shortened by §6.7's cap rules.
///
/// `diff`, `narrative`, `instructions`, each criterion and each command stream are `Capped`
/// because a consumer must be able to tell a complete value from a prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capped<T> {
    pub value: T,
    pub truncated: bool,
    /// Pre-cap length in bytes. For `narrative` this is the *pre-rule-0* length, so it always
    /// means "how long the child's text actually was".
    pub original_bytes: usize,
}

impl<T> Capped<T> {
    pub fn complete(value: T, len: usize) -> Self {
        Self { value, truncated: false, original_bytes: len }
    }
}

impl Capped<String> {
    pub fn whole(value: impl Into<String>) -> Self {
        let value = value.into();
        let n = value.len();
        Self::complete(value, n)
    }
}

/// §6.7: `AgentId` and `TaskId` are lowercase hyphenated UUIDv7 strings. `AgentId` doubles as a
/// filesystem path component (§4.3), so it must stay filesystem-safe.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

/// 40-character hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Oid(pub String);

/// Serialized as its pattern string, never as a compiled matcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Glob(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIdentity {
    pub git_common_dir: PathBuf,
    pub head_branch: Option<String>,
}

/// Externally tagged, per §6.7: `{"Worktree": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Workspace {
    Worktree { path: PathBuf, branch: String },
    SharedCwd { path: PathBuf },
}

impl Workspace {
    pub fn path(&self) -> &PathBuf {
        match self {
            Workspace::Worktree { path, .. } | Workspace::SharedCwd { path } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// §6.7: default 300 s, killed on expiry with `timed_out: true`.
    pub timeout: Duration,
}

/// `stdout` and `stderr` are capped **independently** (cap rule 2), so each carries its own flag;
/// a single outcome-level flag could not say which stream was cut.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutcome {
    pub command: Command,
    /// `None` means the command was signalled.
    pub exit_code: Option<i32>,
    pub stdout: Capped<String>,
    pub stderr: Capped<String>,
    /// A *measurement*, so milliseconds rounded down — not `Duration`, which is the
    /// seconds-rounded-up encoding for bounds. A 412 ms check must not serialize as 1 s, and a
    /// sub-second one must not serialize as 0.
    pub duration: Millis,
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// marion's own explanation ("external termination", "descendant hold expired"), never
    /// derived from the numbers.
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTimestamps {
    pub spawned: SystemTime,
    pub first_output: Option<SystemTime>,
    pub reported: Option<SystemTime>,
    pub exited: Option<SystemTime>,
}

/// `ResultStatus` is a type alias for `ExitStatus` (§3.2), not a second enum — two names for one
/// type, because a *node* exits and a *contract* results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitStatus {
    Ok,
    Failed,
    Cancelled,
    Unreported,
    TimedOut,
    Killed,
}

pub type ResultStatus = ExitStatus;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildRef {
    pub harness: String,
    pub version: String,
}

/// Written once, at the node's terminal transition — not at `report`, which only stages the
/// child-owned payload (§7.6 step 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completion {
    pub status: ResultStatus,
    /// L1's third exemption: marion observed the process die without an observed conclusion.
    pub died_before_gate: bool,
    pub reported_early: bool,
    pub held_to_timeout: bool,
    pub live_descendants_at_report: Vec<AgentId>,
    /// marion-owned field, child-sourced. The only contract field a foreign agent's text fills,
    /// hence `Capped` (cap rule 0).
    pub narrative: Option<Capped<String>>,
    pub narrative_synthesized: bool,
    /// The one field the child owns outright.
    pub result_commits: Vec<Oid>,
    pub changed_paths: Vec<PathBuf>,
    /// All cap metadata lives here, including counters describing `TaskContract` fields: the cap
    /// runs only when the contract is returned, which happens only at the terminal transition.
    pub acceptance_criteria_omitted: usize,
    pub changed_paths_omitted: usize,
    pub scope_violations_omitted: usize,
    /// False only when the workspace affords no git-derived `changed_paths`. Never means
    /// "no violation".
    pub scope_enforced: bool,
    /// Derived from the *full* `changed_paths` before any elision, so a cap can never hide a
    /// violation.
    pub scope_violations: Vec<PathBuf>,
    pub diff: Option<Capped<String>>,
    pub evidence: Vec<CommandOutcome>,
    pub evidence_omitted: usize,
    pub exit: ProcessExit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskContract {
    pub task_id: TaskId,
    pub requester: AgentId,
    pub child: ChildRef,
    pub repo: RepoIdentity,
    pub base_commit: Oid,
    pub workspace: Workspace,
    pub instructions: Capped<String>,
    pub acceptance_criteria: Vec<Capped<String>>,
    pub allowed_tools: Vec<String>,
    /// From the agent type. Default stored as `["**"]`, never absent.
    pub scope_ceiling: Vec<Glob>,
    /// From `spawn`. Default stored as `["**"]`, never absent.
    pub scope_requested: Vec<Glob>,
    /// Always set. Step 6 writes it provisionally; step 9 finalizes it (§9).
    pub timeout: Duration,
    pub verification: Vec<Command>,
    pub timestamps: TaskTimestamps,
    /// `Some` iff a terminal transition was emitted. `None` while the run is live, and `None` if
    /// it ended unobserved (`reap_state: Orphaned`).
    pub completion: Option<Completion>,
}
