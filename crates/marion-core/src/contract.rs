//! The task contract (design §6.7).
//!
//! Field ownership is three-way — parent, marion, child — and §9 is authoritative.
//! JSON encodings are part of the specification, not serde defaults: see `encoding.rs`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::encoding::{Duration, Millis, SystemTime};
use crate::harness::Harness;

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
        Self {
            value,
            truncated: false,
            original_bytes: len,
        }
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
    /// **`None` when the node did not run in a repository at all**, which [`Workspace::SharedCwd`]
    /// makes reachable: §2 keys on the git common dir *"falling back to cwd"*, so a project root is
    /// not evidence of a repository and this field cannot borrow its certainty.
    ///
    /// Optional rather than defaulted to the cwd, for the reason the whole of §6.7 is written
    /// around: a path in a field named `git_common_dir` is a claim that git can be pointed at it.
    /// A cwd written here would satisfy the type and be false, and `None` is the only value that
    /// says *there is no repository* rather than guessing at one — the same discipline
    /// [`Completion::scope_enforced`] holds to one field over.
    ///
    /// It is filled from `socket::git_common_dir`, the same derivation §2 keys on. It used to be
    /// filled from `repo.join(".git")`, which is not the common dir of a **linked worktree** — there
    /// `.git` is a *file* holding `gitdir: …` — so the audit record named a non-directory that did
    /// not identify the repository, and two linked worktrees of one repo recorded two different
    /// "repositories" for what §2 already treats as one project.
    pub git_common_dir: Option<PathBuf>,
    pub head_branch: Option<String>,
}

/// Externally tagged, per §6.7: `{"Worktree": {...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Workspace {
    Worktree { path: PathBuf, branch: String },
    SharedCwd { path: PathBuf },
}

/// §3.1's `isolation` key and §5.4's `spawn` parameter: **which of §6.6's workspaces a node gets**.
///
/// Two variants and not three. §3.1's table lists `worktree | shared-cwd | remote`, but `remote` has
/// no [`Workspace`] to select, no transport, no host and no auth story anywhere in the design — §1
/// puts *"remote hosting"* out of scope in as many words, and §9's only mention of it is forward-
/// looking. A `Remote` variant here would be a name marion could parse and could not serve, and the
/// value of this enum is precisely that **every value it can hold is a workspace marion builds**.
/// `remote` is still refused by name at the request edge, where the refusal can say why; it is not
/// admitted into the type system to be refused again further in.
///
/// **This type does not carry §3.1's default.** §3.1 gives the agent-type key a default of
/// `shared-cwd`; marion resolves an *absent* `spawn` parameter to [`Self::Worktree`] instead, and
/// that divergence is deliberate and recorded rather than accidental. §11 item 23 argues the
/// `shared-cwd → worktree` substitution is harmful because it silently *adds* containment; the
/// substitution in the other direction — resolving silence to `shared-cwd` — silently *removes* it,
/// putting the writes of every caller that named nothing into the operator's own live checkout. That
/// is the strictly worse of the two, so absence keeps the behaviour every existing caller already
/// has, and a caller that wants the user's tree says so. Making `shared-cwd` the default is a
/// behaviour change for every node marion has ever run and belongs to a decision that states itself.
///
/// **Serialized in §5.4's own spelling** (`kebab-case`), not serde's variant names, so the string a
/// caller writes in a `spawn` tool call, the string that crosses `agent/spawn` on the socket and the
/// string [`Self::as_wire`] prints are one spelling. Two spellings for one value is how a refusal
/// ends up naming something the caller never typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    Worktree,
    SharedCwd,
}

impl Isolation {
    /// §5.4's wire spelling, which is hyphenated where the variant is not.
    ///
    /// `None` for every other string, including `"remote"`: this function answers *"is this a
    /// workspace marion builds?"*, and the caller turns a `None` into a refusal that can name the
    /// value. Folding `remote` in here as a recognised-but-unserved value would put the
    /// accept-and-ignore shape inside the parser.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "worktree" => Some(Self::Worktree),
            "shared-cwd" => Some(Self::SharedCwd),
            _ => None,
        }
    }

    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::SharedCwd => "shared-cwd",
        }
    }
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
    /// The §3.1 enum, not free text. Serializes as its wire string, so the JSON is unchanged.
    pub harness: Harness,
    pub version: String,
    /// The model the child was launched with, **in the harness's own spelling and only if one
    /// actually reached the harness** — the compiled value, exactly as `allowed_tools` records
    /// "the compiled, harness-native constraint" rather than what was asked for (§3.1, §6.7).
    ///
    /// `None` is a *measurement*, not an absence of information: `codex exec` takes no model
    /// argument at all, so a Codex child's contract must not name one. Recording the requested
    /// model here would be the same class of lie as recording the requested harness — the bug
    /// `TaskContract.child.harness` was sourced from the adapter to end.
    ///
    /// `#[serde(default)]` keeps the addition backward-compatible: a contract persisted before
    /// this field existed still deserializes, with `None` meaning exactly what it means today.
    #[serde(default)]
    pub model: Option<String>,
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
    ///
    /// **Recorded verbatim, and wrapping a string in [`Oid`] here is not a validation.** marion
    /// does not check that these are well-formed object names, that they exist, or that they are
    /// reachable from the child's branch. The precedent is `narrative`, the other child-supplied
    /// field: also unverified, also recorded as it arrived, with provenance carried by a separate
    /// flag (`narrative_synthesized`) rather than by silently filtering the value. A contract that
    /// dropped an unreachable oid would imply marion had performed a check it did not perform, and
    /// making that honest needs a provenance flag of its own. **Do not add a silent filter here.**
    pub result_commits: Vec<Oid>,
    pub changed_paths: Vec<PathBuf>,
    /// All cap metadata lives here, including counters describing `TaskContract` fields: the cap
    /// runs only when the contract is returned, which happens only at the terminal transition.
    pub acceptance_criteria_omitted: usize,
    pub changed_paths_omitted: usize,
    /// Entries dropped from [`Self::result_commits`] by the cap; 0 iff none.
    ///
    /// It exists because `result_commits` is the only unbounded field a *child* fills, and rule 6's
    /// terminal stub promises a size that "does not depend on the input" — a promise that held only
    /// while the field was hardcoded empty.
    pub result_commits_omitted: usize,
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
    /// **`None` when there was no commit to branch from**, which [`Workspace::SharedCwd`] outside a
    /// repository makes reachable for the first time.
    ///
    /// A [`Workspace::Worktree`] always has one — marion creates the worktree *at* it — so this is
    /// `Some` on every path that existed before `shared-cwd` did. `RootChanged::base_commit` has
    /// been `Option` for the same reason since roots landed (*"there was no HEAD to read"*), and a
    /// contract has no better claim to a commit than a change record does.
    ///
    /// Not a zero oid and not an empty string. §6.7 makes this an audit record and §9's whole
    /// argument is that *"no check performed"* must not serialize as a clean result; a synthetic
    /// 40-hex value is a commit id a reader can look up, fail to find, and mistake for a pruned
    /// object rather than for an absence.
    pub base_commit: Option<Oid>,
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
