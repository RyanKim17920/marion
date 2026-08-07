//! The blocking `spawn` path (design §6.1, §6.7, §9).
//!
//! M1's shape: marion creates a worktree, writes the child's configuration, starts `codex exec`,
//! reads its JSONL until the process ends, then derives the contract from **git** and from the
//! child's `report` call. `spawn` blocks for the whole run and returns the completed contract.

use std::ffi::{OsStr, OsString};
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
    /// marion's own state directory is **inside** the repository a root's change record would
    /// measure ([`TreeSnapshot::open`]).
    ///
    /// A separate variant and not a `Git` failure, because git never failed: marion refused to ask
    /// it a question whose answer would have been about marion. The distinction reaches an operator
    /// — "git is not installed" and "your two directories are nested" are different fixes — and it
    /// reaches a reader of the journal, where this arrives as `RootObservation::Failed` with this
    /// sentence rather than as git's own words.
    #[error(
        "marion's state directory is inside the repository it would measure: {agent_dir} is under \
         {repo}. `git add -A .` would walk marion's own index, object store, journal and \
         configuration and record them as the root's work, and the object store would be changing \
         underneath the snapshot that is writing it — so the measurement is refused rather than \
         taken against a tree marion is itself writing into. Point `--state-dir` outside `--repo`; \
         the default state directory already is."
    )]
    StateDirInsideRepo { repo: PathBuf, agent_dir: PathBuf },
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
    /// §5.4's `allow_concurrent_writes`, in the **only** direction marion cannot serve.
    ///
    /// §11 item 23 left this parameter accepted-and-dropped and called it *inverted* relative to
    /// the other four: with no §6.6 holder check in code, concurrent writes were permitted de
    /// facto, so `true` was said to be honoured accidentally and `false` to be the value marion
    /// could not honour. **That analysis assumed `isolation` was live, and it is not** — the
    /// [`Self::IsolationUnimplemented`] refusal above makes `shared-cwd` unreachable,
    /// `run_spawn` calls `make_worktree` unconditionally, and `Workspace::SharedCwd` is
    /// constructed nowhere. Item 23 is corrected in the same commit that lifts the `background`
    /// refusal, because the two were always one decision.
    ///
    /// Under worktree-always the two values swap:
    /// * **`false` (the default) is honoured, and structurally rather than by a check marion has
    ///   to remember to run.** It asks marion to refuse a second write-capable spawn into an
    ///   occupied cwd; every child gets its own worktree, so no cwd is ever occupied by a second
    ///   writer. The guarantee is a property of the code path, not a promise — which is the
    ///   stronger form of the same answer, and is why `false` is not refused here.
    /// * **`true` asks marion to disable that guard** so the caller can share a cwd with a live
    ///   write-capable sibling. Its only entry point is `isolation: "shared-cwd"`, which is
    ///   refused by name. It is a request whose subject does not exist, and serving it silently
    ///   would tell a caller that marion had granted something it has no code to grant.
    ///
    /// **Why this had to be decided now rather than left.** Item 23 said so: the parameter was
    /// inert *"for exactly one reason: `spawn` blocks"*, and *"the day backgrounding lands in M2,
    /// `allow_concurrent_writes: false` becomes a live silent failure with nobody having touched
    /// this parameter or this code."* This is that day. The answer turned out to be that `false`
    /// is safe for a reason item 23 did not have in view, and that `true` is the half needing a
    /// sentence — but the answer had to be *reached*, not inherited.
    ///
    /// It comes out together with `isolation` when §6.6's holder registry lands.
    #[error(
        "spawn refused: `allow_concurrent_writes: true` is declared in marion's tool schema but \
         not implemented, and has nothing to permit — it is §6.6's escape hatch from the \
         shared-cwd write-conflict \
         rule, and marion creates a git worktree for every child, so no two children ever share a \
         cwd. `isolation: \"shared-cwd\"` is itself refused. Omit the field or pass `false`: with \
         a worktree per child, \"no second writer in my tree\" is what marion already guarantees."
    )]
    ConcurrentWritesUnimplemented,
    /// A **backgrounded** child's thread unwound.
    ///
    /// It exists because `Background::wait` must answer with something. Re-raising the panic into
    /// the bridge's JSON-RPC loop would end the bridge and every *other* live child with it, so a
    /// node's fault would become contagious to its siblings; and answering `Ok` with no contract
    /// would be the silent-success shape. A panic is a marion bug, so the sentence says so and
    /// does not invite the caller to retry.
    // The sentence used to end *"its journal records end at its `SpawnIntent`"*, which was never
    // true of any panic: `run.rs`'s `AbortOnDrop` is armed across the whole child run and `Drop`
    // runs on an unwind, so a `SpawnAborted` is always there — and if the panic came after the
    // child was reaped, so are `Spawned` and `Exited`. Pointing the reader at a record that is not
    // the last one sends them looking for a truncated journal instead of the abort that explains
    // it.
    #[error(
        "spawn failed: marion's own thread running the {0} child panicked, so there is no contract \
         and no way to say what the child did. This is a defect in marion, not in the request; the \
         child's process may have been left running, and the journal resolves the node with a \
         `SpawnAborted` written by the unwind rather than with a record of what it did."
    )]
    Panicked(String),
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
    /// **The bridge could not reach this project's supervisor** — §11 item 28 step 5, and the
    /// refusal that exists instead of a fallback.
    ///
    /// Since step 5 the bridge starts nothing: it dials §2's socket and sends `agent/spawn`. So a
    /// supervisor that is not there costs the *work*, and saying so is the whole point. The
    /// alternative was to spawn in-process when the dial fails, and it is the failure class this
    /// repository keeps re-finding: an identical-looking tool result, a real child, and a live node
    /// that no supervisor owns, can kill, or can hand to a re-attaching client. A node that is
    /// running at all was started by a supervisor, so nothing listening here means one died.
    ///
    /// It names the socket because that is the whole diagnosis — the path is derived, so an
    /// operator reading it can tell "no supervisor" from "the wrong project's supervisor".
    #[error(
        "marion could not use this project's supervisor, which is what runs a child since §11 item \
         28 step 5: {why} ({}). Nothing was started and nothing was journaled. There is \
         deliberately no in-process fallback — a bridge that quietly ran the child itself would \
         leave a live node no supervisor owned, could kill, or could hand to a re-attaching client.",
        socket.display()
    )]
    SupervisorUnreachable { socket: PathBuf, why: String },
    /// **The supervisor's own refusal, carried verbatim.**
    ///
    /// §6.1 step 2's gates, an unknown agent type, a `node_token` this supervisor did not mint —
    /// every one of them is answered by a sentence that already names the rule and the value that
    /// broke it, written for the model that will read it. Re-wording it in the bridge would put
    /// marion's guess in front of marion's answer, and paraphrasing a security refusal is how it
    /// stops naming what was actually wrong.
    #[error("{0}")]
    SupervisorRefused(String),
    /// The node reached a terminal state and marion cannot read the contract it should have left.
    ///
    /// Reachable only through a marion defect or a filesystem failure: `run_spawn` writes the file
    /// **before** the closing bookend this path waits for, precisely so that a reader acting on the
    /// bookend is not racing the write. Kept as its own variant, naming the path, because "the
    /// child produced nothing" and "marion cannot find what the child produced" are different news.
    #[error(
        "the child reached a terminal state and marion could not read the task contract it should \
         have written at {}: {why}. The node's own `events.jsonl` is the record of what it did.",
        path.display()
    )]
    NoContract { path: PathBuf, why: String },
    /// The node's stream ended with an **abort** rather than an exit: marion decided this node's
    /// fate before it produced a contract (§7.2), and the reason is the one marion journaled.
    #[error("the child did not run to a contract: {0}")]
    NodeAborted(String),
    /// **The caller's turn is not held past this, and the node is still running.**
    ///
    /// Not a verdict on the node: it keeps its own wall clock, keeps its slot, and its contract
    /// will be written where it always would have been. What expired is how long the bridge will
    /// block one caller — and, because the bridge dispatches frames on one thread, every caller
    /// behind it. A `spawn { background: true }` plus `wait` is the way to ask again.
    #[error(
        "the child outlived the {0} s marion will hold a synchronous `spawn` for — its own wall \
         clock plus a grace for everything around the run. It is still running and its contract \
         will still be written; nothing was cancelled."
    )]
    OutlivedTheWait(u64),
}

impl SpawnError {
    /// **What happened to the child, in three words**, so [`crate::bridge::spawn_result`] can open
    /// every refusal with one sentence shape without asserting the wrong half of it.
    ///
    /// Every variant above the socket ones is a launch that did not happen, and *"could not be
    /// launched"* is exactly right for them. The ones step 5 added are not: a child that ran, and
    /// whose contract marion then could not deliver, is a different fact — and telling a parent its
    /// child never started when a real process did real work is the kind of false report the rest
    /// of this file exists to delete.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::NoContract { .. } | Self::NodeAborted(_) | Self::OutlivedTheWait(_) => {
                "ran, and marion cannot hand you its contract"
            }
            _ => "could not be launched",
        }
    }
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

/// **Serializes every git command that writes the shared repository**, which backgrounding made
/// necessary and which nothing before it needed.
///
/// While `spawn` was synchronous there was exactly one worktree operation in flight per process and
/// no lock could be contended. A backgrounded `spawn` runs `run_spawn` on a thread, so two children
/// of one parent reach `git worktree add` — and later `git worktree remove` — against **the same
/// `.git`** at the same time, and git does not serialize them.
///
/// **MEASURED, and the measurement corrected the reason.** This comment used to name
/// `index.lock` — *"Unable to create '…/index.lock': File exists"* — and to say that the guard had
/// no failing witness, because removing it did not fail `tests/background_spawn.rs` over ten runs.
/// Both halves were wrong, and `tests/fixtures/s17/README.md` is the run that says so
/// (darwin 25.5.0, git 2.50.1, `spikes/s17/run.sh`, six repetitions at two, four and six
/// concurrent writers).
///
/// 1. **`index.lock` is never the failure.** It does not appear once in any recorded run. Every
///    observed failure is `.git/worktrees/` bookkeeping: *"could not create directory of
///    '.git/worktrees/<name>': Invalid argument"*, the same with *"No such file or directory"*, and
///    *"failed to read .git/worktrees/<name>/commondir: Undefined error: 0"*.
/// 2. **A caller is failed by a *sibling's* half-written entry.** In the third shape the name in
///    the message belongs to a **different** worker's worktree, because `add` and `remove` both
///    walk the whole of `.git/worktrees/` as they prune. Nothing locks that directory — it is not
///    the index — so the hazard is wider than a contended lockfile and cannot be waited out.
/// 3. **A lost race leaks rather than merely failing.** A failed `worktree remove` leaves the
///    worktree registered, so the `branch -D` behind it is refused in turn (*"cannot delete branch
///    … used by worktree at …"*): a directory, a `.git/worktrees/` entry and a branch survive an
///    operation marion believes cleaned up.
/// 4. **The guard is load-bearing.** Four concurrent writers — exactly `max_concurrent_children` —
///    reproduce in every repetition within 300 iterations each. `background_spawn.rs` performs
///    four `worktree add`s **once**, so its exposure is short of the threshold by three orders of
///    magnitude; the surviving mutation measured that test's reach, not this guard's necessity.
///
/// Widening a timeout or retrying would encode the race rather than remove it; a mutex removes it,
/// because the operations are short and marion is the only writer it needs to coordinate.
///
/// **What it does not claim, and this is now a measured gap rather than an assumed one.** It
/// serializes marion's *own* concurrent writers inside **one process**. A second `marion` process,
/// or the operator's own `git`, is outside it — and S17 measured that two *processes* fail at
/// roughly one operation in 1 800, so "that is git's problem and git's lock" was not true: git has
/// no lock there. Closing it needs a file lock beside the repository, which is a decision §5.7 does
/// not contain; it is recorded as design §11 item 31 rather than invented here.
///
/// `PoisonError` is unwrapped through rather than propagated: a panic inside a git call leaves the
/// *repository* consistent (git is transactional over its own locks) and refusing every subsequent
/// spawn for the rest of the process's life would be a larger failure than the one it guards.
static REPO_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold [`REPO_WRITE`] for the duration of a repository-mutating git call.
pub fn repo_write_guard() -> std::sync::MutexGuard<'static, ()> {
    REPO_WRITE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Create the child's worktree at `base_commit`.
///
/// `rev-parse HEAD` is inside the guard as well as `worktree add`, deliberately: the two are one
/// operation — *"branch this child off whatever HEAD is now"* — and reading HEAD outside the lock
/// would let a concurrent sibling's `worktree add` land between them, so two children could record
/// different `base_commit`s for the same instant, or one could record a base its worktree was not
/// actually created at. §6.7 calls `base_commit` an audit record; an audit record raced against the
/// thing it describes is worse than a slower spawn.
pub fn make_worktree(repo: &Path, path: &Path, branch: &str) -> Result<Oid, SpawnError> {
    let _serialized = repo_write_guard();
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

/// `git` in `wt` with an explicit environment overlay, and **nothing inherited that decides where
/// git writes**.
///
/// The overlay is a slice rather than a fixed `GIT_INDEX_FILE` because two callers need different
/// ones and they must not become two implementations: [`git_indexed`] redirects the index, and
/// [`TreeSnapshot`] redirects the index *and* the object store. One function, so a caller cannot
/// half-redirect — which for the snapshot path would mean writing blobs into the operator's own
/// `.git/objects`.
fn git_env(wt: &Path, env: &[(&str, &OsStr)], args: &[&str]) -> Result<String, SpawnError> {
    let mut cmd = SysCommand::new("git");
    cmd.current_dir(wt).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(SpawnError::Git(
            "command",
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `git` in `wt` with `GIT_INDEX_FILE` pointed at the scratch index.
///
/// Every index-mutating call in `diff_text` goes through here. The workspace's own index is never
/// named, which is the property §6.7 states twice: a bare `git add -N` in the workspace permanently
/// changes what `git status`, `git diff`, `git stash` and `git commit -a` do for the user — on
/// files marion was only ever reading — and since `isolation` defaults to `shared-cwd`, that
/// workspace is by default the user's own checkout.
fn git_indexed(wt: &Path, index: &ScratchIndex, args: &[&str]) -> Result<String, SpawnError> {
    git_env(wt, &[("GIT_INDEX_FILE", index.0.as_os_str())], args)
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

/// **The working tree of the operator's own repository, as a git tree object** — the base point of
/// a root's change record (§9, `marion_core::root_change`).
///
/// A child gets `changed_paths` and `diff` from a worktree marion created and later removes. A root
/// runs in `RootSpec::repo`, which marion neither created nor may disturb, so the same measurement
/// needs a different instrument. This is it: the tree is snapshotted as a git tree object at
/// `prepare` and again at exit, and the root's change is the diff of the two.
///
/// # Why two trees, and not `HEAD`, and not a path list
///
/// **Not `HEAD`.** A dirty repo makes `HEAD` attribute the operator's own uncommitted work to the
/// root — a lie in an audit record, and the exact failure class `8a69f22` ended: a clean bill of
/// health nobody measured.
///
/// **Not a recorded pre-run status** (a path list plus hashes) either. That yields a *set*, never a
/// patch, and a file the operator half-edited and the root then edited further gives the wrong hunk
/// — the one case a change record most needs to get right. Against two trees the subtraction is
/// exact, and `changed_paths` and `diff` come out of the same `git diff`, so they cannot describe
/// different runs. The child path needed the `add -N` scratch-index dance in [`diff_text`]
/// precisely because it had no pre-tree to diff against.
///
/// # Why nothing is written to the operator's `.git`
///
/// `GIT_OBJECT_DIRECTORY` sends every blob and tree `git add -A` and `git write-tree` create into
/// `<agent-dir>/objects`; `GIT_ALTERNATE_OBJECT_DIRECTORIES` keeps the repository's real objects
/// *readable*. `GIT_INDEX_FILE` is a **copy** of the repo's index, never the index itself. This
/// extends the non-mutation discipline `git_indexed` already states one step further than the child
/// path needed, because a child's worktree is marion's and the operator's repository is not — and
/// `.git/objects` growing by a run marion made is a mutation even though it breaks nothing.
///
/// The copy is not only about not sharing. It carries the index's **stat cache**, without which
/// `git add -A` re-hashes every file in the worktree. Measured on a 10,000-file / 39 MB tree with
/// git 2.50.1: `add -A` takes **41 ms warm and 382 ms cold**, and the gap grows with the tree, since
/// the cold path reads every byte. On this repository (222 tracked files) it is 27 ms against 43 ms.
/// Both snapshots together cost ~70 ms warm, which is why the copy is worth the disk it uses.
#[derive(Debug)]
pub struct TreeSnapshot {
    /// The copied index. Lives under the agent dir rather than the repo, so it can never itself
    /// show up as an untracked file in the tree it is being used to measure — [`ScratchIndex`]'s
    /// reason, and the same trap.
    index: PathBuf,
    objects: PathBuf,
    /// The repo's real object store plus whatever alternates it already had, `:`-joined for
    /// `GIT_ALTERNATE_OBJECT_DIRECTORIES`.
    alternates: OsString,
}

impl TreeSnapshot {
    /// Prepare the isolated git environment, or say why it is impossible.
    ///
    /// **Every failure here is a refusal to measure, never a silent skip.** The caller turns it into
    /// `RootObservation::NotAttempted` or `Failed` with this error's own words, because a root that
    /// produced no delta because nobody looked must never read as a root that changed nothing.
    pub fn open(repo: &Path, agent_dir: &Path) -> Result<Self, SpawnError> {
        // **The state directory must not be inside the repository being measured** — refused
        // first, because every other failure here is git's and this one is marion's own.
        //
        // `marion run --repo /r --state-dir /r/.marion-state` is accepted by the CLI and, until
        // this check, by everything downstream: the agent dir is created before the pre-snapshot,
        // so the copied index and the snapshot object store land *inside the tree `add -A .` is
        // about to walk*. Three things then go wrong at once and not one of them announces itself.
        // marion's own journal, configuration, sidecar and loose objects are measured as root
        // activity, so the record describes marion. The object store the snapshot is writing into
        // changes underneath it while `write-tree` runs. And the delta grows by whatever marion
        // wrote between the two snapshots, which is a number nobody can subtract afterwards.
        //
        // **Refused rather than excluded.** Skipping the state dir inside the walk was the other
        // option and is worse: the delta's whole claim is that it is the working tree, and an
        // exclusion weakens that claim for every run to rescue one misconfiguration — the class of
        // trade §6.7 refuses elsewhere. This is one directory the operator can move.
        //
        // **Refused even when the state dir is gitignored**, where the walk would in fact skip it.
        // Making the refusal conditional on an ignore rule would make the record's correctness
        // depend on a file the operator owns and can edit mid-run, and the failure when they do is
        // the silent one this whole record exists to end.
        //
        // Both paths are canonicalised first: `/var` and `/private/var` are the same directory on
        // macOS and a textual `starts_with` says they are not, which would make the refusal fire
        // for nobody. A path that cannot be canonicalised is compared as given — a check that
        // cannot resolve its subject must not silently pass.
        let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        if real(agent_dir).starts_with(real(repo)) {
            return Err(SpawnError::StateDirInsideRepo {
                repo: repo.to_path_buf(),
                agent_dir: agent_dir.to_path_buf(),
            });
        }
        // Asked of git rather than by probing for a `.git` entry: a linked worktree's `.git` is a
        // *file*, a bare repo has no worktree at all, and `$GIT_DIR` can point anywhere. One
        // question, answered by the tool that owns it.
        if git(repo, &["rev-parse", "--is-inside-work-tree"])?.trim() != "true" {
            return Err(SpawnError::Git(
                "rev-parse --is-inside-work-tree",
                format!("{} is not inside a git working tree", repo.display()),
            ));
        }
        let real_objects = git_path(repo, "objects")?;
        let objects = agent_dir.join("objects");
        std::fs::create_dir_all(&objects)?;

        // **The repo's own alternates are carried forward.** `GIT_OBJECT_DIRECTORY` replaces the
        // main object store, and git reads `info/alternates` from *that* directory — so a repository
        // created by `git clone --shared`, or one whose objects live in a borrowed store, would lose
        // sight of most of its own history and `add -A` would start re-writing objects it already
        // has. Cheap to carry, and the failure without it is a wrong tree rather than an error.
        let mut alternates = OsString::from(real_objects.as_os_str());
        if let Ok(existing) = std::fs::read_to_string(real_objects.join("info/alternates")) {
            for line in existing.lines().map(str::trim).filter(|l| !l.is_empty()) {
                alternates.push(":");
                alternates.push(line);
            }
        }

        let index = agent_dir.join("snapshot-index");
        // A missing source index is not a failure: a repository with nothing ever added has none,
        // and git will create one. What must not happen is *reusing* a stale copy from an earlier
        // run, which would be read as this run's starting point.
        let _ = std::fs::remove_file(&index);
        let repo_index = git_path(repo, "index")?;
        if repo_index.exists() {
            std::fs::copy(&repo_index, &index)?;
        }
        Ok(Self {
            index,
            objects,
            alternates,
        })
    }

    fn env(&self) -> [(&'static str, &OsStr); 3] {
        [
            ("GIT_INDEX_FILE", self.index.as_os_str()),
            ("GIT_OBJECT_DIRECTORY", self.objects.as_os_str()),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                self.alternates.as_os_str(),
            ),
        ]
    }

    /// The working tree, right now, as a tree object.
    ///
    /// `add -A` and not `add -u`: additions, deletions and modifications all have to be in it, and
    /// a root's most alarming write is a *new* file. It respects `.gitignore`, which is a stated
    /// boundary rather than an oversight — see §11 items 19 and 26.
    pub fn take(&self, repo: &Path) -> Result<Oid, SpawnError> {
        // `.` rather than a bare `-A`: identical at the repo root, and explicit about the subject
        // if this is ever called from anywhere else.
        git_env(repo, &self.env(), &["add", "-A", "."])?;
        Ok(Oid(git_env(repo, &self.env(), &["write-tree"])?
            .trim()
            .into()))
    }

    /// **How many entries [`Self::take`] was never allowed to look at**, right now.
    ///
    /// `git add -A .` respects `.gitignore`, so the tree it writes is the tree *git can see* and a
    /// root writing only `.env` moves nothing (§11 item 26). That is the instrument's boundary and
    /// no amount of git widens it — but the boundary's *size* is one cheap question, and a reader
    /// who can ask it can tell an empty delta that is exhaustive from one that is not.
    ///
    /// `--porcelain --ignored` and not `--ignored=matching`: the default collapses an ignored
    /// directory to one entry, so `target/` costs one line rather than forty thousand. The answer
    /// is therefore a count of *entries* — which is what
    /// `marion_core::root_change::RootDelta::Observed::ignored_not_measured` says it is.
    ///
    /// The snapshot's own environment, for the reason every other call here uses it: this walk must
    /// not be the one thing in this type that touches the operator's index.
    pub fn ignored_entries(&self, repo: &Path) -> Result<usize, SpawnError> {
        Ok(
            git_env(repo, &self.env(), &["status", "--porcelain", "--ignored"])?
                .lines()
                .filter(|l| l.starts_with("!!"))
                .count(),
        )
    }

    /// `HEAD`, as **context** and never as a diff base. `None` where there is no commit yet.
    pub fn head(&self, repo: &Path) -> Option<Oid> {
        git(repo, &["rev-parse", "HEAD"])
            .ok()
            .map(|s| Oid(s.trim().to_string()))
    }

    /// Paths differing between two revisions. `--no-renames` matches [`diff_text`]: a rename
    /// rendered as a rename carries no content, and the path list and the patch must describe the
    /// same run.
    pub fn changed_paths(&self, repo: &Path, a: &Oid, b: &Oid) -> Result<Vec<PathBuf>, SpawnError> {
        Ok(git_env(
            repo,
            &self.env(),
            &["diff", "--name-only", "--no-renames", &a.0, &b.0],
        )?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
    }

    /// The patch between two trees — the same two revisions [`Self::changed_paths`] is asked about,
    /// so the two share one dialect by construction rather than by agreement.
    pub fn diff(&self, repo: &Path, a: &Oid, b: &Oid) -> Result<String, SpawnError> {
        git_env(repo, &self.env(), &["diff", "--no-renames", &a.0, &b.0])
    }

    pub fn objects_dir(&self) -> &Path {
        &self.objects
    }

    /// Drop the copied index once both snapshots are taken.
    ///
    /// Explicit rather than a `Drop` impl: this outlives a `prepare` and is used again at exit, so
    /// the scope that would run `Drop` is not the scope that finishes with it. Best-effort for
    /// [`ScratchIndex`]'s reason — a leftover index costs disk in marion's own state dir and must
    /// not turn a finished run into a failed one.
    pub fn release(&self) {
        let _ = std::fs::remove_file(&self.index);
    }
}

/// One of git's own paths, made absolute.
///
/// `--git-path` and not `<repo>/.git/<leaf>`: a linked worktree's index is under
/// `.git/worktrees/<name>/`, a `$GIT_DIR` override puts it somewhere else entirely, and guessing
/// wrong here means copying a file that is not the index and measuring against a base point that
/// was never the tree. `--path-format=absolute` is deliberately not used — it needs git 2.31 — so
/// the relative answer is joined onto the repo instead.
fn git_path(repo: &Path, leaf: &str) -> Result<PathBuf, SpawnError> {
    let p = git(repo, &["rev-parse", "--git-path", leaf])?;
    let p = Path::new(p.trim());
    Ok(if p.is_absolute() {
        p.to_path_buf()
    } else {
        repo.join(p)
    })
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
