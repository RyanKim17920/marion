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
    /// A profile the launch named cannot be used — refused before the node exists.
    #[error(transparent)]
    Profile(#[from] crate::profiles::ProfileError),
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
    /// The launch names a provider marion cannot point it at — unknown, logged out, no model, or
    /// no wire shared with the harness. Refused before the node exists.
    #[error("{0}")]
    Endpoint(#[from] crate::endpoint::EndpointError),
    /// The tree's `.marion/agents.toml` exists and cannot be used — unreadable, or refused by
    /// `marion_core::agent_type::AgentTypes::parse`. Its own variant rather than
    /// [`Self::UnknownAgentType`], because the fix is in the file rather than the request, and
    /// every spawn against that tree is refused until the file is fixed (§3.1: a load error, never
    /// a default).
    #[error("{path}: {error}")]
    AgentTypesFile { path: PathBuf, error: String },
    /// The type runs a command its tree's `.marion/agents.toml` names, and the operator has not
    /// allowed that file's current bytes (`marion trust allow`). Refused before the node exists.
    #[error(transparent)]
    Untrusted(#[from] crate::trust::TrustError),
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
    /// **§6.6's escape hatch, asked for in a mode that cannot share a cwd.**
    ///
    /// The doc above is the history; this is what the variant now means, and the two differ because
    /// the holder registry landed. `allow_concurrent_writes: true` is **no longer refused outright**
    /// — under `isolation: "shared-cwd"` it is implemented, and it is the one thing that suppresses
    /// [`Self::CwdOccupied`]. What is refused is asking for it under `worktree`, where it has
    /// nothing to permit: marion creates a private tree for that child, so no sibling can be in it
    /// and no guard is being disabled.
    ///
    /// Refused rather than accepted-and-dropped even though the outcome looks harmless, because the
    /// two readings of a granted `true` are not the same promise. A caller passing it believes it
    /// may share a tree with a live writer; under `worktree` it may not, and marion answering
    /// `isError: false` would confirm a capability it did not grant.
    ///
    /// `false` and absence are not refused in either mode: `false` is exactly what marion does.
    #[error(
        "spawn refused: `allow_concurrent_writes: true` has nothing to permit under `isolation: \
         {isolation:?}`. It is §6.6's escape hatch from the shared-cwd write-conflict rule — it \
         lets a child share a directory with a live write-capable sibling — and marion gives a \
         `worktree` child its own tree, so it has no sibling to share with and no guard to lift. \
         Pass `isolation: \"shared-cwd\"` if you meant the child to run in the caller's own \
         directory, or omit this field."
    )]
    ConcurrentWritesUnimplemented { isolation: &'static str },
    /// **§6.6's write-conflict rule: at most one write-capable node per cwd.**
    ///
    /// *"A second write-capable spawn into an occupied cwd is refused, naming the holder"* — and it
    /// names the holder because that is the only part of the sentence a caller can act on. "This
    /// directory is busy" leaves them guessing at which of their own children to wait for; an agent
    /// id is a node they can `wait` on.
    ///
    /// Unreachable until now: every child got its own worktree, so no cwd was ever occupied twice
    /// and §11 item 23 recorded the rule as a refusal that is not in code. `shared-cwd` is what
    /// makes it reachable, which is why the guard lands in the same change as the workspace and not
    /// after it.
    ///
    /// **Two children writing one tree is worse than two humans doing it** (§6.6): each harness
    /// keeps its own checkpoint state, so a checkpoint restore in one child silently reverts the
    /// other's work. `ToolCall.locations` would say afterwards who touched what, but attribution is
    /// forensics — this is prevention.
    ///
    /// `allow_concurrent_writes: true` is the caller's way past it, per §5.4, and is the only thing
    /// that suppresses this check.
    #[error(
        "spawn refused (§6.6): {cwd} already has a live write-capable node in it — agent {holder} — \
         and at most one node with write tools may share a cwd. Nothing was started. Two agents \
         writing one tree is lost-update, and worse than two people doing it: each harness keeps \
         its own checkpoint state, so a checkpoint restore in one child silently reverts the \
         other's work. Wait for {holder} to finish, or pass `isolation: \"worktree\"` to give this \
         child its own tree, or pass `allow_concurrent_writes: true` to take that risk \
         deliberately.",
        cwd = cwd.display(),
        holder = holder.0
    )]
    CwdOccupied {
        cwd: PathBuf,
        holder: marion_core::contract::AgentId,
    },
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
    /// §5.4's `isolation`, for the **one** value that is still a name without a workspace.
    ///
    /// This variant used to cover `shared-cwd` as well, and covering it was the whole of §11 item
    /// 23's `isolation` bullet: `run_spawn` called `make_worktree` unconditionally,
    /// `Workspace::SharedCwd` was constructed nowhere, and marion therefore refused §3.1's own
    /// default and accepted only the value that hard-requires git. `shared-cwd` is now built, so it
    /// is no longer named here — a refusal kept past the gap it described is just a refusal.
    ///
    /// **`remote` stays, and not merely because it is unfinished.** It is the dangerous direction
    /// of the two item 23 separated: a request to run somewhere *else*, served by running on the
    /// operator's own machine. There is no `Workspace::Remote`, no transport, no host and no auth
    /// story anywhere in the design, and §1 puts *"remote hosting"* out of scope in as many words.
    /// So it is refused at the edge and is deliberately not a [`marion_core::contract::Isolation`]
    /// variant: a value the type system can hold is a value some later call site may quietly
    /// default, and the point of that enum is that everything in it is a workspace marion builds.
    ///
    /// `worktree`, `shared-cwd` and absence are **not** refused: all three now name something marion
    /// does.
    #[error(
        "spawn refused: `isolation: {0:?}` is not a workspace marion can build. `\"worktree\"` gives \
         the child its own git worktree branched off HEAD, and `\"shared-cwd\"` runs it in the \
         caller's own directory (§6.6) — those two are implemented. `\"remote\"` is declared in the \
         schema and has no transport, no host and no code path anywhere; design §1 puts remote \
         hosting out of scope. Serving it by running on this machine would be a request to run \
         elsewhere answered by running here, which is worse than this refusal."
    )]
    IsolationUnimplemented(String),
    /// **`isolation: "worktree"` where there is no repository to add a worktree to.**
    ///
    /// The refusal that replaces a leaked `git command failed: fatal: not a git repository`. That
    /// message came out of [`git`]'s generic stderr passthrough inside `make_worktree`, and it named
    /// neither the command marion ran nor the directory it ran it in — so an operator saw git's
    /// words about a directory git did not mention, from a tool they did not invoke, and had no way
    /// to tell which of `--repo`, their cwd, or marion itself was wrong. It also arrived *after* the
    /// node had an identity, a journal intent and an agent directory, for a condition that is
    /// knowable before any of them.
    ///
    /// Checked with `socket::git_common_dir`, which is §2's own derivation, so "marion says this is
    /// not a repository" and "marion keys this project on its cwd" can never disagree.
    ///
    /// **Both exits are named because both are real fixes**, and which one is right is the
    /// operator's call, not marion's: `git init` makes the tree a repository and keeps the
    /// containment; `isolation: "shared-cwd"` keeps the tree as it is and gives up the containment.
    /// Guessing either — silently downgrading to `shared-cwd`, or running `git init` on someone's
    /// directory — is a side effect nobody asked for.
    #[error(
        "spawn refused: `isolation: \"worktree\"` needs a git repository to add a worktree to, and \
         {cwd} is not in one — `git -C {cwd} rev-parse --git-common-dir` reports no repository. \
         Nothing was started. Either run `git init` in {cwd} (the child then gets its own worktree \
         branched off HEAD, and §6.7's diff, changed_paths and scope check all work), or pass \
         `isolation: \"shared-cwd\"` to run the child directly in that directory — with no \
         repository there is no diff route, so its contract will honestly record \
         `scope_enforced: false` (§6.7).",
        cwd = cwd.display()
    )]
    NotAGitRepo { cwd: PathBuf },
    #[error("invalid writable scope: {0}")]
    Scope(#[from] marion_core::scope::ScopeError),
    /// The spawn's `verification` lines are larger than the node's intent record may carry.
    ///
    /// The lines are journaled on the `SpawnIntent` so a resumed child re-runs them, and a record
    /// over `MAX_RECORD_BYTES` is dropped whole — which would lose the node's identity record, not
    /// just its verification. So this is refused before the intent is written, naming the measured
    /// size and the cap ([`crate::run::MAX_VERIFICATION_BYTES`]), rather than served with a node
    /// the journal never heard of.
    #[error(
        "spawn refused: `verification` is {bytes} bytes as the journal encodes it, over marion's \
         {cap}-byte cap (MAX_VERIFICATION_BYTES). The lines are recorded on the child's spawn \
         intent so a resumed child re-runs them, and a record that large could not be journaled. \
         Nothing was started. Move long checks into a script in the repository and pass the \
         command that runs it."
    )]
    VerificationTooLarge { bytes: usize, cap: usize },
    #[error("compiling the child's launch: {0}")]
    Harness(#[from] marion_harness::HarnessError),
    /// §6.1 step 8's gate, failed on a **child**. Carried through rather than flattened so the
    /// cause survives into the tool result the parent reads: the alternative to a loud refusal here
    /// is a child that took its turn without marion's tools, reported nothing, and exited 0 — the
    /// §12 silent-failure shape, indistinguishable from a run that simply had nothing to say.
    #[error("driving the child over its control plane: {0}")]
    Duplex(#[from] crate::duplex::DuplexError),
    /// The same failure class on the **other** typed control plane, kept as its own variant rather
    /// than folded into [`Self::Duplex`].
    ///
    /// Its variants say things `DuplexError` has no way to say and a parent has to be able to act
    /// on: the agent never answered `initialize`, or the *vendor* refused to open a session at all
    /// (S20's `-32000`, which no marion change reaches). Flattening them into "driving the child
    /// over its control plane" would turn a vendor's own refusal into a marion-shaped error, which
    /// is the misattribution §8 spends a whole mode preventing.
    #[error("driving the ACP child: {0}")]
    Acp(#[from] crate::acp_child::AcpChildError),
    /// The app-server child produced no transcript: no handshake, no thread, marion's MCP server
    /// never ready. The driver's own sentence, which carries the server's words.
    #[error("driving the app-server child: {0}")]
    AppServer(#[from] crate::app_server::AppServerError),
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
    /// The proposed child's first durable fact could not be confirmed, so no side effect was taken
    /// and nothing needs aborting. The append may have failed before writing or while syncing
    /// bytes already written; treating either as a known intent would invent certainty.
    #[error(
        "could not durably journal the spawn intent for proposed node {} before launch: {source}",
        agent_id.0
    )]
    SpawnIntentBarrier {
        agent_id: marion_core::contract::AgentId,
        #[source]
        source: crate::journal::JournalError,
    },
    /// **The process started and the journal would not take it, so the process was unwound.**
    ///
    /// §6.1 step 7's `Spawned` is the only record carrying a pid, which makes it the only record
    /// whose loss costs marion the *name* of a live process rather than a stale reading of it:
    /// `procid::audit`'s scope is `node.pid.is_some()`, so a child whose barrier never landed is
    /// invisible to the very audit §9 criterion 3 is decided by. Answering this spawn successfully
    /// would be marion reporting a node its own authoritative record says does not exist.
    ///
    /// So the node is killed and reaped before this is returned, and it is returned *instead of*
    /// whatever the driver made of the kill. By the time a caller sees this there is no process:
    /// the node replays as a bare `SpawnIntent`, which now means exactly that.
    #[error(
        "the child started, but marion could not record it and so did not keep it: writing the \
         `Spawned` barrier for {} failed ({why}). The process and its descendants were \
         killed and reaped rather than left running with nothing on the record able to name them.",
        agent_id.0
    )]
    UnaccountableNode {
        agent_id: marion_core::contract::AgentId,
        why: String,
    },
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
            // A third fact, for the same reason the second one exists: this child's process really
            // did start, so *"could not be launched"* is the false half — and it was killed before
            // it did any work, so *"ran"* is the other false half. Both would be the kind of wrong
            // report this method was written to stop.
            Self::UnaccountableNode { .. } => {
                "was started and then unwound, because marion could \
                                               not record it"
            }
            _ => "could not be launched",
        }
    }
}

/// **`git diff` in its plain dialect**, for a patch marion persists (a task contract, a root-change
/// record). Each flag cancels one piece of operator configuration that would otherwise decide the
/// bytes: `color.ui`, `diff.noprefix`/`diff.mnemonicPrefix`, `diff.external` and a `textconv`
/// driver. The last two would also run an operator-chosen program over content an agent wrote.
/// `--no-renames` matches the `--name-only` listings beside each patch: a rename rendered as a
/// rename carries no content, and the path list and the patch must describe the same run.
const PLAIN_PATCH: [&str; 7] = [
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--no-renames",
];

fn plain_patch_args<'a>(revs: &[&'a str]) -> Vec<&'a str> {
    PLAIN_PATCH
        .iter()
        .copied()
        .chain(revs.iter().copied())
        .collect()
}

fn git(repo: &Path, args: &[&str]) -> Result<String, SpawnError> {
    let mut command = SysCommand::new("git");
    command
        .current_dir(repo)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut command)?
        .wait_with_output()?;
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

/// §6.6's occupancy table: **which cwd currently holds a live write-capable node**.
///
/// Keyed on the *canonicalized* cwd, because occupancy is a fact about a directory and not about a
/// spelling of it: `/tmp/p`, `/tmp/p/.`, and a symlink to either are one tree, and a table keyed on
/// the literal argument would let two writers into it by arriving through two names. Canonicalizing
/// is also what makes the key comparable to the one §2 hashes the supervisor on.
///
/// **What this closes, and what it explicitly does not.** It closes the in-process case: every
/// write-capable `shared-cwd` node this supervisor owns is in this table for its whole life, so a
/// second one is refused by [`SpawnError::CwdOccupied`] with the first one's agent id. It does
/// **not** close the cross-process case, and that is the same open edge §11 item 31 records against
/// [`REPO_WRITE`] one screen up — a second `marion` process, or the operator's own editor, holds no
/// entry here and is not consulted. Item 31 measured the analogous git race failing in *every*
/// repetition at four concurrent processes; nothing in this change moves that number, and the
/// closure it names — a file lock beside the repository — would be needed here too. This guard is
/// the honest half of §6.6, and saying which half is the point.
///
/// A `Mutex<HashMap>` and not a lock per cwd: the critical section is a hash lookup and an insert,
/// the table is at most `max_concurrent_children` deep per node, and a striped design would buy
/// nothing measurable while making the "check and claim" step non-atomic — which is the one property
/// that must hold, since two spawns racing a `contains_key` would both pass it.
static CWD_HOLDERS: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, AgentId>>> =
    std::sync::Mutex::new(None);

/// A claim on a cwd, released when it drops.
///
/// **RAII and not a matched release call**, because every exit from `run_spawn` must release it and
/// several of them are `?` returns — the same reason `run.rs` resolves a journal intent with
/// `AbortOnDrop` rather than at each return. A leaked claim is worse than no guard at all: it
/// refuses every future spawn into that directory for the life of the supervisor, naming a node that
/// has long since exited, and an operator cannot clear it without restarting.
///
/// `None` in [`Self::cwd`] is the un-claimed case — a `worktree` child, a read-only `shared-cwd`
/// child, or one that passed `allow_concurrent_writes: true`. Carrying the guard unconditionally and
/// letting it be empty keeps the release path single, rather than making every caller remember
/// whether it took a claim.
#[derive(Debug)]
pub struct CwdClaim {
    cwd: Option<PathBuf>,
}

impl Drop for CwdClaim {
    fn drop(&mut self) {
        let Some(cwd) = self.cwd.take() else { return };
        let mut t = CWD_HOLDERS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = t.as_mut() {
            map.remove(&cwd);
        }
    }
}

impl CwdClaim {
    /// The claim a node that cannot occupy a cwd holds: nothing, released by dropping nothing.
    pub fn none() -> Self {
        Self { cwd: None }
    }

    /// **Claim `cwd` for `holder`, or refuse naming whoever has it** (§6.6).
    ///
    /// Check and claim are one locked step. Split into "is it free?" then "take it", two spawns
    /// could both read free and both write, which is exactly the lost-update this exists to prevent
    /// — one level up from the trees it is preventing it in.
    pub fn claim(cwd: &Path, holder: &AgentId) -> Result<Self, SpawnError> {
        let key = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let mut t = CWD_HOLDERS.lock().unwrap_or_else(|e| e.into_inner());
        let map = t.get_or_insert_with(std::collections::HashMap::new);
        if let Some(existing) = map.get(&key) {
            return Err(SpawnError::CwdOccupied {
                cwd: key.clone(),
                holder: existing.clone(),
            });
        }
        map.insert(key.clone(), holder.clone());
        Ok(Self { cwd: Some(key) })
    }
}

/// Create the child's worktree at `base_commit`.
///
/// `rev-parse HEAD` is inside the guard as well as `worktree add`, deliberately: the two are one
/// operation — *"branch this child off whatever HEAD is now"* — and reading HEAD outside the lock
/// would let a concurrent sibling's `worktree add` land between them, so two children could record
/// different `base_commit`s for the same instant, or one could record a base its worktree was not
/// actually created at. §6.7 calls `base_commit` an audit record; an audit record raced against the
/// thing it describes is worse than a slower spawn.
///
/// **`carry_work`: the child starts from its parent's current state, not its parent's last
/// commit.** Measured live (s2, 2026-09-27): a claude child in its own worktree wrote the code, then
/// spawned codex grandchildren to run its tests, and every grandchild's worktree was cut from the
/// base commit — none of them saw the edits they were asked to test. So where `repo` is a worktree
/// marion made for the caller, the caller's uncommitted work is first committed onto the caller's
/// own `marion/<task_id>` branch ([`CARRIED_WORK_MESSAGE`]), by the same recipe that lands a child's
/// work at its end ([`commit_child_work`]), and the child is cut from that commit.
///
/// Committed on the caller's branch rather than held in a commit on no branch, because that branch
/// is marion's and is where the caller's work lands at its end anyway: the caller's tree is left
/// clean, its contract's `changed_paths` still count the work (they diff from the caller's own
/// base), and the child's branch then merges back as a fast-forward. The operator's own checkout —
/// a root's tree, or a `shared-cwd` child's — is never committed to, and a child of it branches from
/// its HEAD as it always did. The same guard covers the commit and the cut, so no sibling's spawn
/// can interleave between them.
pub fn make_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    carry_work: bool,
) -> Result<Oid, SpawnError> {
    make_worktree_at(repo, path, branch, None, carry_work)
}

/// [`make_worktree`], cut at `at` where one is named — a reviewer's tree, at the reviewed node's
/// landed commit — and at `repo`'s HEAD otherwise. `carry_work` applies only to a HEAD cut: a
/// named commit already is the work the child is to see.
pub fn make_worktree_at(
    repo: &Path,
    path: &Path,
    branch: &str,
    at: Option<&Oid>,
    carry_work: bool,
) -> Result<Oid, SpawnError> {
    let _serialized = repo_write_guard();
    let head = match at {
        Some(commit) => commit.0.clone(),
        None => {
            if carry_work {
                let head = git(repo, &["rev-parse", "HEAD"])?.trim().to_string();
                commit_all(repo, &Oid(head), CARRIED_WORK_MESSAGE)?;
            }
            git(repo, &["rev-parse", "HEAD"])?.trim().to_string()
        }
    };
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

/// **HEAD of the tree at `cwd`, or `None` because there is no commit to name.**
///
/// The `shared-cwd` counterpart of the `rev-parse HEAD` inside [`make_worktree`], and separate from
/// it because the two answer to different failures. There, a missing HEAD is fatal — the worktree is
/// created *at* it. Here it is one of three ordinary states of a directory marion did not make: no
/// repository at all, a repository with no commits yet (`git init` and nothing since), or a normal
/// checkout. Only the third yields a `base_commit`, and the first two are not errors — they are
/// directories a child can perfectly well run in, with §6.7's diff route simply unavailable.
///
/// **Deliberately not under [`repo_write_guard`]**, which [`make_worktree`] does hold across its own
/// `rev-parse`. That guard exists because reading HEAD and *writing* a worktree must be one
/// operation; this reads and writes nothing, so serializing it would add contention to the path that
/// touches the repository least.
pub fn head_commit(cwd: &Path) -> Option<Oid> {
    git(cwd, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(Oid)
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
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE
        .spawn(&mut cmd)?
        .wait_with_output()?;
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
    git_indexed(wt, &index, &plain_patch_args(&[&base.0]))
}

/// The identity marion commits under when the operator has configured none.
pub const MARION_IDENTITY: (&str, &str) = ("marion", "marion@localhost");

/// **Whose name goes on a child's commit**: the operator's configured `user.name` and
/// `user.email` when both are set, else [`MARION_IDENTITY`]. Read, never written — the identity is
/// passed to the one `git commit` through its environment, so no config file changes.
pub fn commit_identity(name: Option<String>, email: Option<String>) -> (String, String) {
    let set = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    match (set(name), set(email)) {
        (Some(name), Some(email)) => (name, email),
        _ => (MARION_IDENTITY.0.into(), MARION_IDENTITY.1.into()),
    }
}

/// The longest first line a commit subject quotes, in characters.
const SUBJECT_QUOTE_CHARS: usize = 96;

/// `marion: <agent_type> <short-id>: <first line of the narrative, else the prompt>`, with the whole
/// task id in the body so the commit leads back to its contract.
pub fn commit_message(
    agent_type: &str,
    task_id: &str,
    narrative: Option<&str>,
    prompt: &str,
) -> String {
    let first_line = |s: &str| {
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
    };
    let mut quote = narrative
        .and_then(first_line)
        .or_else(|| first_line(prompt))
        .unwrap_or_else(|| "no narrative".into());
    if quote.chars().count() > SUBJECT_QUOTE_CHARS {
        quote = quote.chars().take(SUBJECT_QUOTE_CHARS - 1).collect();
        quote.push('…');
    }
    let short: String = task_id.chars().take(8).collect();
    format!("marion: {agent_type} {short}: {quote}\n\nmarion task {task_id}\n")
}

/// **Commit everything the child changed in its worktree `wt` onto the checked-out branch**, and
/// return the tip when the branch now holds work beyond `base`.
///
/// `git add -A` with no pathspec is the same view of the tree `changed_paths` reads — tracked
/// changes plus untracked files `--exclude-standard` does not ignore — so the commit holds exactly
/// the paths the contract names. Passing those paths back as a pathspec would re-parse git's
/// quoted `--name-only` output and could drop a file with an unusual name.
///
/// The commit is evidence, not a submission, so the operator's hooks do not run
/// (`core.hooksPath=/dev/null`, `--no-verify`) and it is never signed (`--no-gpg-sign`): a
/// failing hook or a locked key would otherwise lose the work this exists to keep. Under
/// [`repo_write_guard`], because the objects and the branch ref are the shared repository's.
///
/// A child that committed its own work and left the tree clean returns its tip with no commit
/// added; a tree with no changes and no commits returns `None`.
pub fn commit_child_work(wt: &Path, base: &Oid, message: &str) -> Result<Option<Oid>, SpawnError> {
    let _serialized = repo_write_guard();
    commit_all(wt, base, message)
}

/// The subject of the commit that carries a caller's work in progress to the child it spawns
/// ([`make_worktree`]'s `carry_work`).
pub const CARRIED_WORK_MESSAGE: &str =
    "marion: work in progress, carried to a child spawned from it";

/// [`commit_child_work`]'s body, for a caller already holding [`repo_write_guard`].
fn commit_all(wt: &Path, base: &Oid, message: &str) -> Result<Option<Oid>, SpawnError> {
    let get = |key: &str| {
        git(wt, &["config", "--get", key])
            .ok()
            .map(|v| v.trim().to_string())
    };
    let (name, email) = commit_identity(get("user.name"), get("user.email"));
    git(wt, &["add", "-A"])?;
    if !git(wt, &["diff", "--cached", "--name-only"])?
        .trim()
        .is_empty()
    {
        let (name, email) = (OsStr::new(&name), OsStr::new(&email));
        git_env(
            wt,
            &[
                ("GIT_AUTHOR_NAME", name),
                ("GIT_AUTHOR_EMAIL", email),
                ("GIT_COMMITTER_NAME", name),
                ("GIT_COMMITTER_EMAIL", email),
            ],
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--no-verify",
                "--no-gpg-sign",
                "-q",
                "-m",
                message,
            ],
        )?;
    }
    let tip = git(wt, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok((tip != base.0).then_some(Oid(tip)))
}

/// **What became of a child's changes when its run ended**, decided before the worktree goes.
#[derive(Debug)]
pub enum Landed {
    /// Nothing to keep here: a `shared-cwd` child's work is already in the caller's directory, and
    /// a worktree child that changed nothing leaves its branch at the base commit, as before.
    Nothing,
    /// The branch holds the work, at `commit`. The worktree may go; the branch stays.
    OnBranch { branch: String, commit: Oid },
    /// marion could not commit it. The worktree is kept, because it is now the only copy.
    Kept { path: PathBuf, why: String },
}

impl Landed {
    /// Commit a worktree child's changes onto its `marion/<task_id>` branch, when it has any.
    ///
    /// `changed` is the contract's own `changed_paths`: `Some(empty)` is the measured "nothing
    /// changed" and skips the commit; `None` (the measurement failed) still tries, since a failed
    /// read is no evidence the tree is clean. Out-of-scope paths are committed with the rest —
    /// dropping them would destroy the evidence `scope_violations` points at — and stay flagged
    /// there, so the parent decides whether to merge.
    pub fn land(
        workspace: &Workspace,
        base: Option<&Oid>,
        changed: Option<&[PathBuf]>,
        message: &str,
    ) -> Self {
        let (Workspace::Worktree { path, branch }, Some(base)) = (workspace, base) else {
            return Self::Nothing;
        };
        if changed.is_some_and(<[PathBuf]>::is_empty) {
            return Self::Nothing;
        }
        match commit_child_work(path, base, message) {
            Ok(Some(commit)) => Self::OnBranch {
                branch: branch.clone(),
                commit,
            },
            Ok(None) => Self::Nothing,
            Err(e) => Self::Kept {
                path: path.clone(),
                why: e.to_string(),
            },
        }
    }

    /// Whether the worktree may be removed: not while it holds the only copy of the work.
    pub fn may_reap(&self) -> bool {
        !matches!(self, Self::Kept { .. })
    }

    /// Record the outcome on the contract: the branch and commit, or why the worktree was kept.
    pub fn apply(&self, contract: &mut TaskContract) {
        let Some(completion) = contract.completion.as_mut() else {
            return;
        };
        match self {
            Self::Nothing => {}
            Self::OnBranch { branch, commit } => {
                completion.branch = Some(branch.clone());
                completion.commit = Some(commit.clone());
            }
            Self::Kept { path, why } => {
                completion.exit.description = format!(
                    "{}; marion could not commit the child's changes ({why}), so its worktree \
                     is kept at {}",
                    completion.exit.description,
                    path.display()
                );
            }
        }
    }
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
    /// so the two share one dialect by construction rather than by agreement. See [`PLAIN_PATCH`].
    pub fn diff(&self, repo: &Path, a: &Oid, b: &Oid) -> Result<String, SpawnError> {
        git_env(repo, &self.env(), &plain_patch_args(&[&a.0, &b.0]))
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
    /// **The last turn's final words, when that turn did not report** — the model's last text
    /// in it, read the row's way. `None` when the last turn reported (or wrote no text). The
    /// contract states it as marion's synthesis, never as the child's report
    /// (`narrative_synthesized`, §7.6 step 5).
    pub unreported_tail: Option<String>,
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
            unreported_tail: None,
        }
    }
}

/// How a synthesized narrative introduces a child's final words after its own earlier report.
const LATER_UNREPORTED: &str = "marion: a later turn did not report; its final message was:";

/// Assemble the contract at the child's terminal transition.
#[allow(clippy::too_many_arguments)]
pub fn build_contract(
    task_id: TaskId,
    requester: AgentId,
    // The harness that ran — the adapter's answer, never the agent type's request (§6.7).
    harness: marion_core::Harness,
    repo: RepoIdentity,
    base: Option<Oid>,
    workspace: Workspace,
    instructions: &str,
    criteria: &[String],
    ceiling: &[Glob],
    requested: &[Glob],
    timeout: Duration,
    spawned: SystemTime,
    outcome: &ChildOutcome,
    // **`None` means neither of §6.7's two routes was available**, not that nothing changed.
    //
    // The distinction is the entire reason `scope_enforced` exists, and it could not be made while
    // this was a bare `Vec`: the caller wrote `changed_paths(..).unwrap_or_default()`, so a
    // workspace that afforded no diff at all — no repository, no `base_commit` — produced
    // `changed_paths: []`, `scope_violations: []`, `scope_enforced: true`, which §6.7 names twice
    // as *the* false-confidence shape the two-field split exists to prevent. An `Option` makes the
    // clean bill of health unreachable without a check having run to issue it.
    changed: Option<Vec<PathBuf>>,
    diff: Option<String>,
    // What the parent asked to be run, and what running it produced. `evidence` is empty where
    // nothing ran (a killed child); `verification` still carries the request, so the two cases —
    // never asked and never run — stay distinguishable on the record.
    verification: Vec<Command>,
    evidence: Vec<CommandOutcome>,
) -> TaskContract {
    // **Both conditions, and neither is redundant.** §6.7's flag records whether the check *ran*,
    // and a check needs two things: a workspace that can answer "what changed" (`changed`), and a
    // pair of scope lists to judge the answer against (`scope`). Either missing means no check
    // happened, and `false` is what says so.
    let scope = Scope::new(ceiling, requested).ok();
    let scope_enforced = scope.is_some() && changed.is_some();
    let changed = changed.unwrap_or_default();
    let violations = scope
        .as_ref()
        .filter(|_| scope_enforced)
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
    // **Only a child that did not finish `Ok` shows its stderr here**: this is the first line its
    // parent reads, and a clean run's stderr is the harness's chatter, never the result.
    let stderr = outcome.stderr.trim();
    let description = if stderr.is_empty() || status == ExitStatus::Ok {
        description
    } else {
        let preview: String = stderr.chars().take(512).collect();
        format!("{description}; stderr: {preview}")
    };
    // **An authentication failure is the cause, so it leads.** Found in the whole of stderr rather
    // than the preview above, which is a prefix: an auth line behind a banner was cut off there
    // and the description said only how the child exited.
    let description = match marion_harness::auth_failure_line(&outcome.stderr) {
        Some(line) => format!("{line}; {description}"),
        None => description,
    };
    // §5.4: any non-zero exit fails the contract. **Only `Ok` is demoted** — a timeout or a
    // stream-reported failure is the more specific finding, and the evidence over a killed
    // workspace is empty anyway (`run_spawn` runs no verification there). The count goes into
    // the description because `bridge::failure_line` prints exactly that field.
    let failed = evidence
        .iter()
        .filter(|e| e.timed_out || e.exit_code != Some(0))
        .count();
    let (status, description) = if status == ExitStatus::Ok && failed > 0 {
        (
            ExitStatus::Failed,
            format!(
                "{description}; verification: {failed} of {} commands did not exit 0",
                evidence.len()
            ),
        )
    } else {
        (status, description)
    };
    // **The child's own report, or marion's words from its last turn, and the flag says which**
    // (§7.6 step 5). A last turn that did not report is on the record either way — after an
    // earlier report its final message follows it, and alone it is the narrative — but the
    // status above never read it.
    let (narrative, narrative_synthesized) = match (
        outcome.narrative.as_deref(),
        outcome.unreported_tail.as_deref(),
    ) {
        (Some(n), Some(tail)) => (Some(format!("{n}\n\n{LATER_UNREPORTED} {tail}")), true),
        (None, Some(tail)) => (Some(tail.to_string()), true),
        (n, None) => (n.map(str::to_string), false),
    };
    let completion = Completion {
        status,
        died_before_gate: false,
        reported_early: false,
        held_to_timeout: false,
        live_descendants_at_report: vec![],
        narrative: narrative.as_deref().map(Capped::whole),
        narrative_synthesized,
        // **The child's, not marion's.** §6.7 calls this the one field the child owns outright, and
        // it was hardcoded empty here — so a child that committed its work and reported the oids
        // had them dropped in transit, and the contract then asserted it had committed nothing.
        result_commits: outcome.result_commits.clone(),
        changed_paths: changed,
        acceptance_criteria_omitted: 0,
        changed_paths_omitted: 0,
        result_commits_omitted: 0,
        scope_violations_omitted: 0,
        scope_enforced,
        scope_violations: violations,
        diff: diff.map(Capped::whole),
        evidence,
        evidence_omitted: 0,
        exit: ProcessExit {
            code: outcome.exit_code,
            signal: outcome.signal,
            description,
        },
        // Set by `run_spawn` once the work is committed onto the child's branch, which happens
        // after the contract's measurements and before the worktree is reaped.
        branch: None,
        // Set by `run_spawn` from the whole run's output (`profiles::classify`), which this
        // outcome no longer carries.
        failure_cause: None,
        commit: None,
        // Set by the review gate once a reviewer has judged the committed work.
        review: None,
        // Set only on a reviewer node's own completion, from its parsed reply.
        findings: None,
        // Set by `run_spawn` from the node's metered stream, which this function never sees.
        usage: None,
    };
    TaskContract {
        task_id,
        requester,
        // `version` and `model` are provisional: `run_spawn` overwrites them from the **compiled
        // invocation**, which is the only thing that knows what actually ran.
        child: ChildRef {
            harness,
            version: "unknown".into(),
            model: None,
            provider: None,
            route: None,
            credential: None,
            credential_failover: vec![],
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
        verification,
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

    /// The move's whole claim, stated as a test: routing codex's exec stream through its row
    /// changes nothing about what marion reads out of it. The row is `codex::EXEC` since the
    /// registry moved codex to app-server (S36); its reading of these shapes is unchanged.
    #[test]
    fn the_codex_exec_row_parses_exactly_what_the_pre_move_function_did() {
        for s in codex_corpus() {
            let before = parse_child_stream_before_the_move(&s);
            let after = codex_reads(&s);
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

    /// What marion reads out of a codex exec stream: the exec row's grammar, in codex's spelling.
    fn codex_reads(s: &str) -> marion_harness::StreamOutcome {
        let prefix = adapter_for(Harness::Codex).unwrap().marion_tool_name("");
        marion_harness::grammar::parse_stream(
            marion_harness::codex::EXEC
                .stream
                .expect("the exec row reads its JSONL"),
            s,
            &prefix,
        )
    }

    #[test]
    fn a_report_is_read_from_the_mcp_tool_call_item() {
        // Verbatim shape from tests/fixtures/s6/exec-mcp-report.stream.jsonl.
        let s = r#"{"type":"item.completed","item":{"id":"item_0","type":"mcp_tool_call","server":"marion","tool":"report","arguments":{"narrative":"did the work"},"status":"completed"}}"#;
        assert_eq!(codex_reads(s).narrative.as_deref(), Some("did the work"));
    }

    #[test]
    fn file_changes_are_collected_as_corroboration() {
        let s = r#"{"type":"item.completed","item":{"id":"item_0","type":"file_change","changes":[{"path":"/wt/a.rs","kind":"update"}],"status":"completed"}}"#;
        assert_eq!(
            codex_reads(s).file_change_paths,
            vec![PathBuf::from("/wt/a.rs")]
        );
    }

    /// **A persisted patch is git's plain dialect, whatever the operator configured.** Task
    /// contracts and root-change records are machine-read audit data: colour, a prefix setting, an
    /// external diff program or a textconv driver would each change their bytes, and the last two
    /// run an operator-chosen program over content an agent wrote.
    ///
    /// The positive control proves the textconv driver is live, so a pass means marion bypassed it
    /// rather than that git never consulted it. Mutation: drop any one flag from `PLAIN_PATCH` and
    /// the matching case fails.
    #[test]
    fn persisted_patches_ignore_the_operators_diff_configuration() {
        let root = marion_testsupport::scratch("plain-task-diffs");
        let repo = marion_testsupport::fixture_repo(&root);
        let base = Oid(marion_testsupport::git(&repo, &["rev-parse", "HEAD"])
            .trim()
            .into());
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let snapshot = TreeSnapshot::open(&repo, &agent_dir).expect("the root snapshot opens");
        let pre = snapshot.take(&repo).expect("the pre-tree is measured");
        std::fs::write(repo.join("src/keep.txt"), "changed\n").unwrap();
        let post = snapshot.take(&repo).expect("the post-tree is measured");

        let attributes = root.join("global-attributes");
        std::fs::write(&attributes, "*.txt diff=marionreview\n").unwrap();
        let config: [(&str, &str); 6] = [
            ("color.ui", "always"),
            ("diff.noprefix", "true"),
            ("diff.external", "/usr/bin/false"),
            ("core.attributesFile", attributes.to_str().unwrap()),
            ("diff.marionreview.textconv", "/usr/bin/false"),
            ("diff.marionreview.command", "/usr/bin/false"),
        ];
        for (key, value) in config {
            marion_testsupport::git(&repo, &["config", key, value]);
        }
        assert!(
            git(&repo, &["diff", "--no-ext-diff", &base.0]).is_err(),
            "positive control: the configured textconv driver is consulted by a bare diff"
        );

        for (label, patch) in [
            ("diff_text", diff_text(&repo, &base)),
            ("TreeSnapshot::diff", snapshot.diff(&repo, &pre, &post)),
        ] {
            let patch = patch.unwrap_or_else(|e| panic!("{label} failed: {e}"));
            assert!(
                !patch.as_bytes().contains(&0x1b),
                "{label} carried colour: {patch:?}"
            );
            assert!(
                patch.contains("diff --git a/src/keep.txt b/src/keep.txt\n"),
                "{label} lost git's default prefixes: {patch}"
            );
            assert!(
                patch.lines().any(|l| l == "+changed"),
                "{label} did not carry the file's own content: {patch}"
            );
        }
    }

    #[test]
    fn a_tool_call_from_another_server_is_not_a_report() {
        let s = r#"{"type":"item.completed","item":{"type":"mcp_tool_call","server":"other","tool":"report","arguments":{"narrative":"nope"}}}"#;
        assert!(codex_reads(s).narrative.is_none());
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
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
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
            Some(vec![]),
            None,
            vec![],
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
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
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
            Some(vec![]),
            None,
            vec![],
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
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
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
            Some(vec![]),
            None,
            vec![],
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
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
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
            Some(vec![]),
            None,
            vec![],
            vec![],
        );
        assert_eq!(c.completion.unwrap().status, ExitStatus::TimedOut);
    }

    #[test]
    fn a_silent_child_is_unreported_not_ok() {
        let c = build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
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
            Some(vec![]),
            None,
            vec![],
            vec![],
        );
        assert_eq!(c.completion.unwrap().status, ExitStatus::Unreported);
    }

    #[test]
    fn an_out_of_scope_write_is_recorded_with_scope_enforced_true() {
        let c = build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
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
            Some(vec![
                PathBuf::from("src/a.rs"),
                PathBuf::from("outside/b.txt"),
            ]),
            None,
            vec![],
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

    // --- §6.6's cwd occupancy table -----------------------------------------------------------

    /// **Check and claim are one locked step, and this is the assertion that says so.**
    ///
    /// Split into "is it free?" then "take it", two spawns could both read free and both write —
    /// which is the lost-update this exists to prevent, one level up from the trees it prevents it
    /// in. The interface is what makes that unrepresentable: there is no "is it free" to call, only
    /// a `claim` that answers with the holder, so a caller cannot assemble the racy pair even by
    /// trying.
    #[test]
    fn a_claim_on_an_occupied_cwd_is_refused_naming_the_holder() {
        let root = marion_testsupport::scratch("claim-occ");
        let dir = root.join("cwd");
        std::fs::create_dir_all(&dir).unwrap();
        let first = CwdClaim::claim(&dir, &AgentId("holder-a".into())).expect("an empty cwd");
        match CwdClaim::claim(&dir, &AgentId("holder-b".into())) {
            Err(SpawnError::CwdOccupied { holder, cwd }) => {
                assert_eq!(holder.0, "holder-a", "the *first* holder, not the newcomer");
                assert_eq!(cwd, dir.canonicalize().unwrap());
            }
            other => panic!("expected CwdOccupied, got {other:?}"),
        }
        drop(first);
    }

    /// **Released by dropping, on every exit including a `?`.**
    ///
    /// A leaked claim is worse than no guard at all: it refuses every future spawn into that
    /// directory for the life of the supervisor, naming a node that has long since exited, and an
    /// operator cannot clear it without restarting. `run_spawn` has several `?` returns below the
    /// claim, so a matched release call would have to be right at each of them.
    #[test]
    fn a_claim_is_released_when_it_drops() {
        let root = marion_testsupport::scratch("claim-drop");
        let dir = root.join("cwd");
        std::fs::create_dir_all(&dir).unwrap();
        drop(CwdClaim::claim(&dir, &AgentId("transient".into())).expect("an empty cwd"));
        CwdClaim::claim(&dir, &AgentId("next".into())).expect("the cwd is free again");
    }

    /// **Occupancy is a fact about a directory, not about a spelling of it.**
    ///
    /// `/tmp/p` and `/tmp/p/.` are one tree, and a table keyed on the literal argument would let
    /// two writers in by arriving through two names — a guard that holds only for callers who
    /// happen to type the path the same way is not a guard.
    #[test]
    fn two_spellings_of_one_directory_are_one_entry() {
        let root = marion_testsupport::scratch("claim-spell");
        let dir = root.join("cwd");
        std::fs::create_dir_all(&dir).unwrap();
        let _held = CwdClaim::claim(&dir, &AgentId("holder".into())).expect("an empty cwd");
        assert!(
            matches!(
                CwdClaim::claim(&dir.join("."), &AgentId("other".into())),
                Err(SpawnError::CwdOccupied { .. })
            ),
            "the same tree reached by a second spelling is the same entry"
        );
    }

    /// **`CwdClaim::none()` occupies nothing and releases nothing.**
    ///
    /// The un-claimed case — a `worktree` child, a read-only `shared-cwd` child, or one that passed
    /// `allow_concurrent_writes: true`. Carried unconditionally and left empty so the release path
    /// stays single; the risk of that shape is an empty claim that nonetheless evicts a real one on
    /// drop, which is what this rules out.
    #[test]
    fn an_empty_claim_does_not_evict_a_real_one() {
        let root = marion_testsupport::scratch("claim-none");
        let dir = root.join("cwd");
        std::fs::create_dir_all(&dir).unwrap();
        let _real = CwdClaim::claim(&dir, &AgentId("holder".into())).expect("an empty cwd");
        drop(CwdClaim::none());
        assert!(
            matches!(
                CwdClaim::claim(&dir, &AgentId("other".into())),
                Err(SpawnError::CwdOccupied { .. })
            ),
            "dropping an empty claim must not release someone else's directory"
        );
    }

    // -----------------------------------------------------------------------------------------
    // §5.4's `verification` on the status ladder.
    // -----------------------------------------------------------------------------------------

    /// **A child that could not authenticate says so first.** The stderr preview is the first 512
    /// characters, so an auth line behind a banner or a stack trace used to be cut off entirely and
    /// the description read "child exited with code 55" and nothing an operator could act on.
    #[test]
    fn a_child_that_could_not_authenticate_leads_its_description_with_the_auth_line() {
        let line = "Error authenticating: IneligibleTierError: This client is no longer supported \
                    for Gemini Code Assist for individuals.";
        let banner = "x".repeat(600);
        let c = verified_contract(
            ChildOutcome {
                exit_code: Some(55),
                stderr: format!("{banner}\n{line}\n"),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        let d = c.completion.unwrap().exit.description;
        assert!(d.starts_with(line), "the cause leads: {d}");
        assert!(d.contains("child exited with code 55"), "{d}");
    }

    /// **A child that finished `Ok` carries no stderr in its description**: the line is what its
    /// parent reads first, and a live run's read "child exited with code 0; stderr: Reading
    /// additional input from stdin..." over a clean result. A failed child keeps its stderr there,
    /// since that is where its cause usually is.
    #[test]
    fn only_a_failed_child_carries_its_stderr_in_the_description() {
        let clean = verified_contract(
            ChildOutcome {
                narrative: Some("did it".into()),
                exit_code: Some(0),
                stderr: "some harness chatter\n".into(),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        let d = clean.completion.unwrap().exit.description;
        assert_eq!(d, "child exited with code 0");
        let failed = verified_contract(
            ChildOutcome {
                narrative: Some("did it".into()),
                exit_code: Some(2),
                stderr: "panic: out of cheese\n".into(),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        let d = failed.completion.unwrap().exit.description;
        assert_eq!(d, "child exited with code 2; stderr: panic: out of cheese");
        let unreported = verified_contract(
            ChildOutcome {
                exit_code: Some(0),
                stderr: "why it stopped\n".into(),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        assert!(
            unreported
                .completion
                .unwrap()
                .exit
                .description
                .ends_with("; stderr: why it stopped")
        );
    }

    /// **A narrative marion wrote says so** (`narrative_synthesized`). A child whose last turn did
    /// not report gets its final words on the record, marked as marion's: after an earlier
    /// report they follow it, and the status stays that report's; with no report at all they are
    /// the whole narrative, and the status stays `Unreported` — a final message is never
    /// promoted to an answer.
    #[test]
    fn a_last_turn_that_did_not_report_leaves_a_synthesized_narrative() {
        let reported_then_quiet = verified_contract(
            ChildOutcome {
                narrative: Some("raised on empty input".into()),
                unreported_tail: Some("Now returns 0.0 on empty input.".into()),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        let comp = reported_then_quiet.completion.unwrap();
        assert_eq!(comp.status, ExitStatus::Ok);
        assert!(comp.narrative_synthesized);
        let n = comp.narrative.unwrap().value;
        assert!(
            n.starts_with("raised on empty input")
                && n.ends_with("Now returns 0.0 on empty input."),
            "{n}"
        );
        assert!(n.contains("did not report"), "{n}");

        let never = verified_contract(
            ChildOutcome {
                unreported_tail: Some("I think I am done.".into()),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        let comp = never.completion.unwrap();
        assert_eq!(comp.status, ExitStatus::Unreported, "never promoted");
        assert!(comp.narrative_synthesized);
        assert_eq!(comp.narrative.unwrap().value, "I think I am done.");

        let own = verified_contract(
            ChildOutcome {
                narrative: Some("did it".into()),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        assert!(!own.completion.unwrap().narrative_synthesized);
    }

    /// `git` for a unit test's scratch repository, panicking with git's own words on failure.
    fn tgit(dir: &Path, args: &[&str]) -> String {
        let out = SysCommand::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A one-commit repository with its own identity, and its base commit. The scratch guard is
    /// returned because dropping it removes the directory.
    fn committed_repo(name: &str) -> (marion_testsupport::Scratch, PathBuf, Oid) {
        let dir = marion_testsupport::scratch(name);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        tgit(&repo, &["init", "-q", "-b", "main"]);
        tgit(&repo, &["config", "user.name", "Op Erator"]);
        tgit(&repo, &["config", "user.email", "op@example.invalid"]);
        tgit(&repo, &["config", "maintenance.auto", "false"]);
        std::fs::write(repo.join("keep.txt"), "keep\n").unwrap();
        tgit(&repo, &["add", "-A"]);
        tgit(&repo, &["commit", "-qm", "base"]);
        let base = Oid(tgit(&repo, &["rev-parse", "HEAD"]));
        (dir, repo, base)
    }

    #[test]
    fn the_operators_configured_identity_is_used_and_marion_s_only_in_its_absence() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            commit_identity(some("Op"), some("op@x")),
            ("Op".to_string(), "op@x".to_string())
        );
        let marion = (MARION_IDENTITY.0.to_string(), MARION_IDENTITY.1.to_string());
        assert_eq!(commit_identity(None, some("op@x")), marion);
        assert_eq!(commit_identity(some("Op"), None), marion);
        assert_eq!(
            commit_identity(some("  "), some("op@x")),
            marion,
            "a blank name is not an identity"
        );
    }

    #[test]
    fn the_commit_subject_names_the_type_the_short_task_and_the_first_line() {
        let msg = |narrative: Option<&str>, prompt: &str| {
            commit_message("codex-impl", "01a0ca90ffee", narrative, prompt)
        };
        let subject = |m: String| m.lines().next().unwrap().to_string();
        assert_eq!(
            subject(msg(Some("Added subtract.\nMore detail."), "Edit calc.py")),
            "marion: codex-impl 01a0ca90: Added subtract."
        );
        assert_eq!(
            subject(msg(None, "Edit calc.py\nand test it")),
            "marion: codex-impl 01a0ca90: Edit calc.py",
            "with no narrative, the prompt's first line"
        );
        assert_eq!(
            subject(msg(Some("\n   \n"), "Edit calc.py")),
            "marion: codex-impl 01a0ca90: Edit calc.py",
            "a blank narrative is no narrative"
        );
        let long = subject(msg(Some(&"é".repeat(300)), "p"));
        assert!(long.ends_with('…'), "{long}");
        assert!(long.chars().count() <= 130, "{}", long.chars().count());
        assert!(
            msg(None, "p").contains("marion task 01a0ca90ffee"),
            "the body carries the whole task id"
        );
    }

    /// **Exactly the changed paths, one commit, on the child's branch, hooks and signing off.**
    /// A failing `pre-commit` hook is installed to prove the commit does not run the operator's
    /// hooks: marion is keeping evidence, not submitting work for review.
    #[test]
    fn a_childs_changes_are_committed_as_one_commit_and_the_tip_is_returned() {
        let (_dir, repo, base) = committed_repo("spawn-commit-work");
        let hooks = repo.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::write(hooks.join("post-commit"), "#!/bin/sh\ntouch hooked\n").unwrap();
        for h in ["pre-commit", "post-commit"] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(hooks.join(h), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        tgit(&repo, &["config", "commit.gpgsign", "true"]);
        std::fs::write(repo.join("keep.txt"), "edited\n").unwrap();
        std::fs::write(repo.join("new.txt"), "created\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(repo.join("ignored.txt"), "never\n").unwrap();
        let changed = changed_paths(&repo, &base).unwrap();

        let tip = commit_child_work(&repo, &base, "marion: t: subject\n\nbody\n")
            .expect("the commit succeeds")
            .expect("and names a tip beyond the base");
        assert_eq!(tip.0, tgit(&repo, &["rev-parse", "HEAD"]));
        assert_eq!(
            tgit(&repo, &["rev-parse", "HEAD^"]),
            base.0,
            "exactly one commit"
        );
        let mut committed: Vec<PathBuf> = tgit(
            &repo,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
        )
        .lines()
        .map(PathBuf::from)
        .collect();
        committed.sort();
        let mut changed = changed;
        changed.sort();
        assert_eq!(committed, changed, "exactly the contract's changed paths");
        assert!(!committed.contains(&PathBuf::from("ignored.txt")));
        assert_eq!(
            tgit(&repo, &["log", "-1", "--format=%an <%ae>|%s"]),
            "Op Erator <op@example.invalid>|marion: t: subject"
        );
        assert!(!repo.join("hooked").exists(), "no hook ran");
        assert_eq!(
            tgit(&repo, &["status", "--porcelain"]),
            "",
            "and the tree is clean after it, the ignored file still ignored"
        );
    }

    /// **A child cut from a worktree marion made starts from that tree's current state**, not its
    /// last commit — the s2 grandchildren that tested old code (live smoke, 2026-09-27). The
    /// parent's uncommitted edit and its new file are committed onto the parent's own branch, its
    /// tree is left clean, and the child's worktree holds both. Without `carry_work` — a child of
    /// the operator's own checkout — nothing is committed and the child is cut from HEAD.
    #[test]
    fn a_child_cut_from_a_marion_worktree_starts_from_its_parents_uncommitted_work() {
        let (dir, repo, base) = committed_repo("spawn-carry-work");
        let parent = dir.join("parent-wt");
        make_worktree(&repo, &parent, "marion/parent", false).expect("the parent's worktree");
        std::fs::write(parent.join("keep.txt"), "the parent's edit\n").unwrap();
        std::fs::write(parent.join("new.txt"), "the parent's new file\n").unwrap();

        let child = dir.join("child-wt");
        let child_base = make_worktree(&parent, &child, "marion/child", true)
            .expect("the child's worktree, carrying the parent's work");
        assert_eq!(
            std::fs::read_to_string(child.join("keep.txt")).unwrap(),
            "the parent's edit\n"
        );
        assert!(
            child.join("new.txt").exists(),
            "an untracked file travels too"
        );
        assert_eq!(
            child_base.0,
            tgit(&parent, &["rev-parse", "HEAD"]),
            "the child's base is the parent's new tip"
        );
        assert_eq!(
            tgit(&parent, &["rev-parse", "HEAD^"]),
            base.0,
            "one commit, on the parent's own branch"
        );
        assert_eq!(
            tgit(&parent, &["log", "-1", "--format=%s"]),
            CARRIED_WORK_MESSAGE
        );
        assert_eq!(tgit(&parent, &["status", "--porcelain"]), "");
        assert_eq!(
            changed_paths(&parent, &base).unwrap().len(),
            2,
            "the parent's contract still counts its own work"
        );

        // The operator's checkout is never committed to.
        std::fs::write(repo.join("keep.txt"), "the operator's edit\n").unwrap();
        let plain = dir.join("plain-wt");
        let plain_base = make_worktree(&repo, &plain, "marion/plain", false).unwrap();
        assert_eq!(plain_base, base, "cut from HEAD");
        assert_eq!(tgit(&repo, &["status", "--porcelain"]), "M keep.txt");
    }

    #[test]
    fn a_tree_with_no_changes_gets_no_commit_and_no_tip() {
        let (_dir, repo, base) = committed_repo("spawn-commit-none");
        assert_eq!(commit_child_work(&repo, &base, "m").unwrap(), None);
        assert_eq!(tgit(&repo, &["rev-parse", "HEAD"]), base.0);
    }

    /// A child that committed its own work and left the tree clean still has work on the branch,
    /// and marion adds no empty commit on top of it.
    #[test]
    fn a_childs_own_commit_is_the_tip_and_marion_adds_nothing() {
        let (_dir, repo, base) = committed_repo("spawn-commit-own");
        std::fs::write(repo.join("keep.txt"), "the child's\n").unwrap();
        tgit(&repo, &["commit", "-qam", "child's own"]);
        let own = tgit(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(
            commit_child_work(&repo, &base, "m").unwrap(),
            Some(Oid(own))
        );
        assert_eq!(tgit(&repo, &["rev-list", "--count", "main"]), "2");
    }

    /// **A commit that fails keeps the worktree**: removing it would destroy the only copy of the
    /// work, so the landing says the tree may not be reaped, and the contract says why.
    #[test]
    fn a_commit_that_fails_keeps_the_worktree_and_the_contract_says_why() {
        let (_dir, repo, base) = committed_repo("spawn-commit-fails");
        std::fs::write(repo.join("new.txt"), "created\n").unwrap();
        // A held index lock is what a concurrent git in the same tree looks like.
        std::fs::write(repo.join(".git/index.lock"), "").unwrap();
        let workspace = Workspace::Worktree {
            path: repo.clone(),
            branch: "main".into(),
        };
        let changed = changed_paths(&repo, &base).unwrap();
        let landed = Landed::land(&workspace, Some(&base), Some(&changed), "m");
        assert!(matches!(landed, Landed::Kept { .. }), "{landed:?}");
        assert!(!landed.may_reap());
        let mut contract = verified_contract(
            ChildOutcome {
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        landed.apply(&mut contract);
        let comp = contract.completion.as_ref().unwrap();
        assert_eq!((&comp.branch, &comp.commit), (&None, &None));
        assert!(
            comp.exit.description.contains("could not commit")
                && comp.exit.description.contains(&repo.display().to_string()),
            "{}",
            comp.exit.description
        );
    }

    /// The two cases that must not commit at all: a `shared-cwd` child, whose directory is the
    /// caller's, and a worktree child whose measured changes are empty.
    #[test]
    fn a_shared_cwd_child_and_an_unchanged_worktree_land_nothing() {
        let (_dir, repo, base) = committed_repo("spawn-commit-skip");
        std::fs::write(repo.join("new.txt"), "created\n").unwrap();
        let shared = Workspace::SharedCwd { path: repo.clone() };
        let changed = changed_paths(&repo, &base).unwrap();
        assert!(matches!(
            Landed::land(&shared, Some(&base), Some(&changed), "m"),
            Landed::Nothing
        ));
        let worktree = Workspace::Worktree {
            path: repo.clone(),
            branch: "main".into(),
        };
        assert!(matches!(
            Landed::land(&worktree, Some(&base), Some(&[]), "m"),
            Landed::Nothing
        ));
        assert_eq!(
            tgit(&repo, &["rev-parse", "HEAD"]),
            base.0,
            "neither made a commit in the caller's repository"
        );
    }

    fn sh(line: &str) -> Command {
        Command {
            program: "sh".into(),
            args: vec!["-c".into(), line.into()],
            cwd: "/wt".into(),
            timeout: Duration::from_secs(300),
        }
    }

    fn evidence_for(command: Command, exit_code: Option<i32>, timed_out: bool) -> CommandOutcome {
        CommandOutcome {
            command,
            exit_code,
            stdout: Capped::whole(""),
            stderr: Capped::whole(""),
            duration: marion_core::encoding::Millis(std::time::Duration::from_millis(7)),
            timed_out,
        }
    }

    fn verified_contract(
        outcome: ChildOutcome,
        verification: Vec<Command>,
        evidence: Vec<CommandOutcome>,
    ) -> TaskContract {
        build_contract(
            TaskId("t".into()),
            AgentId("r".into()),
            marion_core::Harness::Codex,
            RepoIdentity {
                git_common_dir: Some("/r/.git".into()),
                head_branch: None,
            },
            Some(Oid("a".repeat(40))),
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
            &outcome,
            Some(vec![]),
            None,
            verification,
            evidence,
        )
    }

    /// A child that reported cleanly and exited 0 is still not `Ok` when a verification command
    /// it was judged by did not exit 0: the evidence is what the parent asked to be judged on.
    #[test]
    fn a_failed_verification_command_demotes_an_ok_child_to_failed() {
        let cmds = vec![sh("cargo build"), sh("cargo test"), sh("sleep 400")];
        let c = verified_contract(
            ChildOutcome {
                narrative: Some("all green".into()),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            cmds.clone(),
            vec![
                evidence_for(cmds[0].clone(), Some(0), false),
                evidence_for(cmds[1].clone(), Some(101), false),
                // A timed-out command is a failed one even where no code was read.
                evidence_for(cmds[2].clone(), None, true),
            ],
        );
        let comp = c.completion.unwrap();
        assert_eq!(comp.status, ExitStatus::Failed);
        assert!(
            comp.exit
                .description
                .contains("verification: 2 of 3 commands did not exit 0"),
            "the description says why, since `failure_line` prints it: {}",
            comp.exit.description
        );
        assert_eq!(
            comp.evidence.len(),
            3,
            "every outcome is kept, passing ones included"
        );
        assert_eq!(c.verification, cmds);
    }

    /// Only `Ok` is demoted. A timeout is the more specific finding and must survive: relabelling
    /// it `Failed` would hide that the child never finished, and the evidence is empty for a
    /// timed-out child anyway (`run_spawn` runs no verification over a killed workspace).
    #[test]
    fn a_timeout_is_never_relabelled_by_verification() {
        let cmds = vec![sh("cargo test")];
        let c = verified_contract(
            ChildOutcome {
                timed_out: true,
                ..ChildOutcome::default()
            },
            cmds.clone(),
            vec![],
        );
        let comp = c.completion.unwrap();
        assert_eq!(comp.status, ExitStatus::TimedOut);
        assert!(
            !comp.exit.description.contains("verification"),
            "nothing ran, so nothing is claimed: {}",
            comp.exit.description
        );
        assert_eq!(
            c.verification, cmds,
            "what was asked for is still on the record"
        );
        assert!(comp.evidence.is_empty());
    }

    /// The request survives even where nothing ran, so a reader can tell "asked for and never
    /// run" from "never asked for" — the distinction the old `verification: vec![]` erased.
    #[test]
    fn verification_commands_are_recorded_even_when_none_ran() {
        let cmds = vec![sh("cargo test")];
        let c = verified_contract(
            ChildOutcome {
                narrative: Some("done".into()),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            cmds.clone(),
            vec![evidence_for(cmds[0].clone(), Some(0), false)],
        );
        let comp = c.completion.as_ref().unwrap();
        assert_eq!(
            comp.status,
            ExitStatus::Ok,
            "a passing verification leaves Ok alone"
        );
        assert!(!comp.exit.description.contains("verification"));
        assert_eq!(comp.evidence_omitted, 0);
        assert_eq!(c.verification, cmds);

        let none = verified_contract(
            ChildOutcome {
                narrative: Some("done".into()),
                exit_code: Some(0),
                ..ChildOutcome::default()
            },
            vec![],
            vec![],
        );
        assert!(none.verification.is_empty());
        assert_eq!(none.completion.unwrap().status, ExitStatus::Ok);
    }
}
