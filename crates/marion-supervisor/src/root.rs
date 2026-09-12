//! Launching the **root node** (design §9).
//!
//! > *"marion launches the root node itself. The `claude` root is not hand-started: `marion run
//! > <agent-type> --prompt <…>` spawns it through the same §6.1 path as any child, which is what
//! > gives it an `AgentId`, an agent-dir, and a capability token — without which its `spawn` call
//! > cannot be stamped and `TaskContract.requester` has no value. **A root has no `TaskContract`**
//! > […]; `requester` for a top-level `spawn` is the root's `AgentId`."*
//!
//! So this module mints the id, writes the `--mcp-config` declaration that carries that id to the
//! bridge, compiles the root argv through `ClaudeCodeAdapter::compile`, and drives the
//! `stream-json` conversation.
//!
//! # Why the prompt is not written the moment the process starts
//!
//! Measured on 2.1.220: MCP servers declared through `--mcp-config` are connected
//! **asynchronously and non-blockingly** — the debug log says so in as many words — and the first
//! turn is *not* held for them. Against a real endpoint that is invisible, because a model reply
//! takes seconds and the ~70 ms connect has long finished. Against the CannedProvider the reply
//! comes back in microseconds, so the root's first request goes out with `tools: []`, marion's
//! `spawn` is not among the tools, and the turn ends with plain text. **Nothing anywhere reports
//! an error** — the same class of silent failure the provider's shape-dispatch exists to prevent.
//!
//! marion owns both ends of that race, so it closes it rather than sleeping through it. The bridge
//! touches [`READY_FILE_ENV`] once it has answered `tools/list`, which is the actual event that
//! matters ("the harness has marion's tool list"), and the root's prompt is written only after
//! that file appears. A `control_request`/`control_response` round trip then follows, so the
//! prompt is written only after the harness's event loop has demonstrably run at least once since
//! the tool list was flushed to it.
//!
//! **None of that lives here.** §6.1 step 8 states the gate normatively, binding on *any* launcher
//! driving Claude Code headlessly — root or child — so the whole protocol sits in [`crate::duplex`]
//! and this module drives it. `run_spawn` drives the same code for a `Typed(_)` child, which is
//! what closed the last red cell of the harness matrix: until then it drove every child as
//! `LaunchOnly` and a `claude` child took turn one with `tools: []`.
//!
//! # Two root paths, chosen by the surface and never by the name
//!
//! Everything above is Claude-Code-specific *scaffolding*, not something a root needs in general.
//! §3.4 is explicit that code branches on `ExecutionSurfaces`, so [`launch`] dispatches on
//! `surfaces().control`:
//!
//! - **`Typed(_)`** — the path described above: pipes, a readiness marker, an `initialize` round
//!   trip, a user frame written after launch, and `can_use_tool` answered over the same channel.
//! - **`LaunchOnly`** — the prompt rides argv, so there is no frame to withhold and nothing to
//!   steer. §6.1 step 8 binds *"only surfaces whose prompt is written after launch"*, and says such
//!   a node's MCP readiness *"is asserted post hoc from the `mcp_tool_call` items in its stream"*.
//!   Permissions on those three harnesses are settled at **config** time — codex's
//!   `default_tools_approval_mode = "approve"`, gemini's `"trust": true`, opencode's allow-by-
//!   default — which is why no runtime permission channel is missing from this path rather than
//!   merely unimplemented.
//!
//! A root on either path has **no `TaskContract` and cannot `report`** (§9). Its result is its
//! stream and its exit.

use std::path::PathBuf;
use std::process::Command as SysCommand;
use std::time::Duration as StdDuration;

use marion_core::contract::{AgentId, Capped, ExitStatus, Oid, ProcessExit};
use marion_core::harness::Harness;
use marion_core::ids::{new_agent_id, uuid_v7};
use marion_core::journal::{Exited, RecordKind, SpawnAborted, SpawnIntent, Spawned};
use marion_core::paths::{AgentDir, ProjectDir};
use marion_core::proto::NativeLaunchContext;
use marion_core::root_change::{
    Reason, RootChange, RootChanged, RootDelta, RootGrant, RootObservation, RootScope,
};
use marion_harness::{
    Auth, CallOutcome, ChildExit, ExecutionSurfaces, Extras, HarnessAdapter, Invocation,
    LaunchSpec, MarionCall, McpDeclaration, SpawnCtx, adapter_for, json_frames,
};
use serde_json::Value;

use crate::duplex::{self, DuplexError, DuplexSpec};
use crate::run::{entropy, run_bounded_watched, unix_millis};
use crate::spawn::SpawnError;

pub use crate::depth::ROOT_DEPTH;

/// The typed-control-plane protocol, which a root shares with a child (`crate::duplex`).
///
/// Re-exported rather than reimplemented: §6.1 step 8's gate is normative for **any** launcher
/// driving Claude Code headlessly, so one implementation serves `marion run` and `run_spawn` both.
/// `permission_round_trip.rs` drives these directly.
pub use crate::duplex::{
    can_use_tool_request, deny_response, initialize_request, is_control_response_to, user_message,
};

// The declaration's env-var names and the document that carries them are the Claude Code adapter's
// business (§3.1: config emission is part of the adapter contract), so they live in
// `marion-harness` and are re-exported here — `marion-supervisor mcp`, the other end of the
// handshake, reads them from this module.
pub use marion_harness::claude_code::{
    AGENT_ID_ENV, AGENT_TYPE_ENV, AUTH_ENV, BASE_URL_ENV, BridgeEnv, DEPTH_ENV, READY_FILE_ENV,
    anthropic_base_url, mcp_config_json,
};

/// The permission axis for an M1 root (§9), in **marion's** vocabulary.
///
/// `spawn` is the load-bearing one; without it the root's single call is denied. The other three
/// are the only descendant verbs an M1 root can reach — `spawn` blocks and backgrounding is M2+,
/// so its child is terminal by the time the root regains control and §5.4 denies `send`/`cancel`
/// against terminal targets. `report` is rejected on a root. Omitting a *reachable* verb would
/// deny calls that then block until the root's bound expires, which is why the list is stated in
/// full rather than trimmed to what the canned script happens to use.
///
/// Verbs, not spellings. This was `ROOT_VERBS`, four `mcp__marion__*` strings — Claude
/// Code's spelling, compiled into every root's `LaunchSpec` whatever its harness. That was
/// invisible while Claude Code was the only adapter reading `allowed_tools`; copilot reads them
/// into `--available-tools`, where `mcp__marion__spawn` names no tool and the root would launch
/// with none of marion's verbs, at exit 0. So the root does what `run_spawn` does for a child:
/// name the verb, and let the adapter spell it (`HarnessAdapter::marion_tool_name`, §3.1).
pub const ROOT_VERBS: [&str; 4] = ["spawn", "status", "wait", "list"];

/// §3.1's **availability** axis for a root — *"the agent type's `tools:` list"*, exactly as
/// `run::run_spawn` gives a child, **and the gate the grant is conditional on.**
///
/// # What replaced the invariant that used to live here
///
/// This was `ROOT_TOOLS: [&str; 0]` — a compiled empty constant, deliberately *not* a read of
/// `AgentType::tools`, on two arguments. Only one of them was ever about the operator's tree, and
/// the other is now answered by a mechanism rather than by an absence. Both are restated here
/// because a deleted invariant that leaves a blank behind is how the next reader re-derives the
/// wrong half of it:
///
/// - **Containment (overruled, by the operator, deliberately).** A root's cwd is `RootSpec::repo`,
///   the operator's own repository, where a child gets a worktree marion made and later removes.
///   The old constant generalised the child's case one step too far: a child is unwatched, and a
///   root is the operator's own node, launched by their own hand and watched live as it runs
///   (`marion run`'s frame view). Nothing here *contains* a root, and nothing claims to — see the
///   "what this does not solve" list on `marion_core::root_change`.
/// - **Audit (survives, and is what this function now enforces).** A root that writes with no diff
///   behind it produces `changed_paths: []` and no record — byte-identical to a child whose write
///   escaped its worktree (§11 item 24), which is the ambiguity `8a69f22` exists to destroy.
///   **Rule: no audit, no grant.** Not "grant it anyway and note the absence".
///
/// # What the audit is, exactly — because that bullet used to claim more than it delivers
///
/// The record behind the grant is a **git-visible** working-tree delta ([`crate::spawn::TreeSnapshot`]),
/// and `git add -A` respects `.gitignore`. A root that writes only `.env`, in a repository that
/// ignores `.env`, therefore produces the very reading the bullet above calls indistinguishable.
/// So what this gate buys is the difference between *a measurement* and *no measurement*, which is
/// the rule it enforces; it is **not** the difference between a complete account of the root's
/// writes and a partial one, and nothing here can be. §11 item 26 has always said so, and
/// `marion_core::root_change::RootDelta::Observed::ignored_not_measured` is what lets a reader see
/// the size of the gap instead of taking this comment's word for it.
///
/// # Why this is a gate and not a refusal to run
///
/// The rule is *no audit, no grant* — not *no audit, no run*. So the refusal is **co-extensive with
/// the grant**: a root whose type declares nothing meets no gate at all and runs in an unversioned
/// directory exactly as it always has, recording `RootObservation::Failed` with git's own words. A
/// gate that fired where nothing was at stake would teach an operator to pass
/// `--no-change-record` by reflex, and then it is off for the one run that matters. That is the
/// judgement call in this change, and it is stated rather than hidden: the design document's own
/// gate paragraph refuses *any* unsnapshotable root, which is stronger than the rule it is derived
/// from and costs a capability — `marion run claude` in a directory nobody `git init`ed — for a
/// safety gain of zero, since a `tools: []` root has no built-in tool to write with.
///
/// # The shape, which is the part that must not be undone
///
/// A non-empty axis is reachable through **one** arm, the one holding a `pre_tree`. Every other
/// path either returns an empty list or returns [`RootError::NoChangeRecord`]. So a later edit
/// cannot hand a root a tool without a change record behind it by forgetting to check something —
/// it would have to delete an arm of this match. A `debug_assert` would be a reminder; this is a
/// compile-time one, and `a_non_empty_availability_axis_is_reachable_only_from_a_recorded_base`
/// asserts the implication over roots `prepare` really built.
///
/// **The permission axis follows from the same list and is not this function's to add.** §11 item
/// 24 measured what happens when only availability opens: the call goes to
/// `--permission-prompt-tool stdio`, marion has no answerer, and the node gets item 22's dead-end
/// string instead of the file. `ClaudeCodeAdapter::permission_axis` unions this list into
/// `--allowedTools` from the same declaration, so the two cannot be declared apart — which is why
/// [`ROOT_VERBS`] carries marion's own verbs *only*, and nothing here appends to it.
/// marion never compiles `--disallowedTools`, on this path or any other (§3.1).
fn availability_axis(
    declared: &[String],
    base: &RootChangeBase,
    repo: &std::path::Path,
) -> Result<Vec<String>, RootError> {
    match base {
        // Nothing was declared, so there is nothing to gate and nothing to refuse. Checked first so
        // the two arms below are only ever reached by a root that actually asked for a tool.
        _ if declared.is_empty() => Ok(Vec::new()),
        RootChangeBase::Taken { .. } => Ok(declared.to_vec()),
        // The operator said so, in as many words, on the command line. A grant-free run is what
        // they asked for, so this is not a refusal — it is the flag doing what it says.
        RootChangeBase::NotAttempted { .. } => Ok(Vec::new()),
        // marion looked and could not see. **No audit, no grant** — and loudly, because the
        // alternative is a root that quietly gets no tools, does no work, and exits 0.
        RootChangeBase::Unavailable { reason } => Err(RootError::NoChangeRecord {
            repo: repo.to_path_buf(),
            declared: declared.join(", "),
            reason: reason.clone(),
        }),
    }
}

/// Which of the two launch paths a root takes, derived from its adapter's `ExecutionSurfaces`.
///
/// **One derivation, two names.** A root and a child face the same question — is this node's prompt
/// a frame written after launch, or an argv element? — and it has the same answer, so the enum and
/// the function both live in [`crate::duplex`] and are re-exported here under the names this
/// module has always used. Two copies of §3.4's dispatch would be exactly the drift §9 warns about.
pub use crate::duplex::{LaunchPath as RootPath, launch_path as root_path};

/// What `marion run` was asked for.
#[derive(Debug, Clone)]
pub struct RootSpec {
    /// Agent type of the root itself, e.g. `claude`.
    pub agent_type: String,
    /// The root's single turn.
    ///
    /// Held here rather than passed to [`launch`] because on a `LaunchOnly` surface it is compiled
    /// **into argv** at `prepare` time. One field, so the prompt that was compiled and the prompt
    /// that is written can never be two different strings.
    pub prompt: String,
    /// Byte-exact native process state captured at the facade boundary.
    ///
    /// This slice carries the context into the root domain unchanged but never compiles or launches
    /// it: the handler's readiness gate refuses production native requests before construction.
    /// Keeping it here is the preparatory threading proof and prevents a later transport from
    /// recovering these values from lossy generic fields.
    pub native_launch: Option<NativeLaunchContext>,
    /// Canonical repository root. The root's cwd, and the repo its children are worktrees of.
    pub repo: PathBuf,
    /// Resolved state directory (`<state>` of §4.3), *not* the per-project subdirectory.
    pub state: PathBuf,
    /// The CannedProvider's base URL, in the `…/v1` form a Codex `model_providers` entry takes.
    ///
    /// `None` under [`Auth::Inherited`], where marion overrides no endpoint and the harness resolves
    /// its own — which is the point: a live node talks to the vendor it is already logged in to.
    /// Adapters that *require* one (codex's `model_providers`, opencode's provider block) refuse by
    /// name rather than compiling a config that points nowhere.
    pub base_url: Option<String>,
    /// Path to the `marion-supervisor` binary the harness will start as the MCP server.
    pub bridge: PathBuf,
    pub model: Option<String>,
    /// Whether the root presents a credential marion minted or the operator's own login (`--live`).
    pub auth: Auth,
    /// `marion run --no-change-record`: do not snapshot the operator's working tree.
    ///
    /// **Named for the flag, so there is no translation layer between what an operator typed and
    /// what marion branched on.** What it buys is a run in a directory marion cannot snapshot —
    /// or one the operator would rather marion did not walk — and what it costs is the grant: a
    /// root launched this way gets `tools: []` whatever its agent type declares, and journals
    /// `RootObservation::NotAttempted` naming the flag. It is the escape hatch on
    /// [`availability_axis`]'s gate and never a way past it.
    pub no_change_record: bool,
    /// `marion run --pane`: run this root in a terminal marion owns, so a client can `marion
    /// attach` to it and drive it by keystrokes (§9's M3).
    ///
    /// **Per run, and that is the whole design of the pane.** It selects
    /// [`marion_harness::HarnessAdapter::pane_surfaces`] and
    /// [`marion_harness::HarnessAdapter::compile_pane`] in place of `surfaces`/`compile`, which is
    /// why a run that does not ask is compiled from bytes this field cannot reach: M1's measured
    /// `stream-json` path stays exactly the path it was measured on. Threading the request into
    /// `surfaces()` instead — a fact about the harness, not about a run — would have moved *every*
    /// `claude` node onto a pty to serve the runs that attach.
    ///
    /// A harness with no interactive shape refuses by name
    /// ([`marion_harness::HarnessError::NoPaneSurface`]) rather than quietly launching headless: a
    /// caller that asked for a pane is a caller that is about to attach, and a headless node would
    /// answer that attach with *"no display plane"* — a true sentence about a node marion made
    /// headless after being told not to.
    pub pane: bool,
    /// **A resume, or a fresh run** (`plan-restart-resume.md` step 6). `Some((agent_id, session))`
    /// relaunches a lost node **under its own id** and hands the harness back the session its
    /// stream named, through the row's measured resume flag — a launch a `node/resume` reconstructs
    /// from the node's own journal. `None` is a fresh run, where `prepare` mints a new id. The id
    /// rides here rather than being minted so a resumed node keeps the identity every other record
    /// about it already names.
    pub resume: Option<(AgentId, String)>,
    /// **§9's node-level bound for this run, already resolved** by [`blocked_bound_secs`] from
    /// `marion run --timeout` and the agent type — not the operator's `Option<u64>`.
    ///
    /// It rides on the spec rather than being passed to [`launch`] alone because `prepare` is what
    /// writes the `SpawnIntent`, and the intent is where the bound has to be recorded for anything
    /// downstream to be able to say what clock the node is under. One resolution, one field: the
    /// number the launch enforces and the number the journal reports are the same value, so
    /// `marion tree` cannot show a bound the run is not actually keeping to.
    pub bound_secs: u64,
}

/// The base point of the root's change record, or why there is none (§9).
///
/// Taken at `prepare`, **before** the launch spec is compiled, because [`availability_axis`] reads
/// it: what a root may do has to be decided after marion knows whether it can record what was done.
///
/// Two arms and not an `Option<Oid>`: an absence has to say *why*, or the record it produces is
/// only a shorter silence — the same reason `marion_core::root_change::RootObservation` has a
/// `reason` on both of its non-measuring variants.
#[derive(Debug, Clone)]
pub enum RootChangeBase {
    Taken {
        /// The isolated git environment, kept so the **post**-snapshot is taken with the same
        /// index copy and the same object store. Re-deriving it at exit would mean a second
        /// resolution of the same three paths, and a warm stat cache thrown away.
        snapshot: std::sync::Arc<crate::spawn::TreeSnapshot>,
        /// `HEAD` at prepare — context only. See `RootChanged::base_commit`.
        base_commit: Option<Oid>,
        pre_tree: Oid,
    },
    /// marion looked and could not see: the directory is not a git worktree, `git` is not on
    /// `PATH`, the object directory could not be created. Journalled as
    /// `RootObservation::Failed` with this sentence, never as a clean reading.
    Unavailable { reason: String },
    /// marion did not look, because the operator said not to (`marion run --no-change-record`).
    ///
    /// **A third arm and not an `Unavailable` with a different sentence**, because the two are
    /// different journal readings and the whole deliverable is that a reader can tell them apart:
    /// `RootObservation::NotAttempted` is *marion did not look, here is why*, and `Failed` is
    /// *marion looked and could not see*. Collapsing them would put a false event in the audit
    /// record — a git failure that never happened — for the one run where the absence was a
    /// decision. It also decides the gate differently: [`availability_axis`] refuses a declared
    /// grant on `Unavailable` and simply withholds it here, since a run with no tools is precisely
    /// what the operator asked for.
    NotAttempted { reason: String },
}

/// A prepared, not-yet-started root node.
#[derive(Debug, Clone)]
pub struct RootNode {
    pub agent_id: AgentId,
    pub agent_dir: AgentDir,
    /// `<state>/<project-hash>` — §4.3's per-project directory, and therefore the journal this
    /// node's records go to. Carried on the node rather than re-derived in [`launch`] because
    /// re-deriving it would need `spec.state` and `spec.repo` again, and two resolutions of the
    /// same path are two chances to write a run's records into two different files.
    pub project: ProjectDir,
    /// The first configuration document the adapter emitted — the one carrying marion's MCP server
    /// declaration.
    ///
    /// `None` when this node's declaration travels by another route: a live opencode node carries
    /// it in `OPENCODE_CONFIG_CONTENT` ([`marion_harness::McpRoute::Environment`]) and a live codex
    /// node on `-c` flags ([`marion_harness::McpRoute::Argv`]), so neither writes a file — and that
    /// absence has already been checked against the route the adapter stated, not passed over.
    pub mcp_config: Option<PathBuf>,
    /// The bridge's readiness marker. `None` on a `LaunchOnly` root: its prompt is already in argv,
    /// so there is no frame to withhold and nothing to gate (§6.1 step 8).
    pub ready_file: Option<PathBuf>,
    /// The per-run bearer token (§9). Never a real credential: the endpoint is the canned server.
    pub token: String,
    pub invocation: Invocation,
    /// Which harness this root is, for the error messages that must name a cause.
    pub harness: Harness,
    /// The shapes this run selected (§3.4) — `HarnessAdapter::surfaces` for an ordinary run,
    /// `pane_surfaces` for one that asked for a pane.
    ///
    /// **Carried rather than re-derived, because `launch_terminal` needs the `PtyWitness` and the
    /// witness may only come from the surfaces the argv was compiled for.** Asking the adapter
    /// again at launch would be a second answer to a question already answered — and the two could
    /// differ for exactly the node where it matters, since which method is asked depends on
    /// `RootSpec::pane`, which `RootNode` does not carry.
    pub surfaces: marion_harness::ExecutionSurfaces,
    pub path: RootPath,
    pub prompt: String,
    /// Carried onto the node because the credential decision is not finished at `compile`: the
    /// `LaunchOnly` run below pushes `MARION_DUMMY_KEY`, and under [`Auth::Inherited`] it must not.
    pub auth: Auth,
    /// §9's change record, half-built: what the root's directory looked like when it started.
    pub change_base: RootChangeBase,
    /// The scope the root's writes are judged against (§5.4).
    ///
    /// Resolved here rather than at exit because the agent type is resolved here, and re-resolving
    /// `spec.agent_type` at the terminal transition would be a second lookup that could answer
    /// differently. `CeilingOnly` and not a ceiling-plus-request pair: **no parent authored a
    /// request** — see `marion_core::root_change::RootScope`.
    pub scope: RootScope,
}

/// What the root's run produced.
#[derive(Debug, Default)]
pub struct RootOutcome {
    pub exit_code: Option<i32>,
    /// Every frame the root emitted, parsed. `stream-json` on the duplex path, the harness's own
    /// JSONL on the `LaunchOnly` one.
    pub transcript: Vec<Value>,
    pub stderr: String,
    /// `tool_name` of each permission request marion denied on expiry of the root's bound.
    pub denied_permissions: Vec<String>,
    /// marion's verbs the root's stream shows it calling **and what came of each** — the post-hoc
    /// readiness evidence on a `LaunchOnly` root (§6.1 step 8). Empty on the duplex path, where
    /// readiness was gated *before* the turn instead and this would only restate it.
    ///
    /// The outcome is carried, not just the verb, because a verb that was *called* is not evidence
    /// the root could delegate — see [`assert_a_verb_was_answered`].
    pub marion_calls: Vec<MarionCall>,
    /// marion's own wall-clock bound expired and the root's process group was killed.
    pub timed_out: bool,
    /// The root's stream made a failure claim of its own, in the harness's words. Populated on the
    /// `LaunchOnly` path, where it is often the *only* diagnosis there is: S13 measured opencode
    /// failing with exit 1 and an empty stderr. `None` never means "it succeeded" — only that the
    /// stream itself claimed nothing (see [`marion_harness::StreamOutcome::failure`]).
    pub failure: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "the harness never fetched marion's tool list within {0:?}: no {1} appeared. \
         The root would have taken its first turn without mcp__marion__spawn, so the run is \
         refused rather than allowed to end in plain text with no error anywhere."
    )]
    McpNeverReady(StdDuration, PathBuf),
    #[error("the root exited before answering marion's initialize control request")]
    DiedBeforeInitialize,
    #[error("compiling the root's launch: {0}")]
    Harness(#[from] marion_harness::HarnessError),
    #[error("unknown agent type {0}")]
    UnknownAgentType(String),
    /// §9's grant gate: the type declared a built-in tool and marion cannot record what the root
    /// does with it.
    ///
    /// **The refusal is of the *grant*, never of a write** — nothing here contains a root. What it
    /// refuses is issuing a tool whose use would leave `changed_paths: []` and no record, which
    /// §11 item 24 measured as byte-identical to a child whose write escaped its worktree. The
    /// error names the directory, the declaration and git's own words, because all three are
    /// separately actionable: the wrong directory, the wrong agent type, or a git that is not
    /// installed are three different fixes.
    ///
    /// **The sentence says "git-visible", and that word is load-bearing rather than hedging.** The
    /// record this refusal is protecting is a diff of two git tree objects, and `git add -A`
    /// respects `.gitignore` (§11 item 26) — so an operator told they would get "a record of what
    /// the root did" and then handed one that cannot see `.env` was promised something marion never
    /// builds. An error message is the one place that promise is made to a person, so it is the
    /// first place the narrowing has to appear.
    ///
    /// It fires **only** when a grant would otherwise be issued. A root whose type declares no tool
    /// never reaches it — see [`availability_axis`] for why the gate is co-extensive with the grant
    /// rather than with the snapshot.
    #[error(
        "the agent type declares built-in tool(s) [{declared}] and marion cannot record what a \
         root does with them in {repo}: {reason}. A grant with no diff behind it produces \
         `changed_paths: []` and no record — indistinguishable from a write that escaped (§11 \
         item 24) — so the grant is refused rather than issued blind. What the grant would have \
         bought is a git-visible working-tree delta: paths git ignores are outside it either way \
         (§11 item 26). Either `git init` that directory, or run an agent type that declares no \
         tools, or pass `--no-change-record` to launch with no built-in tool at all."
    )]
    NoChangeRecord {
        repo: PathBuf,
        /// What the type asked for, in marion's vocabulary. Quoted rather than counted: "read,
        /// write" tells an operator which agent type they picked, and "2 tools" does not.
        declared: String,
        /// Why the snapshot could not be taken, in git's words or marion's — never marion's guess.
        reason: String,
    },
    #[error(
        "{0} drives its node through a terminal, and marion MUST NOT give a headless node a pty \
         on stdin (§5.2). There is no root launch path for that surface, so the run is refused \
         rather than pushed down one that does not fit it."
    )]
    UnsupportedRootSurface(Harness),
    /// **`acp` runs as a child and not yet as a root, and the difference is a watcher.**
    ///
    /// Not a gap in the adapter: `crate::acp_child` drives an ACP turn end to end and `run_spawn`
    /// spawns one. What a root has that a child does not is a person looking at it — the frames are
    /// teed to a `watcher` as they arrive, `attach` re-subscribes to them, and `launch_terminal`
    /// renders them. `run_acp_child` owns its frame loop for the whole turn and hands the transcript
    /// back at the end, so a root on this path would sit silent until it finished and then print
    /// everything at once, and `marion attach` would find nothing to attach to.
    ///
    /// Refused by name rather than run that way, per §6.4's rule about marion choosing for the
    /// operator: "your root produced no output for four minutes" is not a thing an operator can
    /// diagnose, and it is what accept-and-degrade would deliver here.
    #[error(
        "`{0}` is an ACP agent, and marion runs ACP nodes as **children** — `marion spawn` with an \
         `acp` agent type — not as roots. A root's frames are teed to a watcher as they arrive and \
         re-subscribed to by `marion attach`; the ACP driver owns its frame loop for the whole turn \
         and yields the transcript at the end, so a root here would be silent until it finished. \
         Spawn it from a root on another harness, or use `marion doctor --harness acp` to exercise \
         the agent directly."
    )]
    AcpIsNotARootHarness(String),
    #[error("running the root: {0}")]
    Run(#[from] SpawnError),
    /// **Relaxed in shape, not in strength.** It used to read an empty `config_files` as the
    /// failure, which was wrong for an adapter whose declaration is legitimately fileless — a live
    /// opencode node carries it in `OPENCODE_CONFIG_CONTENT`, because a file under an isolated
    /// `$XDG_CONFIG_HOME` would isolate away the login the node exists to use. The fix is *not* to
    /// accept an empty vec: that would let any adapter that simply forgot its declaration launch a
    /// node with no bridge at all, which is §6.1 step 8's failure class and §12's silent one — a
    /// turn taken without marion's tools, ending in plain text, exit 0. So the adapter states its
    /// route ([`marion_harness::McpRoute`]) and marion checks *that* route was taken.
    #[error(
        "the adapter for {harness} declares marion's bridge by {route}, and none arrived — the \
         node would take its turn with no bridge at all, so the run is refused rather than allowed \
         to end in plain text with no error anywhere (§6.1 step 8)"
    )]
    NoMcpDeclaration { harness: Harness, route: String },
    /// §6.1 step 8's post-hoc assertion, failed. **The loud error that must exist**: the alternative
    /// is a run whose turn went out without marion's tools, ended as plain text, and exited 0 with
    /// nothing anywhere reporting it (§12).
    #[error(
        "{harness}: the root never reached marion's bridge — not one marion tool call appears in \
         the {frames} frame(s) it emitted, so its turn went out without marion's tools and it \
         cannot have delegated anything. Refused rather than reported as a success, which is what \
         a plain-text run that exits 0 is indistinguishable from. child exit \
         {exit:?}{failure}{stderr}"
    )]
    BridgeNeverReached {
        harness: Harness,
        frames: usize,
        exit: Option<i32>,
        /// The harness's own words about the failure, taken from its stream. Pre-formatted, like
        /// `stderr`. S13 measured opencode failing with **exit 1 and an empty stderr**, its whole
        /// description in-stream; `spawn::build_contract` already keeps that beside the exit
        /// numbers, and without it here a live root's refusal read "child exit Some(1)" and nothing
        /// else — true, and useless for telling a retired model from a dead credential.
        failure: String,
        /// Pre-formatted, so the empty case adds no dangling label.
        stderr: String,
    },
    /// The **other half** of §6.1 step 8, and the one that was missing. The root did reach marion's
    /// bridge — so [`Self::BridgeNeverReached`] is the wrong diagnosis and would send an operator to
    /// the wrong fix — and every call it made came back refused, or came back not at all.
    ///
    /// Measured: a gemini root whose `spawn` was refused by gemini's own schema validator finished
    /// its turn, exited 0, and was journalled `ExitStatus::Ok` for a run that delegated nothing
    /// (`tasks/todo.md`, owed item 0). The `exit:?` is in the sentence for the same reason it is in
    /// the variant above — a clean exit code beside a run that did nothing is the whole trap.
    #[error(
        "{harness}: the root reached marion's bridge and not one of the {calls} verb(s) it called \
         was answered — {detail}. It cannot have delegated anything, so the run is refused rather \
         than reported as the success its exit code claims (§6.1 step 8). child exit \
         {exit:?}{failure}{stderr}"
    )]
    NoVerbAnswered {
        harness: Harness,
        /// How many marion calls the stream showed. Non-zero by construction — zero is the variant
        /// above — so the two refusals can never be confused by a reader counting.
        calls: usize,
        /// Each call and what became of it, pre-formatted. The verb matters as much as the verdict:
        /// "spawn was refused" and "status was refused" are different runs.
        detail: String,
        exit: Option<i32>,
        /// The harness's own words about the failure, taken from its stream. Pre-formatted, like
        /// `stderr`. S13 measured opencode failing with **exit 1 and an empty stderr**, its whole
        /// description in-stream; `spawn::build_contract` already keeps that beside the exit
        /// numbers, and without it here a live root's refusal read "child exit Some(1)" and nothing
        /// else — true, and useless for telling a retired model from a dead credential.
        failure: String,
        /// Pre-formatted, so the empty case adds no dangling label.
        stderr: String,
    },
    /// **The root started and the journal would not take it, so the root was unwound.**
    ///
    /// The mirror of [`crate::spawn::SpawnError::UnaccountableNode`], and deliberately the same
    /// decision spelled the same way: a policy that differed between a root and a child would make
    /// the journal's meaning depend on which node it is about, which is the argument
    /// `journal::record` already makes about itself.
    ///
    /// `Spawned` is the only record that carries a pid, so losing it does not leave a stale reading
    /// of the root — it leaves a live process `procid::audit`, scoped to `node.pid.is_some()`,
    /// cannot see. By the time a caller sees this variant there is no such process: the tree was
    /// killed at the failed barrier and reaped by the driver, and the node replays as a bare
    /// `SpawnIntent`, which means no process exists.
    #[error(
        "the root started, but marion could not record it and so did not keep it: writing the \
         `Spawned` barrier failed ({why}). The process and its descendants were killed and reaped \
         rather than left running with nothing on the record able to name them."
    )]
    UnaccountableNode { why: String },
}

/// How long the root may take to have marion's tool list before the run is refused (§6.1 step 8).
///
/// Generous — the measured connect is ~70 ms — because the alternative to waiting is a run that
/// ends in plain text with no error anywhere. It lives here rather than in `bin/marion.rs` for the
/// same reason [`blocked_bound_secs`] does: since §11 item 28 step 6 the supervisor is what launches
/// a root, so this is the bound the launch is actually made under.
pub const MCP_READY_TIMEOUT: StdDuration = StdDuration::from_secs(30);

/// **The root's node-level bound (§9), resolved from the flag and the agent type.**
///
/// Lives here rather than in `bin/marion.rs` because since §11 item 28 step 6 the value is decided
/// by the **supervisor** — `marion run` states `--timeout` on the wire as an `Option<u64>` and the
/// bound is resolved where the root is built. One function, so the number an operator typed and the
/// number the node runs under cannot be two resolutions of the same rule.
///
/// Zero from either source means *unset*, not *instant*: an agent type with `timeout: 0` and a
/// `--timeout 0` are both a bound nobody chose, and §3.1's default is what a node gets then.
pub fn blocked_bound_secs(explicit: Option<u64>, agent_type_secs: u64) -> u64 {
    match explicit.filter(|s| *s > 0) {
        Some(s) => s,
        None if agent_type_secs > 0 => agent_type_secs,
        None => marion_core::agent_type::DEFAULT_TIMEOUT_SECS,
    }
}

/// Mint the root's identity, write its configuration, and compile its argv.
///
/// For a caller that owns the node's lifecycle, see [`prepare_watched`].
pub fn prepare(spec: &RootSpec) -> Result<RootNode, RootError> {
    prepare_watched(spec, &crate::run::Unwatched)
}

/// [`prepare`], with the node's **owner** told the moment the root has an identity.
///
/// The hook is [`crate::run::SpawnObserver::identified`], the same one `run_spawn` calls for a
/// child, and it is called at the same place in the same sense: the instant the node has an
/// `AgentId` and **before any side effect** — before the agent directory exists, before the working
/// tree is snapshotted, before a configuration document is written. That position is what makes the
/// token it returns reach the one file that carries it: the MCP declaration compiled below.
///
/// **It differs from a child's in one respect, and the difference is stated rather than smoothed
/// over.** For a child the `SpawnIntent` is already durable when `identified` fires; a root's is
/// journaled at the *end* of this function, where it has always been, because that is the last
/// instant at which everything immutable about the node is known and no process exists yet. So a
/// `prepare` that fails between the two leaves the supervisor holding a claimed node with no record
/// in the journal — which is why the supervisor's root thread files an outcome on **every** exit
/// path (`handler::spawn_root`), rather than only on the ones that got as far as a process. Moving
/// the intent earlier would trade that for an *unresolved* intent on the same failure, which is
/// strictly worse: §7.2 reads one of those as a node marion may have lost.
pub fn prepare_watched(
    spec: &RootSpec,
    observer: &dyn crate::run::SpawnObserver,
) -> Result<RootNode, RootError> {
    // **A resume reuses the node's own id; a fresh run mints one.** The id is minted here for a
    // fresh run so it is marion's own entropy and never a caller's, and carried in on a resume so
    // the second `Spawned` lands on the same node every earlier record names — which is the whole
    // of what a resume is (`registry.rs` folds a second `Spawned` on a known id as generation two).
    let agent_id = match &spec.resume {
        Some((id, _)) => id.clone(),
        None => new_agent_id(unix_millis(), entropy()?),
    };
    // **Before `create_dir_all`, which is this function's first side effect.** See the doc above.
    let node_token = observer.identified(&agent_id);
    // §2: *"both the supervisor and its state are keyed on the project root (git common-dir,
    // falling back to cwd)"*. `spec.repo` is the root's cwd and the base of §6.6's worktrees, which
    // is a different question — keying the journal on it gave a linked worktree its own tree while
    // `socket::resolve` pointed every bridge and TUI at the main repository's supervisor. The same
    // call is made in `marion run` and in the bridge's `spawn_env`, so the three cannot drift.
    let project = ProjectDir::new(&spec.state, &crate::socket::project_root(&spec.repo));
    let agent_dir = project.agent(&agent_id);
    std::fs::create_dir_all(agent_dir.config_dir())?;

    // §6.1 step 5, through the seam, **dispatched on the root's own agent type** — the root used to
    // be `Harness::ClaudeCode` by constant, which is why `marion run codex` compiled a Claude Code
    // launch and then failed somewhere else entirely.
    //
    // Through the tree's own table (`run::agent_types`), so a `.marion/agents.toml` row runs as a
    // root exactly as a built-in does; the file's own refusal arrives as `RootError::Run`.
    let agent_type = crate::run::agent_types(&spec.repo)?
        .resolve(&spec.agent_type)
        .ok_or_else(|| RootError::UnknownAgentType(spec.agent_type.clone()))?;
    // The type's standing instruction, once, exactly where `run_spawn_watched` applies it; `spec`
    // is the prefixed spec from this line on, so the node and the record read one prompt.
    let spec = &RootSpec {
        prompt: crate::run::prefixed_prompt(&agent_type, &spec.prompt),
        ..spec.clone()
    };
    let harness = agent_type.harness;
    let adapter = adapter_for(harness)?;
    // **§3.4's two shapes, and which one this *run* asked for.** `surfaces()` is a fact about the
    // harness; the pane is a fact about the run. Selecting here — once, before anything is
    // compiled — is what keeps the two from being decided in two places and disagreeing: the
    // `Invocation` below and the launch path are both derived from this one value, and it is
    // carried onto the node so `launch_terminal` reads the same surfaces that chose it rather than
    // asking the adapter a second question.
    let surfaces = root_surfaces(adapter.as_ref(), spec.pane, harness)?;
    let path = root_path(&surfaces).ok_or(RootError::UnsupportedRootSurface(harness))?;
    // **Here, and not further down.** Nothing has been created yet — no worktree, no config
    // document, no process — so the refusal leaves the filesystem as it found it. The same
    // reasoning as the pane refusal above, and see the variant for why it is a refusal at all.
    if path == RootPath::Acp {
        return Err(RootError::AcpIsNotARootHarness(agent_type.name.clone()));
    }

    let ready_file = root_ready_file(path, &agent_dir);

    // **§9's change record, first half — taken before anything decides what the root may do.**
    //
    // The order is the point. `availability_axis` below reads this value, so marion knows whether
    // it can record what the root did *before* it decides what the root is allowed to do. Taking
    // the snapshot after compiling the launch would put the two in the other order and make the
    // gate a comment rather than a control flow.
    let change_base = root_change_base(spec, &agent_dir);

    // §9's grant gate, evaluated **here** rather than inside the `LaunchSpec` literal, so the
    // refusal precedes every side effect the launch has — no configuration written, no journal
    // record, no process. A gate whose failure left files behind would be a gate that ran too late.
    let tools = availability_axis(&agent_type.tools, &change_base, &spec.repo)?;

    let token = per_run_token()?;
    // The adapter decides argv, env, and which configuration files exist. marion writes what it is
    // handed and derives none of those paths itself — one derivation, so `--mcp-config` can never
    // name a document nobody wrote.
    let launch = root_launch_spec(spec, path, tools, &token, adapter.as_ref(), &agent_dir);
    let ctx = SpawnCtx {
        agent_id: agent_id.clone(),
        // The canonical name, not `spec.agent_type`: `marion run codex` and `marion run codex-impl`
        // are one type, and the root's own bridge has to re-resolve exactly one of them.
        agent_type: agent_type.name.clone(),
        // **§3.1: the root is depth 0, by definition.** Its `spawn` therefore creates depth 1, and
        // this is the value that makes the whole chain measurable — without it every node in the
        // tree would look like a root to its own bridge.
        depth: ROOT_DEPTH,
        // **§5.4's per-node capability, minted by whoever owns this node's lifecycle** — which
        // since §11 item 28 step 6 is the supervisor, for every root a person can start. It arrives
        // through [`crate::run::SpawnObserver::identified`] and is written into exactly one place:
        // the declaration compiled a few lines below, which is the only channel the root's own
        // bridge reads. That is what will let the root's bridge present a `SpawnCaller` the
        // supervisor can check, rather than a name any process on this socket could assert.
        //
        // `None` is still reachable and still means the same thing it always did: nobody owns this
        // node's lifecycle beyond the call that started it ([`crate::run::Unwatched`], which is
        // every in-process `prepare` in the tests and every caller outside the supervisor). A token
        // minted by a process that is about to exit would be a credential with nothing behind it.
        node_token,
        ready_file: ready_file.clone(),
        repo: spec.repo.clone(),
        state_dir: spec.state.clone(),
        bridge: spec.bridge.clone(),
        bridge_args: vec!["mcp".into()],
    };
    let written = crate::run::write_config_documents(adapter.config_files(&launch, &ctx)?)?;
    let mut invocation = compile_root(adapter.as_ref(), spec.pane, &launch, &ctx)?;

    // §6.1 step 8, checked against the route the adapter *stated* rather than against the presence
    // of a file. Every branch here is a refusal except the two that positively found the
    // declaration, which is what keeps "this harness declares MCP some other way" from becoming
    // "this node silently got no bridge".
    // §6.1 step 8, checked against the route the adapter *stated* rather than against the presence
    // of a file. The check itself is `McpRoute::verify`, beside the enum whose promise it is
    // checking, because `marion doctor --adapter` reports the same finding and a second copy here
    // is exactly where the two would drift.
    // The post-launch half is compiled here, before the check, for the same reason the files above
    // are written before it: the check must see the values this launch will actually use, not a
    // second derivation of them.
    let session = adapter.session_declaration(&launch, &ctx)?;
    let mcp_config = adapter
        .mcp_route(&launch)
        .verify(&written, &invocation, session.as_ref())
        .map_err(|route| RootError::NoMcpDeclaration { harness, route })?;
    // **`Inherited` skips this too, and that is the whole of live mode on this path.** The adapter
    // already withheld the three env vars it compiles; a push here would put two of them straight
    // back, and `ANTHROPIC_API_KEY=""` in particular would blank the operator's own key on a node
    // that is supposed to be using it.
    if path == RootPath::Duplex && spec.auth == Auth::Canned {
        // §9: `ANTHROPIC_AUTH_TOKEN=<per-run token>` and `ANTHROPIC_API_KEY=""` — a non-empty key
        // silently wins (§6.4), so it is set to empty rather than left inherited.
        invocation
            .env
            .push(("ANTHROPIC_AUTH_TOKEN".into(), token.clone()));
        invocation
            .env
            .push(("ANTHROPIC_API_KEY".into(), String::new()));
    }

    // §6.1 step 7, first half, for a **root**: *"journal the spawn intent, start the process,
    // journal confirmation."* Written here, at the end of `prepare`, because this is the last
    // instant at which everything immutable about the node is known and **no process exists yet**:
    // `launch` is the act. A crash in the window between the two therefore leaves an intent with no
    // confirmation — recoverable — rather than a live root marion has no record of, which is
    // precisely the untracked-live-process M2's criteria forbid.
    //
    // §9's two absences are written as absences, not placeholders: a root has **no parent** and
    // **no `TaskContract`**. Its depth is `ROOT_DEPTH`, from the same constant the declaration its
    // own bridge reads is stamped from.
    crate::journal::record(
        &project,
        RecordKind::SpawnIntent(SpawnIntent {
            agent_id: agent_id.clone(),
            parent_id: None,
            // The **canonical** name, not `spec.agent_type`: `marion run codex` and `marion run
            // codex-impl` are one definition, and the journal must not record two.
            agent_type: agent_type.name.clone(),
            harness,
            depth: ROOT_DEPTH,
            task_id: None,
            // §3.1's bound as this run resolved it (`blocked_bound_secs`, on the spec), so the
            // tree reports the clock the operator asked for instead of the agent type's default.
            timeout_secs: Some(spec.bound_secs),
        }),
    );
    // **§6.1 step 7's shape, applied to the other thing this function decides.**
    //
    // `journal_the_roots_outcome` writes the change record when `launch_watched` *returns*, so
    // until this record existed a marion that panicked, was SIGKILLed or lost power mid-run left
    // the journal saying nothing at all — not that a grant had been issued, not what the operator's
    // tree looked like when it was. Written **after** the intent, because a record about a node the
    // journal has not yet introduced is a record with nowhere to attach; written **before** the
    // process, because that is the whole point, and it is a barrier so "before" survives the crash
    // it is about.
    //
    // The oid is recoverable evidence and not a bare number: the tree object lives in
    // `<agent-dir>/objects`, so it can be read back after the run that produced no post-tree.
    crate::journal::record(
        &project,
        root_grant_record(&agent_id, &change_base, &launch.tools),
    );

    Ok(RootNode {
        agent_id,
        project,
        agent_dir,
        mcp_config,
        ready_file,
        token,
        invocation,
        harness,
        surfaces,
        path,
        prompt: spec.prompt.clone(),
        auth: spec.auth,
        change_base,
        scope: RootScope::CeilingOnly {
            ceiling: agent_type.scope_ceiling.clone(),
        },
    })
}

/// A token scoped to this run and nothing else. Not a credential — the endpoint is canned — but
/// distinct per run so a request log attributes traffic to one run.
fn per_run_token() -> Result<String, RootError> {
    Ok(format!("marion-run-{}", uuid_v7(unix_millis(), entropy()?)))
}

/// **§3.4's two shapes, and which one this *run* asked for.** `surfaces()` is a fact about the
/// harness; the pane is a fact about the run. Selecting once, before anything is compiled, is what
/// keeps the two from being decided in two places and disagreeing: the `Invocation` and the launch
/// path are both derived from this one value, and it is carried onto the node so `launch_terminal`
/// reads the same surfaces that chose it rather than asking the adapter a second question.
fn root_surfaces(
    adapter: &dyn HarnessAdapter,
    pane: bool,
    harness: Harness,
) -> Result<ExecutionSurfaces, RootError> {
    Ok(match pane {
        false => adapter.surfaces(),
        true => adapter
            .pane_surfaces()
            .ok_or(marion_harness::HarnessError::NoPaneSurface(harness))?,
    })
}

/// The compile half of [`root_surfaces`]'s selection. Two methods rather than a flag inside one,
/// for the reason `HarnessAdapter::pane_surfaces` states: the pane's argv is a different launch of
/// the same harness (a TUI with a seeded composer), not the headless launch with a switch on it.
fn compile_root(
    adapter: &dyn HarnessAdapter,
    pane: bool,
    launch: &LaunchSpec,
    ctx: &SpawnCtx,
) -> Result<Invocation, RootError> {
    Ok(match pane {
        false => adapter.compile(launch, ctx)?,
        true => adapter.compile_pane(launch, ctx)?,
    })
}

/// Not one of §4.3's normative files: this is marion's own start-up handshake with a process it
/// did not spawn, so it lives beside the node's state rather than in the layout. Only the duplex
/// path has a frame to withhold, so only it has a marker to wait on (§6.1 step 8).
fn root_ready_file(path: RootPath, agent_dir: &AgentDir) -> Option<PathBuf> {
    match path {
        // **Dead, and kept honest rather than wildcarded.** `prepare` refused this path by name
        // before anything was created (`RootError::AcpIsNotARootHarness`), so control never reaches
        // here. Spelling the arm out means the day ACP becomes a root harness, the compiler asks
        // this question again instead of a `_` answering it silently.
        RootPath::Acp => None,
        // **The marker is minted on the pane path too, and it is deliberately not a gate there.**
        //
        // Two different things have been collapsed under one name: the *file*, which the bridge
        // touches when the harness fetches marion's tool list, and the *wait* on it, which is §6.1
        // step 8's gate. A pane needs no wait — its prompt is seeded into the composer and the
        // operator presses return, so there is no frame being withheld for a marker to release —
        // but Claude Code's declaration requires somewhere to write it, and `compile_pane` emits
        // the same `--mcp-config` document the headless shape does. Withholding the path here
        // failed the whole launch with a sentence about a readiness gate this path does not have.
        //
        // It is not wasted either way: the file's existence afterwards is the pane's only evidence
        // that the bridge was ever reached, and it is the only such evidence a TUI can leave.
        RootPath::Duplex | RootPath::Terminal => {
            let f = agent_dir.path().join("mcp-ready");
            let _ = std::fs::remove_file(&f);
            Some(f)
        }
        // A `LaunchOnly` node's readiness is asserted post hoc, over its stream
        // (`assert_a_verb_was_answered`), and its adapters ask for no marker.
        RootPath::LaunchOnly => None,
    }
}

/// **§9's change record, first half: the operator's tree as it stood at launch.**
///
/// A failure here is fatal **only for a type that declares a tool** — `availability_axis` turns
/// it into `RootError::NoChangeRecord` there and into an empty axis everywhere else. So a root
/// in a directory that is not a git worktree still runs when it asked for nothing, and its
/// record still says `Failed` with git's own words.
fn root_change_base(spec: &RootSpec, agent_dir: &AgentDir) -> RootChangeBase {
    match spec.no_change_record {
        // Not even attempted: `TreeSnapshot::open` is not called, so `--no-change-record` really
        // does mean marion does not walk the operator's tree — not "walks it and discards the
        // result", which would cost exactly as much and be a different claim from the one the flag
        // makes.
        true => RootChangeBase::NotAttempted {
            reason: "the operator passed --no-change-record".into(),
        },
        false => match crate::spawn::TreeSnapshot::open(&spec.repo, agent_dir.path()) {
            Ok(snapshot) => {
                let base_commit = snapshot.head(&spec.repo);
                match snapshot.take(&spec.repo) {
                    Ok(pre_tree) => RootChangeBase::Taken {
                        snapshot: std::sync::Arc::new(snapshot),
                        base_commit,
                        pre_tree,
                    },
                    Err(e) => RootChangeBase::Unavailable {
                        reason: format!("snapshotting the working tree at launch: {e}"),
                    },
                }
            }
            Err(e) => RootChangeBase::Unavailable {
                reason: format!(
                    "preparing an isolated git environment for {}: {e}",
                    spec.repo.display()
                ),
            },
        },
    }
}

/// The root's launch, in the neutral vocabulary the adapter compiles from. `tools` is the axis
/// [`availability_axis`] already gated, and `token` the per-run credential [`per_run_token`] minted.
fn root_launch_spec(
    spec: &RootSpec,
    path: RootPath,
    tools: Vec<String>,
    token: &str,
    adapter: &dyn HarnessAdapter,
    agent_dir: &AgentDir,
) -> LaunchSpec {
    LaunchSpec {
        cwd: spec.repo.clone(),
        model: spec.model.clone(),
        prompt: match path {
            // Written after launch, not compiled into argv (§6.1 step 8). `Acp` is dead here —
            // refused in `prepare` — and would be the same answer: the prompt is a
            // `session/prompt` frame and reaches argv on no ACP agent.
            RootPath::Duplex | RootPath::Acp => String::new(),
            // A positional, and what the harness then does with it **differs by harness** —
            // stated rather than generalised, because marion compiles the same field twice and
            // gets two behaviours. Claude Code seeds its composer and waits for a return
            // (`marion_harness::claude_code::SPEC`'s pane row); codex 0.147.0 **submits** it, so a
            // paned codex has taken a turn before anybody attaches
            // (`marion_harness::codex::SPEC`'s pane row). Neither is a race the way an argv prompt on
            // `--print` is: a TUI's MCP servers are connected before it accepts the turn.
            //
            // Both are safe for the reason the headless refusal is not: see the same two doc
            // comments for the measurement.
            RootPath::LaunchOnly | RootPath::Terminal => spec.prompt.clone(),
        },
        // §3.1's availability axis: the agent type's own `tools:` list, exactly as
        // `run::run_spawn` gives a child — but only ever through `availability_axis`, which is
        // where the list and the audit that justifies it are decided together rather than in two
        // places. `ROOT_VERBS` below stays a constant, and for a different reason: it carries
        // marion's own verbs, which no agent type may widen — spelled per harness by the adapter.
        tools,
        allowed_tools: ROOT_VERBS
            .iter()
            .map(|verb| adapter.marion_tool_name(verb))
            .collect(),
        mcp: McpDeclaration::Marion,
        // The session this launch resumes, handed to the row's measured resume flag — or `None`
        // for a fresh run. A row with no measured flag for this shape refuses at `compile`, by
        // name, rather than starting fresh under the resumed session's id.
        resume: spec.resume.as_ref().map(|(_, session)| session.clone()),
        base_url: spec.base_url.clone(),
        // On the duplex path the root's credential is the per-run `ANTHROPIC_AUTH_TOKEN` pushed
        // onto the invocation below. On the other three it is **not** an env var marion can push
        // after the fact — gemini wants `GEMINI_API_KEY`, opencode wants it *inside* the generated
        // config — so it goes through the neutral field and each adapter puts it where that harness
        // reads it.
        //
        // Under `Inherited` there is no credential to place at all, on any path: the node is meant
        // to present the login the operator already has, and a placeholder beside it would be a
        // second credential competing with the real one.
        api_key: match (spec.auth, path) {
            // `Acp` is dead here — refused in `prepare`. Under a canned provider the credential
            // travels in the agent's own config document (S23), never in this field, so `None` is
            // also the answer it would have if the refusal were lifted.
            (Auth::Inherited, _) | (Auth::Canned, RootPath::Duplex | RootPath::Acp) => None,
            // The pane shape compiles the credential itself, exactly as the `LaunchOnly` adapters
            // do — `compile_pane` emits `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY` from this field
            // — so there is nothing for the post-`compile` push below to do.
            (Auth::Canned, RootPath::LaunchOnly | RootPath::Terminal) => Some(token.to_string()),
        },
        auth: spec.auth,
        config_dir: agent_dir.config_dir(),
        extra: Extras::default(),
    }
}

/// **§6.1 step 7's shape, applied to the other thing `prepare` decides.**
///
/// `journal_the_roots_outcome` writes the change record when `launch_watched` *returns*, so until
/// this record existed a marion that panicked, was SIGKILLed or lost power mid-run left the journal
/// saying nothing at all — not that a grant had been issued, not what the operator's tree looked
/// like when it was. Written **after** the intent, because a record about a node the journal has
/// not yet introduced is a record with nowhere to attach; written **before** the process, because
/// that is the whole point, and it is a barrier so "before" survives the crash it is about.
///
/// The oid is recoverable evidence and not a bare number: the tree object lives in
/// `<agent-dir>/objects`, so it can be read back after the run that produced no post-tree.
fn root_grant_record(
    agent_id: &AgentId,
    change_base: &RootChangeBase,
    granted: &[String],
) -> RecordKind {
    RecordKind::RootGrantDecided(RootGrant {
        agent_id: agent_id.clone(),
        base_commit: match change_base {
            RootChangeBase::Taken { base_commit, .. } => base_commit.clone(),
            _ => None,
        },
        pre_tree: match change_base {
            RootChangeBase::Taken { pre_tree, .. } => Some(pre_tree.clone()),
            _ => None,
        },
        // The **compiled** axis, not `agent_type.tools`: what a crash investigator needs is what
        // the root was actually handed, and those two differ in exactly the case this record is
        // most useful in — a declaration the gate withheld.
        granted: Reason::new(granted.join(", ")),
    })
}

/// Start the root and run it to completion, on whichever path its **surfaces** select (§3.4).
///
/// `bound` is the root's node-level timeout (§9), and what it bounds is derived from the surface
/// exactly like everything else here:
///
/// - on [`RootPath::Duplex`] it is §9's **per-episode `Blocked`-only** budget, consumed only while
///   marion is holding an answer the root is waiting on. Deliberately not a wall-clock ceiling —
///   marion offers a root none — so that path has no deadline of its own;
/// - on [`RootPath::LaunchOnly`] there *is* no `Blocked` state to budget: the node has no
///   permission channel to block on and no descendant hold marion can observe, so a `Blocked`-only
///   budget would bound nothing at all. The same knob is therefore the **wall-clock** bound, and it
///   is not optional: measured in S13, **opencode never exits on a provider hang** — a 500 still
///   retrying at 90 s, a connection-refused still hung at 180 s, no backoff ceiling. Without a
///   bound `marion run opencode` is an unbounded hang.
pub fn launch(
    node: &RootNode,
    bound: StdDuration,
    mcp_ready_timeout: StdDuration,
) -> Result<RootOutcome, RootError> {
    launch_watched(node, bound, mcp_ready_timeout, None)
}

/// [`launch`], with somewhere to send the root's frames **as they arrive**.
///
/// A run is minutes long and, until this existed, produced nothing at all until it returned, so a
/// person could not tell a root that was delegating from one that was wedged. `watcher` is what
/// `marion run` passes to render those frames for a human; `launch` is the same call with none, and
/// is what every non-interactive caller uses.
///
/// **Additive, and only on the duplex path.** The accumulated `RootOutcome::transcript` is
/// unchanged whether a watcher is present or not — many tests read it, and a live view is a second
/// way to observe the same run, not a replacement for the record of it. A `LaunchOnly` root is
/// launched through `run_bounded`, which has no frame loop to watch: its prompt is already in argv
/// and marion reads its stream only after it exits, so there is nothing to stream and this
/// deliberately does not pretend otherwise.
pub fn launch_watched(
    node: &RootNode,
    bound: StdDuration,
    mcp_ready_timeout: StdDuration,
    watcher: Option<duplex::StreamSink<'_>>,
) -> Result<RootOutcome, RootError> {
    launch_owned(node, bound, mcp_ready_timeout, watcher, None, None)
}

/// **Where a root's pane goes**, for an owner that can serve it.
///
/// A pane is only useful to a process that answers `node/attach` — the supervisor — and `root.rs`
/// must not know what a `RegistryHandle` is, so the seam is this pair of calls rather than a
/// registry parameter. It is the display-plane counterpart of
/// [`crate::run::SpawnObserver::started`]: the same "tell the owner at the one instant it becomes
/// true" shape, on the other axis of §3.4.
///
/// **`closed` is not tidiness.** The `Arc` the owner holds keeps the host — and therefore the
/// master fd — alive for as long as the entry exists, and an entry left behind answers a later
/// `node/attach` with a pane onto a pty whose child marion reaped. `launch_terminal` calls it
/// **before** it shuts the host down, so the node stops being advertised at the instant marion
/// decides to stop rather than at the instant the fd closes.
pub trait PaneOwner: Send + Sync {
    /// This node now owns a pty, and a client may attach to it.
    fn opened(&self, agent_id: &AgentId, host: std::sync::Arc<crate::pty::PtyHost>);
    /// Stop admitting live operations while the launcher drains this exact host.
    fn closing(&self, agent_id: &AgentId, host: &std::sync::Arc<crate::pty::PtyHost>);
    /// Publish this exact, successfully drained host for read-only replay.
    fn completed(
        &self,
        agent_id: &AgentId,
        host: &std::sync::Arc<crate::pty::PtyHost>,
        charged_bytes: usize,
    );
    /// Permanently forget and invalidate the pane after a failed close or explicit removal.
    fn failed(&self, agent_id: &AgentId, host: &std::sync::Arc<crate::pty::PtyHost>);
}

/// [`launch_watched`], with the node's **owner** told the instant a process exists.
///
/// `on_started` is the second half of [`crate::run::SpawnObserver`] for a root — see
/// [`prepare_watched`] for the first. It is `Option` because ownership is: a caller that holds this
/// blocking call for the root's whole turn already knows everything the hook announces, while a
/// supervisor that answers `agent/spawn` from another thread does not, and its answer is a claim
/// about a process that must not be made before one exists.
///
/// **The journal does not depend on it.** `Spawned { pid: Some(_) }` is written from the same
/// instant whether or not anybody is listening — see [`launch_inner`] — so the record is a property
/// of the run rather than of who asked for it.
/// `pane` is the same idea on §3.4's display axis — see [`PaneOwner`]. `None` is a caller with
/// nowhere to put a pty, and on [`RootPath::Terminal`] it means the node runs and records exactly
/// as it would have, with nobody able to attach: the recording at `AgentDir::pty_cast()` is written
/// either way, because what marion observed is not a function of who was watching.
pub fn launch_owned(
    node: &RootNode,
    bound: StdDuration,
    mcp_ready_timeout: StdDuration,
    watcher: Option<duplex::StreamSink<'_>>,
    on_started: Option<&dyn Fn(i32)>,
    pane: Option<&dyn PaneOwner>,
) -> Result<RootOutcome, RootError> {
    launch_inner(node, bound, mcp_ready_timeout, watcher, on_started, pane)
}

fn launch_inner(
    node: &RootNode,
    bound: StdDuration,
    mcp_ready_timeout: StdDuration,
    watcher: Option<duplex::StreamSink<'_>>,
    on_started: Option<&dyn Fn(i32)>,
    pane: Option<&dyn PaneOwner>,
) -> Result<RootOutcome, RootError> {
    // **§6.1 step 7's confirmation, at the instant the process exists** — §11 item 28 step 1's rule,
    // applied to the node it had left out. Written here rather than after the run returns, and
    // carrying the pid rather than an absence, because both halves are what a *second* process needs
    // to act on this node at all: `SpawnIntent` alone now means "no process exists, full stop", and
    // a `Spawned` with a pid is a signal target §6.7's kill can reach.
    //
    // The window this opens is the one §6.1 step 7 is written to have: one `write(2)` plus one
    // fsync between the process existing and the journal saying so. Today's equivalent window was
    // the root's entire turn.
    //
    // **The append is fallible and its failure is not survivable**, which is why this one record
    // does not go through `journal::record`'s never-fail-the-run policy. `Spawned` is the only
    // record carrying a pid, so losing it does not leave a stale reading of the root — it leaves a
    // live process `procid::audit`, whose scope is `node.pid.is_some()`, cannot see at all. That is
    // §11 item 30's untracked live process, and §9 criterion 3 is decided by that audit. The only
    // two states marion may leave behind are *recorded and live* or *not live*, and `spawn` cannot
    // be taken back — so the tree is killed here, the driver below reaps it, and the run fails as
    // [`RootError::UnaccountableNode`] rather than succeeding over a node nothing can name.
    let spawned = std::cell::Cell::new(false);
    let unaccountable: std::cell::Cell<Option<crate::journal::JournalError>> =
        std::cell::Cell::new(None);
    let started = |pid: i32| confirm_root_started(node, on_started, &spawned, &unaccountable, pid);
    // **§7.3.3's replay leg for a root**, alongside the journal's record of the same run and for the
    // complementary reason: the journal says a root existed and how it ended, this says what it
    // said. `None` on an open failure — a viewer may never fail a run (`events::EventSink::open`).
    let mut events = crate::events::EventSink::open(
        &node.agent_dir,
        &node.agent_id,
        node.harness,
        crate::run::init_request_id(&node.agent_id),
    );
    if let Some(es) = &events {
        es.lifecycle(marion_core::event::Lifecycle::Opened);
    }
    // The root's harness session, journaled on the frame that names it — what a later
    // `node/resume` hands back to the harness. Beside `events` for the same reason it is on the
    // child path: a root lost mid-run never reaches its capture.
    // A root's shape is its launch path: a pane is the pty marion owns (`Terminal`), everything
    // else is headless. Recorded on the session so a resume takes the same shape.
    let session = crate::session_watch::SessionWatch::new(
        &node.project,
        &node.agent_id,
        node.harness,
        node.path == RootPath::Terminal,
    );
    let result = match node.path {
        // Unreachable by `prepare`'s refusal, and stated as the refusal rather than as a panic: a
        // `Node` reaching here on this path would mean the guard had been removed, and an operator
        // deserves the sentence that explains why over a supervisor abort.
        RootPath::Acp => {
            return Err(RootError::AcpIsNotARootHarness(node.harness.to_string()));
        }
        // The root's frames go to **two** places now, and they are different kinds of destination:
        // `watcher` renders them for a human as they arrive and keeps nothing, `events` keeps them
        // and renders nothing. Teeing rather than choosing, because a run watched by a person must
        // still be re-attachable afterwards — and a run nobody watched must be re-attachable too.
        RootPath::Duplex => {
            let tee = |ev: duplex::StreamEvent<'_>| {
                tee_root_frame(events.as_ref(), &session, watcher, ev)
            };
            launch_duplex(
                node,
                bound,
                mcp_ready_timeout,
                Some(&tee as duplex::StreamSink<'_>),
                &started,
            )
        }
        // No live seam at all on this path — `run_bounded` drains the pipe whole — so the stream is
        // recovered from the capture inside `launch_only`, where the raw stdout still exists.
        // Recovering it from `RootOutcome::transcript` out here would silently drop every non-JSON
        // line, which is the unexplained-silence failure `duplex::StreamEvent` has two variants to
        // prevent.
        RootPath::LaunchOnly => launch_only(node, bound, events.as_mut(), &started, &session),
        // §9's M3: a node in a terminal marion owns. No stream to tee — a TUI emits bytes, not
        // frames — so `events` gets only the lifecycle bookends `launch_inner` writes itself, and
        // the byte-level record is `AgentDir::pty_cast()`.
        RootPath::Terminal => launch_terminal(node, bound, &started, pane),
    };
    // **Substituted for whatever the kill made the driver return.** The kill above produces a
    // signalled exit on one path and a torn stream on the other, and reporting either as itself
    // would name the symptom and bury the cause. `journal_the_roots_outcome` then journals this as
    // a `SpawnAborted` — the record for a node marion decided the fate of before it produced
    // anything — and, because `spawned` is still false, does not claim a confirmation either.
    let result = match unaccountable.take() {
        // The error's shape, never a copy of the record that would not fit: this string is
        // journaled, and pasting an over-cap record into the explanation of why it was over the cap
        // would fail the same cap twice.
        Some(why) => Err(RootError::UnaccountableNode {
            why: why.to_string(),
        }),
        None => result,
    };
    journal_the_roots_outcome(node, &result, spawned.get());
    // The closing bookend, from the same reading the journal's `Exited` record is written from.
    if let Some(es) = &events {
        es.lifecycle(roots_terminal_lifecycle(&result));
    }
    result
}

/// `launch_inner`'s `on_started` hook: `Spawned { pid: Some(_) }` through the fallible barrier,
/// then the owner — or the tree killed and the run marked unaccountable.
///
/// The owner is told **after** the record is durable-ordered, so a supervisor that answers its
/// caller on this hook is making a claim the journal already backs. On the failure arm it is
/// deliberately not told at all: the claim would not be backed, and there is about to be no
/// process to make it about.
fn confirm_root_started(
    node: &RootNode,
    on_started: Option<&dyn Fn(i32)>,
    spawned: &std::cell::Cell<bool>,
    unaccountable: &std::cell::Cell<Option<crate::journal::JournalError>>,
    pid: i32,
) {
    match crate::journal::append(&node.project, spawned_record(node, Some(pid))) {
        Ok(_) => {
            spawned.set(true);
            if let Some(hook) = on_started {
                hook(pid);
            }
        }
        Err(e) => {
            // The whole tree, not the pid alone: a root is a group leader and may already have
            // descendants, and they are as unnameable as it is. Not `_and_wait` — the driver in
            // `launch_inner` is already waiting on this exact child.
            crate::run::kill_process_tree(pid);
            unaccountable.set(Some(e));
        }
    }
}

/// One duplex frame to its three readers, in order: the event file keeps it, the session watch
/// reads the session id off it, and the watcher renders it for a human and keeps nothing.
fn tee_root_frame(
    events: Option<&crate::events::EventSink>,
    session: &crate::session_watch::SessionWatch<'_>,
    watcher: Option<duplex::StreamSink<'_>>,
    ev: duplex::StreamEvent<'_>,
) {
    if let Some(es) = events {
        es.record(ev);
    }
    session.observe_event(ev);
    if let Some(w) = watcher {
        w(ev);
    }
}

/// §6.1 step 7's second half and the node's terminal transition, for a root.
///
/// **Both are written after the run returns, and the ordering is honest rather than convenient.**
/// A root's process is started and reaped inside a single blocking call — `run_bounded` on the
/// `LaunchOnly` path, `duplex::run_duplex` on the other — so the first instant at which marion can
/// truthfully say *"the process exists"* and the instant it says *"it exited"* are the same
/// instant. Writing `Spawned` **before** the call would be a confirmation of something that had
/// not happened, which is exactly the claim §6.1's intent-then-confirm split exists to keep marion
/// from making; and the alternative reading — a crash mid-run leaves the intent unconfirmed — is
/// the *right* one, because that is a node whose fate marion genuinely does not know (§7.2).
///
/// Every failure of `launch` is recorded too, and none of them is left as silence: §7.2 is emphatic
/// that a node marion decided the fate of must never be mistaken for one marion *lost*.
///
/// **"Every exit path" below means every path on which this function is called — that is, every way
/// `launch_watched` can *return*.** It does not and cannot mean every way a run can end. If marion
/// itself panics, is SIGKILLed or loses power while the root is running, nothing here executes and
/// there is no change record, because there is no post-snapshot: the tree was never read a second
/// time. That window is covered by a different record and not by a stronger claim about this one —
/// `RecordKind::RootGrantDecided`, written in `prepare` before the process starts, which leaves the
/// grant and the pre-tree oid on disk so a crashed run is evidence rather than silence. The old
/// sentence said "survives every exit path" flatly, and a reader who took it at face value would
/// have concluded the crash case was covered when it was the one case it was not.
/// **A root's terminal transition, derived once.**
///
/// Extracted so the journal's `Exited` record and `events.jsonl`'s closing bookend read the *same*
/// derivation rather than each computing it. `run_spawn` gets this for free by reading both off the
/// contract's `completion`, and states the reason there: two derivations of one status are two
/// chances to disagree about the same run. A root has no contract, so the shared source has to be a
/// function.
///
/// §6.7's derivation, applied to a node that has no contract to record it in. `Unreported` is
/// deliberately absent: it means *no `report` arrived*, and §9 says a root **cannot `report`** at
/// all, so spending that status here would make every root look like a child that stayed silent. A
/// timeout outranks everything for the same reason it does in `build_contract` — it is marion's own
/// attributed kill.
fn roots_exit(outcome: &RootOutcome) -> (ExitStatus, ProcessExit) {
    let status = if outcome.timed_out {
        ExitStatus::TimedOut
    } else if outcome.failure.is_some() || outcome.exit_code.unwrap_or(0) != 0 {
        ExitStatus::Failed
    } else {
        ExitStatus::Ok
    };
    let description = if outcome.timed_out {
        "the root exceeded marion's bound and its process group was killed".to_string()
    } else {
        match outcome.exit_code {
            Some(code) => format!("root exited with code {code}"),
            None => "root exit status was unavailable".to_string(),
        }
    };
    let description = match &outcome.failure {
        Some(f) => format!("{description}; the root's stream reported: {f}"),
        None => description,
    };
    (
        status,
        ProcessExit {
            code: outcome.exit_code,
            // `run_bounded` reports one, but a root's outcome does not carry it: `RootOutcome` is
            // the shared shape of both paths and the duplex driver has none. Left absent rather
            // than guessed at — §6.7's description carries what marion actually observed.
            signal: None,
            description,
        },
    )
}

/// The `ProcessExit` both records of a [`RootError::BridgeNeverReached`] carry.
///
/// **The description is marion's own sentence, not a summary of it** — and since §11 item 28 step 6
/// that is load-bearing rather than tidy. The refusal names the harness, how many frames the root
/// emitted, the exit code it claimed and whatever the stream said about the failure; a `marion run`
/// that drove the root in-process printed all of it from the `RootError` in its hand, and a client
/// watching over the socket has only this field. A fixed string here is the difference between
/// *"opencode: the root never reached marion's bridge … child exit Some(1)"* and a run that failed
/// for no stated reason — which is the same silence §6.1 step 8 exists to refuse.
///
/// One function, so the journal's `Exited` and `events.jsonl`'s closing bookend cannot come to
/// describe one refusal two ways.
fn bridge_never_reached_exit(exit: Option<i32>, e: &RootError) -> ProcessExit {
    ProcessExit {
        code: exit,
        signal: None,
        description: e.to_string(),
    }
}

/// The closing bookend for `events.jsonl`, mirroring [`journal_the_roots_outcome`]'s three arms so
/// the two records of one run never disagree about which of them happened.
fn roots_terminal_lifecycle(
    result: &Result<RootOutcome, RootError>,
) -> marion_core::event::Lifecycle {
    use marion_core::event::Lifecycle;
    match result {
        Ok(o) => {
            let (status, exit) = roots_exit(o);
            Lifecycle::Exited { status, exit }
        }
        // The run happened and marion refused the *result*, so this is an exit — the same reading
        // the journal takes of this one variant, and the reason it is the only error arm that is
        // not an abort.
        Err(e @ RootError::BridgeNeverReached { exit, .. }) => Lifecycle::Exited {
            status: ExitStatus::Failed,
            exit: bridge_never_reached_exit(*exit, e),
        },
        Err(e) => Lifecycle::Aborted {
            reason: e.to_string(),
        },
    }
}

/// `spawned` is whether [`launch_inner`]'s hook already journaled the confirmation. See
/// [`spawned_record`]: after §11 item 28 step 6 that is the ordinary path and this function writes
/// no `Spawned` at all, which is the whole point — the record now names the instant the process
/// existed rather than the instant it stopped existing.
fn journal_the_roots_outcome(
    node: &RootNode,
    result: &Result<RootOutcome, RootError>,
    spawned: bool,
) {
    // A confirmation for a process whose existence this function is only *inferring*, and only when
    // nothing observed it directly. Unreachable through either driver today — both call the hook
    // between `spawn()` and the first byte — and kept as an absence rather than an `unreachable!()`
    // because the thing it would be asserting is exactly the thing that must not fail silently: a
    // run that reached a terminal reading with no `Spawned` behind it would replay as a node whose
    // intent was never resolved (§7.2), which is the reading reserved for a node marion *lost*.
    let confirm = |node: &RootNode| {
        if !spawned {
            crate::journal::record(&node.project, spawned_record(node, None));
        }
    };
    // **First, and before the match on `result`** — §9's change record is written on every path
    // that reaches this function, which is every path `launch_watched` returns on. (A crash inside
    // the run reaches nothing here; see this function's doc comment for what covers that instead.)
    //
    // The record derives from the filesystem at two instants, not from the stream, so it does not
    // inherit the visibility asymmetry `RootOutcome::marion_calls` exists to paper over: a duplex
    // root and a `LaunchOnly` one produce the same shape of record, which is the point. Putting
    // this on the `Ok` arm would lose it for exactly the runs that need it most — a `LaunchOnly`
    // root killed on the wall-clock bound (`launch_only`), and a root refused as
    // `BridgeNeverReached` below, both of which are runs where a write already happened and marion
    // is about to say the run failed.
    record_the_roots_change(node);
    let outcome = match result {
        Ok(o) => o,
        // The run happened, the process exited, and marion refused the *result* — so this is an
        // exit, not an abandonment. It is the only error variant that can say so.
        Err(e @ RootError::BridgeNeverReached { exit, .. }) => {
            confirm(node);
            crate::journal::record(
                &node.project,
                RecordKind::Exited(Exited {
                    agent_id: node.agent_id.clone(),
                    status: ExitStatus::Failed,
                    exit: bridge_never_reached_exit(*exit, e),
                }),
            );
            return;
        }
        Err(e) => {
            crate::journal::record(
                &node.project,
                RecordKind::SpawnAborted(SpawnAborted {
                    agent_id: node.agent_id.clone(),
                    // marion's own explanation, which is what these errors already are — never
                    // derived from an exit code.
                    reason: e.to_string(),
                }),
            );
            return;
        }
    };
    confirm(node);
    // §6.7's status derivation, read onto a node that has no contract to record it in.
    // `Unreported` is deliberately absent: it means *no `report` arrived*, and §9 says a root
    // **cannot `report`** at all — spending that status on a root would make every root look like
    // a child that stayed silent. A timeout outranks everything for the same reason it does in
    // `build_contract`: it is marion's own attributed kill.
    let (status, exit) = roots_exit(outcome);
    crate::journal::record(
        &node.project,
        RecordKind::Exited(Exited {
            agent_id: node.agent_id.clone(),
            status,
            exit,
        }),
    );
    // Every permission marion refused, in the journal rather than in a contract — §9 says so in as
    // many words, *because* the node it happens to may be a root and a root has no contract. The
    // emitter is shared with the **child** path (`run::run_spawn`), which used to discard its
    // denials outright; see `journal::record_permission_denials` for why the journal stays the one
    // destination even for a node that does have a contract.
    crate::journal::record_permission_denials(
        &node.project,
        &node.agent_id,
        &outcome.denied_permissions,
        // **One sentence for two routes to the same denial**, because `denied_permissions` carries
        // tool names and not reasons: an ask marion cannot answer is held for the root's whole
        // `Blocked` budget and then denied (§9), while one §5.4 decides — a root's `report` — is
        // denied on arrival (`duplex::decided_permission`). Saying "the bound expired" about both
        // would put a false event in the audit record for the second. The per-ask sentence is not
        // lost: it is written to the node itself as the denial's `message`, and a root's transcript
        // is what `marion run` prints.
        "denied without an answerer: M1 has no permission answerer, so an ask marion cannot decide \
         is held for the root's Blocked bound and then denied (§9), and one §5.4 decides is denied \
         at once",
    );
}

/// §9's change record, written: the sidecar first, then the journal's pointer at it.
///
/// **The order is load-bearing and the failure is not silent.** `RootChanged` carries counts whose
/// authority rests on `<agent-dir>/root-change.json` being there — that is the whole
/// `ContractPersisted` shape. So if the file cannot be written, the journal is told marion could
/// not see, rather than being handed counts with nothing behind them. A record pointing at a file
/// that does not exist is a *worse* artifact than an honest `Failed`.
fn record_the_roots_change(node: &RootNode) {
    let change = observe_the_roots_change(node);
    let record = match serde_json::to_vec_pretty(&change)
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            std::fs::write(node.agent_dir.root_change(), bytes).map_err(|e| e.to_string())
        }) {
        Ok(()) => change.record(),
        Err(e) => {
            let path = node.agent_dir.root_change();
            eprintln!("marion: writing {}: {e}", path.display());
            RootChanged {
                agent_id: node.agent_id.clone(),
                base_commit: change.base_commit.clone(),
                head_at_exit: change.head_at_exit.clone(),
                observation: RootObservation::Failed {
                    reason: Reason::new(format!(
                        "the delta was measured and {} could not be written: {e}",
                        path.display()
                    )),
                },
            }
        }
    };
    crate::journal::record(&node.project, RecordKind::RootChanged(record));
}

/// Take the second snapshot and assemble what the two say.
///
/// **The subject is `invocation.cwd`, the directory the process actually had** — not `RootSpec::repo`
/// re-read from somewhere. They are the same value by construction, and reading the compiled one
/// keeps them one value: a record measuring a directory the root did not run in would be the same
/// class of lie as `TaskContract.child.harness` sourced from the request rather than the adapter.
///
/// Nothing here reads `RootOutcome::transcript` or `marion_calls`. §6.7 is explicit that a stream's
/// `locations` are **corroboration and never a source**, and a root would be the one place that
/// rule got quietly inverted.
fn observe_the_roots_change(node: &RootNode) -> RootChange {
    let repo = &node.invocation.cwd;
    let (snapshot, base_commit, pre_tree) = match &node.change_base {
        RootChangeBase::Taken {
            snapshot,
            base_commit,
            pre_tree,
        } => (snapshot, base_commit.clone(), pre_tree.clone()),
        // marion never got a base point. `Failed` and not `NotAttempted`: it tried and could not
        // see. The two are kept apart deliberately — see the arm below and `RootChangeBase`.
        RootChangeBase::Unavailable { reason } => {
            return RootChange {
                agent_id: node.agent_id.clone(),
                base_commit: None,
                head_at_exit: None,
                scope: node.scope.clone(),
                working_tree_delta: RootDelta::Failed {
                    reason: Reason::new(reason),
                },
            };
        }
        // marion did not look, and the record says which decision that was. This is the variant
        // `RootDelta::NotAttempted` was written for: *the journal is silent* / *marion did not
        // look, here is why* / *marion looked and nothing changed* are three distinct readings, and
        // that triple is the whole deliverable of the change record (§9).
        RootChangeBase::NotAttempted { reason } => {
            return RootChange {
                agent_id: node.agent_id.clone(),
                base_commit: None,
                head_at_exit: None,
                scope: node.scope.clone(),
                working_tree_delta: RootDelta::NotAttempted {
                    reason: Reason::new(reason),
                },
            };
        }
    };
    let head_at_exit = snapshot.head(repo);
    let (scope, delta) = match observe(node, snapshot, repo, &pre_tree) {
        Ok(v) => v,
        Err(e) => (
            node.scope.clone(),
            RootDelta::Failed {
                reason: Reason::new(format!("snapshotting the working tree at exit: {e}")),
            },
        ),
    };
    // Both snapshots are taken; the copied index has nothing left to carry.
    snapshot.release();
    RootChange {
        agent_id: node.agent_id.clone(),
        base_commit,
        head_at_exit,
        scope,
        working_tree_delta: delta,
    }
}

/// The measurement itself, as one fallible expression so a partial reading is impossible: either
/// every term of the delta was derived from the same pair of trees, or there is no delta at all.
fn observe(
    node: &RootNode,
    snapshot: &crate::spawn::TreeSnapshot,
    repo: &std::path::Path,
    pre_tree: &Oid,
) -> Result<(RootScope, RootDelta), SpawnError> {
    let post_tree = snapshot.take(repo)?;
    let changed_paths = snapshot.changed_paths(repo, pre_tree, &post_tree)?;
    // Against `base_commit`, which is what that field is *for*. Absent where there is no commit to
    // measure from — an empty list, not a failure, because "no commits yet" is a normal repository.
    let pre_dirty_paths = match &node.change_base {
        RootChangeBase::Taken {
            base_commit: Some(b),
            ..
        } => snapshot.changed_paths(repo, b, pre_tree)?,
        _ => Vec::new(),
    };
    let (scope, scope_violations) = judge_scope(&node.scope, &changed_paths);
    let patch = snapshot.diff(repo, pre_tree, &post_tree)?;
    // **Not `?`.** Every term above is part of the delta, and a delta assembled from two different
    // pairs of trees would be a lie — hence one fallible expression. This is not part of the delta:
    // it is a statement about how much the delta could not see, and losing it must not turn a
    // measured run into `Failed`. `None` says the count was not obtained, which is a weaker record
    // than `Some(0)` and a truthful one; `Some(0)` obtained by swallowing an error would be the
    // strongest claim in this record made by accident.
    let ignored_not_measured = snapshot.ignored_entries(repo).ok();
    Ok((
        scope,
        RootDelta::Observed {
            pre_tree: pre_tree.clone(),
            post_tree,
            changed_paths,
            pre_dirty_paths,
            scope_violations,
            // **`Some("")` for an empty patch, never `None`.** `run_spawn` used to drop an empty
            // diff, and `worktree_reap.rs` pins what that cost: a contract carrying `changed_paths`
            // with no bytes anywhere behind them. Here an empty patch is a *measurement* — the
            // trees were identical — and the sidecar is authoritative, so it is stored complete
            // and uncapped. `Capped` is the type only so a reader never has to guess whether it is.
            diff: Some(Capped::whole(patch)),
            ignored_not_measured,
        },
    ))
}

/// §5.4's detective check, asked with **one** list.
///
/// `Scope::new(ceiling, ceiling)` and not `Scope::new(ceiling, &["**"])`: the conjunction of a list
/// with itself is that list, so this puts the one-list question to the two-list API without
/// inventing a second author. `["**"]` in the request slot would be a *fabricated* request — the
/// exact thing `RootScope::CeilingOnly` exists to make unsayable in the record.
///
/// A ceiling that will not compile yields `NotEnforced` with globset's own words. Never an empty
/// violation list beside a `CeilingOnly` claim, which would read as "checked, nothing wrong".
fn judge_scope(scope: &RootScope, changed: &[PathBuf]) -> (RootScope, Vec<PathBuf>) {
    let RootScope::CeilingOnly { ceiling } = scope else {
        return (scope.clone(), Vec::new());
    };
    match marion_core::scope::Scope::new(ceiling, ceiling) {
        Ok(s) => (scope.clone(), s.violations(changed)),
        Err(e) => (
            RootScope::NotEnforced {
                reason: Reason::new(format!(
                    "the agent type's scope ceiling does not compile: {e}"
                )),
            },
            Vec::new(),
        ),
    }
}

/// §6.1 step 7's confirmation for a root. `harness_version` and `model` are resolved by *launching*
/// (§3.2), which is why they are here and not on the intent.
///
/// **`pid` is `Some` on every path a process took**, since §11 item 28 step 6. It used to be `None`
/// with the reason *"marion drives the root through a helper that owns the child and does not
/// surface one"* — true when `marion run` held the whole turn in one blocking call, and false the
/// moment the supervisor started owning roots: there is now a second process that can act on this
/// number, and §6.7's kill needs a signal target. It is still not an *identity* — a bare pid does
/// not distinguish a survivor from a recycled number, which is `restart.rs`'s standing reason for
/// refusing §7.2's probe branch, and step 6 does not change that.
///
/// `None` is left constructible for the one case where it is the honest answer: a terminal reading
/// reached without the launch hook ever firing, i.e. a process whose existence is inferred rather
/// than observed. See [`journal_the_roots_outcome`].
///
/// **`harness_version` is `"unknown"` on a root, and that is a refusal to invent a side effect.**
/// A child's version is measured because `run_spawn` already runs `<program> --version` for
/// §6.7's `TaskContract.child.version`; a root has no contract, so nothing on this path has ever
/// executed the harness binary a second time. Doing it here to fill a journal field would add a
/// process execution to every `marion run` — an observable change, and one `launch_only_root`'s
/// fake-harness tests measure directly, since they assert over the argv their stub was invoked
/// with and a stub that never exits would turn the extra call into a hang. §4.3 puts the binary's
/// path and version in `meta.json` — declared and unwritten, and with no accessor to reach for
/// (`marion_core::paths`). When marion writes that file the value comes from there, measured once,
/// and this reads it rather than re-deriving it. Until then `"unknown"` is what marion knows.
fn spawned_record(node: &RootNode, pid: Option<i32>) -> RecordKind {
    RecordKind::Spawned(Spawned {
        agent_id: node.agent_id.clone(),
        harness_version: "unknown".into(),
        // The **compiled** invocation's model, for §6.7's reason: what went on the wire, not what
        // was asked for. `None` on codex, whose `exec` surface takes no model argument at all.
        model: node.invocation.model.clone(),
        pid,
        // **Derived from `pid` here rather than passed in, so the two cannot disagree.**
        //
        // Both call sites are covered by construction: `launch_inner`'s `on_started` hook passes a
        // real pid and gets an identity, and the no-process fallback passes `None` and gets `None`.
        // Deriving it at the one place the pid is turned into a record is what stops a future third
        // call site from recording a signal target with no identity beside it — which is exactly
        // what this path did until the criterion-3 measurement caught it.
        //
        // The read is only sound at `on_started`, while marion still holds the `Child`: a pid read
        // later may already belong to someone else, and a reaped pid stops resolving at all
        // (measured). That is where the hook fires, so that is where this runs.
        start_id: pid.and_then(|p| match crate::procid::read(p) {
            crate::procid::Read::Id(id) => Some(id),
            // The process was spawned moments ago and marion holds it, so neither should be
            // reachable — and a wrong identity would be far worse than a missing one, which
            // resolves to an honest `cannot-tell`.
            crate::procid::Read::NoSuchProcess | crate::procid::Read::Unavailable(_) => None,
        }),
    })
}

/// The `LaunchOnly` root: spawn with the prompt already in argv, read the stream, assert post hoc
/// that it reached marion's bridge (§6.1 step 8).
///
/// **The bounded run and the kill are [`run_bounded`]'s, not a second copy.** It already does every
/// part of what a root needs — `process_group(0)`, `stdin(Stdio::null())`, piped stdout/stderr with
/// bounded drains, and §9's two-step group kill whose ordering is load-bearing (enumerate the
/// descendants' distinct pgids *first*, because once the parent dies its descendants reparent to
/// pid 1 and no `ps` walk recovers them). A second implementation of that kill is exactly the drift
/// §9 warns about, so there is one.
///
/// stdin is `null` rather than a pty for two independent reasons: §5.2 says marion **MUST NOT**
/// give a headless node a pty on stdin, and on this surface there is nothing to write to it.
fn launch_only(
    node: &RootNode,
    bound: StdDuration,
    mut events: Option<&mut crate::events::EventSink>,
    on_started: &dyn Fn(i32),
    session: &crate::session_watch::SessionWatch<'_>,
) -> Result<RootOutcome, RootError> {
    let inv = &node.invocation;
    let mut cmd = SysCommand::new(&inv.program);
    cmd.args(&inv.args)
        .envs(inv.env.iter().cloned())
        .current_dir(&inv.cwd);
    if node.auth == Auth::Canned {
        // Codex's generated `config.toml` names this as its provider `env_key`, and a provider
        // whose key is unset refuses to start. The per-run token rather than a constant, for
        // the same reason `ANTHROPIC_AUTH_TOKEN` carries it on the duplex path: it attributes a
        // request log to one run. It is not a credential — the endpoint is the canned server.
        //
        // Under `Inherited` there is no canned endpoint to name a key for, and pushing one would
        // put a placeholder credential beside the operator's real login.
        cmd.env("MARION_DUMMY_KEY", &node.token);
    }
    // The one live seam this path has, and the session watch is its one reader: the first frame
    // names the session, and a root lost mid-run never reaches the capture below.
    let on_line = |line: &str| session.observe_line(line);
    let out = run_bounded_watched(&mut cmd, bound, on_started, Some(&on_line))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // Recorded **here**, because this is the last place the raw stdout exists: `RootOutcome`'s
    // `transcript` is `json_frames(&stdout)`, which keeps only the parseable lines. S12 measured
    // gemini interleaving `[STARTUP] Phase 1` and `Warning: Basic terminal detected` on stdout, and
    // a recording built from `transcript` would drop exactly those — rendering a root that printed
    // a stack trace as an unexplained silence, which is the failure `duplex::StreamEvent` has two
    // variants to prevent.
    if let Some(es) = events.as_mut() {
        es.record_capture(&stdout);
    }
    let adapter = adapter_for(node.harness)?;
    let exit = ChildExit {
        code: out.code,
        signal: None,
        timed_out: out.timed_out,
    };
    let outcome = RootOutcome {
        exit_code: out.code,
        transcript: json_frames(&stdout),
        marion_calls: adapter.marion_calls(&stdout),
        // The same reading `spawn::build_contract` gives a child's stream, asked of the root's. A
        // root has no `TaskContract` to record it in, so its only destination is the refusal below.
        failure: adapter.parse_stream(&stdout, exit).failure,
        stderr,
        denied_permissions: vec![],
        timed_out: out.timed_out,
    };
    // Evaluated **after** the transcript is assembled and before anything is returned, so the
    // refusal is the run's outcome rather than a warning printed beside a success.
    //
    // A root marion killed on its own bound also reached no bridge, trivially — but "it never got
    // marion's tools" is the wrong diagnosis for it, and the expiry is the one marion observed. So
    // the expiry wins the report, exactly as §6.7's status derivation lets `TimedOut` outrank every
    // other claim about the same run.
    if !outcome.timed_out {
        assert_a_verb_was_answered(node.harness, &outcome)?;
    }
    Ok(outcome)
}

/// The pane's geometry before anybody has attached.
///
/// **Somebody has to choose, and it cannot be the operator's terminal**: the master is sized before
/// the child exists, by a supervisor that does not know what will eventually attach — or whether
/// anything ever will. 80x24 is the size every harness's own fallback assumes when `TIOCGWINSZ`
/// answers nothing, so it is the one shape a TUI is certain to lay out for. `node/attach` reports
/// it back rather than letting the client render its own guess, and the first `NodeResize` replaces
/// it; the `r` record in `pty.cast` is what makes the change legible afterwards.
const PANE_SIZE: crate::pty::WinSize = crate::pty::WinSize { cols: 80, rows: 24 };

/// What `TERM` the pane's recording claims, and it is a claim about marion's emulator rather than
/// about the operator's.
///
/// The bytes on the pty are produced by a harness reading *this* value out of its environment, and
/// they are replayed by `marion_term`, whose capability set is what the name has to describe. It is
/// the same string the five committed captures were made under, so `pty.cast` stays comparable with
/// the corpus §5.3's claims are read off.
const PANE_TERM: &str = "xterm-256color";

/// Block until the node exits or `bound` expires, whichever comes first.
///
/// Whether the wait timed out. Split out of [`launch_terminal`] so its one fallible step can be
/// held as a value across the un-advertisement — see the call site. Exit observation deliberately
/// does not reap: shutdown must sweep the still-pinned process group before consuming the status.
pub(crate) fn wait_for_the_pane_to_end(
    host: &crate::pty::PtyHost,
    bound: Option<StdDuration>,
) -> std::io::Result<bool> {
    let deadline = bound.map(|bound| std::time::Instant::now() + bound);
    loop {
        if host.poll_exited_unreaped()? {
            return Ok(false);
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Ok(true);
        }
        std::thread::sleep(PANE_POLL);
    }
}

/// How often the launcher asks whether the node has exited. See [`launch_terminal`].
const PANE_POLL: StdDuration = StdDuration::from_millis(25);

/// Finish one advertised terminal without letting any fallible teardown step strand it live.
///
/// The closures keep the lifecycle decision in one place while [`launch_terminal`] retains the
/// concrete host operations: core shutdown success is independent of whether the drained replay
/// is safe to cache.
pub(crate) fn finish_terminal_pane<Shutdown, ReplayCharge>(
    pane: Option<&dyn PaneOwner>,
    agent_id: &AgentId,
    host: &std::sync::Arc<crate::pty::PtyHost>,
    waited: std::io::Result<bool>,
    shutdown: Shutdown,
    replay_charge: ReplayCharge,
) -> std::io::Result<(Option<std::process::ExitStatus>, bool)>
where
    Shutdown: FnOnce(bool) -> std::io::Result<Option<std::process::ExitStatus>>,
    ReplayCharge: FnOnce() -> std::io::Result<usize>,
{
    if let Some(owner) = pane {
        owner.closing(agent_id, host);
    }

    let timed_out = match waited {
        Ok(waited) => waited,
        Err(error) => {
            let _ = shutdown(false);
            tell_pane_owner_failed(pane, agent_id, host);
            return Err(error);
        }
    };

    let status = match shutdown(timed_out) {
        Ok(status) => status,
        Err(error) => {
            tell_pane_owner_failed(pane, agent_id, host);
            return Err(error);
        }
    };

    if let Some(owner) = pane {
        match replay_charge() {
            Ok(charged_bytes) => owner.completed(agent_id, host, charged_bytes),
            Err(_) => owner.failed(agent_id, host),
        }
    }

    Ok((status, timed_out))
}

/// The pane's terminal transition on a failed teardown, for an owner that has one. A caller with
/// nowhere to put a pty (`None`) has nobody to tell.
fn tell_pane_owner_failed(
    pane: Option<&dyn PaneOwner>,
    agent_id: &AgentId,
    host: &std::sync::Arc<crate::pty::PtyHost>,
) {
    if let Some(owner) = pane {
        owner.failed(agent_id, host);
    }
}

/// **§9's M3 criterion C1: the root, in a terminal marion owns.**
///
/// The whole of what makes this a *production* node rather than the pty module's test scaffolding
/// is the four things it does in order, and the order is the argument:
///
/// 1. **The master, then the recorder, then the child.** `PtyHost::start` opens `pty.cast` and
///    starts the reader thread *before* anything is spawned, so there is no window in which the
///    node is writing and nobody is reading — a pty whose buffer fills with nobody draining it
///    blocks the harness in `write(2)`, which looks exactly like a hung model.
/// 2. **`Spawned` with a real pid, through the fallible barrier.** `on_started` is `launch_inner`'s
///    closure: it appends the record, fsyncs, and only then tells the owner. If the append fails it
///    kills the tree and the run is refused as [`RootError::UnaccountableNode`] — because the one
///    thing marion may never leave behind is a live process no record can name (§11 item 30), and
///    on this path that process owns a terminal.
/// 3. **The pane is advertised only after the process exists.** Registering earlier would answer a
///    `node/attach` with a pane onto a pty no child is on: the client would attach successfully,
///    see nothing for ever, and have no way to tell that from a quiet node.
/// 4. **Un-advertised, then killed and reaped, then the master closes.** [`PtyHost::shutdown`] owns
///    the last two and states why: closing the master first hangs up the slave, the kernel SIGHUPs
///    the foreground group, and the record then reads *"the terminal hung up"* on every node, every
///    time, when the truth is that marion decided to stop.
///
/// # What `bound` means here, and why it is the wall clock
///
/// It is [`RootPath::LaunchOnly`]'s reading rather than the duplex path's. A pane has no typed
/// control plane, so there is no `Blocked` state for marion to budget: marion is not holding an
/// answer this node is waiting on, and never will be. The only thing marion can bound is elapsed
/// time. A node still running when it expires is killed and reported `timed_out`, which
/// `roots_exit` turns into [`ExitStatus::TimedOut`] — marion's own attributed kill, and not a
/// failure of the harness.
///
/// # What this deliberately does not do
///
/// **No [`assert_a_verb_was_answered`].** §6.1 step 8's post-hoc gate asks whether the node reached
/// marion's bridge, and it is read off a stream this node does not produce. A TUI takes no turn at
/// all until a human presses return, so a root launched in a pane and looked at for ten seconds has
/// legitimately called nothing — and refusing it would make "the operator did not type anything"
/// indistinguishable from "the bridge never came up". The evidence for a pane is the recording.
///
/// **No transcript and no `marion_calls`.** `RootOutcome`'s stream fields stay empty rather than
/// being filled from a scrape of the pty bytes: those bytes are a *rendering*, full of cursor
/// motion and repaints, and anything recovered from them would be a guess presented in the same
/// shape as a parsed frame.
fn launch_terminal(
    node: &RootNode,
    bound: StdDuration,
    on_started: &dyn Fn(i32),
    pane: Option<&dyn PaneOwner>,
) -> Result<RootOutcome, RootError> {
    use crate::pty::{PtyHost, PtyMaster, spawn_pty, stdin_plan};

    // **Unforgeable, and from the surfaces this run selected.** `spawn_pty` cannot be called
    // without it, and it exists only where `display == NativePty` — so the refusal for a harness
    // with no pane shape has already happened in `prepare_watched`, and this is the type-level
    // restatement rather than a second check that could disagree.
    let witness = node
        .surfaces
        .display_plane()
        .ok_or(RootError::UnsupportedRootSurface(node.harness))?;
    // **S11 MUST #2, derived rather than assumed.** `StdinPlan::TerminalSlave` is the right answer
    // here and writing the constant would still be wrong: `stdin_plan` is a total match on the
    // control axis, and a surface that reached this path with `Typed(_)` control must get a
    // **pipe** on fd 0, because S11 measured `claude -p` exiting 1 with *"Input must be provided…"*
    // on an `isatty(0)` stdin. Reading the answer off the axis is what keeps that impossible to get
    // wrong by editing one adapter.
    let stdin = stdin_plan(node.surfaces.control);

    let master = PtyMaster::open(PANE_SIZE)?;
    let host = std::sync::Arc::new(PtyHost::start(
        node.agent_id.clone(),
        master,
        &node.agent_dir.pty_cast(),
        PANE_SIZE,
        PANE_TERM,
        std::time::Instant::now(),
    )?);

    let inv = &node.invocation;
    let mut cmd = SysCommand::new(&inv.program);
    cmd.args(&inv.args)
        .envs(inv.env.iter().cloned())
        .current_dir(&inv.cwd)
        // The harness lays out for the terminal it thinks it is on, and the one it is on is
        // marion's. Pushed here rather than compiled into the `Invocation` because it is a fact
        // about the pty this launcher just opened, which no adapter can know.
        .env("TERM", PANE_TERM);
    if node.auth == Auth::Canned {
        // The same push `launch_only` makes and for the same reason: codex's generated
        // `config.toml` names this as its provider `env_key`, and a provider whose key is unset
        // refuses to start. Claude Code's pane compiles its own credential in `compile_pane`, so
        // this is here for the second harness to declare a pane rather than for the first.
        cmd.env("MARION_DUMMY_KEY", &node.token);
    }

    // The pid is announced by `spawn_pty` itself, through `on_started`, between `spawn()` and the
    // first byte — that is the only instant at which a durable record can name this process while
    // marion still holds the handle.
    host.adopt(spawn_pty(
        witness,
        &mut cmd,
        host.master(),
        stdin,
        Some(on_started),
    )?);
    // **After the process exists, never before.** See this function's doc, point 3.
    if let Some(owner) = pane {
        owner.opened(&node.agent_id, std::sync::Arc::clone(&host));
    }

    // **Not `?`, and that is the point of the binding.** From the `opened` above until the `closed`
    // below there is a registry entry pointing at this host, and an early return through `?` would
    // leave it there — answering a later `node/attach` with a pane onto a master that is about to
    // close, which is precisely what `forget_pane` exists to prevent. So the wait's failure is
    // *carried* past the un-advertisement rather than thrown through it.
    let waited = wait_for_the_pane_to_end(&host, Some(bound));

    // Stop admitting live attaches and control before teardown, but retain the same host while the
    // reader drains its final bytes and terminal End. The polling loop deliberately left even a
    // naturally exited leader waitable, so shutdown sweeps while its process identity is pinned.
    let (status, timed_out) = finish_terminal_pane(
        pane,
        &node.agent_id,
        &host,
        waited,
        |timed_out| host.shutdown_with_timeout_outcome(timed_out),
        || host.completed_replay_charge(),
    )?;
    Ok(RootOutcome {
        exit_code: status.and_then(|s| s.code()),
        // Empty, deliberately. See this function's doc.
        transcript: vec![],
        marion_calls: vec![],
        failure: None,
        // Nothing of the node's output belongs here: on a pty stdout *is* the pane, and copying the
        // rendering into a field a reader will print would reproduce a screenful of escape
        // sequences in an error message.
        stderr: String::new(),
        denied_permissions: vec![],
        timed_out,
    })
}

/// §6.1 step 8's post-hoc readiness assertion, as a decision over what the run produced.
///
/// A `LaunchOnly` node's MCP readiness is not observable before its turn — the prompt is already in
/// argv — so the only honest check is afterwards, over its stream. §6.1 is emphatic that a turn
/// taken without marion's tools **MUST NOT** be allowed to end as plain text: it terminates
/// `exit 0` with no diagnostic anywhere, which is indistinguishable from success. §12 records the
/// Claude Code version of exactly this bug — first request toolless, a session title, exit 0 in
/// 63 ms.
///
/// **The question this asks used to be "was a verb *called*", and that was the defect.** The old
/// reasoning here argued that a call the bridge refused still proves the node had marion's tools,
/// *"which is the only thing being asserted"* — and the second half of that sentence is what was
/// wrong. What §6.1 step 8 is protecting is not the node's possession of a tool list; it is the
/// operator's ability to tell a run that delegated from one that did not. A gemini root whose
/// `spawn` was refused by gemini's own schema validator satisfied the old gate, exited 0, and was
/// journalled `ExitStatus::Ok` having delegated nothing (`tasks/todo.md`, owed item 0) — the same
/// silent success the gate exists to prevent, one level in. It is also why the root-`report` defect
/// fixed in `7ff470e` stayed invisible for as long as it did: once marion started *refusing* a
/// root's `report` (and `36fbbee` widened that to an unreadable depth), every one of those refusals
/// still read here as evidence the run had worked. A gate that passes on its own refusals is not a
/// gate.
///
/// So the assertion is now: **at least one verb was answered.** Three shapes, three sentences:
///
/// * no marion call at all → [`RootError::BridgeNeverReached`], unchanged. The node never got the
///   tools, and the fix is in the launch;
/// * calls, none answered → [`RootError::NoVerbAnswered`]. The node had the tools and the bridge
///   turned it away, and the fix is in whatever refused it. Two different pieces of news, so two
///   errors — the same split `36fbbee` made between "you broke a rule" and "you were started
///   wrong";
/// * one answered call → `Ok`, whatever became of the rest. One is sufficient because the claim
///   being made is about *the bridge*, not about the run's quality.
///
/// It is deliberately *not* keyed on `spawn` specifically. Which verb the node reached for is the
/// node's business, and a check that demanded one would refuse a legitimate root that only listed
/// its children.
///
/// **What this still cannot see is a limit of the streams, and is written down rather than papered
/// over.** [`CallOutcome::Refused`] is populated from each harness's *structural* error signal; a
/// refusal marion's own bridge issued arrives as an MCP result with `isError: true`, and whether
/// codex and gemini re-surface that as a stream-level error is unmeasured — see
/// [`marion_harness::CallOutcome`] for exactly which fixtures record what. On opencode the shape is
/// recorded. Where a harness hides it, this gate will read a marion-side refusal as an answer, and
/// closing that needs a recording, not a cleverer predicate.
fn assert_a_verb_was_answered(harness: Harness, outcome: &RootOutcome) -> Result<(), RootError> {
    if outcome.marion_calls.iter().any(|c| c.outcome.is_answered()) {
        return Ok(());
    }
    // Pre-formatted once, so both variants below quote the run's own words the same way.
    let failure = match &outcome.failure {
        Some(f) => format!("; the root's stream reported: {f}"),
        None => String::new(),
    };
    let stderr = match outcome.stderr.trim() {
        "" => String::new(),
        s => format!("; stderr: {}", s.chars().take(512).collect::<String>()),
    };
    if outcome.marion_calls.is_empty() {
        return Err(RootError::BridgeNeverReached {
            harness,
            frames: outcome.transcript.len(),
            exit: outcome.exit_code,
            failure,
            stderr,
        });
    }
    Err(RootError::NoVerbAnswered {
        harness,
        calls: outcome.marion_calls.len(),
        detail: outcome
            .marion_calls
            .iter()
            .map(describe_call)
            .collect::<Vec<_>>()
            .join(", "),
        exit: outcome.exit_code,
        failure,
        stderr,
    })
}

/// One call, as a clause an operator can act on.
///
/// `Unknown` says what was observed — a call with no result frame — rather than guessing which way
/// it went. It is a different fix from a refusal (a run that stopped mid-call, against a rule that
/// turned the node away), and naming it "refused" would send someone looking for a rule that does
/// not exist.
fn describe_call(call: &MarionCall) -> String {
    match &call.outcome {
        CallOutcome::Answered => format!("{} was answered", call.verb),
        CallOutcome::Refused(why) => format!("{} was refused: {why}", call.verb),
        CallOutcome::Unknown => format!(
            "{} was called and its stream never showed a result",
            call.verb
        ),
    }
}

/// The duplex root: §6.1 step 8's gate, then the prompt — **driven by [`crate::duplex`], the one
/// implementation a root and a child share.**
///
/// What is left here is only what a root *is*: it has no `TaskContract`, so its result is its
/// stream and its exit (§9); and marion offers it no wall clock, so `wall_clock` is `None` and
/// `bound` is spent only on a `Blocked` episode.
fn launch_duplex(
    node: &RootNode,
    blocked_bound: StdDuration,
    mcp_ready_timeout: StdDuration,
    watcher: Option<duplex::StreamSink<'_>>,
    on_started: &dyn Fn(i32),
) -> Result<RootOutcome, RootError> {
    let ready_file = node
        .ready_file
        .clone()
        .ok_or(RootError::UnsupportedRootSurface(node.harness))?;
    let inv = &node.invocation;
    let out = duplex::run_duplex(
        SysCommand::new(&inv.program)
            .args(&inv.args)
            .envs(inv.env.iter().cloned())
            .current_dir(&inv.cwd),
        &DuplexSpec {
            ready_file: &ready_file,
            prompt: &node.prompt,
            init_id: format!("marion-init-{}", node.agent_id.0),
            mcp_ready_timeout,
            blocked_bound,
            // A root, by construction: this is `root::launch`.
            depth: ROOT_DEPTH,
            // §9: marion offers a root no wall-clock ceiling on this path, so there is none here.
            wall_clock: None,
            // A root's frames are the only ones with a human on the other end; a child's stream is
            // never streamed anywhere, see `duplex::DuplexSpec::sink`.
            sink: watcher,
            // **The root's half of §11 item 28, closed by step 6.** A child's `Spawned` goes out
            // at the instant its process exists and carries its pid (`run_spawn`); a root's now
            // does too. The old absence was justified by `marion run` owning the whole turn in one
            // blocking call, so that *"nothing outside this process could act on the pid if it had
            // it"* — and the supervisor owning the root is precisely something outside that can.
            // See `launch_inner` for the hook and `spawned_record` for the record.
            on_started: Some(on_started),
        },
    )
    .map_err(|e| root_error(e, mcp_ready_timeout))?;
    Ok(RootOutcome {
        exit_code: out.exit_code,
        transcript: out.transcript,
        stderr: out.stderr,
        denied_permissions: out.denied_permissions,
        // Gated *before* the turn on this path, so restating it post hoc would add nothing.
        marion_calls: vec![],
        // Same reason: the duplex driver never reaches the post-hoc assertion, so there is nowhere
        // for a stream failure claim to be read from here.
        failure: None,
        timed_out: out.timed_out,
    })
}

/// The shared driver's refusals, in the root's own words. The cause is the same event; the sentence
/// names the node it happened to, which is what a reader of `marion run`'s stderr needs.
fn root_error(e: DuplexError, mcp_ready_timeout: StdDuration) -> RootError {
    match e {
        DuplexError::Io(e) => RootError::Io(e),
        DuplexError::McpNeverReady(_, path) => RootError::McpNeverReady(mcp_ready_timeout, path),
        DuplexError::DiedBeforeInitialize => RootError::DiedBeforeInitialize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_core::agent_type::builtin;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    struct RecordingPaneOwner {
        host: usize,
        events: Mutex<Vec<&'static str>>,
    }

    impl RecordingPaneOwner {
        fn record(&self, host: &Arc<crate::pty::PtyHost>, event: &'static str) {
            assert_eq!(Arc::as_ptr(host) as usize, self.host);
            self.events
                .lock()
                .expect("pane lifecycle events")
                .push(event);
        }
    }

    impl PaneOwner for RecordingPaneOwner {
        fn opened(&self, _: &AgentId, _: Arc<crate::pty::PtyHost>) {
            panic!("the finish seam must not re-open a pane");
        }

        fn closing(&self, _: &AgentId, host: &Arc<crate::pty::PtyHost>) {
            self.record(host, "closing");
        }

        fn completed(&self, _: &AgentId, host: &Arc<crate::pty::PtyHost>, charge: usize) {
            assert!(
                charge > 0,
                "a completed replay has a nonzero retained charge"
            );
            self.record(host, "completed");
        }

        fn failed(&self, _: &AgentId, _: &Arc<crate::pty::PtyHost>) {
            panic!("an eligible pane must not be failed");
        }
    }

    struct FailingPaneOwner {
        host: usize,
        events: Mutex<Vec<&'static str>>,
    }

    impl FailingPaneOwner {
        fn record(&self, host: &Arc<crate::pty::PtyHost>, event: &'static str) {
            assert_eq!(Arc::as_ptr(host) as usize, self.host);
            self.events
                .lock()
                .expect("pane lifecycle events")
                .push(event);
        }
    }

    impl PaneOwner for FailingPaneOwner {
        fn opened(&self, _: &AgentId, _: Arc<crate::pty::PtyHost>) {
            panic!("the finish seam must not re-open a pane");
        }

        fn closing(&self, _: &AgentId, host: &Arc<crate::pty::PtyHost>) {
            self.record(host, "closing");
        }

        fn completed(&self, _: &AgentId, _: &Arc<crate::pty::PtyHost>, _: usize) {
            panic!("a failed or ineligible pane must never be completed");
        }

        fn failed(&self, _: &AgentId, host: &Arc<crate::pty::PtyHost>) {
            self.record(host, "failed");
        }
    }

    /// Root exit polling must observe without reaping: shutdown needs the waitable leader to pin
    /// its process-group identity while it sweeps a same-group slave holder. This fixture calls
    /// the production wait and finish helpers; a direct `PtyHost::shutdown` test cannot catch a
    /// consuming root poll.
    #[test]
    fn root_wait_keeps_the_leader_waitable_until_the_same_group_sweep() {
        use std::io::Read;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct KillHolderOnDrop {
            pid_file: PathBuf,
            armed: bool,
        }

        impl Drop for KillHolderOnDrop {
            fn drop(&mut self) {
                if !self.armed {
                    return;
                }
                let Ok(raw) = std::fs::read_to_string(&self.pid_file) else {
                    return;
                };
                let Ok(pid) = raw.parse::<i32>() else {
                    return;
                };
                if let Some(pid) = rustix::process::Pid::from_raw(pid)
                    .filter(|pid| *pid != rustix::process::Pid::INIT)
                {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                }
            }
        }

        fn alive(pid: i32) -> bool {
            let Some(pid) = rustix::process::Pid::from_raw(pid) else {
                return false;
            };
            match rustix::process::test_kill_process(pid) {
                Ok(()) => true,
                Err(rustix::io::Errno::SRCH) => false,
                Err(_) => true,
            }
        }

        let dir = marion_testsupport::scratch("root-wait-unreaped-pgid-holder");
        let holder_file = dir.join("holder.pid");
        let ready_file = dir.join("holder.ready");
        let id = AgentId("root-wait-unreaped-pgid-holder".into());
        let host = Arc::new(
            crate::pty::PtyHost::start(
                id.clone(),
                crate::pty::PtyMaster::open(PANE_SIZE).expect("open pty"),
                &dir.join("pty.cast"),
                PANE_SIZE,
                PANE_TERM,
                std::time::Instant::now(),
            )
            .expect("start pane host"),
        );
        let witness = marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("opaque execution owns a pty");
        let mut command = SysCommand::new("/bin/sh");
        command
            .arg("-c")
            .arg(
                "(trap '' HUP TERM; printf ready >\"$READY_FILE\"; exec sleep 30) & \
                 holder=$!; printf '%s' \"$holder\" >\"$HOLDER_FILE\"; \
                 while [ ! -s \"$READY_FILE\" ]; do :; done; \
                 printf 'leader-tail'; exit 37",
            )
            .env("HOLDER_FILE", &holder_file)
            .env("READY_FILE", &ready_file);
        host.adopt(
            crate::pty::spawn_pty(
                witness,
                &mut command,
                host.master(),
                crate::pty::StdinPlan::TerminalSlave,
                None,
            )
            .expect("spawn fast leader and same-group holder"),
        );
        let mut cleanup = KillHolderOnDrop {
            pid_file: holder_file.clone(),
            armed: true,
        };

        let marker_deadline = std::time::Instant::now() + StdDuration::from_secs(2);
        let holder_pid = loop {
            if let Ok(file) = std::fs::File::open(&holder_file) {
                let mut raw = String::new();
                file.take(64)
                    .read_to_string(&mut raw)
                    .expect("read holder pid");
                if let Ok(pid) = raw.parse::<i32>() {
                    break pid;
                }
            }
            assert!(
                std::time::Instant::now() < marker_deadline,
                "the same-group holder never published its pid"
            );
            std::thread::yield_now();
        };
        let leader_pid = host.child_pid().expect("the leader is adopted");
        let leader = rustix::process::Pid::from_raw(leader_pid).expect("positive leader pid");
        let saw_waitable_leader = Arc::new(AtomicBool::new(false));
        let saw_waitable_leader_in_hook = Arc::clone(&saw_waitable_leader);
        host.set_before_child_sweep_hook(Box::new(move || {
            let observed = rustix::process::waitid(
                rustix::process::WaitId::Pid(leader),
                rustix::process::WaitIdOptions::EXITED
                    | rustix::process::WaitIdOptions::NOHANG
                    | rustix::process::WaitIdOptions::NOWAIT,
            )
            .expect("root polling must leave the leader waitable until the sweep");
            assert!(
                observed.is_some(),
                "the sweep boundary must observe the exited leader as a waitable zombie"
            );
            saw_waitable_leader_in_hook.store(true, Ordering::SeqCst);
        }));

        let waited = wait_for_the_pane_to_end(&host, Some(StdDuration::from_secs(5)));
        let (status, timed_out) = finish_terminal_pane(
            None,
            &id,
            &host,
            waited,
            |timed_out| host.shutdown_with_timeout_outcome(timed_out),
            || unreachable!("an unadvertised test pane requests no replay charge"),
        )
        .expect("root wait and shutdown succeed");
        assert!(!timed_out);
        assert_eq!(status.and_then(|status| status.code()), Some(37));
        assert!(
            saw_waitable_leader.load(Ordering::SeqCst),
            "shutdown reached the pre-sweep waitability assertion"
        );
        assert!(
            matches!(
                rustix::process::waitid(
                    rustix::process::WaitId::Pid(leader),
                    rustix::process::WaitIdOptions::EXITED
                        | rustix::process::WaitIdOptions::NOHANG
                        | rustix::process::WaitIdOptions::NOWAIT,
                ),
                Err(rustix::io::Errno::CHILD)
            ),
            "the sole shutdown wait must consume the leader status"
        );
        let holder_deadline = std::time::Instant::now() + StdDuration::from_secs(2);
        while alive(holder_pid) && std::time::Instant::now() < holder_deadline {
            std::thread::yield_now();
        }
        assert!(
            !alive(holder_pid),
            "the same-group holder survived the sweep"
        );
        cleanup.armed = false;
    }

    /// Mutation: hard-code `timed_out: false` while sealing the authoritative PTY stream. The
    /// root outcome can still report the wait timeout correctly, so only recovering the durable
    /// terminal End proves that the recorded lifecycle agrees with the root result.
    #[test]
    fn bounded_terminal_timeout_is_recorded_in_the_recovered_session_end() {
        use std::os::unix::process::ExitStatusExt;

        let dir = marion_testsupport::scratch("root-pane-timeout-outcome");
        let cast = dir.join("pty.cast");
        let id = AgentId("root-pane-timeout-outcome".into());
        let host = Arc::new(
            crate::pty::PtyHost::start(
                id.clone(),
                crate::pty::PtyMaster::open(PANE_SIZE).expect("open pty"),
                &cast,
                PANE_SIZE,
                PANE_TERM,
                std::time::Instant::now(),
            )
            .expect("start pane host"),
        );
        let witness = marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("opaque execution owns a pty");
        let mut command = SysCommand::new("/bin/sleep");
        command.arg("30");
        host.adopt(
            crate::pty::spawn_pty(
                witness,
                &mut command,
                host.master(),
                crate::pty::StdinPlan::TerminalSlave,
                None,
            )
            .expect("spawn bounded pane"),
        );

        let waited = wait_for_the_pane_to_end(&host, Some(StdDuration::from_millis(25)));
        let (status, timed_out) = finish_terminal_pane(
            None,
            &id,
            &host,
            waited,
            |timed_out| host.shutdown_with_timeout_outcome(timed_out),
            || unreachable!("an unadvertised test pane requests no replay charge"),
        )
        .expect("timed-out pane is killed and reaped");
        let status = status.expect("the adopted child has an exact kill status");
        assert_eq!(status.code(), None);
        assert_eq!(status.signal(), Some(9));
        assert!(timed_out);

        let stream_path = crate::pty::stream::stream_path_for_cast(&cast).expect("stream path");
        let encoded = std::fs::read(stream_path).expect("sealed authoritative stream");
        let recovered = crate::pty::stream::recover_session_bytes(&encoded)
            .expect("the sealed stream recovers");
        let crate::pty::stream::RecordKind::End(outcome) = recovered
            .records
            .last()
            .expect("the session has a terminal record")
            .kind
        else {
            panic!("the final authoritative record is a typed End")
        };
        assert_eq!(outcome.exit_code, None);
        assert_eq!(outcome.signal, Some(9));
        assert!(outcome.timed_out);
    }

    #[test]
    fn terminal_pane_completes_only_after_shutdown_and_replay_proof() {
        let id = AgentId("root-pane-finish".into());
        // A private scratch directory, like every other pane test here, and not `temp_dir()`:
        // marion's own replay check refuses a stream whose parent is not private to the effective
        // user, and on Linux `temp_dir()` is `/tmp` at mode 1777. The refusal was correct; the
        // fixture was putting the cast somewhere the production code is right to distrust.
        let dir = marion_testsupport::scratch("root-pane-finish");
        let cast = dir.join("pty.cast");
        let host = Arc::new(
            crate::pty::PtyHost::start(
                id.clone(),
                crate::pty::PtyMaster::open(PANE_SIZE).expect("open pty"),
                &cast,
                PANE_SIZE,
                PANE_TERM,
                std::time::Instant::now(),
            )
            .expect("start pane host"),
        );
        let witness = marion_harness::ExecutionSurfaces::opaque()
            .display_plane()
            .expect("opaque execution owns a pty");
        let mut command = SysCommand::new("/bin/sh");
        command.arg("-c").arg("printf 'fast-tail'; exit 37");
        host.adopt(
            crate::pty::spawn_pty(
                witness,
                &mut command,
                host.master(),
                crate::pty::StdinPlan::TerminalSlave,
                None,
            )
            .expect("spawn fast pane"),
        );
        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        while !host
            .poll_exited_unreaped()
            .expect("non-consuming exit poll")
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the fast pane did not exit within the bounded fixture"
            );
            std::thread::yield_now();
        }
        let owner = RecordingPaneOwner {
            host: Arc::as_ptr(&host) as usize,
            events: Mutex::new(Vec::new()),
        };

        let (status, timed_out) = finish_terminal_pane(
            Some(&owner),
            &id,
            &host,
            Ok(false),
            |timed_out| {
                owner
                    .events
                    .lock()
                    .expect("pane lifecycle events")
                    .push("shutdown");
                host.shutdown_with_timeout_outcome(timed_out)
            },
            || {
                owner
                    .events
                    .lock()
                    .expect("pane lifecycle events")
                    .push("replay-proof");
                host.completed_replay_charge()
            },
        )
        .expect("eligible pane finish");

        assert_eq!(status.and_then(|status| status.code()), Some(37));
        assert!(!timed_out);
        assert_eq!(
            *owner.events.lock().expect("pane lifecycle events"),
            ["closing", "shutdown", "replay-proof", "completed"]
        );

        let lines = std::fs::read_to_string(&cast).expect("read cast after shutdown");
        let exit = lines.lines().last().expect("cast has an exit record");
        let exit: Value = serde_json::from_str(exit).expect("exit record is JSON");
        assert_eq!(exit[1], "x", "the completed callback follows the cast exit");
        assert_eq!(exit[2], "37", "the cast preserves the exact process status");

        let conn = crate::serve::ConnId(9_001);
        let (out, rx) = crate::serve::capture(conn);
        let descriptor = host
            .begin_pane_replay(conn, out)
            .expect("eligible completed replay");
        host.pane_ready(conn, &descriptor.token, descriptor.cut);
        let frames = rx
            .try_iter()
            .map(|line| {
                marion_core::proto::Frame::from_line(
                    std::str::from_utf8(&line).expect("outbound replay is UTF-8"),
                )
                .expect("outbound replay frame")
            })
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        let mut ends = 0;
        for frame in frames {
            let marion_core::proto::Frame::Notification(note) = frame else {
                panic!("pane replay emitted a non-notification")
            };
            let marion_core::proto::Event::NodePaneFrame(frame) = note.event else {
                panic!("pane replay emitted another event")
            };
            match frame.frame {
                marion_core::proto::PaneFrameKindV1::Output { bytes } => {
                    output.extend_from_slice(bytes.as_bytes());
                }
                marion_core::proto::PaneFrameKindV1::End {} => ends += 1,
                marion_core::proto::PaneFrameKindV1::Resize { .. } => {}
            }
        }
        assert_eq!(output, b"fast-tail");
        assert_eq!(ends, 1, "the late replay has one terminal End");
        drop(host);
        let _ = std::fs::remove_file(cast);
    }

    #[test]
    fn terminal_pane_wait_shutdown_and_replay_failures_are_permanently_failed() {
        use std::os::unix::process::ExitStatusExt;

        let id = AgentId("root-pane-failures".into());
        let cast = std::env::temp_dir().join(format!(
            "marion-root-pane-failures-{}-{}.cast",
            std::process::id(),
            crate::run::unix_millis()
        ));
        let host = Arc::new(
            crate::pty::PtyHost::start(
                id.clone(),
                crate::pty::PtyMaster::open(PANE_SIZE).expect("open pty"),
                &cast,
                PANE_SIZE,
                PANE_TERM,
                std::time::Instant::now(),
            )
            .expect("start pane host"),
        );
        let owner = FailingPaneOwner {
            host: Arc::as_ptr(&host) as usize,
            events: Mutex::new(Vec::new()),
        };

        let error = finish_terminal_pane(
            Some(&owner),
            &id,
            &host,
            Err(std::io::Error::other("wait failed")),
            |_| {
                owner
                    .events
                    .lock()
                    .expect("pane lifecycle events")
                    .push("shutdown");
                Ok(None)
            },
            || -> std::io::Result<usize> { panic!("wait failure cannot publish replay") },
        )
        .expect_err("wait failure stays a root error");
        assert!(error.to_string().contains("wait failed"));
        assert_eq!(
            *owner.events.lock().expect("pane lifecycle events"),
            ["closing", "shutdown", "failed"]
        );

        owner.events.lock().expect("pane lifecycle events").clear();
        let error = finish_terminal_pane(
            Some(&owner),
            &id,
            &host,
            Ok(false),
            |_| {
                owner
                    .events
                    .lock()
                    .expect("pane lifecycle events")
                    .push("shutdown");
                Err(std::io::Error::other("shutdown failed"))
            },
            || -> std::io::Result<usize> { panic!("shutdown failure cannot publish replay") },
        )
        .expect_err("shutdown failure stays a root error");
        assert!(error.to_string().contains("shutdown failed"));
        assert_eq!(
            *owner.events.lock().expect("pane lifecycle events"),
            ["closing", "shutdown", "failed"]
        );

        owner.events.lock().expect("pane lifecycle events").clear();
        let (status, timed_out) = finish_terminal_pane(
            Some(&owner),
            &id,
            &host,
            Ok(true),
            |_| {
                owner
                    .events
                    .lock()
                    .expect("pane lifecycle events")
                    .push("shutdown");
                Ok(Some(std::process::ExitStatus::from_raw(37 << 8)))
            },
            || {
                owner
                    .events
                    .lock()
                    .expect("pane lifecycle events")
                    .push("replay-proof");
                Err(std::io::Error::other("replay ineligible"))
            },
        )
        .expect("replay ineligibility must not change core shutdown success");
        assert_eq!(status.and_then(|status| status.code()), Some(37));
        assert!(timed_out);
        assert_eq!(
            *owner.events.lock().expect("pane lifecycle events"),
            ["closing", "shutdown", "replay-proof", "failed"]
        );

        host.shutdown().expect("shut down fixture host");
        drop(host);
        let _ = std::fs::remove_file(cast);
    }

    fn env() -> BridgeEnv {
        BridgeEnv {
            bridge: "/bin/marion-supervisor".into(),
            args: vec!["mcp".into()],
            repo: "/repo".into(),
            state: "/state".into(),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: Auth::Canned,
            agent_id: AgentId("019f-root".into()),
            agent_type: "claude".into(),
            depth: ROOT_DEPTH,
            node_token: None,
            ready_file: Some("/state/x/mcp-ready".into()),
        }
    }

    #[test]
    fn claude_gets_the_base_url_without_the_v1_it_appends_itself() {
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099/v1"),
            "http://127.0.0.1:8099",
            "leaving it on yields a request to /v1/v1/messages"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099/v1/"),
            "http://127.0.0.1:8099"
        );
        assert_eq!(
            anthropic_base_url("http://127.0.0.1:8099"),
            "http://127.0.0.1:8099"
        );
    }

    #[test]
    fn the_declaration_carries_the_root_agent_id_so_requester_is_not_a_placeholder() {
        let v = mcp_config_json(&env());
        assert_eq!(
            v["mcpServers"]["marion"]["env"][AGENT_ID_ENV], "019f-root",
            "§9: requester for a top-level spawn is the root's own AgentId"
        );
        assert_eq!(v["mcpServers"]["marion"]["args"][0], "mcp");
        assert_eq!(
            v["mcpServers"]["marion"]["env"]["MARION_BASE_URL"], "http://127.0.0.1:8099/v1",
            "the child's model_providers entry wants the /v1 form"
        );
    }

    #[test]
    fn the_declaration_names_the_readiness_marker_the_prompt_waits_on() {
        let v = mcp_config_json(&env());
        assert_eq!(
            v["mcpServers"]["marion"]["env"][READY_FILE_ENV],
            "/state/x/mcp-ready"
        );
    }

    #[test]
    fn the_root_allowlist_is_every_verb_an_m1_root_can_reach() {
        // Omitting a reachable verb denies a call that then blocks until the root's bound expires.
        assert_eq!(ROOT_VERBS.to_vec(), vec!["spawn", "status", "wait", "list"]);
        assert!(
            !ROOT_VERBS.contains(&"report"),
            "report is rejected on a node without a contract, and a root has none"
        );
        // And on Claude Code the adapter spells them to exactly the four strings the constant used
        // to carry, so a claude root's `--allowedTools` is byte-identical to before the rename.
        let claude = adapter_for(Harness::ClaudeCode).unwrap();
        assert_eq!(
            ROOT_VERBS
                .iter()
                .map(|v| claude.marion_tool_name(v))
                .collect::<Vec<_>>(),
            vec![
                "mcp__marion__spawn",
                "mcp__marion__status",
                "mcp__marion__wait",
                "mcp__marion__list"
            ]
        );
        // Whereas copilot, the other adapter that compiles this list, spells them its own way.
        assert_eq!(
            adapter_for(Harness::Copilot)
                .unwrap()
                .marion_tool_name(ROOT_VERBS[0]),
            "marion-spawn"
        );
    }

    /// The gate itself is `duplex::wait_for_ready`, tested there. What is asserted here is the
    /// **root's own sentence** for it: the symptom is a run that "succeeds" with plain text, so the
    /// message a `marion run` operator reads has to name the tool that was missing.
    #[test]
    fn a_ready_marker_that_never_appears_is_a_refusal_not_a_silent_first_turn() {
        let missing = std::env::temp_dir().join("marion-never");
        let e = root_error(
            crate::duplex::DuplexError::McpNeverReady(StdDuration::from_millis(30), missing),
            StdDuration::from_millis(30),
        );
        assert!(matches!(e, RootError::McpNeverReady(_, _)), "{e}");
        assert!(e.to_string().contains("without mcp__marion__spawn"));
    }

    /// A pty-only surface has no root launch path, and §5.2 forbids inventing one by handing a
    /// headless node a pty on stdin. `TerminalInput` is unreachable from today's four adapters, so
    /// this is asserted at the derivation rather than through one.
    #[test]
    fn a_terminal_input_surface_is_refused_rather_than_pushed_down_one_of_the_two_paths() {
        let e = RootError::UnsupportedRootSurface(Harness::Codex);
        assert!(e.to_string().contains("pty on stdin"));
    }

    fn root_spec(dir: &Path, agent_type: &str) -> RootSpec {
        RootSpec {
            agent_type: agent_type.into(),
            prompt: "delegate it".into(),
            native_launch: None,
            repo: dir.join("repo"),
            state: dir.join("state"),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            bridge: "/bin/marion-supervisor".into(),
            model: builtin(agent_type).unwrap().model.clone(),
            auth: Auth::Canned,
            no_change_record: false,
            pane: false,
            resume: None,
            // What `blocked_bound_secs(None, <type>)` resolves to for every type this fixture
            // drives — §3.1's default, stated rather than left to a `Default`.
            bound_secs: marion_core::agent_type::DEFAULT_TIMEOUT_SECS,
        }
    }

    /// A scratch dir whose `repo/` is a **real one-commit repository**, so a root prepared in it
    /// has a change record and can therefore be granted what its agent type declares.
    ///
    /// The fixture decision, stated: the tests that drive an `-impl` root `git init`, and the ones
    /// that drive an orchestrator root keep the bare directory [`temp`] gives them. Passing
    /// `--no-change-record` everywhere instead would have been one line, and would have left the
    /// whole root suite exercising the **ungranted** path while claiming to test the grant — this
    /// repository's documented failure mode, a check that passes by failing to look. Keeping the
    /// bare directories where no tool is declared is not laziness either: it is now the evidence
    /// that the gate is co-extensive with the grant and does not fire where nothing is at stake.
    fn temp_repo(name: &str) -> (marion_testsupport::Scratch, PathBuf) {
        let dir = marion_testsupport::scratch(&format!("root-{name}"));
        let repo = marion_testsupport::fixture_repo(&dir);
        (dir, repo)
    }

    /// **A root gets the grant its agent type declares — and only over a recorded repository.**
    ///
    /// This replaces the invariant that a root's availability axis is empty by construction. The
    /// operator overruled its containment half; [`availability_axis`] carries the whole argument
    /// and what took its place. What is asserted here is the *positive* half of that trade, which
    /// nothing asserted before: `marion run claude-impl` in a real repository compiles
    /// `--tools Read,Write` **and** `--allowedTools …,Read,Write`, in claude's own spelling.
    ///
    /// **Both flags, or the grant is item 22's dead end rather than a tool.** §11 item 24 measured
    /// that: availability alone routes the call to `--permission-prompt-tool stdio`, where marion
    /// has no answerer, and the node receives a denial string instead of the file. `Read` and
    /// `Write` are the harness's spellings and not marion's — `tests/fixtures/s14/README.md`
    /// measured `--tools read`, marion's own word, producing `body.tools []` with exit 0 and an
    /// empty stderr.
    ///
    /// The orchestrator type is driven through the same path and must still compile `--tools ""`
    /// with the flag present: the empty string is documented as *"disable all tools"*, so a dropped
    /// flag would be a silently different grant that a substring search for `Write` would pass.
    #[test]
    fn a_root_over_a_recorded_repository_compiles_the_grant_its_agent_type_declares() {
        assert_eq!(
            builtin("claude-impl").unwrap().tools,
            vec!["read".to_string(), "write".to_string()],
            "this test is vacuous unless the type really does declare a grant"
        );
        let (dir, repo) = temp_repo("root-tools");
        let axis = |agent_type: &str| -> (String, String) {
            let node = prepare(&RootSpec {
                repo: repo.clone(),
                state: dir.join("state"),
                ..root_spec(&dir, agent_type)
            })
            .expect("the root compiles");
            let args = node.invocation.args.clone();
            let after = |flag: &str| {
                let i = args
                    .iter()
                    .position(|a| a == flag)
                    .unwrap_or_else(|| panic!("{agent_type}: {flag} is always compiled"));
                args[i + 1].clone()
            };
            (after("--tools"), after("--allowedTools"))
        };

        let (tools, allowed) = axis("claude-impl");
        assert_eq!(
            tools, "Read,Write",
            "availability, in claude's own spelling"
        );
        assert_eq!(
            allowed,
            "mcp__marion__spawn,mcp__marion__status,mcp__marion__wait,mcp__marion__list,Read,Write",
            "permission must carry the same grant beside marion's own verbs, or the tool exists \
             and every call to it is refused (§11 items 22 and 24)"
        );
        assert!(
            !node_args_mention_a_denylist(&repo, &dir),
            "marion must never compile --disallowedTools (§3.1)"
        );

        let (tools, allowed) = axis("claude");
        assert_eq!(
            tools, "",
            "an orchestrator type declares nothing and must still get the flag, empty"
        );
        assert!(
            allowed.starts_with("mcp__marion__spawn") && !allowed.contains("Write"),
            "marion's own verbs are the root's permission axis and no agent type widens them: \
             {allowed}"
        );
    }

    /// **A root's `SpawnIntent` records the bound the run is actually held to.**
    ///
    /// `prepare` is the only writer of a root's intent, and until it recorded this the journal said
    /// nothing at all about the node's clock: every reader — `marion tree`'s detail pane first
    /// among them — re-resolved §3.1's bound from the *agent type* and printed 900 s for a root the
    /// operator had launched with `--timeout 300`. The value is [`RootSpec::bound_secs`] rather
    /// than a second resolution here, so the number the launch enforces and the number the journal
    /// reports are one value.
    #[test]
    fn a_roots_intent_records_the_bound_its_launch_was_resolved_to() {
        let (dir, repo) = temp_repo("root-bound");
        let node = prepare(&RootSpec {
            repo: repo.clone(),
            state: dir.join("state"),
            bound_secs: 300,
            ..root_spec(&dir, "claude")
        })
        .expect("the root compiles");

        let journalled = std::fs::read_to_string(node.project.journal()).expect("a journal");
        let intents: Vec<u64> = journalled
            .lines()
            .filter_map(|l| marion_core::journal::decode(l.as_bytes()))
            .filter_map(|r| match r.kind {
                marion_core::journal::RecordKind::SpawnIntent(i) if i.agent_id == node.agent_id => {
                    Some(i.timeout_secs.expect("the bound is on the record"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            intents,
            vec![300],
            "one intent, carrying the operator's `--timeout 300` — not §3.1's default:\n\
             {journalled}"
        );
        assert_ne!(
            300,
            marion_core::agent_type::DEFAULT_TIMEOUT_SECS,
            "the assertion above is not passing by coincidence with the default"
        );
    }

    /// `--disallowedTools` must never appear on a root's command line, whatever it was granted.
    ///
    /// §3.1 is absolute about it: enumerating the complement of the built-in set silently escalates
    /// privilege the first time the harness adds a tool. Asserted over a **granted** root, since
    /// that is the only configuration in which anyone would be tempted to write one.
    fn node_args_mention_a_denylist(repo: &Path, dir: &Path) -> bool {
        let node = prepare(&RootSpec {
            repo: repo.to_path_buf(),
            state: dir.join("state"),
            ..root_spec(dir, "claude-impl")
        })
        .expect("the root compiles");
        node.invocation
            .args
            .iter()
            .any(|a| a.contains("disallowed") || a.contains("Disallowed"))
    }

    /// **NC-5 — the grant gate, in both directions, over roots `prepare` really built.**
    ///
    /// The rule is *no audit, no grant*, and it is asserted as an implication rather than as a spot
    /// check: whenever a root's `--tools` is non-empty, its `RootNode` carries a base point. The
    /// only source of that axis is [`availability_axis`], and exactly one of its arms can return a
    /// non-empty list, so this cannot be defeated by an edit that forgets a check — only by one
    /// that deletes an arm.
    ///
    /// Four roots, because the interesting cases are the corners:
    ///
    /// 1. `claude-impl` in a bare directory — **refused by name**, and the message names the
    ///    directory and the declaration. This is the case the whole gate exists for.
    /// 2. `claude` in the same bare directory — **runs**, with an empty axis. The gate is
    ///    co-extensive with the grant, so a root that asked for nothing meets no gate; the
    ///    behaviour every root fixture in this repository has always had is unchanged.
    /// 3. `claude-impl` in a git fixture — `Taken`, and granted.
    /// 4. `claude-impl` with `--no-change-record` in the same git fixture — `NotAttempted`, and
    ///    **not** granted, which is what makes the flag an escape hatch rather than a way past the
    ///    gate.
    #[test]
    fn a_non_empty_availability_axis_is_reachable_only_from_a_recorded_base() {
        // The unrecorded arm, direct — a declaration marion cannot record is a refusal.
        let err = availability_axis(
            &["write".to_string()],
            &RootChangeBase::Unavailable {
                reason: "not a git worktree".into(),
            },
            Path::new("/nowhere"),
        )
        .expect_err("no audit, no grant");
        assert!(matches!(err, RootError::NoChangeRecord { .. }), "{err}");
        // ...and the same arm with nothing declared is not a refusal at all.
        assert!(
            availability_axis(
                &[],
                &RootChangeBase::Unavailable {
                    reason: "not a git worktree".into()
                },
                Path::new("/nowhere"),
            )
            .expect("a root that asked for nothing meets no gate")
            .is_empty()
        );

        // 1. Declared, unrecordable: refused, naming both halves.
        let outside = temp("gate-outside");
        let e = prepare(&root_spec(&outside, "claude-impl"))
            .expect_err("a grant with no record behind it must not be issued");
        let msg = e.to_string();
        assert!(matches!(e, RootError::NoChangeRecord { .. }), "{msg}");
        for needle in [
            &*outside.join("repo").display().to_string(),
            "read, write",
            "--no-change-record",
        ] {
            assert!(
                msg.contains(needle),
                "the refusal must name the directory, the declaration and the remedy; missing \
                 `{needle}` in: {msg}"
            );
        }

        // 2. Nothing declared, same directory: unchanged, and that is the co-extensive half.
        let a = prepare(&root_spec(&outside, "claude"))
            .expect("a root that declares no tool still runs outside a repository");
        match &a.change_base {
            RootChangeBase::Unavailable { reason } => assert!(
                reason.contains("git"),
                "the absence must name its cause: {reason}"
            ),
            other => panic!("a bare directory is not a worktree: {other:?}"),
        }

        // 3. Declared and recorded: granted.
        let (inside, repo) = temp_repo("gate-inside");
        let b = prepare(&RootSpec {
            repo: repo.clone(),
            state: inside.join("state"),
            ..root_spec(&inside, "claude-impl")
        })
        .expect("a git worktree is snapshottable");
        assert!(
            matches!(b.change_base, RootChangeBase::Taken { .. }),
            "a real repository must yield a base point, or every record below is vacuous"
        );

        // 4. Declared, recordable, and declined: not granted, and not refused either.
        let c = prepare(&RootSpec {
            repo,
            state: inside.join("state"),
            no_change_record: true,
            ..root_spec(&inside, "claude-impl")
        })
        .expect("declining the record is not an error, it is a decision");
        assert!(
            matches!(c.change_base, RootChangeBase::NotAttempted { .. }),
            "the flag must record a decision not to look, never a git failure that never happened"
        );

        // The implication itself, over every root this test built.
        for (label, node) in [("orchestrator", &a), ("granted", &b), ("declined", &c)] {
            let args = &node.invocation.args;
            let i = args.iter().position(|x| x == "--tools").expect("compiled");
            let granted = !args[i + 1].is_empty();
            assert_eq!(
                granted,
                matches!(node.change_base, RootChangeBase::Taken { .. }),
                "{label}: a root was granted `{}` with no change record behind it — that is \
                 `8a69f22`'s ambiguity taken on purpose (see `availability_axis`)",
                args[i + 1]
            );
        }
    }

    /// **The grant is durable before the process is, and the base point with it.**
    ///
    /// `prepare` decides the grant and takes the pre-tree; `journal_the_roots_outcome` writes the
    /// change record only once `launch_watched` **returns**. Between the two there is a whole run,
    /// and a marion that panics, is SIGKILLed, or loses power inside it used to leave the journal
    /// saying nothing at all — not that a grant was issued, not what the tree looked like when it
    /// was. The evidence is recoverable, too: the pre-tree object is in the agent dir's own object
    /// store, so an oid in the journal is a handle on the real tree and not a bare number.
    ///
    /// This test is the crash: `prepare` returns and nothing is ever launched. What must already
    /// be on disk at that instant is the grant and the base.
    #[test]
    fn a_prepared_root_journals_its_grant_and_its_base_before_any_process_exists() {
        let (dir, repo) = temp_repo("grant-before-launch");
        let node = prepare(&RootSpec {
            repo,
            state: dir.join("state"),
            ..root_spec(&dir, "claude-impl")
        })
        .expect("a git worktree is snapshottable, so the grant is issued");
        let RootChangeBase::Taken { pre_tree, .. } = &node.change_base else {
            panic!(
                "this test is vacuous unless a base was taken: {:?}",
                node.change_base
            );
        };
        let journal = String::from_utf8_lossy(
            &std::fs::read(node.project.journal()).expect("prepare journals before it returns"),
        )
        .into_owned();
        assert!(
            journal.contains(&pre_tree.0),
            "the tree the grant was issued against is not in the journal, so a crash before \
             `launch_watched` returns leaves no evidence of any kind — not even that a grant was \
             issued. §6.1's intent-then-confirm split says the intent is written before the act; \
             a grant is an act.\n{journal}"
        );
        assert!(
            journal.contains("read") && journal.contains("write"),
            "…and what was granted must be in it too: `read` and a `write` on the operator's own \
             checkout are different runs to audit.\n{journal}"
        );

        // The same claim structurally, through the reader an operator would actually use. A
        // substring search passes on a journal that merely mentions the oid somewhere; this
        // asserts replay folds it into the node, and that the pair of fields says *which* run this
        // was — granted, and no outcome recorded.
        let tree = marion_core::registry::replay(journal.as_bytes());
        let n = tree
            .get(&node.agent_id)
            .expect("the intent introduced the node");
        let grant = n.root_grant.as_ref().expect("the grant is journalled");
        assert_eq!(grant.pre_tree.as_ref(), Some(pre_tree));
        assert_eq!(grant.granted.as_str(), "read, write");
        assert!(
            n.granted_without_a_record(),
            "this is the crash: a grant on the operator's checkout with nothing yet saying what \
             came of it. A reader that could not see this would read the run as one that never \
             started."
        );
        assert!(
            !n.did_marion_look(),
            "and no measurement exists yet — the post-snapshot happens at exit, which never came"
        );
    }

    /// A root's scope is its type's **ceiling and nothing else**, because no parent authored a
    /// request. `["**"]` in a request slot would fabricate an author; `CeilingOnly` says there was
    /// never one to fabricate.
    #[test]
    fn a_roots_scope_is_a_ceiling_with_no_request_beside_it() {
        let dir = temp("scope");
        let node = prepare(&root_spec(&dir, "claude")).unwrap();
        match &node.scope {
            RootScope::CeilingOnly { ceiling } => assert_eq!(
                ceiling,
                &builtin("claude").unwrap().scope_ceiling,
                "the agent type's own ceiling, not a copy that could drift"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// **`prepare` accepts a fileless declaration, and only because the adapter named the route.**
    ///
    /// A live opencode root writes no configuration document at all — its declaration rides
    /// `OPENCODE_CONFIG_CONTENT`, because a file under an isolated `$XDG_CONFIG_HOME` would isolate
    /// away the login the run exists to use. The old check read an empty `config_files` as
    /// `NoMcpDeclaration` and refused it; the new one checks the route the adapter *stated*, so this
    /// passes while a genuinely bridgeless node still cannot.
    #[test]
    fn a_live_opencode_root_declares_its_bridge_without_writing_a_file() {
        let dir = temp("opencode-live");
        let node = prepare(&RootSpec {
            auth: Auth::Inherited,
            base_url: None,
            // The built-in default names marion's own generated provider block, which a live node
            // does not write — the adapter refuses it by name, so a real one is given here.
            model: Some("anthropic/claude-sonnet-4-5".into()),
            ..root_spec(&dir, "opencode")
        })
        .expect("a fileless MCP route is legal when the adapter says that is its route");
        assert_eq!(
            node.mcp_config, None,
            "no document: the declaration is in the environment"
        );
        let (_, content) = node
            .invocation
            .env
            .iter()
            .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
            .expect("and marion checked that it is actually there");
        let v: Value = serde_json::from_str(content).unwrap();
        assert_eq!(
            v["mcp"]["marion"]["environment"]["MARION_AUTH"],
            "inherited"
        );
        // Nothing marion could write escaped the agent dir, because marion wrote nothing.
        assert!(
            !node
                .invocation
                .env
                .iter()
                .any(|(k, _)| k == "HOME" || k.starts_with("XDG_"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other side of the same coin: the refusal still exists and still names the route. An
    /// adapter that declared *nothing* must not be waved through just because filelessness is now
    /// legal for somebody.
    #[test]
    fn a_node_with_no_declaration_on_any_route_is_still_refused_by_name() {
        for (route, needle) in [
            ("a configuration document", "configuration document"),
            ("$OPENCODE_CONFIG_CONTENT", "OPENCODE_CONFIG_CONTENT"),
            ("no route at all", "no route at all"),
        ] {
            let e = RootError::NoMcpDeclaration {
                harness: Harness::OpenCode,
                route: route.into(),
            };
            let s = e.to_string();
            assert!(s.contains(needle), "{s}");
            assert!(
                s.contains("no bridge at all"),
                "the refusal must name the failure class, not merely fail: {s}"
            );
        }
    }

    /// A scratch dir that removes itself, plus the `repo/` every root fixture here expects.
    ///
    /// The guard is `marion-testsupport`'s, not a tenth copy: this file's own `temp` returned a
    /// bare `PathBuf` and cleaned up nowhere, so every run stranded a directory — 26 of them by the
    /// time it was caught, from a single test added late. That is the shape cb6ab2e removed from
    /// every other test module, and the reason the guard lives in a dev-dependency crate is
    /// precisely so `#[cfg(test)]` code in the lib can reach it.
    ///
    /// Bind the result for the whole test. `temp("x").join("y")` drops the guard at the end of that
    /// statement and deletes the directory out from under the test.
    fn temp(name: &str) -> marion_testsupport::Scratch {
        let dir = marion_testsupport::scratch(&format!("root-{name}"));
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        dir
    }

    /// **Every built-in prepares** — argv compiled, configuration on disk, prompt where its surface
    /// puts it. This is the whole of `marion run <agent-type>` short of the process itself.
    ///
    /// The opencode case is a regression that was real: its document lands at
    /// `<config_dir>/config/opencode/opencode.json`, a layout `$XDG_CONFIG_HOME` gives the harness
    /// and marion does not create, so writing it failed the launch with a bare
    /// `No such file or directory` naming neither the path nor the harness.
    ///
    /// Driven off `builtin_names()` rather than a list kept here, because a list kept here is a
    /// list a new built-in is silently absent from — which is exactly what happened when
    /// `acp-opencode` landed: it is the one type that is *not* a root, and a hardcoded sweep of the
    /// other five would have called that fact proven without ever asking. Its refusal is asserted
    /// by name below, so "ACP is not a root harness" is a measured claim rather than an omission.
    #[test]
    fn every_builtin_agent_type_prepares_as_a_root_with_its_config_actually_on_disk() {
        let dir = temp("prepare");
        // A **git** repository, not the bare directory `temp` makes. Driving the sweep off
        // `builtin_names()` surfaced `claude-impl` and `gemini-impl`, which declare tools — and
        // §6.1's rule is "no audit, no grant", so `prepare` refuses a tool-declaring root outright
        // where no change record can be taken. That refusal is correct; the fixture was wrong, and
        // a bare directory silently tested only the types that ask for nothing.
        marion_testsupport::fixture_repo(&dir);
        let mut refused = 0;
        for name in marion_core::agent_type::builtin_names() {
            if builtin(name).unwrap().harness == Harness::Acp {
                let e = prepare(&root_spec(&dir, name))
                    .expect_err("`acp` runs as a child, not as a root");
                assert!(
                    matches!(e, RootError::AcpIsNotARootHarness(_)),
                    "{name}: expected the named refusal, got {e}"
                );
                // Nothing was created before the refusal: a root that will not run must not leave a
                // config document, a worktree or a state directory behind it.
                assert!(
                    !dir.join("state").join("agents").exists(),
                    "{name}: the refusal came after something was written"
                );
                refused += 1;
                continue;
            }
            let node = prepare(&root_spec(&dir, name))
                .unwrap_or_else(|e| panic!("{name} cannot be a root: {e}"));
            // Canned: every harness declares marion's bridge here — in a document, or, on goose,
            // as the one `--with-extension marion:…` argv token its row routes through
            // (`McpRoute::Argv`), which `verify` has already checked the argv for.
            match node.mcp_config.as_ref() {
                Some(declaration) => assert!(
                    declaration.is_file(),
                    "{name}: the MCP declaration marion compiled a path to must exist: {}",
                    declaration.display()
                ),
                None => assert!(
                    node.invocation
                        .args
                        .iter()
                        .any(|a| a.starts_with("marion:")),
                    "{name}: no MCP declaration document and none on argv: {:?}",
                    node.invocation.args
                ),
            }
            let in_argv = node.invocation.args.iter().any(|a| a == "delegate it");
            match node.path {
                RootPath::LaunchOnly => {
                    assert!(
                        in_argv,
                        "{name}: a LaunchOnly prompt rides argv (§6.1 step 8)"
                    );
                    assert!(node.ready_file.is_none(), "{name}: nothing to gate");
                }
                RootPath::Duplex => {
                    assert!(
                        !in_argv,
                        "{name}: a duplex prompt is a frame, not an argument"
                    );
                    assert!(node.ready_file.is_some(), "{name}: the §6.1 step 8 marker");
                }
                // Unreachable from a built-in agent type: `prepare_watched` derives the path from
                // `adapter.surfaces()`, and no adapter's *default* shape is `TerminalInput` — the
                // pane shape is `HarnessAdapter::pane_surfaces`, asked for per run. Named rather
                // than wildcarded so a fifth harness that declares a pane by default fails here.
                RootPath::Terminal => panic!(
                    "{name}: a built-in agent type reached the pane path without a run asking \
                     for one"
                ),
                // `prepare` refuses this path before a node exists, so a node holding it means the
                // refusal was removed and the branch above was skipped.
                RootPath::Acp => panic!("{name}: an ACP type prepared as a root"),
            }
            assert_eq!(node.prompt, "delegate it", "{name}");
        }
        assert!(
            refused > 0,
            "no built-in exercises the refusal, so this test asserts only the happy side"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A run that did not ask for a pane is prepared from the bytes it was prepared from before
    /// panes existed. That is M1's whole guarantee against this feature.**
    ///
    /// The pty module already asserts the *declaration* half — no adapter's `surfaces()` claims a
    /// display plane (`pty::tests::a_node_that_did_not_ask_for_a_pane_is_not_given_one`) — and that
    /// is not the half that can now break. `prepare_watched` reads `RootSpec::pane` and picks
    /// between two surfaces methods and two compile methods, and a mistake there gives a node a pty
    /// nobody asked for while every adapter test stays green. M1's measured path is
    /// `--print --output-format stream-json` over pipes; a pane is a TUI with the prompt in argv,
    /// and the two are not near-misses of each other.
    ///
    /// Asserted on both sides, so it cannot pass by answering "no pane" unconditionally.
    ///
    /// **Mutation:** in `prepare_watched`, ignore `spec.pane` — take `pane_surfaces()` whenever it
    /// is `Some`, or take `surfaces()` always. Either fails here.
    #[test]
    fn only_a_run_that_asked_for_a_pane_is_compiled_as_one() {
        use marion_harness::ControlTransport;

        let dir = temp("pane-selection");
        let headless = prepare(&root_spec(&dir, "claude")).expect("a claude root");
        let paned = prepare(&RootSpec {
            pane: true,
            ..root_spec(&dir, "claude")
        })
        .expect("a claude root with a pane");

        // The path, the surfaces and the stdin the surfaces imply — three derivations of the one
        // decision, and all three have to move together or `launch_terminal` gets a witness for a
        // node whose stdin is a pipe.
        assert_eq!(headless.path, RootPath::Duplex);
        assert!(
            headless.surfaces.display_plane().is_none(),
            "a run that asked for nothing was given a display plane, so `launch_terminal` is \
             reachable from M1's own path"
        );
        assert_eq!(
            crate::pty::stdin_plan(headless.surfaces.control),
            crate::pty::StdinPlan::Piped,
            "S11 measured `claude -p` exiting 1 with \"Input must be provided…\" on an isatty(0) \
             stdin"
        );

        assert_eq!(paned.path, RootPath::Terminal);
        assert!(
            paned.surfaces.display_plane().is_some(),
            "the pane shape must mint the witness `spawn_pty` requires, or C1 cannot launch"
        );
        assert_eq!(paned.surfaces.control, ControlTransport::TerminalInput);
        assert_eq!(
            crate::pty::stdin_plan(paned.surfaces.control),
            crate::pty::StdinPlan::TerminalSlave
        );

        // And the argv, which is what actually reaches the harness. The headless launch keeps
        // M1's two switches and does **not** carry the prompt (§6.1 step 8 writes it as a frame);
        // the pane carries neither switch and seeds the composer instead.
        let args = |n: &RootNode| n.invocation.args.join("\u{1}");
        assert!(
            headless.invocation.args.iter().any(|a| a == "-p"),
            "M1's measured launch lost `-p`: {:?}",
            headless.invocation.args
        );
        assert!(
            args(&headless).contains("stream-json"),
            "M1's measured launch lost its output format: {:?}",
            headless.invocation.args
        );
        assert!(
            !headless.invocation.args.iter().any(|a| a == "delegate it"),
            "a duplex prompt is a frame, not an argument: {:?}",
            headless.invocation.args
        );

        assert!(
            !paned.invocation.args.iter().any(|a| a == "-p"),
            "the pane compiled the headless launch: {:?}",
            paned.invocation.args
        );
        assert!(
            !args(&paned).contains("stream-json"),
            "the pane compiled a frame protocol onto a node marion drives by keystrokes: {:?}",
            paned.invocation.args
        );
        assert!(
            paned.invocation.args.iter().any(|a| a == "delegate it"),
            "the pane's prompt must be seeded into the composer: {:?}",
            paned.invocation.args
        );

        // **The same two-sided check on codex, because codex now has a pane too.** Its headless
        // shape is `codex exec --json`, which is M1's child launch, and a selection bug that gave
        // every codex node the TUI would take that path away without any adapter test noticing.
        let cx = prepare(&root_spec(&dir, "codex")).expect("a codex root");
        let cx_paned = prepare(&RootSpec {
            pane: true,
            ..root_spec(&dir, "codex")
        })
        .expect("a codex root with a pane");
        assert_eq!(cx.path, RootPath::LaunchOnly);
        assert_eq!(cx.invocation.args.first().map(String::as_str), Some("exec"));
        assert!(
            cx.invocation.args.iter().any(|a| a == "--json"),
            "M1's codex launch lost its JSONL stream: {:?}",
            cx.invocation.args
        );
        assert!(
            cx.surfaces.display_plane().is_none(),
            "a codex run that asked for nothing was given a pty"
        );
        assert_eq!(cx_paned.path, RootPath::Terminal);
        assert_ne!(
            cx_paned.invocation.args.first().map(String::as_str),
            Some("exec"),
            "the codex pane compiled the exec shape, whose flags the interactive command rejects: \
             {:?}",
            cx_paned.invocation.args
        );
        assert!(
            cx_paned.invocation.args.iter().any(|a| a == "delegate it"),
            "the codex pane dropped its prompt: {:?}",
            cx_paned.invocation.args
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A harness with no interactive shape refuses by name, and is never quietly launched
    /// headless.**
    ///
    /// A caller that asked for a pane is a caller that is about to run `marion attach`. Downgrading
    /// it would answer that attach with *"node has no display plane"* — a true sentence about a
    /// node marion made headless after being told not to, and the operator has no way to tell it
    /// from a harness that never supported one.
    ///
    /// Asserted over every built-in that declares no pane, and with a positive control, so it
    /// cannot pass by refusing everything.
    ///
    /// **Mutation:** in `prepare_watched`, fall back to `adapter.surfaces()` when `pane_surfaces()`
    /// is `None`. This fails.
    #[test]
    fn asking_for_a_pane_a_harness_does_not_have_is_refused_rather_than_downgraded() {
        let dir = temp("pane-refusal");
        let mut refused = 0;
        for name in ["claude", "codex", "codex-impl", "gemini", "opencode"] {
            let has_pane = adapter_for(builtin(name).unwrap().harness)
                .unwrap()
                .pane_surfaces()
                .is_some();
            let got = prepare(&RootSpec {
                pane: true,
                ..root_spec(&dir, name)
            });
            match (has_pane, got) {
                (true, Ok(node)) => assert_eq!(node.path, RootPath::Terminal, "{name}"),
                (true, Err(e)) => panic!("{name} declares a pane shape and would not prepare: {e}"),
                (false, Ok(node)) => panic!(
                    "{name} has no pane shape and was silently prepared as a {:?} node anyway. A \
                     `marion attach` against it would then be refused for having no display \
                     plane, which reads as marion never having supported one",
                    node.path
                ),
                (false, Err(e)) => {
                    refused += 1;
                    assert!(
                        e.to_string().contains("no pane shape"),
                        "{name}: the refusal must name the cause, not just fail: {e}"
                    );
                }
            }
        }
        assert!(refused > 0, "nothing was refused, so this asserted nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The root is depth 0, on every harness, in the document its own bridge will read.**
    ///
    /// The adapters are tested for carrying whatever `SpawnCtx` hands them; this is the other end
    /// of that wire — that `marion run` hands them the right thing. It is the one place a literal
    /// could be wrong while every adapter test still passed, and getting it wrong is not a cosmetic
    /// error: a root declared at any other depth would mis-measure its whole subtree, and one
    /// declared at `max_depth` could not delegate at all.
    ///
    /// Read off the bytes on disk rather than off `ctx`, because the bytes are what the bridge gets.
    #[test]
    fn every_root_declares_itself_at_depth_zero_and_names_its_own_type() {
        let dir = temp("depth");
        for name in ["claude", "codex", "codex-impl", "gemini", "opencode"] {
            let node = prepare(&root_spec(&dir, name)).unwrap();
            let doc = std::fs::read_to_string(node.mcp_config.as_ref().unwrap()).unwrap();
            assert!(
                doc.contains("\"0\""),
                "{name}: a root is depth 0 (§3.1), and the value must be in the document its own \
                 bridge reads:\n{doc}"
            );
            assert!(
                doc.contains(DEPTH_ENV),
                "{name}: {DEPTH_ENV} is missing:\n{doc}"
            );
            // The canonical name, so `marion run codex` and `marion run codex-impl` hand the bridge
            // one spelling and it re-resolves one definition.
            assert!(
                doc.contains(&format!("\"{}\"", builtin(name).unwrap().name)),
                "{name}: its canonical agent type name must reach the bridge, or §6.1 step 2's \
                 gates have no max_depth to read:\n{doc}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **§5.4's per-node capability reaches the root's own bridge — and only when somebody owns
    /// the node.**
    ///
    /// The token is minted by whoever owns the node's lifecycle, which since §11 item 28 step 6 is
    /// the supervisor, and [`prepare_watched`] writes it into exactly one place: the declaration
    /// this root's bridge reads. Asserted off the bytes the bridge actually gets, for the reason
    /// `every_root_declares_itself_at_depth_zero_and_names_its_own_type` gives — a `SpawnCtx` field
    /// that never reaches a file is a credential nothing can present.
    ///
    /// **The `Unwatched` half is not symmetry for its own sake.** `node_token: None` is the
    /// pre-step-6 record and it is still constructible, so a build that quietly went back to it
    /// would pass every other test in this file: a root would prepare, launch, run and exit
    /// normally, and the only symptom would be that its bridge could present no `SpawnCaller` and
    /// the socket's `agent/spawn` refused it by name. Both rows are here so that the difference
    /// between "nobody minted one" and "one was minted and dropped" is a test rather than a
    /// reading.
    #[test]
    fn a_watched_roots_declaration_carries_the_token_its_owner_minted() {
        struct Owner(String);
        impl crate::run::SpawnObserver for Owner {
            fn identified(&self, _: &AgentId) -> Option<String> {
                Some(self.0.clone())
            }
            fn started(&self, _: &AgentId, _: i32) {}
        }
        /// Everywhere a harness can be told something: the declaration document, the compiled argv
        /// (codex's `-c mcp_servers.marion.env.…` route), and the invocation's environment. Asked
        /// as one question because *which* of the three a harness uses is `McpRoute`'s business,
        /// and a test that picked one would silently stop measuring a harness that moved.
        fn declared_anywhere(node: &RootNode, needle: &str) -> bool {
            let doc = node
                .mcp_config
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            doc.contains(needle)
                || node.invocation.args.iter().any(|a| a.contains(needle))
                || node.invocation.env.iter().any(|(_, v)| v.contains(needle))
        }

        // Distinctive enough that it cannot collide with an id, a path or a model name.
        const TOKEN: &str = "root-node-token-8f1c-4a20-b7de";
        let dir = temp("node-token");
        for name in ["claude", "codex", "codex-impl", "gemini", "opencode"] {
            let owned = prepare_watched(&root_spec(&dir, name), &Owner(TOKEN.into()))
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                declared_anywhere(&owned, TOKEN),
                "{name}: the token its owner minted must reach the bridge, or this root cannot \
                 delegate at all (§5.4)"
            );
            let unowned = prepare(&root_spec(&dir, name)).unwrap();
            assert!(
                !declared_anywhere(&unowned, TOKEN),
                "{name}: `Unwatched` mints nothing, so nothing may appear — a token that showed \
                 up here would have come from somewhere other than the owner"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §9: the root's credential never leaks past the harness that reads it. Claude Code takes it
    /// as `ANTHROPIC_AUTH_TOKEN` pushed onto the invocation; the other three take it through the
    /// adapter, which puts it where that harness looks — so pushing the Anthropic pair onto *them*
    /// would present the root's token to a harness that ignores it and, worse, would be a second
    /// place the same decision is made.
    #[test]
    fn only_the_duplex_root_carries_the_anthropic_env_pair() {
        let dir = temp("token");
        for name in ["claude", "codex", "gemini", "opencode"] {
            let node = prepare(&root_spec(&dir, name)).unwrap();
            let has = |k: &str| node.invocation.env.iter().any(|(n, _)| n == k);
            assert_eq!(
                has("ANTHROPIC_AUTH_TOKEN"),
                node.path == RootPath::Duplex,
                "{name}"
            );
            assert_eq!(
                has("ANTHROPIC_API_KEY"),
                node.path == RootPath::Duplex,
                "{name}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The other end of `--live`, asserted on a root marion actually prepared.**
    ///
    /// The adapter withholds the three env vars it compiles; `prepare` must also skip the *post*-
    /// `compile` push of `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`, which is a second place the
    /// same decision is made and therefore the one that drifts. Blanking `ANTHROPIC_API_KEY` on a
    /// live node is the specific harm: it is set to `""` under `Canned` precisely so a real key
    /// cannot silently win, which is exactly the wrong thing to do to a node meant to use it.
    ///
    /// Only `claude` is swept: it is the only harness part 1 makes live, and the other three refuse
    /// under `Inherited` because their generated configs require a base URL there is none of.
    #[test]
    fn a_live_claude_root_pushes_no_anthropic_pair_and_keeps_its_fileless_isolation() {
        let dir = temp("live");
        let node = prepare(&RootSpec {
            base_url: None,
            auth: Auth::Inherited,
            ..root_spec(&dir, "claude")
        })
        .unwrap();
        for k in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CONFIG_DIR",
        ] {
            assert!(
                !node.invocation.env.iter().any(|(n, _)| n == k),
                "{k} must not be overlaid on a --live root: {:?}",
                node.invocation.env
            );
        }
        // §6.4's MUST is unchanged by live mode: the declaration is still marion's, inside the
        // node's own agent dir, and it is still what argv names.
        let declaration = node.mcp_config.as_ref().unwrap();
        assert!(declaration.is_file());
        assert!(declaration.starts_with(node.agent_dir.path()));
        assert!(
            node.invocation
                .args
                .iter()
                .any(|a| a == "--strict-mcp-config")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **§6.4's central MUST at the one place `--live` could break it on codex: marion must never
    /// mutate the operator's own harness config.**
    ///
    /// Unsetting `CODEX_HOME` is what makes the operator's `~/.codex/auth.json` visible — and it
    /// makes `~/.codex/config.toml` the *only* config codex will read. `prepare` writes whatever
    /// `config_files` returns, unconditionally and with `create_dir_all` on the parent, so an
    /// adapter that kept emitting a document here would have marion write over the operator's real
    /// codex config, and the first symptom would be a broken login on a harness marion was not even
    /// running. Asserted three ways: nothing was written, `CODEX_HOME` is absent by name, and the
    /// declaration is on argv where [`McpRoute::Argv`]'s verification above found it.
    #[test]
    fn a_live_codex_root_writes_no_file_and_so_cannot_touch_the_operators_own_codex_config() {
        let dir = temp("live-codex");
        let node = prepare(&RootSpec {
            base_url: None,
            auth: Auth::Inherited,
            ..root_spec(&dir, "codex")
        })
        .unwrap();
        assert!(
            node.mcp_config.is_none(),
            "the live route is argv, so there is no document to name: {:?}",
            node.mcp_config
        );
        assert!(
            std::fs::read_dir(node.agent_dir.config_dir())
                .unwrap()
                .next()
                .is_none(),
            "marion wrote into {} on a route whose only readable config.toml is ~/.codex/config.toml",
            node.agent_dir.config_dir().display()
        );
        for k in ["CODEX_HOME", "MARION_DUMMY_KEY"] {
            assert!(
                !node.invocation.env.iter().any(|(n, _)| n == k),
                "{k} must be absent, not blank: {:?}",
                node.invocation.env
            );
        }
        // The declaration `McpRoute::Argv` promised, actually present — including the two settings
        // whose absence is silent (§12): the approval mode and the plugin fetch.
        let joined = node.invocation.args.join(" ");
        for needle in [
            "-c mcp_servers.marion.command=",
            r#"-c mcp_servers.marion.default_tools_approval_mode="approve""#,
            "-c features.plugins=false",
            r#"-c mcp_servers.marion.env.MARION_DEPTH="0""#,
            r#"-c mcp_servers.marion.env.MARION_AUTH="inherited""#,
        ] {
            assert!(
                joined.contains(needle),
                "missing `{needle}` from:\n{joined}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The canned root is untouched by the axis existing — the pair is still pushed, and it is still
    /// only pushed on the duplex path.
    #[test]
    fn a_canned_root_still_carries_the_pair_exactly_where_it_always_did() {
        let dir = temp("canned-auth");
        let node = prepare(&root_spec(&dir, "claude")).unwrap();
        let get = |k: &str| {
            node.invocation
                .env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), Some(node.token.clone()));
        assert_eq!(get("ANTHROPIC_API_KEY"), Some(String::new()));
        assert_eq!(
            get("ANTHROPIC_BASE_URL"),
            Some("http://127.0.0.1:8099".into())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_agent_type_nobody_declared_is_refused_by_name_before_anything_is_written() {
        let dir = temp("unknown");
        let mut spec = root_spec(&dir, "claude");
        spec.agent_type = "not-a-harness".into();
        spec.model = None;
        assert!(matches!(
            prepare(&spec),
            Err(RootError::UnknownAgentType(t)) if t == "not-a-harness"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run whose marion calls all came back **answered** — the only shape that satisfies the gate,
    /// so every case below states its own departure from it rather than inheriting one.
    fn ran(calls: &[&str], frames: usize, exit: Option<i32>, stderr: &str) -> RootOutcome {
        outcome(
            &calls
                .iter()
                .map(|v| (*v, CallOutcome::Answered))
                .collect::<Vec<_>>(),
            frames,
            exit,
            stderr,
        )
    }

    fn outcome(
        calls: &[(&str, CallOutcome)],
        frames: usize,
        exit: Option<i32>,
        stderr: &str,
    ) -> RootOutcome {
        RootOutcome {
            exit_code: exit,
            transcript: vec![json!({}); frames],
            stderr: stderr.to_string(),
            marion_calls: calls
                .iter()
                .map(|(verb, outcome)| MarionCall {
                    verb: (*verb).to_string(),
                    outcome: outcome.clone(),
                })
                .collect(),
            ..RootOutcome::default()
        }
    }

    /// **§6.1 step 8's post-hoc assertion, and the failure it exists for.** A `LaunchOnly` root that
    /// never reached marion's bridge exited 0 having done nothing — the §12 shape — so it must be a
    /// refusal that *names the cause*, never a success.
    #[test]
    fn a_launch_only_root_that_never_reached_the_bridge_is_a_refusal_not_an_exit_zero() {
        let clean_looking = ran(&[], 3, Some(0), "");
        let err = assert_a_verb_was_answered(Harness::Codex, &clean_looking)
            .expect_err("a run with no marion call must not be reported as a success");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "it must name the harness: {msg}");
        assert!(
            msg.contains("never reached marion's bridge"),
            "it must name the cause, not just fail: {msg}"
        );
        assert!(
            msg.contains("Some(0)"),
            "and it must say that the exit code looked clean, which is the whole trap: {msg}"
        );
        assert!(matches!(
            err,
            RootError::BridgeNeverReached {
                harness: Harness::Codex,
                frames: 3,
                exit: Some(0),
                ..
            }
        ));
    }

    /// **The refusal must carry the harness's own words when the exit code has none.** Measured
    /// live: an opencode root against a retired model exits 1 with an **empty stderr** and the whole
    /// diagnosis — a 404 naming the model — in a single in-stream `error` frame. Before this, the
    /// refusal read `child exit Some(1)` and stopped, which is true and tells the operator nothing
    /// about whether the model is gone, the credential is dead, or marion mis-declared the bridge.
    /// `spawn::build_contract` has kept this beside a *child's* exit numbers since S13; a root has
    /// no `TaskContract`, so the refusal is the only place it can go.
    #[test]
    fn the_refusal_carries_the_streams_own_failure_when_stderr_is_empty() {
        let silent_but_failed = RootOutcome {
            failure: Some("APIError: model gemini-2.5-flash-lite is no longer available".into()),
            ..ran(&[], 1, Some(1), "")
        };
        let msg = assert_a_verb_was_answered(Harness::OpenCode, &silent_but_failed)
            .expect_err("no marion call is still a refusal")
            .to_string();
        assert!(
            msg.contains("no longer available"),
            "the stream's diagnosis is the only one there is here: {msg}"
        );
        assert!(
            msg.contains("the root's stream reported:"),
            "and it must be labelled as the harness's claim, not marion's: {msg}"
        );
        // The empty case adds no dangling label — the same rule `stderr` already follows.
        let quiet = assert_a_verb_was_answered(Harness::OpenCode, &ran(&[], 1, Some(1), ""))
            .expect_err("still a refusal")
            .to_string();
        assert!(
            !quiet.contains("stream reported"),
            "a stream that claimed nothing must not be quoted as claiming nothing: {quiet}"
        );
    }

    /// The other half, so the check above cannot pass by always failing: one **answered** call to
    /// any marion verb satisfies the gate, whatever the run then did with it.
    #[test]
    fn one_answered_marion_call_of_any_verb_satisfies_the_post_hoc_assertion() {
        for verb in ["spawn", "status", "wait", "list", "report"] {
            assert!(
                assert_a_verb_was_answered(Harness::Gemini, &ran(&[verb], 1, Some(0), "")).is_ok(),
                "{verb}: the question is whether a verb was answered, not which verb it was"
            );
        }
        // Even a run that then failed: an answered verb and a successful run are different facts,
        // and conflating them would relabel every genuine child failure as a launch failure.
        assert!(
            assert_a_verb_was_answered(Harness::OpenCode, &ran(&["spawn"], 2, Some(1), "it broke"))
                .is_ok()
        );
    }

    /// **The question the gate now asks, as a table** — because the change is which of these rows
    /// is a pass, and a table is where that is legible.
    ///
    /// Row 3 is the defect owed item 0 recorded: a root that reached the bridge, had its one verb
    /// refused, and exited 0. Row 5 is why `Unknown` is not folded into an answer: a stream that
    /// showed a call and never showed its result is a run that stopped mid-call, and reading it as
    /// success is the "passes because it failed to look" shape this repository keeps re-finding.
    #[test]
    fn the_gate_passes_only_on_an_answered_verb() {
        let refused = || CallOutcome::Refused("§5.4 rejects `report` on a root".into());
        // (label, the run's marion calls, does the gate pass)
        let cases = [
            ("no call at all", vec![], false),
            (
                "one answered call",
                vec![("spawn", CallOutcome::Answered)],
                true,
            ),
            ("one refused call", vec![("spawn", refused())], false),
            (
                "every call refused",
                vec![("report", refused()), ("spawn", refused())],
                false,
            ),
            (
                "a call with no result",
                vec![("spawn", CallOutcome::Unknown)],
                false,
            ),
            (
                "one answered among refusals — the bridge worked, so the run is not refused here",
                vec![
                    ("report", refused()),
                    ("spawn", CallOutcome::Answered),
                    ("status", CallOutcome::Unknown),
                ],
                true,
            ),
        ];
        for (label, calls, ok) in cases {
            assert_eq!(
                assert_a_verb_was_answered(Harness::Gemini, &outcome(&calls, 1, Some(0), ""))
                    .is_ok(),
                ok,
                "{label}"
            );
        }
    }

    /// **Two refusals, because they are two different pieces of news** — the split `36fbbee` made
    /// for the bridge's own refusals, applied here.
    ///
    /// A root that never reached the bridge was started wrong and the fix is in the launch. A root
    /// whose calls were all refused *had* marion's tools and something turned it away; telling that
    /// operator "the root never reached marion's bridge" sends them to re-check a configuration that
    /// is working.
    #[test]
    fn a_refused_call_is_not_reported_as_a_bridge_that_was_never_reached() {
        let refused = outcome(
            &[(
                "spawn",
                CallOutcome::Refused("invalid arguments for spawn".into()),
            )],
            2,
            Some(0),
            "",
        );
        let err = assert_a_verb_was_answered(Harness::Gemini, &refused)
            .expect_err("a root whose only verb was refused delegated nothing");
        let msg = err.to_string();
        assert!(matches!(
            err,
            RootError::NoVerbAnswered {
                harness: Harness::Gemini,
                calls: 1,
                exit: Some(0),
                ..
            }
        ));
        assert!(
            !msg.contains("never reached marion's bridge"),
            "it reached the bridge; blaming the launch sends the operator to the wrong fix: {msg}"
        );
        assert!(
            msg.contains("spawn was refused: invalid arguments for spawn"),
            "the refusal must carry the verb AND the refuser's own words: {msg}"
        );
        assert!(
            msg.contains("Some(0)"),
            "and the clean exit code, which is the whole trap: {msg}"
        );

        // The other spelling: a call whose result never arrived says so, rather than claiming a
        // refusal nobody issued.
        let quiet = assert_a_verb_was_answered(
            Harness::Codex,
            &outcome(&[("spawn", CallOutcome::Unknown)], 1, None, ""),
        )
        .expect_err("no answer is not an answer")
        .to_string();
        assert!(
            quiet.contains("never showed a result") && !quiet.contains("was refused"),
            "a stream that showed no result must not be reported as a rule violation: {quiet}"
        );
    }

    /// The refusal carries the child's own words when it had any — S13 measured an opencode failure
    /// arriving with an **empty** stderr, so the label must not be printed when there is nothing
    /// behind it.
    #[test]
    fn the_refusal_quotes_stderr_only_when_there_is_some() {
        let with = assert_a_verb_was_answered(
            Harness::OpenCode,
            &ran(&[], 0, None, "  no provider configured\n"),
        )
        .unwrap_err()
        .to_string();
        assert!(with.contains("stderr: no provider configured"), "{with}");
        let without = assert_a_verb_was_answered(Harness::OpenCode, &ran(&[], 0, None, "  \n"))
            .unwrap_err()
            .to_string();
        assert!(!without.contains("stderr:"), "{without}");
    }
}
