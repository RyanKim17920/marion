use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marion_core::agent_type;
use marion_core::contract::{AgentId, Isolation, ProcessExit, TaskId};
use marion_core::journal::{KillConfirmed, KillIntent, RecordKind, SpawnIntent};
use marion_core::node::{NodeState, ReapState};
use marion_core::proto::{FailureKind, NativeLaunchContext, RpcError, SpawnCaller};
use marion_core::registry::ReplayedNode;
use marion_core::secret::Secret;

use super::summary::Unprojectable;
use super::{
    DEFAULT_SPAWN_TIMEOUT_SECS, KillBy, NodeOutcome, RegistryHandle, StopReason,
    journal_failure_after_signal, journal_failure_before_signal, lock, steer, workflow,
};
use crate::native_binding::{NativeBindingError, refuse_untrusted_native_launch};
use crate::serve::Peer;

#[cfg(test)]
use super::panic_after_claim;

/// Failures made at the native-launch boundary.
///
/// A public versioned value has already passed protocol-version validation during deserialization.
/// This early gate owns only root/child pairing; root binding runs after peer authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NativeLaunchGateError {
    #[error("native launch context belongs only on a root request")]
    ChildMisuse,
}

impl NativeLaunchGateError {
    fn into_rpc(self) -> RpcError {
        match self {
            Self::ChildMisuse => RpcError::refused(
                "native_launch",
                "native launch context belongs only on a root: a spawn carrying `caller` is a \
                 managed child whose launch marion derives from its agent type and contract. \
                 Refused rather than dropping the native context or replacing the managed launch.",
                "§6.1, §9, §11 item 23",
            ),
        }
    }
}

fn native_binding_error_into_rpc(error: NativeBindingError) -> RpcError {
    match error {
        NativeBindingError::UnsupportedPlatform(error) => RpcError::unsupported(
            "native_launch",
            format!(
                "this native launch request requires byte-exact operating-system values, but \
                 {error}. Refused before any launch state was created."
            ),
            "§9",
        ),
        error => RpcError::refused(
            "native_launch",
            format!("{error}. Refused before any launch state was created."),
            "§9, §11 item 23",
        ),
    }
}

pub(super) fn native_launch_refusal(context: &NativeLaunchContext) -> RpcError {
    native_binding_error_into_rpc(refuse_untrusted_native_launch(context))
}

/// Refuse child/native pairing before token lookup or any launch state is consulted.
///
/// Root requests pass unchanged here; their opaque selector/program binding is deliberately later,
/// after [`root_spawn_authorized`] authenticates the socket peer.
pub(crate) fn validate_native_launch_boundary(
    caller: Option<&SpawnCaller>,
    native_launch: Option<&NativeLaunchContext>,
) -> Result<(), NativeLaunchGateError> {
    if native_launch.is_none() {
        return Ok(());
    }
    if caller.is_some() {
        return Err(NativeLaunchGateError::ChildMisuse);
    }

    Ok(())
}

/// **Run a node's body so that a panic in it is still an outcome.**
///
/// Neither node thread had this, and the hole it left is the worst shape §5.7 has: a panic after
/// `SpawnObserver::identified` has claimed the node, but before `mark_finished`, left
/// [`NodeHandle::outcome`] `None` **for ever**. `None` means *still running*, so
/// [`RegistryHandle::running_nodes`] counted the node, [`RegistryHandle::idle_exit_eligible`]
/// refused, [`RegistryHandle::join_finished_nodes`] never reaped the thread, and the supervisor
/// could not exit for the rest of its life. Not a lost result — a permanent phantom.
///
/// **A panicking node is `Exited`-with-a-reason, not a new disposition.** `SpawnError::Panicked`
/// already existed for exactly this, with a carefully written sentence, and was **constructed
/// nowhere** — a documented variant describing a mechanism that did not run. `handler.rs`'s own
/// comment at [`RegistryHandle::join_finished_nodes`] asserted that a panic *"already reached the
/// caller as `SpawnError::Panicked`"*, which was false. Making that true is better than deleting
/// the claim and much better than adding a third `NodeOutcome` arm: the two arms exist to keep a
/// root's and a child's *vocabularies* apart (§9 gives them different results), and a panic is a
/// failure **within** each vocabulary, not a third kind of node. A third arm would force every
/// reader — [`RegistryHandle::owned_failure`], [`NodeHandle::running`] — to grow a case for
/// something both can already say.
///
/// **The payload is printed rather than folded into the sentence.** `SpawnError::Panicked`'s
/// message is about the *kind* of node and points the reader at the journal, and a panic message is
/// neither; a supervisor that swallowed its own defect's text entirely would make the one record
/// that says what went wrong unavailable anywhere.
///
/// The journal is left to the run's own unwind. `run.rs`'s `AbortOnDrop` is armed across the whole
/// child run and `Drop` runs on an unwind, so a `SpawnAborted` is already there — which is what the
/// variant's own doc says, and is why this does not write a record of its own over a node whose
/// process may still be alive.
/// `panicked` spells the panic in the caller's own error vocabulary — [`spawn_panicked`] for a
/// child, [`root_panicked`] for a root — rather than this function choosing one for both. The two
/// arms of [`NodeOutcome`] exist precisely because those vocabularies differ, and a shared helper
/// that picked one would put a `SpawnError` on a node that has no spawn result or a bare sentence
/// on one that does.
pub(super) fn caught<T, E>(
    what: &str,
    panicked: impl FnOnce(&str) -> E,
    body: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(outcome) => outcome,
        Err(payload) => {
            eprintln!(
                "marion-supervisor: the thread running the {what} node panicked, which is a defect \
                 in marion: {}. The node is resolved as a failed run so this supervisor can still \
                 exit (§5.7); its journal records end with the abort the unwind wrote.",
                panic_text(&payload)
            );
            Err(panicked(what))
        }
    }
}

/// A child's panic, in `run_spawn`'s vocabulary.
pub(super) fn spawn_panicked(agent_type: &str) -> crate::spawn::SpawnError {
    crate::spawn::SpawnError::Panicked(agent_type.to_string())
}

/// A root's panic, in marion's own — §9 gives a root no `TaskContract`, so there is no
/// `SpawnError` to be had and [`NodeOutcome::Root`] carries a sentence.
pub(super) fn root_panicked(agent_type: &str) -> String {
    format!(
        "marion's own thread running the {agent_type} root panicked, so there is no reading of \
         what the root did. This is a defect in marion, not in the request; the root's process may \
         have been left running, and the journal resolves the node with whatever the unwind wrote \
         rather than with a record of what it did."
    )
}

/// The best sentence available for a panic payload, which is a `&str` or a `String` for every panic
/// `panic!` produces and opaque otherwise.
fn panic_text(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic payload of an unprintable type".to_string())
}

/// **How long `agent/spawn` will hold its caller waiting for a process to exist.**
///
/// Not the node's wall clock, and deliberately much shorter than one: this bounds only the span
/// between `SpawnIntent` and `command.spawn()` — the worktree, the config documents, `compile`, and
/// `harness_version`'s own five-second probe, which §6.1 step 3 puts on the pre-launch path. A
/// minute is roughly an order of magnitude above the slowest of those.
///
/// It exists because the alternative is a JSON-RPC call with no bound at all. `git worktree add`
/// blocked on a repository lock is the measured case (`background.rs` found it blocking a `wait`
/// forever), and a call that never returns is worse here than there: `serve` answers each
/// connection on its own thread, but a client with no answer has no way to learn whether the node
/// it asked for exists.
///
/// **Expiry is never a verdict on the node.** The thread keeps running, the table keeps the entry,
/// and the refusal says which node marion has stopped waiting for — see [`launch_bound_expired`].
const LAUNCH_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// **32 bytes from `/dev/urandom`, as hex** — §5.4's per-node capability.
///
/// Not `run::entropy()`, which is 10 bytes and is sized for *uniqueness* in a UUIDv7 whose other
/// half is a millisecond timestamp. Uniqueness and unguessability are different requirements, and
/// reusing an id generator for a credential is how the second silently inherits the first's budget.
///
/// A failure to read `/dev/urandom` yields `None`, and [`RegistryHandle::claim`]'s caller turns
/// that into a node with no token — which is a node whose bridge can never spawn, and never a node
/// with a predictable one.
pub(crate) fn mint_token() -> Secret {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)) {
        Ok(()) => Secret::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        // **A token nothing can present, rather than one anything can guess.** The empty string
        // never matches: `Secret`'s equality compares lengths first, and every real `SpawnCaller` must
        // carry a non-empty `node_token` to deserialize at all. So the node runs and cannot spawn,
        // which is the safe direction for a machine whose entropy source is unreadable.
        Err(_) => Secret::new(String::new()),
    }
}

/// **A fixed decoy of a real token's shape**, so a `SpawnCaller` naming a node this supervisor does
/// not own takes the same comparison path as one naming a node it does. Minted once per process and
/// never written anywhere, so it matches nothing.
pub(super) fn decoy_token() -> &'static Secret {
    static DECOY: std::sync::OnceLock<Secret> = std::sync::OnceLock::new();
    DECOY.get_or_init(mint_token)
}

/// What a node's own thread tells the call that started it. Three moments, in this order.
pub(super) enum Progress {
    /// The node has an identity and its `SpawnIntent` is durable.
    Identified(AgentId),
    /// A process exists and `Spawned { pid: Some(_) }` is journaled.
    Started,
    /// `run_spawn` refused before the node had an identity, in its own words — a containment
    /// refusal says how to allow it, and a generic sentence would lose that.
    Refused(String),
    /// `run_spawn` returned, whichever way. Sent last and always, so a launch that fails before
    /// either of the above cannot leave the call waiting out [`LAUNCH_BOUND`] for nothing.
    Finished,
}

/// The supervisor, watching one of its own nodes launch. See [`crate::run::SpawnObserver`].
pub(super) struct NodeOwner {
    pub(super) handle: Arc<RegistryHandle>,
    /// `None` for a root — see [`NodeHandle::task_id`].
    pub(super) task_id: Option<TaskId>,
    /// The tree this node is being launched in, carried so [`RegistryHandle::claim`] can record it
    /// on the entry the node's *own* children will inherit from. See [`NodeHandle::repo`].
    pub(super) repo: PathBuf,
    pub(super) tx: std::sync::mpsc::Sender<Progress>,
    /// The id this spawn minted, once it has one — read by the thread body after `run_spawn`
    /// returns, so the outcome can be filed under the node it belongs to.
    pub(super) identified: Mutex<Option<AgentId>>,
    /// The parent this child's end is owed to as a message (a background `spawn`,
    /// `AgentSpawnParams::notify_parent`), or `None`.
    pub(super) announce_to: Option<AgentId>,
    /// The parent's inbox recorded the debt at claim ([`crate::inbox::Inboxes::owe`]), so the end
    /// must settle it — and only then, or it would settle another child's.
    pub(super) owes: std::sync::atomic::AtomicBool,
}

impl NodeOwner {
    fn identified_id(&self) -> Option<AgentId> {
        lock(&self.identified).clone()
    }
}

/// **The pane's half of the same ownership**, on §3.4's display axis (§9's M3 criterion C1).
///
/// `root::launch_terminal` opens the pty and reaps the child; this supervisor is the only process
/// that can *serve* it, because `node/attach` is answered here. The two calls are what make a
/// launched pane an attachable one, and the window between them is the node's whole life.
impl crate::root::PaneOwner for NodeOwner {
    fn opened(&self, agent_id: &AgentId, host: Arc<crate::pty::PtyHost>) {
        self.handle.register_pane(agent_id, host);
    }

    fn closing(&self, agent_id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.handle.closing_pane(agent_id, host);
    }

    fn completed(&self, agent_id: &AgentId, host: &Arc<crate::pty::PtyHost>, charged_bytes: usize) {
        self.handle.completed_pane(agent_id, host, charged_bytes);
    }

    /// Permanent invalidation is the failure/removal path; ordinary completion keeps replay state.
    fn failed(&self, agent_id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.handle.failed_pane(agent_id, host);
    }
}

impl crate::run::SpawnObserver for NodeOwner {
    /// A worktree child's own children branch from its worktree, so its entry's tree becomes that
    /// worktree. A `shared-cwd` child runs in the tree it inherited, which its entry already names.
    fn workspace_chosen(&self, agent_id: &AgentId, workspace: &marion_core::contract::Workspace) {
        if let marion_core::contract::Workspace::Worktree { path, .. } = workspace
            && let Some(node) = lock(&self.handle.nodes).get_mut(agent_id)
        {
            node.repo = path.clone();
        }
    }

    fn identified(&self, agent_id: &AgentId) -> Option<Secret> {
        let token = self
            .handle
            .claim(agent_id, self.task_id.clone(), self.repo.clone());
        *lock(&self.identified) = Some(agent_id.clone());
        // Held open from here: the parent's driver must not end it while this child runs.
        if let Some(parent) = &self.announce_to {
            self.owes.store(
                self.handle.inboxes.owe(parent),
                std::sync::atomic::Ordering::SeqCst,
            );
        }
        // Ignored: a receiver dropped before this fires means the call that started the spawn has
        // already given up on it, and the node goes on running either way. Panicking here would
        // unwind through `AbortOnDrop` and journal `SpawnAborted` over a node that is fine.
        let _ = self.tx.send(Progress::Identified(agent_id.clone()));
        // **The one place the tests can make a node's thread panic in the window that matters.**
        // Here and not at the top of the thread body, because the defect is specifically a panic
        // *after* the node is claimed and *before* `mark_finished` — a panic before the claim has
        // no node to strand. Placed after the `Progress::Identified` send so the calling frame gets
        // its id and its refusal comes from the thread's epilogue rather than from `LAUNCH_BOUND`.
        // `cfg(test)`, so no production build carries a branch that panics on purpose.
        #[cfg(test)]
        if panic_after_claim::armed_for(&self.repo) {
            panic!("injected: a node thread panicking after its claim");
        }
        (!token.is_empty()).then_some(token)
    }

    fn spending(&self) -> Option<Arc<crate::spending::Spending>> {
        Some(self.handle.spending.clone())
    }

    fn started(&self, agent_id: &AgentId, pid: i32) {
        self.handle.mark_started(agent_id, pid);
        let _ = self.tx.send(Progress::Started);
    }

    /// The node's own inbox, opened at [`RegistryHandle::claim`] a moment before this is asked.
    fn turn_source(&self, agent_id: &AgentId) -> Option<Arc<dyn crate::inbox::TurnSource>> {
        Some(Arc::new(crate::inbox::BoundInbox::new(
            Arc::clone(&self.handle.inboxes),
            agent_id.clone(),
        )))
    }

    /// §7.6's subtree scan, off the registry this supervisor already follows. Refreshed first: the
    /// registry is a follower of the journal, and a descendant's `Exited` that landed since the last
    /// poll is exactly the transition a held node is waiting to see.
    fn live_descendants(&self, agent_id: &AgentId) -> Option<Vec<AgentId>> {
        self.handle.live.refresh();
        Some(
            self.handle
                .live
                .read(|r| crate::descendant_gate::live_descendants(r.tree(), agent_id)),
        )
    }

    /// [`RegistryHandle::process_ended`]: this supervisor is the one a `node/kill` reaches.
    fn process_ended(&self, agent_id: &AgentId) -> bool {
        self.handle.process_ended(agent_id)
    }

    fn ended_by(&self, agent_id: &AgentId) -> Option<marion_core::journal::CancelBy> {
        self.handle.ended_by(agent_id)
    }
}

/// **The uid half of what authorizes an operator-only call** (a root, a resume, an operator's
/// steer or kill).
///
/// *Who the operator is* is decided first and elsewhere: the connection's `session/hello` presented
/// the operator's key ([`session`]). Open question 3 once answered "filesystem permission and
/// deliberately not a token", on the argument that a key at rest is readable by every same-uid
/// process that can reach the socket. The 2026-09-29 audit found that answer made every node's own
/// shell the operator; a key marion never hands a node, refused besides from any process descending
/// from a node, excludes exactly the processes the uid cannot. This check stays behind it.
///
/// *Peer credentials are necessary and not sufficient, and both halves are load-bearing.*
/// `getpeereid` answers **which user**; §5.4's question is **which node**. §11 item 28's open
/// question 3 records a code-verified audit of a third-party harness with this exact topology whose
/// authorization keyed on a client-asserted origin field, reachable by any process of the same
/// user — and notes that peer credentials *would not have saved them*, because the attacker's
/// credentials matched. That is why this check is confined to the one call where there is no node
/// to ask about: every spawn that names a caller goes through [`RegistryHandle::resolve_caller`]
/// and proves it with the token, and this check is not a substitute for that anywhere.
///
/// *What it does buy.* `socket.rs` creates the socket and its directory `0700` and the socket
/// `0600` (`a_created_socket_directory_is_private_and_so_is_the_socket`), so the kernel already
/// excludes other users at `connect`. This makes that exclusion a **checked** property of the
/// supervisor rather than an inherited property of a mode bit somebody could change, and it turns
/// the one case the mode bits cannot cover — a socket whose permissions were widened, deliberately
/// or by an umask accident — from a silent grant into a named refusal.
///
/// *An unreadable peer is a refusal.* [`Peer::Unknown`] means `getpeereid` failed, and a check that
/// cannot be made is not a check that passed.
/// What [`RegistryHandle::reviewed`] read about the node a review names.
pub(super) struct Reviewed {
    pub(super) review: crate::review::Target,
    pub(super) contract: marion_core::contract::TaskContract,
    pub(super) intent: SpawnIntent,
    pub(super) model: Option<String>,
}

/// **§2's one-socket-one-project rule for a spawn that names its own `repo`** — a root, or an
/// operator's review: the tree named must key to the project this supervisor serves.
pub(super) fn check_project(env: &crate::run::Env, repo: &Path) -> Result<(), RpcError> {
    let named = marion_core::paths::ProjectDir::new(&env.state, &crate::socket::project_root(repo));
    if named != env.project_dir {
        return Err(RpcError::refused(
            "repo",
            format!(
                "this supervisor serves the project keyed at {}, and {} keys to {}. §2 keys a \
                     supervisor and its state on the project root — the git common dir — so one \
                     socket serves one repository and every linked worktree of it, and no other. A \
                     root created here would be journaled under the project it named while this \
                     supervisor kept following its own, so marion would report a node that no \
                     `tree/subscribe` on this socket could ever show. Dial the supervisor for that \
                     repository instead; refused before the node is claimed, so nothing was written \
                     under either project.",
                env.project_dir.path().display(),
                repo.display(),
                named.path().display(),
            ),
            "§2, §5.4",
        ));
    }
    Ok(())
}

/// A fresh contract id. Minted by marion rather than by the caller, for §9's reason: the id names a
/// run marion performed, and a caller-chosen one would let two runs share a contract file.
pub(super) fn mint_task_id() -> Result<TaskId, RpcError> {
    crate::run::entropy()
        .map(|e| marion_core::contract::new_task_id(crate::run::unix_millis(), e))
        .map_err(|e| {
            RpcError::internal(format!(
                "marion could not mint a task id for this spawn, so nothing was started: {e}"
            ))
        })
}

/// A child's launch request from a spawn's terms, for one agent type and model: a plain spawn's
/// own, or one seat's of a race.
fn child_request(
    p: &marion_core::proto::params::AgentSpawnParams,
    repo: PathBuf,
    agent_type: String,
    model: Option<String>,
    race: Option<marion_core::race::RaceSeat>,
    budget: Option<marion_core::budget::Budget>,
    timeout_secs: u64,
) -> crate::run::SpawnRequest {
    crate::run::SpawnRequest {
        budget,
        review: None,
        agent_type,
        prompt: p.prompt.clone(),
        repo,
        acceptance_criteria: p.acceptance_criteria.clone(),
        verification: p.verification.clone(),
        race,
        writable_scope: p.writable_scope.clone(),
        // Resolved by the caller from `p.timeout_secs` and the clock above it; `effective_timeout`
        // clamps it exactly as it clamps a stated one.
        timeout_secs,
        model,
        // **Absence resolved here, once.** §3.1's agent-type key defaults to `shared-cwd`;
        // marion resolves an absent *`spawn` parameter* to `Worktree` instead, and
        // `contract::Isolation` carries the argument: silence must not silently *remove*
        // containment from every caller that names nothing.
        isolation: p.isolation.unwrap_or(Isolation::Worktree),
        // Absent is `false` — marion holds §6.6's guard. See `AgentSpawnParams`.
        allow_concurrent_writes: p.allow_concurrent_writes.unwrap_or(false),
        // A client `agent/spawn` is always a fresh run: resume is `node/resume`'s path, which
        // reconstructs this same request from the journal rather than from a caller.
        resume: None,
        profile: None,
        read_only: false,
        workflow: None,
    }
}

/// The task as the child will receive it, resolved the way `run_spawn_watched` resolves it,
/// before the request moves into the launch.
fn sent_task(req: &crate::run::SpawnRequest) -> Option<marion_core::proto::result::TaskSent> {
    let prompt = crate::run::agent_types(&req.repo)
        .ok()
        .and_then(|t| t.resolve(&req.agent_type))
        .map(|t| crate::run::child_prompt(&t, &req.prompt))?;
    Some(crate::node_detail::task_of(
        &prompt,
        req.acceptance_criteria.clone(),
        req.verification.clone(),
    ))
}

/// §5.4's refusal of a node token this supervisor holds no binding for — at `session/hello` and at
/// every call that re-proves a `caller`.
pub(super) fn unminted_token(agent: &AgentId) -> RpcError {
    RpcError::refused(
        &agent.0,
        "this supervisor did not mint that node token, so it cannot tell the caller it claims to \
         be from any other process that can reach this socket. §5.4 binds a capability token to an \
         `AgentId`; the supervisor holds the binding and there is no round trip that could \
         establish one for a node it does not own. A node whose supervisor has restarted is in \
         this case and it is not a mistake the caller made — its parent is `Orphaned` (§7.2) and \
         needs an operator, not a retry.",
        "§5.4, §6.1",
    )
}

/// **A spawn with no `caller` that asks for a worktree**: the operator asking for a contracted node
/// rather than starting a root in their checkout. A race with no caller is one too, seat by seat.
fn operator_contracted(p: &marion_core::proto::params::AgentSpawnParams) -> bool {
    p.caller.is_none() && p.isolation == Some(Isolation::Worktree)
}

/// What a contracted node the operator asked for cannot carry: a root's own fields. It runs in a
/// worktree under a contract, so there is no checkout to snapshot, no terminal for an operator to
/// drive (a TUI takes no turn and would only time out), and no native session to adopt.
fn check_operator_contracted(
    p: &marion_core::proto::params::AgentSpawnParams,
) -> Result<(), RpcError> {
    let root_only = [
        ("no_change_record", p.no_change_record.is_some()),
        ("pane", p.pane.is_some()),
        ("native_launch", p.native_launch.is_some()),
        (
            "allow_concurrent_writes",
            p.allow_concurrent_writes.is_some(),
        ),
    ];
    match root_only.into_iter().find(|(_, stated)| *stated) {
        Some((field, _)) => Err(RpcError::refused(
            field,
            format!(
                "a spawn with no `caller` and a worktree is a contracted node the operator asked \
                 for: it runs in its own worktree under a contract, and `{field}` belongs to a \
                 root in the operator's checkout. Refused rather than dropped (§11 item 23)."
            ),
            "§6.6, §9, §11 item 23",
        )),
        None => Ok(()),
    }
}

pub(super) fn root_spawn_authorized(peer: Peer) -> Result<(), RpcError> {
    let own = crate::socket::own_uid();
    match peer {
        Peer::Uid(uid) if uid == own => Ok(()),
        Peer::Uid(uid) => Err(RpcError::refused(
            "caller",
            format!(
                "this connection's peer runs as uid {uid} and this supervisor runs as uid {own}. A \
                 spawn with no `caller` creates a **root**, which is the one call on this socket \
                 that starts work rather than describing it, and marion authorizes it by \
                 filesystem permission on the socket: §2's path is under `<state>` (or \
                 `/tmp/marion-<uid>`), created 0700 with the socket 0600, so another user reaching \
                 it means the permissions are not what marion set."
            ),
            "§2, §5.4",
        )),
        Peer::Unknown => Err(RpcError::refused(
            "caller",
            "marion could not read this connection's peer credentials, so it cannot establish that \
             the caller is this supervisor's own user — and a check that could not be made is not \
             a check that passed.",
            "§2, §5.4",
        )),
    }
}

/// §6.1 step 2's refusal, in the caller's own vocabulary rather than as an internal error.
fn gate_refusal(e: marion_core::agent_type::SpawnGateError) -> RpcError {
    RpcError::refused(
        "caller",
        format!(
            "{e}. The bound and the value that broke it are read from the **caller's** agent type \
             and the supervisor's own registry, never from the call — §6.1 step 2 says the gates \
             read the caller's type, and a caller that could state its own depth or child count \
             could state one that passes."
        ),
        "§6.1, §3.1",
    )
}

fn spawn_refused_before_the_node_existed() -> RpcError {
    RpcError::refused(
        "agent_type",
        "marion refused this spawn before it minted a node: the agent type is not a built-in and \
         not a row of the tree's .marion/agents.toml, or the requested writable scope is outside \
         that type's ceiling. Nothing was journaled and nothing was started, so there is no node \
         to look up. (§3.1, §5.4)",
        "§3.1",
    )
}

/// The agent types the tree at `repo` can spawn, or the file's own refusal as a frame's error.
///
/// [`crate::run::agent_types`]'s one non-`Ok` arm is a `.marion/agents.toml` that exists and cannot
/// be used; that is the operator's file, so the refusal carries its path and reason verbatim.
fn tree_types(repo: &Path) -> Result<marion_core::agent_type::AgentTypes, RpcError> {
    crate::run::agent_types(repo)
        .map_err(|e| RpcError::refused("agent_type", e.to_string(), "§3.1"))
}

/// A journaled node's type, re-resolved from today's table for the tree it runs in — and refused
/// if the type has since moved to another harness.
///
/// The journal records the name and the harness the node launched under. A `.marion/agents.toml`
/// row is the operator's and may have been edited since; a row that now names another harness
/// would relaunch a session that harness has never seen, under a node claiming to be the same
/// one. The built-ins cannot move, so this only ever fires on a user row.
///
/// With `project`, the node's own recorded table ([`crate::types_snapshot`]) where it has one, so a
/// resumed child is the type its tree started with, not whatever its parent's worktree now says.
pub(super) fn recorded_type(
    project: Option<&marion_core::paths::ProjectDir>,
    repo: &Path,
    intent: &SpawnIntent,
) -> Result<marion_core::agent_type::AgentType, RpcError> {
    let resolved = match project {
        Some(project) => {
            crate::types_snapshot::for_caller(project, &intent.agent_id, repo, &intent.agent_type)
                .and_then(|s| s.resolve(&intent.agent_type))
                .map_err(|e| RpcError::refused("agent_type", e.to_string(), "§3.1"))?
        }
        None => tree_types(repo)?.resolve(&intent.agent_type),
    };
    let ty = resolved.ok_or_else(|| {
        Unprojectable::UnknownAgentType(intent.agent_type.clone()).as_error(&intent.agent_id)
    })?;
    if ty.harness != intent.harness {
        return Err(RpcError::refused(
            "agent_type",
            format!(
                "`{}` was journaled as {} and {} now says {}; marion will not resume a node under \
                 a different harness than it recorded",
                intent.agent_type,
                intent.harness,
                crate::run::AGENT_TYPES_FILE,
                ty.harness
            ),
            "§3.1, §8",
        ));
    }
    Ok(ty)
}

/// A **child**'s launch failed between its intent and its process, and this is why.
///
/// `why` is the sentence `run_spawn`'s thread filed as its outcome — a worktree git refused, a
/// configuration document that would not write, an adapter that would not compile the launch
/// because the row promised a tool the harness has none of. It is quoted rather than summarised:
/// the parent reading this `spawn` result is the one who can change the row, and "a worktree, a
/// configuration document, or the harness `--version` probe" told it nothing it could act on. The
/// journal's `SpawnAborted` beside the intent carries the same sentence (`run::AbortOnDrop`).
///
/// [`launch_failed_reason`] supplies the fallback for a thread that filed no reason.
pub(super) fn spawn_failed_before_the_process_existed(
    agent_id: &AgentId,
    why: Option<&str>,
) -> RpcError {
    RpcError::of(
        FailureKind::Internal,
        Some(&agent_id.0),
        format!(
            "node `{}` was journaled and its launch failed before any process existed: {}. Its \
             `SpawnIntent` is resolved by a `SpawnAborted` beside it, which after §11 item 28 step \
             1 is evidence that **no process exists**, not merely consistent with it (§7.2). A \
             worktree may be left behind; a process is not.",
            agent_id.0,
            launch_failed_reason(why)
        ),
        "§7.2",
    )
}

/// The sentence a launch failure is reported with: the thread's own, or an honest statement that
/// it filed none — which is itself the fault to report, never a reason invented in its place.
fn launch_failed_reason(why: Option<&str>) -> String {
    match why {
        Some(w) => w.to_string(),
        None => "marion's own thread for it filed no reason, which is itself the fault to report"
            .to_string(),
    }
}

/// [`spawn_failed_before_the_process_existed`], for a **root**.
///
/// The same shape, kept separate because the two have different evidence to offer and different
/// readers. A root's caller is a **person at `marion run`**, and the failures they hit are
/// marion's own refusals — an unrecordable working tree, a `<state>` inside the repository, a
/// harness with no root surface — each of which already names the directory and the remedy.
/// Dropping that on the floor and answering "the launch failed" is a refusal an operator cannot
/// act on.
fn root_launch_failed(agent_id: &AgentId, why: Option<&str>) -> RpcError {
    let why = launch_failed_reason(why);
    RpcError::of(
        FailureKind::Internal,
        Some(&agent_id.0),
        format!(
            "root `{}` was journaled and its launch failed before any process existed: {why}",
            agent_id.0
        ),
        "§7.2",
    )
}

/// **Never a verdict on the node**, which is why this is separate from the failure above.
///
/// marion has stopped waiting; the node has not stopped launching. The thread runs on, the table
/// keeps the entry, and the node's own wall clock still bounds it. Saying "the spawn failed" here
/// would be the false-receipt shape this codebase keeps deleting — in the direction that leaves a
/// live process a caller believes is dead.
fn launch_bound_expired(agent_id: Option<&AgentId>) -> RpcError {
    let subject = match agent_id {
        Some(id) => format!("node `{}`", id.0),
        None => "this spawn".to_string(),
    };
    RpcError::of(
        FailureKind::Internal,
        agent_id.map(|i| i.0.as_str()),
        format!(
            "marion started {subject} and did not observe a process within {}s, so it stopped \
             holding this call. **This is not a statement that the spawn failed**: the node's \
             thread is still running, the supervisor still owns it, and its own wall clock still \
             bounds it. Watch it through `tree/subscribe` and `node/attach`; a `SpawnAborted` or a \
             `Spawned` will say which way it went.",
            LAUNCH_BOUND.as_secs()
        ),
        "§6.1",
    )
}

/// One bounded wait on a launch thread's progress channel: whatever time is left of `deadline`, so
/// the two waits a launch makes share one `LAUNCH_BOUND` rather than each getting their own.
fn recv_progress(
    progress: &std::sync::mpsc::Receiver<Progress>,
    deadline: std::time::Instant,
) -> Result<Progress, std::sync::mpsc::RecvTimeoutError> {
    progress.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
}

/// A launch's first wait: the node's id, or the refusal that it never had one.
///
/// Anything but `Identified` here means the launch refused before it minted an id — there is no
/// node and nothing to report on — or the bound expired first. Shared by the child and root paths
/// because the two answers are the same sentence on both.
fn await_identified(
    progress: &std::sync::mpsc::Receiver<Progress>,
    deadline: std::time::Instant,
) -> Result<AgentId, RpcError> {
    match recv_progress(progress, deadline) {
        Ok(Progress::Identified(id)) => Ok(id),
        Ok(_) => Err(spawn_refused_before_the_node_existed()),
        Err(_) => Err(launch_bound_expired(None)),
    }
}

/// Assemble the root domain input from the validated socket request and supervisor environment.
///
/// Pure by construction: it clones values already in memory and performs no adapter lookup,
/// compilation, argv synthesis, filesystem access, PTY allocation, or launch. Production uses
/// this same seam; while the native transport gate is closed, its focused test proves the opaque
/// context is nevertheless threaded through unchanged for the slice that will eventually open it.
/// **The working tree a resumed root runs in**, from the project root this supervisor serves.
///
/// A resume has no `repo` from a client — §8 rebuilds the launch from the node's journal, and the
/// journal keys on the git common dir (§2), not the working tree. A harness resumes a session only
/// from the **same cwd** it was created in (claude silently starts fresh otherwise), so the cwd
/// matters and must be the working tree, not the `.git` directory the socket keys on. For a
/// standard repository the working tree is the parent of `.git`, which is what this recovers.
///
/// **The known limit, stated rather than hidden:** a linked worktree's common dir is not its
/// working tree's parent, so a root started in one is not yet resumable — its cwd would need
/// recording. Every root a person starts with `marion run` in a plain checkout is, which is M2's
/// slice. A `node/resume` of a worktree root reaches the harness with the wrong cwd and the harness
/// refuses or starts fresh; until the cwd is journaled, that is the honest boundary.
/// How long a harness's session listing may take before the resume it serves is refused.
const SESSION_LISTING_BOUND: std::time::Duration = std::time::Duration::from_secs(20);

/// **The workspace a child ran in, from the directory its harness says its session was created in**,
/// for a child whose stream never named a session and so never journaled one. A directory that is
/// the worktree marion cuts for this node is that worktree, on the branch marion cuts for its task;
/// any other is the caller's own directory it shared. `None` for a child with no task.
fn listed_workspace(
    node: &ReplayedNode,
    env: &crate::run::Env,
    dir: &Path,
) -> Option<marion_core::contract::Workspace> {
    use marion_core::contract::Workspace;
    let task = node.intent.as_ref()?.task_id.as_ref()?;
    let worktree = env.project_dir.agent(&node.agent_id).worktree();
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    Some(if same(dir, &worktree) {
        // The branch it has checked out, whichever way it was named; a tree too far gone to say
        // is on the name its task's branch had before names were readable.
        let branch = crate::spawn::checked_out_branch(crate::run::tree_of(env, &worktree))
            .unwrap_or_else(|| crate::run::legacy_worktree_branch(task));
        Workspace::Worktree {
            path: worktree,
            branch,
        }
    } else {
        Workspace::SharedCwd {
            path: dir.to_path_buf(),
        }
    })
}

fn resumable_root_cwd(project_root: &std::path::Path) -> PathBuf {
    match project_root.file_name() {
        Some(name) if name == ".git" => project_root
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| project_root.to_path_buf()),
        _ => project_root.to_path_buf(),
    }
}

pub(super) fn root_spec_from_spawn(
    p: &marion_core::proto::params::AgentSpawnParams,
    repo: PathBuf,
    env: &crate::run::Env,
    agent_type: &marion_core::agent_type::AgentType,
) -> crate::root::RootSpec {
    crate::root::RootSpec {
        os_sandbox: env.os_sandbox,
        wider_children: p.wider_children.unwrap_or(false),
        // A root has no ancestor to narrow it: its type's budget and the tree limit it was run with.
        budget: marion_core::budget::resolve(agent_type.budget, p.budget_tokens, None),
        agent_type: p.agent_type.clone(),
        prompt: p.prompt.clone(),
        native_launch: p.native_launch.as_deref().cloned(),
        repo,
        state: env.state.clone(),
        base_url: env.base_url.clone(),
        bridge: env.bridge.clone(),
        // §3.1's precedence, the same one a child's spawn gets: the stated model, else the agent
        // type's own `model` key — dropped on a live run when it names marion's canned plumbing
        // (`AgentType::default_model`).
        model: p
            .model
            .clone()
            .or_else(|| agent_type.default_model(env.auth == marion_harness::Auth::Canned)),
        // Absent is `false` — marion looks. See `AgentSpawnParams::no_change_record`.
        no_change_record: p.no_change_record.unwrap_or(false),
        auth: env.auth,
        // Absent is `false` — a node gets a pane because a run asked for one. See
        // `AgentSpawnParams::pane`.
        pane: p.pane.unwrap_or(false),
        // A client `agent/spawn` is always a fresh run: resume is `node/resume`'s path, which
        // builds its own `RootSpec` from the node's journal.
        resume: None,
        // §9's node-level bound, resolved from what the client stated and the agent type — by the
        // same function `marion run` used to call in-process, so the number an operator typed and
        // the number the node runs under are one resolution. It rides on the spec because
        // `root::prepare` journals it onto the node's `SpawnIntent`, and `launch_root` enforces
        // the same value: one clock, enforced and reported.
        bound_secs: crate::root::blocked_bound_secs(p.timeout_secs, agent_type.timeout.0.as_secs()),
        // The operator's choice of login for this root, where the client stated one.
        profile: p.profile.clone(),
    }
}

impl RegistryHandle {
    fn resolve_caller(
        &self,
        c: &marion_core::proto::SpawnCaller,
    ) -> Result<crate::run::Caller, RpcError> {
        // **The token is checked before anything else is said about the node**, so a caller who
        // guesses an `AgentId` learns nothing from the shape of the refusal beyond "no".
        // The caller's repository rides out with the answer: it is the tree whose
        // `.marion/agents.toml` defines the caller's own type, read below, outside the registry lock.
        let repo = self.authenticate(c);
        let Some(repo) = repo else {
            return Err(unminted_token(&c.agent_id));
        };
        let (type_name, depth) = self.live.read(|r| {
            let node = r.tree().get(&c.agent_id).ok_or_else(|| {
                RpcError::internal(format!(
                    "this supervisor owns node `{}` and its journal has no `SpawnIntent` for it, \
                     so §6.1 step 2's gates have no agent type and no depth to read. Refusing \
                     rather than assuming a default: an assumed `max_depth` is a constant \
                     pretending to be a lookup.",
                    c.agent_id.0
                ))
            })?;
            let intent = node
                .intent
                .as_ref()
                .ok_or_else(|| Unprojectable::NoIntent.as_error(&c.agent_id))?;
            Ok::<_, RpcError>((intent.agent_type.clone(), intent.depth))
        })?;
        let agent_type = match self.spawn_env.as_ref() {
            // The table the caller's tree started with, bounded by its root's `max_depth`: never
            // the caller's worktree, which it can edit (see `crate::types_snapshot`).
            Some(env) => {
                crate::types_snapshot::for_caller(&env.project_dir, &c.agent_id, &repo, &type_name)
                    .and_then(|s| s.resolve(&type_name))
                    .map_err(|e| RpcError::refused("agent_type", e.to_string(), "§3.1"))?
            }
            None => tree_types(&repo)?.resolve(&type_name),
        }
        .ok_or_else(|| Unprojectable::UnknownAgentType(type_name.clone()).as_error(&c.agent_id))?;
        Ok(crate::run::Caller {
            agent_id: c.agent_id.0.clone(),
            agent_type,
            depth,
            live_children: self.live_children_of(&c.agent_id),
        })
    }

    /// §2's `agent/spawn` — see [`agent_spawn`](Self::agent_spawn)'s doc for the whole shape.
    ///
    /// `peer` is the connection's credentials and is consulted on exactly one path — a spawn with
    /// no caller. See [`root_spawn_authorized`] for what that decides and, more importantly, for
    /// what it deliberately does not.
    /// **§11 item 28 step 6's `caller`/`repo` pairing**, answered from the frame alone.
    fn check_caller_repo_pairing(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        // **The `caller`/`repo` pairing, before anything else and before any state is consulted.**
        // It is a property of the frame alone, so it is answered from the frame alone — and
        // answering it first is what keeps both halves reachable no matter which other refusals
        // this build still owes (see the step-6 arm below).
        //
        // `deny_unknown_fields` makes an *unknown* key a refusal already; these are the two ways a
        // known key can be wrong.
        match (p.caller.as_ref(), p.repo.as_ref()) {
            (None, None) => {
                return Err(RpcError::refused(
                    "repo",
                    "a spawn with no `caller` is a client creating a root, and only the client \
                     knows which tree the root is of. §2 keys this supervisor on `git rev-parse \
                     --git-common-dir`, so it serves a repository *and every linked worktree of \
                     it* over one socket and one journal — `<state>/<project-hash>` names the \
                     project, never the working tree, and no derivation here could recover which \
                     of them was meant. A worktree branched from the wrong tree's HEAD is a real \
                     branch off real commits with nothing reporting a problem, so the field is \
                     required rather than defaulted to the supervisor's own project root.",
                    "§2, §6.6",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(RpcError::refused(
                    "repo",
                    "a spawn with a `caller` must not state a `repo`: the supervisor already knows \
                     which tree that node lives in, and a caller that states it is a caller that \
                     can lie about it. This is the same rule that keeps `agent_type` and `depth` \
                     off `SpawnCaller` — every gated fact about a caller is derived from what the \
                     supervisor minted, never from what the frame asserts. A node that could name \
                     its own repository could branch its children off a tree its parent never \
                     entitled it to touch.",
                    "§5.4, §6.1",
                ));
            }
            (Some(_), None) | (None, Some(_)) => {}
        }
        Ok(())
    }

    /// **A root's workspace is not a choice**: `isolation` on a spawn with no `caller`.
    fn check_root_isolation(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        // **The other half of the same pairing**, and refused for §11 item 23's reason rather than
        // ignored: §9's change record exists only for a root, because only a root runs in the
        // operator's own checkout. A child's writes are judged against the worktree marion made it,
        // so a caller that asked marion not to snapshot and was told nothing would have been given
        // a wrong answer that looks like a right one.
        // **The mirror of the two below**: `isolation` is a *child's* field, and stating it for a
        // root is a request marion cannot serve in either direction. A root **is** the operator's
        // checkout — the tree they named with `--repo` — and §9's change record is built on exactly
        // that; accepting `shared-cwd` and doing what marion already does would be an
        // accept-and-ignore, and honouring `worktree` would branch the operator's own run into a
        // directory they never asked marion to make.
        if p.caller.is_none()
            && let Some(iso) = p.isolation
            && !operator_contracted(p)
            && p.candidates.is_empty()
        {
            return Err(RpcError::refused(
                "isolation",
                format!(
                    "a spawn without a `caller` creates a **root**, and a root's workspace is not a \
                     choice: it is the operator's own checkout at the `repo` this call names, which \
                     is what §9's change record measures. `isolation: {:?}` is therefore either a \
                     description of what marion already does or a request to run the operator's own \
                     run somewhere else. Refused rather than dropped, because a parameter that is \
                     accepted and not acted on tells the caller their choice was honoured (§11 item \
                     23). `isolation` belongs on a child spawn, where the parent really is choosing \
                     between §6.6's two workspaces.",
                    iso.as_wire()
                ),
                "§6.6, §9, §11 item 23",
            ));
        }
        Ok(())
    }

    /// **A root has no caller's cwd to share**: `allow_concurrent_writes` without a `caller`.
    fn check_root_allow_concurrent_writes(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        if p.caller.is_none() && p.allow_concurrent_writes.is_some() && !operator_contracted(p) {
            return Err(RpcError::refused(
                "allow_concurrent_writes",
                "a spawn without a `caller` creates a root, and §6.6's write-conflict rule is about \
                 a *child* sharing a caller's cwd. A root has no parent to share with, so there is \
                 no guard here to lift and nothing for this field to permit. Refused rather than \
                 dropped (§11 item 23).",
                "§6.6, §11 item 23",
            ));
        }
        Ok(())
    }

    /// **A root has no contract to check**: `verification` or `writable_scope` on a spawn with no
    /// `caller`.
    ///
    /// Both are checks a contract carries: a child's verification runs over its worktree and its
    /// scope judges that worktree's diff. §9 gives a root no contract, so a root stating them would
    /// run in the operator's checkout with nothing checked while the caller believed it had asked
    /// for checks — the silent drop a live `marion mcp` session hit. Refused by name.
    ///
    /// `acceptance_criteria` is not refused: nothing checks it on a child either (it is the
    /// caller's statement of done, which the child reports against), and `spawn`'s schema requires
    /// it on every call, so refusing it would refuse every well-formed top-level spawn.
    pub(super) fn check_root_contract_fields(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        // A child's, and a contracted node's the operator asked for: both run under a contract.
        if p.caller.is_some() || operator_contracted(p) || !p.candidates.is_empty() {
            return Ok(());
        }
        let stated = [
            ("verification", p.verification.is_empty()),
            ("writable_scope", p.writable_scope.is_empty()),
        ];
        let Some((field, _)) = stated.into_iter().find(|(_, empty)| !empty) else {
            return Ok(());
        };
        Err(RpcError::refused(
            field,
            format!(
                "a spawn without a `caller` creates a **root**, and §9 gives a root no contract: it \
                 runs in the operator's own checkout, and nothing would check `{field}` against \
                 it. Refused rather than dropped, because a caller told nothing would believe its \
                 checks ran (§11 item 23). State it on a child spawn, whose worktree and contract \
                 are what it is checked against."
            ),
            "§6.7, §9, §11 item 23",
        ))
    }

    /// **§9's change record is a root's**: `no_change_record` on a spawn with a `caller`.
    fn check_child_no_change_record(
        p: &marion_core::proto::params::AgentSpawnParams,
    ) -> Result<(), RpcError> {
        if p.caller.is_some() && p.no_change_record.is_some() {
            return Err(RpcError::refused(
                "no_change_record",
                "a spawn with a `caller` must not state `no_change_record`: §9's change record is \
                 a root's, because only a root runs in the operator's own checkout. A child runs \
                 in a worktree marion made for it (§6.6) and its writes are judged against that \
                 worktree's own scope, so there is no snapshot of anybody's tree here to decline. \
                 Refused rather than dropped, because a parameter that is accepted and not acted \
                 on tells the caller their choice was honoured (§11 item 23).",
                "§9, §11 item 23",
            ));
        }
        Ok(())
    }

    /// **The opt-in to wider children is the operator's, for a whole tree**: a spawn with a
    /// `caller` — a node asking for its child — must not state `wider_children`, or a node could
    /// lift its own limits by asking.
    fn check_child_wider(p: &marion_core::proto::params::AgentSpawnParams) -> Result<(), RpcError> {
        if p.caller.is_some() && p.wider_children.is_some() {
            return Err(RpcError::refused(
                "wider_children",
                "only the operator can let an agent start agents with more authority than its own, \
                 for a whole run: `marion run --allow-wider-children`, or `[delegation] \
                 allow_wider_children = true` in ~/.config/marion/config.toml. An agent cannot \
                 ask for it.",
                "delegation",
            ));
        }
        Ok(())
    }

    pub(super) fn agent_spawn(
        &self,
        p: &marion_core::proto::params::AgentSpawnParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        if p.caller.is_some() {
            validate_native_launch_boundary(p.caller.as_ref(), p.native_launch.as_deref())
                .map_err(NativeLaunchGateError::into_rpc)?;
        }
        let me = self.me.upgrade().ok_or_else(|| {
            RpcError::internal(
                "this supervisor is being dropped and will not start a node it could not then own",
            )
        })?;
        Self::check_caller_repo_pairing(p)?;
        Self::check_root_isolation(p)?;
        Self::check_root_allow_concurrent_writes(p)?;
        Self::check_root_contract_fields(p)?;
        Self::check_child_no_change_record(p)?;
        Self::check_child_pane(p)?;
        Self::check_child_wider(p)?;
        crate::profiles::check_child_profile(p)?;
        crate::trust::check_child_spawn(p)?;
        let Some(env) = self.spawn_env.clone() else {
            return Err(RpcError::unimplemented(
                "agent/spawn",
                "this handle was built by `RegistryHandle::new`, which describes nodes and owns \
                 none, so there is no environment to run one in. A detached supervisor is built by \
                 `RegistryHandle::owning` and does serve this method; a handle in this state is a \
                 describing fixture. Refused here rather than further in, where the failure would \
                 be about a directory instead of about a build.",
                "§2",
            ));
        };
        let racing = !p.candidates.is_empty() || p.race.is_some();
        if racing && p.review_of.is_some() {
            return Err(RpcError::refused(
                "candidates",
                "a review reads one finished node and a race runs one task on several; name \
                 either `review_of` or `candidates`, not both.",
                "race",
            ));
        }
        if let Some(target) = &p.review_of {
            return self.spawn_review(me, env, p, target, peer);
        }
        let Some(caller_id) = p.caller.as_ref() else {
            // **The operator asking for a contracted node** — a race's seats, or one node in a
            // worktree: the child path, with the operator as its requester. Authorized as a root
            // is, by the connection, and for the tree the call names.
            if racing || operator_contracted(p) {
                root_spawn_authorized(peer)?;
                check_operator_contracted(p)?;
                let repo = p.repo.clone().ok_or_else(|| {
                    RpcError::internal("a spawn with no caller reached the launcher with no repo")
                })?;
                check_project(&env, &repo)?;
                return if racing {
                    self.spawn_race(me, env, p, None, repo, None)
                } else {
                    self.spawn_contracted(me, env, p, repo)
                };
            }
            // **A client creating a root** — §11 item 28 step 6. Answered on its own path: a root
            // is not a contracted node with no parent but the operator's own session, and
            // `root::prepare` owns everything that makes it one — its agent-dir layout,
            // `ROOT_DEPTH`, §9's change record over the operator's checkout, no contract.
            return self.spawn_root(me, env, p, peer);
        };
        if racing {
            let repo = self.caller_repo(caller_id)?;
            return self.spawn_race(me, env, p, Some(caller_id), repo, None);
        }

        // **Held from here to the child's durable intent, and no further.** See
        // [`Self::spawn_decision`]: the registry is a follower, so two callers that both evaluated
        // the gate before either wrote its intent would both pass a bound only one fits under.
        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        let caller = self.resolve_caller(caller_id)?;
        self.refuse_if_cancelling(caller_id)?;
        let repo = self.caller_repo(caller_id)?;
        // §6.1 step 2, before every side effect — the same pure function `run_spawn` calls, run
        // here as well so the refusal arrives in the frame that asked for it rather than as a node
        // that was never going to start. Two call sites of one function, never two rules.
        agent_type::check_spawn_gates(&caller.agent_type, caller.depth, caller.live_children)
            .map_err(gate_refusal)?;

        let task_id = mint_task_id()?;
        let budget = self.child_budget(
            &repo,
            &p.agent_type,
            p.budget_tokens,
            Some(&caller_id.agent_id),
        );
        // Never past what is left of the clock above it: a child cannot outlive its parent.
        let wall = self.child_wall_secs(
            p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS),
            &caller_id.agent_id,
        )?;
        let req = child_request(
            p,
            repo.clone(),
            p.agent_type.clone(),
            p.model.clone(),
            None,
            budget,
            wall,
        );
        // A background spawn's end is owed to its caller as a message (turn delivery).
        let announce_to = p.notify_parent.then(|| caller_id.agent_id.clone());
        self.start_child(
            me,
            env,
            req,
            task_id,
            caller.into(),
            repo,
            decision,
            announce_to,
        )
    }

    /// **A contracted node the operator asked for directly**: the child path — a worktree on a
    /// `marion/` branch, verification, a contract, usage on the record — with the operator as its
    /// requester ([`crate::run::Requester::Operator`]) and a top-level place in the tree. No caller
    /// gates it or bounds its clock or budget; its type's own ceilings and the operator's config
    /// do. Its end is announced to nobody: the client that asked reads the contract.
    fn spawn_contracted(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        p: &marion_core::proto::params::AgentSpawnParams,
        repo: PathBuf,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        // Held to the intent for the same reason a child's spawn holds it: a quit or a race's
        // count must see this node once it is decided.
        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        let task_id = mint_task_id()?;
        let budget = self.child_budget(&repo, &p.agent_type, p.budget_tokens, None);
        let req = child_request(
            p,
            repo.clone(),
            p.agent_type.clone(),
            p.model.clone(),
            None,
            budget,
            p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS),
        );
        let requester = crate::run::Requester::Operator {
            allow_wider_children: p.wider_children == Some(true),
        };
        self.start_child(me, env, req, task_id, requester, repo, decision, None)
    }

    /// The end every `agent/spawn` of a child shares, ordinary or review: launch it through
    /// [`Self::launch_child`], remember the task it was sent, and answer with its contract's id.
    #[allow(clippy::too_many_arguments)]
    fn start_child(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        req: crate::run::SpawnRequest,
        task_id: TaskId,
        requester: crate::run::Requester,
        repo: PathBuf,
        decision: std::sync::MutexGuard<'_, ()>,
        announce_to: Option<AgentId>,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        // Kept out of the thread's move, because the answer names it: the composing client reads
        // `contracts/<task_id>.json` and cannot mint this id itself (see `AgentSpawnResult`).
        let answered_task_id = task_id.clone();
        let sent = sent_task(&req);
        let (agent_id, state) = self.launch_child(
            me,
            env,
            req,
            task_id,
            requester,
            repo,
            decision,
            announce_to,
        )?;
        self.remember_sent(&agent_id, sent);
        Ok(marion_core::proto::result::AgentSpawnResult {
            state,
            agent_id,
            // §9's contract, named before it exists: this call answers at `Spawned`, and the id is
            // what lets the caller find the file when the run ends.
            task_id: Some(answered_task_id),
            note: None,
            race: None,
        })
    }

    /// **The child's tree is its caller's tree**, read off the entry `resolve_caller` has just
    /// proved this caller owns. Not derived from `<project-hash>` — that is the git common dir
    /// shared by every linked worktree of this repository, so deriving it would branch a
    /// feature-worktree node's children off the main tree's HEAD. See [`NodeHandle::repo`].
    fn caller_repo(
        &self,
        caller_id: &marion_core::proto::SpawnCaller,
    ) -> Result<PathBuf, RpcError> {
        lock(&self.nodes)
            .get(&caller_id.agent_id)
            .map(|n| n.repo.clone())
            .ok_or_else(|| {
                // Unreachable through `resolve_caller`, which compared this caller's token against
                // this same table. Kept as a refusal rather than an `expect` because the lock is
                // dropped and retaken between the two reads, and the honest answer to "the entry
                // went away" is not a panic in a supervisor that owns other nodes.
                RpcError::refused(
                    &caller_id.agent_id.0,
                    "this supervisor no longer owns that node, so it cannot say which tree a \
                     child of it would branch from.",
                    "§5.4",
                )
            })
    }

    /// What `node/get` shows for a child this supervisor started, filed once the node has an id.
    fn remember_sent(
        &self,
        agent_id: &AgentId,
        sent: Option<marion_core::proto::result::TaskSent>,
    ) {
        if let Some(sent) = sent {
            lock(&self.sent).insert(agent_id.clone(), sent);
        }
    }

    /// **A race: one spawn, one seat per candidate**, each an ordinary child of the caller under
    /// the same verification, tagged with the race and its seat. Everything that can refuse the
    /// race — its shape, an unknown agent type, the caller's child bound for every seat at once —
    /// refuses before anything is written; a seat whose launch then fails is reported as not
    /// started and the rest run, and only a race with no seat started at all is refused.
    ///
    /// The spawn decision is held across every seat's intent, so another spawn cannot take the
    /// room the gate counted for a later seat. The race is decided later, from the seats' own
    /// ends ([`Self::drive_race`]); this answers once the seats exist.
    pub(super) fn spawn_race(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        p: &marion_core::proto::params::AgentSpawnParams,
        caller_id: Option<&marion_core::proto::SpawnCaller>,
        repo: PathBuf,
        step: Option<&crate::run::StepLaunch>,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        use marion_core::proto::result::{RaceStarted, SeatStarted};
        use marion_core::race::{self, RacePolicy, RaceRole, RaceSeat};
        let race_refused =
            |subject: &str, e: race::RaceError| RpcError::refused(subject, e.to_string(), "race");
        if !p.agent_type.is_empty() {
            return Err(RpcError::refused(
                "agent_type",
                "a race names its seats in `candidates`, each `agent_type[:model]`; name either \
                 one `agent_type` or the candidates, not both.",
                "race",
            ));
        }
        if p.model.is_some() {
            return Err(RpcError::refused(
                "model",
                "each seat of a race names its own model, as `agent_type:model` in `candidates`.",
                "race",
            ));
        }
        if p.isolation == Some(Isolation::SharedCwd) {
            return Err(RpcError::refused(
                "isolation",
                "every seat of a race runs in its own worktree, so the seats cannot write over \
                 one another's work; `shared-cwd` would put them all in one directory.",
                "race, §6.6",
            ));
        }
        let candidates =
            race::parse_candidates(&p.candidates).map_err(|e| race_refused("candidates", e))?;
        race::check_race(p.verification.len()).map_err(|e| race_refused("verification", e))?;

        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        // A node's race seats its children; the operator's seats contracted nodes of its own.
        let caller = match caller_id {
            Some(c) => {
                let caller = self.resolve_caller(c)?;
                self.refuse_if_cancelling(c)?;
                Some(caller)
            }
            None => None,
        };
        let asked = p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS);
        // Every seat's clock, never past what is left of the caller's: refused before any seat.
        let wall = match caller_id {
            Some(c) => self.child_wall_secs(asked, &c.agent_id)?,
            None => asked,
        };
        let types = crate::run::agent_types(&repo)
            .map_err(|e| RpcError::refused("agent_type", e.to_string(), "§3.1"))?;
        let policy = RacePolicy::try_from(
            p.race
                .clone()
                .unwrap_or_default()
                .over(types.race().clone()),
        )
        .map_err(|e| race_refused("race", e))?;
        if let Some(c) = candidates
            .iter()
            .find(|c| types.resolve(&c.agent_type).is_none())
        {
            return Err(RpcError::refused(
                "candidates",
                format!(
                    "no agent type is named {:?}; known types are {}.",
                    c.agent_type,
                    types.names().join(", ")
                ),
                "§3.1",
            ));
        }
        // Every seat is one more child of the caller, counted now: the gate passes only if the
        // last seat would still fit. The operator's seats have no caller to gate them.
        if let Some(caller) = &caller {
            let extra = u32::try_from(candidates.len() - 1).unwrap_or(u32::MAX);
            agent_type::check_spawn_gates(
                &caller.agent_type,
                caller.depth,
                caller.live_children.saturating_add(extra),
            )
            .map_err(gate_refusal)?;
        }
        let requester = match &caller {
            Some(c) => crate::run::Requester::from(c.clone()),
            None => crate::run::Requester::Operator {
                allow_wider_children: p.wider_children == Some(true),
            },
        };

        let race_id = crate::run::entropy()
            .map(|e| race::new_race_id(crate::run::unix_millis(), e))
            .map_err(|e| {
                RpcError::internal(format!(
                    "marion could not mint a race id, so nothing was started: {e}"
                ))
            })?;
        self.races.open(&race_id, &requester.id());
        // Before the first seat's intent, whose fsync makes this durable too.
        if let Err(e) =
            self.journal_append(RecordKind::RaceOpened(marion_core::journal::RaceOpened {
                race_id: race_id.clone(),
                parent_id: requester.parent_id(),
                policy,
                seats: candidates.clone(),
            }))
        {
            self.races.close(&race_id);
            return Err(RpcError::internal(format!(
                "marion could not journal the race, so no seat was started: {e}"
            )));
        }

        let mut seats = Vec::with_capacity(candidates.len());
        let mut first: Option<(AgentId, NodeState, TaskId)> = None;
        for (i, candidate) in candidates.iter().enumerate() {
            let seat = u8::try_from(i + 1).unwrap_or(u8::MAX);
            let mut started = SeatStarted {
                seat,
                candidate: candidate.label(),
                agent_id: None,
                task_id: None,
                refused: None,
            };
            let launched = mint_task_id().and_then(|task_id| {
                let mut req = child_request(
                    p,
                    repo.clone(),
                    candidate.agent_type.clone(),
                    candidate.model.clone(),
                    Some(RaceSeat {
                        race_id: race_id.clone(),
                        role: RaceRole::Candidate(seat),
                    }),
                    self.child_budget(
                        &repo,
                        &candidate.agent_type,
                        p.budget_tokens,
                        caller_id.map(|c| &c.agent_id),
                    ),
                    wall,
                );
                // A workflow's race: each seat carries the step, as its own part.
                req.workflow = step.map(|s| crate::run::StepLaunch {
                    seat: marion_core::workflow::WorkflowSeat {
                        part: seat - 1,
                        ..s.seat.clone()
                    },
                    base: s.base.clone(),
                });
                let sent = sent_task(&req);
                let (agent_id, state) = self.launch_child(
                    me.clone(),
                    env.clone(),
                    req,
                    task_id.clone(),
                    requester.clone(),
                    repo.clone(),
                    &decision,
                    None,
                )?;
                self.remember_sent(&agent_id, sent);
                Ok((agent_id, state, task_id))
            });
            match launched {
                Ok((agent_id, state, task_id)) => {
                    started.agent_id = Some(agent_id.clone());
                    started.task_id = Some(task_id.clone());
                    first.get_or_insert((agent_id, state, task_id));
                }
                Err(e) => started.refused = Some(e.to_string()),
            }
            seats.push(started);
        }
        drop(decision);
        // A node that backgrounds its race is owed the decision as a message, and its inbox is held
        // open for it as for a backgrounded child's end; the seats announce nothing one by one.
        // Registered before the race can be decided, and only for a race that has a seat running.
        if let Some(c) = caller_id.filter(|_| p.notify_parent && first.is_some()) {
            let owed = self.inboxes.owe(&c.agent_id);
            self.races.announce_to(&race_id, c.agent_id.clone(), owed);
        }
        self.races.launched(&race_id);
        // A seat may already have ended; and a race with no seat started is decided here.
        self.drive_race(&race_id);
        let Some((agent_id, state, task_id)) = first else {
            let why = seats
                .iter()
                .filter_map(|s| {
                    s.refused
                        .as_deref()
                        .map(|r| format!("{}: {r}", s.candidate))
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(RpcError::refused(
                "candidates",
                format!("no seat of the race could be started — {why}"),
                "race",
            ));
        };
        Ok(marion_core::proto::result::AgentSpawnResult {
            agent_id,
            state,
            task_id: Some(task_id),
            // A seat is never a reviewer, so nothing about its write guard is owed.
            note: None,
            race: Some(RaceStarted { race_id, seats }),
        })
    }

    /// **Step a race and do what the step says** — the one driver, called wherever a race can
    /// change: a seat's thread after its end is filed, the spawn once every seat is started, the
    /// requester's own end, and a restart's boot. Reads the state afresh from the journal and the
    /// seats' contracts every time, so calling it twice, or after a restart, is always safe.
    pub(crate) fn drive_race(&self, race_id: &marion_core::race::RaceId) {
        use marion_core::race::Step;
        let crate::race::Drive::Go { requester } = self.races.begin_drive(race_id) else {
            return;
        };
        let Some(env) = self.spawn_env.as_ref() else {
            self.races.end_drive(race_id);
            return;
        };
        self.live.refresh();
        // The race and its seats' nodes, copied out so the contract files are read after the
        // registry lock is released.
        let Some((race, nodes)) = self.live.read(|r| {
            let race = r.tree().race(race_id)?.clone();
            let nodes: Vec<_> = race
                .seats
                .iter()
                .filter_map(|(_, id)| r.tree().get(id).cloned())
                .collect();
            Some((race, nodes))
        }) else {
            self.races.end_drive(race_id);
            return;
        };
        // The operator is not a node, and does not end: only a node's race can be abandoned.
        let requester_running = requester.0 == crate::run::OPERATOR_REQUESTER
            || self.owned_running(&requester) == Some(true);
        let Some((policy, state)) = crate::race::gather(
            &nodes,
            &env.project_dir,
            &race,
            &requester,
            requester_running,
        ) else {
            self.races.end_drive(race_id);
            return;
        };
        match marion_core::race::step(&policy, &state) {
            Step::Wait => self.races.end_drive(race_id),
            Step::Stop(seats) => {
                let ids: Vec<AgentId> = race
                    .seats
                    .iter()
                    .filter(|(seat, _)| seats.contains(seat))
                    .map(|(_, id)| id.clone())
                    .collect();
                let why = if state.abandoned {
                    StopReason::RaceAbandoned
                } else {
                    StopReason::RaceDecided
                };
                let told = self.races.to_stop(race_id, ids);
                // Released before stopping: a stopped seat's own thread drives the race again
                // once its end is filed, and that drive is what decides it.
                self.races.end_drive(race_id);
                for id in told {
                    if !self.stop_node(&id, why) {
                        // Not stoppable yet (still spawning): a later drive tries again.
                        self.races.forget_stop(race_id, &id);
                    }
                }
            }
            Step::Decided(result) => self.decide_race(&env.project_dir, &env.project_root, result),
        }
    }

    /// Write a decided race: its scoreboard file first, then the journal record that says it is
    /// decided, so a reader that sees the record finds the file.
    fn decide_race(
        &self,
        project: &marion_core::paths::ProjectDir,
        repo: &std::path::Path,
        mut result: marion_core::race::RaceResult,
    ) {
        // Losers' branches go first, so the scoreboard records what is actually gone.
        for seat in result.prunable() {
            if let Some(row) = result.seats.iter_mut().find(|r| r.seat == seat) {
                row.pruned = crate::race::prune_branch(repo, project, row);
            }
        }
        if let Err(e) = crate::race::write_result(project, &result) {
            // The race stays open and a later drive (another seat's end, the next boot) retries.
            eprintln!(
                "marion: could not write race {}'s result, left open: {e}",
                result.race_id.0
            );
            self.races.end_drive(&result.race_id);
            return;
        }
        if let Err(e) = self.journal_now(RecordKind::RaceDecided(crate::race::decided_record(
            &result,
        ))) {
            eprintln!(
                "marion: could not journal race {}'s decision: {e}",
                result.race_id.0
            );
        }
        if let Some((parent, owed)) = self.races.take_announcement(&result.race_id) {
            self.announce_race_end(&parent, &result, owed);
        }
        self.races.close(&result.race_id);
        // A workflow's race step is decided by its race: step the run on.
        let run = self.live.read(|r| {
            result
                .seats
                .iter()
                .filter_map(|row| row.agent_id.as_ref())
                .find_map(|id| r.tree().get(id)?.intent.as_ref()?.workflow.clone())
        });
        if let Some(seat) = run {
            self.drive_workflow(&seat.wf_id);
        }
    }

    /// **The one seam through which marion stops a node it decided to stop itself** — a race's
    /// loser once a first passer has won, a seat whose requester is gone. Today that is §6.7's kill,
    /// through [`Self::kill_node`] exactly as `node/kill` does it and recorded as cancelled; a
    /// graceful cancel can replace this body without touching its callers. `false` when the node
    /// cannot be stopped yet (no recorded pid), or its own thread already saw it end.
    fn stop_node(&self, agent_id: &AgentId, why: StopReason) -> bool {
        let _decision = lock(&self.quit);
        self.live.refresh();
        let Some(node) = self.live.read(|r| r.tree().get(agent_id).cloned()) else {
            return false;
        };
        if node.state.is_exited() {
            return true;
        }
        let retire = node.reap_state == ReapState::ReapedIdle;
        if !retire && (node.pid.is_none() || !self.claim_kill(agent_id)) {
            return false;
        }
        if let Err(e) = self.kill_node(&node, KillBy::Stop(why)) {
            eprintln!("marion: could not stop `{}`: {}", agent_id.0, e.message);
            return false;
        }
        self.live.refresh();
        true
    }

    /// **A restart re-drives every race its predecessor left open.** Decided now if every seat has
    /// ended; a seat marion lost counts as cancelled. Its requester is gone with the predecessor, so
    /// the result is the record's, never delivered.
    pub(super) fn redrive_open_races(&self) {
        let open: Vec<_> = self.live.read(|r| {
            r.tree()
                .races()
                .iter()
                .filter(|race| race.decided.is_none())
                .filter_map(|race| {
                    // A race with no parent is the operator's.
                    let requester = race
                        .opened
                        .as_ref()?
                        .parent_id
                        .clone()
                        .unwrap_or_else(|| AgentId(crate::run::OPERATOR_REQUESTER.into()));
                    Some((race.race_id.clone(), requester))
                })
                .collect()
        });
        for (race_id, requester) in open {
            self.races.open(&race_id, &requester);
            self.races.launched(&race_id);
            self.drive_race(&race_id);
        }
    }

    /// **The node a review names, read and checked**: known, ended, a child with a contract that
    /// changed something. Every refusal is [`crate::review::refusal`]'s plain sentence. Shared by a
    /// fresh review and by the resume of a reviewer, so a resumed reviewer is as read-only and as
    /// grounded as the one it continues.
    pub(super) fn reviewed(
        &self,
        env: &crate::run::Env,
        target: &AgentId,
    ) -> Result<Reviewed, RpcError> {
        let refuse = |why: String| RpcError::refused(&target.0, why, "review");
        let (intent, state, model) = self
            .live
            .read(|r| {
                r.tree()
                    .get(target)
                    .map(|n| (n.intent.clone(), n.state, n.model.clone()))
            })
            .ok_or_else(|| {
                refuse(crate::review::refusal(
                    target,
                    "no node with that id is in this project's journal",
                ))
            })?;
        if !state.is_exited() {
            return Err(refuse(crate::review::refusal(
                target,
                "it has not ended yet; wait for it to finish, then ask again",
            )));
        }
        let intent = intent.ok_or_else(|| {
            refuse(crate::review::refusal(
                target,
                "its journal has no spawn record, so marion cannot tell what it was asked to do",
            ))
        })?;
        let task_id = intent.task_id.clone().ok_or_else(|| {
            refuse(crate::review::refusal(
                target,
                "it is a root, which has no task contract to review",
            ))
        })?;
        let path = env.project_dir.agent(target).contract(&task_id);
        let contract = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| {
                serde_json::from_slice::<marion_core::contract::TaskContract>(&b)
                    .map_err(|e| e.to_string())
            })
            .map_err(|e| {
                refuse(crate::review::refusal(
                    target,
                    &format!("its contract at {} could not be read ({e})", path.display()),
                ))
            })?;
        let review = crate::review::target(target, &contract).map_err(refuse)?;
        Ok(Reviewed {
            review,
            contract,
            intent,
            model,
        })
    }

    /// **`agent/spawn` with `review_of`: a read-only reviewer of an ended node** ([`crate::review`]).
    ///
    /// One spawn path for both client forms: `marion review` sends this with no `caller` (the
    /// operator, authorized as a root spawn is, naming the `repo`), and a parent model sends it
    /// through its bridge's `spawn` with its own token. Either way the reviewer is placed **under
    /// the node it reviews** — the reviewed node stands as the caller for §6.1's gates, the tree
    /// and the contract's `requester` — so the tree shows the review where the work is.
    ///
    /// Refused, in plain words and before anything is written, when the node is unknown, is a
    /// root, has not ended, or changed nothing.
    fn spawn_review(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        p: &marion_core::proto::params::AgentSpawnParams,
        target: &AgentId,
        peer: Peer,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        let repo = match p.caller.as_ref() {
            Some(c) => self.authenticate(c).ok_or_else(|| {
                RpcError::refused(
                    &c.agent_id.0,
                    "this supervisor did not mint that node token, so it will not start a review \
                     on its word.",
                    "§5.4",
                )
            })?,
            None => {
                root_spawn_authorized(peer)?;
                let repo = p.repo.clone().ok_or_else(|| {
                    RpcError::internal("a review with no caller reached the launcher with no repo")
                })?;
                check_project(&env, &repo)?;
                repo
            }
        };
        let decision = lock(&self.spawn_decision);
        self.live.refresh();
        let Reviewed {
            review,
            contract,
            intent,
            model,
        } = self.reviewed(&env, target)?;
        let agent_type = tree_types(&repo)?
            .resolve(&intent.agent_type)
            .ok_or_else(|| {
                Unprojectable::UnknownAgentType(intent.agent_type.clone()).as_error(target)
            })?;
        let caller = crate::run::Caller {
            agent_id: target.0.clone(),
            agent_type,
            depth: intent.depth,
            live_children: self.live_children_of(target),
        };
        agent_type::check_spawn_gates(&caller.agent_type, caller.depth, caller.live_children)
            .map_err(gate_refusal)?;
        let task_id = mint_task_id()?;
        // No type named: a reviewer from another model family than the work's.
        let reviewer = match p.agent_type.trim() {
            "" => crate::review::default_reviewer(intent.harness, model.as_deref()).to_string(),
            named => named.to_string(),
        };
        // A reviewer on a row that cannot refuse a write is allowed when asked for by name, and
        // said so in the answer; its contract says it again (`run::record_review`).
        let note = tree_types(&repo)?
            .resolve(&reviewer)
            .and_then(|t| crate::review::unguarded(t.harness))
            .map(str::to_string);
        let req = crate::run::SpawnRequest {
            race: None,
            budget: self.child_budget(&repo, &reviewer, None, Some(target)),
            agent_type: reviewer,
            prompt: crate::review::prompt(&review, &contract),
            repo: repo.clone(),
            acceptance_criteria: vec![],
            verification: vec![],
            writable_scope: vec![],
            timeout_secs: self
                .child_wall_secs(p.timeout_secs.unwrap_or(DEFAULT_SPAWN_TIMEOUT_SECS), target)?,
            model: p.model.clone(),
            isolation: Isolation::Worktree,
            allow_concurrent_writes: false,
            resume: None,
            profile: None,
            review: Some(review),
            read_only: false,
            workflow: None,
        };
        // A parent that asked for its child's review in the background is owed its end.
        let announce_to = p
            .caller
            .as_ref()
            .filter(|_| p.notify_parent)
            .map(|c| c.agent_id.clone());
        self.start_child(
            me,
            env,
            req,
            task_id,
            caller.into(),
            repo,
            decision,
            announce_to,
        )
        .map(|r| marion_core::proto::result::AgentSpawnResult { note, ..r })
    }

    /// **The child launcher, driven from a fully-built [`crate::run::SpawnRequest`]** — the mirror
    /// of [`Self::launch_root`], and for the same reason: `agent/spawn` and `node/resume` are one
    /// launch path, differing only in the request (`agent/spawn` builds a fresh one, `node/resume`
    /// reconstructs a node's own with its id, its session and the workspace it ran in on it). A
    /// second launcher beside this one is how a resumed child ends up gated, journaled or claimed
    /// differently from a spawned one.
    ///
    /// `repo` is the tree this node's own children branch from, `decision` is §6.1 step 2's
    /// serialization — held until the node's intent is durable and dropped here, not by the caller,
    /// so the window it covers is the same on both paths. Returns once the process exists, with the
    /// node's state as the journal has it.
    ///
    /// `decision` is the guard itself, or a borrow of it for a race: dropping a borrow releases
    /// nothing, so the guard spans every seat's intent and the caller drops it after the last.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn launch_child<Decision>(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        req: crate::run::SpawnRequest,
        task_id: TaskId,
        requester: crate::run::Requester,
        repo: PathBuf,
        decision: Decision,
        announce_to: Option<AgentId>,
    ) -> Result<(AgentId, NodeState), RpcError> {
        let (tx, progress) = std::sync::mpsc::channel();
        let observer = NodeOwner {
            handle: me.clone(),
            task_id: Some(task_id.clone()),
            // The child inherits its caller's tree and hands the same one to *its* children.
            repo,
            tx,
            identified: Mutex::new(None),
            announce_to,
            owes: Default::default(),
        };
        // **Everything the thread needs is owned**, for `Background::start`'s reason: this work
        // outlives the JSON-RPC frame that asked for it, so it cannot borrow from this stack frame.
        let owner = me.clone();
        let join = std::thread::spawn(move || {
            let agent_type = req.agent_type.clone();
            // **A panic here must still resolve the node.** See [`caught`]: without this the
            // outcome stays `None` for ever and the supervisor can never exit.
            let outcome = caught(&agent_type, spawn_panicked, || {
                crate::run::run_spawn_watched(&env, &req, &task_id, &requester, &observer)
            });
            // The agent id is known only if `identified` fired. A spawn refused above it — an
            // unknown agent type, a scope outside the ceiling — never minted a node, so there is
            // nothing to file the outcome under and nothing holding the supervisor open.
            if let Some(agent_id) = observer.identified_id() {
                // The parent's owed announcement, before the outcome moves into the table.
                // Owed where the parent's inbox was open when this child started; where it was not,
                // the parent had ended and the end goes to the nearest live ancestor.
                if let Some(parent) = &observer.announce_to {
                    owner.announce_child_end(
                        parent,
                        steer::ChildEnd {
                            child: &agent_id,
                            agent_type: &agent_type,
                            task_id: &task_id,
                            outcome: &outcome,
                        },
                        observer.owes.load(std::sync::atomic::Ordering::SeqCst),
                    );
                }
                owner.mark_finished(&agent_id, NodeOutcome::Child(Box::new(outcome)));
                // A seat's end may decide its race; its contract is on disk by now.
                if let Some(seat) = &req.race {
                    owner.drive_race(&seat.race_id);
                }
                // And a workflow step's end may decide its step and start the next.
                if let Some(seat) = &req.workflow {
                    workflow::after_step_node(&owner, seat);
                }
            } else if let Err(e) = &outcome {
                let _ = observer.tx.send(Progress::Refused(e.to_string()));
            }
            // Sent last and unconditionally, so a launch that failed before either earlier moment
            // cannot leave the call waiting out `LAUNCH_BOUND` for something that will not come.
            let _ = observer.tx.send(Progress::Finished);
        });

        // **The response is sent when the process exists, not when the run finishes.** A child
        // spawn that blocked until its contract was written would put a minutes-long call on the
        // JSON-RPC surface, which `background.rs` records as taking a whole bridge down when it
        // hangs.
        let deadline = std::time::Instant::now() + LAUNCH_BOUND;
        let recv = |deadline: std::time::Instant| {
            progress.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        };
        let agent_id = match recv(deadline) {
            Ok(Progress::Identified(id)) => id,
            // `run_spawn` refused before it minted an id — an unknown agent type, a writable scope
            // outside that type's ceiling, a less-contained child. There is no node; the caller is
            // told the refusal in `run_spawn`'s own words.
            Ok(Progress::Refused(why)) => {
                return Err(RpcError::refused("agent_type", why, "§6.1"));
            }
            Ok(_) => return Err(spawn_refused_before_the_node_existed()),
            Err(_) => return Err(launch_bound_expired(None)),
        };
        // The node is in the table and its intent is durable, so no later spawn can miss it in the
        // count. Everything from here on is this one node's own launch, which no other caller's
        // gate depends on.
        drop(decision);
        // The join handle is filed now rather than at `spawn`, because the table's key is the id
        // this call has only just learned. Nothing races: `Progress::Identified` is sent from
        // inside `claim`, so the entry exists before this line can run.
        //
        // **A call that gave up before this point leaves the thread detached**, and that is
        // deliberate rather than overlooked: the node is still claimed, so §5.7 still refuses to
        // exit while it runs, and the alternative — holding the handle somewhere keyed by nothing
        // — would be a second table to keep consistent with this one.
        if let Some(node) = lock(&self.nodes).get_mut(&agent_id) {
            node.join = Some(join);
        }
        match recv(deadline) {
            Ok(Progress::Started) => {}
            // The launch failed between the intent and the process: no worktree, a config that
            // would not compile, a row promising a tool the harness lacks. The thread filed its
            // reason through `mark_finished` before it sent `Finished`, so it is there to quote.
            Ok(_) => {
                return Err(spawn_failed_before_the_process_existed(
                    &agent_id,
                    self.owned_failure(&agent_id).as_deref(),
                ));
            }
            Err(_) => return Err(launch_bound_expired(Some(&agent_id))),
        }
        self.live.refresh();
        Ok((agent_id.clone(), self.spawned_state(&agent_id)))
    }

    /// **§11 item 28 step 6: a client creating a root, served.**
    ///
    /// The same shape step 4 established for a child and for the same reasons — a thread per node,
    /// the node claimed the instant it has an identity, the caller answered when a **process
    /// exists** rather than when the run finishes — over `root::prepare`/`root::launch_owned`
    /// instead of `run_spawn`, because those are what make a root a root (§9: no parent, no
    /// contract, `ROOT_DEPTH`, the change record over the operator's own checkout).
    ///
    /// **What is different from the child path, stated rather than left to be noticed:**
    ///
    /// * **No §6.1 step 2 gate.** Those gates read the *caller's* type and depth, and there is no
    ///   caller. A root is depth 0 by definition and has no parent whose `max_concurrent_children`
    ///   it could exceed. The authorization that does apply is [`root_spawn_authorized`], which is
    ///   about the connection rather than about the tree.
    /// * **No [`Self::spawn_decision`].** That lock exists to make a gate evaluation and the write
    ///   derived from it indivisible; with no gate there is nothing to serialize, and holding it
    ///   across a root's `prepare` — which snapshots a working tree — would block every child spawn
    ///   in the fleet on one `git` walk.
    /// * **No task id.** §9: a root has no `TaskContract`. See [`NodeHandle::task_id`].
    /// * **The outcome is filed on every exit path**, including a `prepare` that failed before the
    ///   `SpawnIntent` was journaled. A root is claimed *before* its intent (see
    ///   `root::prepare_watched`), so a thread that returned without filing one would leave a
    ///   claimed, never-running node holding this supervisor open against §5.7 for ever.
    fn spawn_root(
        &self,
        me: Arc<RegistryHandle>,
        env: crate::run::Env,
        p: &marion_core::proto::params::AgentSpawnParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::AgentSpawnResult, RpcError> {
        root_spawn_authorized(peer)?;
        if let Some(context) = p.native_launch.as_deref() {
            return Err(native_launch_refusal(context));
        }
        let repo = p.repo.clone().ok_or_else(|| {
            // Unreachable: the frame-shape match above refuses `(None, None)` by name before any
            // state is consulted. A refusal rather than an `expect`, because a supervisor owning a
            // fleet must not be taken down by a shape it has already answered.
            RpcError::internal(
                "a root spawn reached the launcher with no repository, which the frame-shape check \
                 refuses by name",
            )
        })?;
        // **The repository must be one this socket serves** — before `NodeOwner::claim` and before
        // any side effect, because everything after this point writes somewhere.
        //
        // `root_spawn_authorized` answers *who* is calling and nothing about *what they named*. A
        // same-uid process that dials this project's socket and states another project's `repo`
        // would otherwise be obeyed: `root::prepare_watched` keys the agent directory and the
        // `SpawnIntent` on `ProjectDir::new(state, project_root(&spec.repo))`, so the node would be
        // journaled under the *named* project while this supervisor's `LiveRegistry` went on
        // following its own. marion would answer "spawned" for a root that never appears in
        // `tree/subscribe`, `session/quit` or any other tree operation on the socket that started
        // it — an authorization hole and a broken contract in one.
        //
        // The comparison is on the **project key**, not on the path, and that is what keeps §2's
        // worktree rule intact: `project_root` resolves to the git common dir, so every linked
        // worktree of one repository hashes to the same `<project-hash>` and a legitimate worktree
        // root is accepted. What it rejects is a `repo` in a *different* repository, which is the
        // only case that could put a record in another project's journal.
        check_project(&env, &repo)?;
        // Resolved here so an unknown type is refused **in the frame that asked for it** rather
        // than arriving as a node that was never going to start. `root::prepare` refuses it again
        // one layer down; two call sites of one lookup, never two rules.
        let agent_type = tree_types(&repo)?
            .resolve(&p.agent_type)
            .ok_or_else(spawn_refused_before_the_node_existed)?;
        // §9's node-level bound is resolved inside the spec — see [`root_spec_from_spawn`] — so
        // the value the launch enforces is the value `prepare` journals.
        let spec = root_spec_from_spawn(p, repo.clone(), &env, &agent_type);

        let (agent_id, state) = self.launch_root(me, spec, repo)?;
        Ok(marion_core::proto::result::AgentSpawnResult {
            state,
            agent_id,
            // §9: a root has no `TaskContract`, so there is no file to name. See
            // [`NodeHandle::task_id`], which is `None` here for the same reason.
            task_id: None,
            note: None,
            race: None,
        })
    }

    /// **The root launcher, driven from a fully-built [`RootSpec`]** — spawn and resume are one
    /// launch path, differing only in the spec (`agent/spawn` builds a fresh one, `node/resume`
    /// reconstructs a node's own with an id and a session on it). The thread owns the node for its
    /// whole life through [`NodeOwner`]; `repo` is the tree its children branch from. Returns once
    /// the process exists, with the node's state as the registry has it at that instant —
    /// `Running` once `journal::confirm_spawned`'s records have been followed, `Spawning` if the
    /// tail has not caught up yet — the same instant `agent/spawn` and `node/resume` both answer
    /// at.
    fn launch_root(
        &self,
        me: Arc<RegistryHandle>,
        spec: crate::root::RootSpec,
        repo: PathBuf,
    ) -> Result<(AgentId, NodeState), RpcError> {
        // §9's bound, off the spec that journaled it: the clock this launch is held to and the
        // clock the node's `SpawnIntent` records are one number by construction.
        let bound = std::time::Duration::from_secs(spec.bound_secs);
        let (tx, progress) = std::sync::mpsc::channel();
        let owner = me.clone();
        let join = std::thread::spawn(move || {
            let observer = NodeOwner {
                handle: me,
                // §9: a root has no contract.
                task_id: None,
                // The tree the client named, remembered so this root's own children branch from it
                // — which is the whole of what step 5 needs from step 6.
                repo,
                tx,
                identified: Mutex::new(None),
                announce_to: None,
                owes: Default::default(),
            };
            // **Wrapped for [`caught`]'s reason**, in the root's own vocabulary: `NodeOutcome::Root`
            // carries marion's sentence rather than a `SpawnError`, so the panic becomes that
            // sentence. The alternative — leaving the root path uncaught because only the child path
            // has an `AbortOnDrop` — would keep the whole defect alive on the one node that holds
            // the supervisor open on its own.
            let outcome = caught(&spec.agent_type, root_panicked, || -> Result<(), String> {
                let node = crate::root::prepare_watched(&spec, &observer)
                    .map_err(|e| format!("marion could not prepare the root node: {e}"))?;
                // The trait's second hook, called by the launcher at `command.spawn()`. In
                // scope explicitly rather than through a blanket import, so the two halves of
                // `SpawnObserver` are visibly the same trait here as on the child path.
                use crate::run::SpawnObserver as _;
                let started = |pid: i32| observer.started(&node.agent_id, pid);
                // The same attribution the child path asks through the trait: did a `node/kill`
                // (or KillTree) end this root, so its terminal record is the kill's?
                let ended_by_kill = || observer.process_ended(&node.agent_id);
                // And the ask between a failed attempt and a key rotation's next, which settles
                // nothing: a kill asked for then ends the root rather than its next credential.
                let kill_requested = || observer.ended_by(&node.agent_id).is_some();
                crate::root::launch_owned(
                    &node,
                    bound,
                    node.boot.budget(),
                    // No live sink here: this runs inside `marion-supervisor`, and the client
                    // watches through `node/attach` over the socket rather than through a pipe
                    // this process would have to own. `events.jsonl` is what both legs read.
                    None,
                    Some(&started),
                    // §9's M3: where a pane goes if this run asked for one. The same observer,
                    // because it is the same ownership — this supervisor answers `node/attach`,
                    // so it is the only process for which a registered pane means anything.
                    Some(&observer),
                    Some(&ended_by_kill),
                    Some(&kill_requested),
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
            });
            // **Filed whenever there is a node to file it under**, which after `identified` there
            // always is. See this function's doc for why a missing outcome would be a supervisor
            // that can never exit.
            if let Some(agent_id) = observer.identified_id() {
                owner.mark_finished(&agent_id, NodeOutcome::Root(outcome));
            }
            // Last and unconditional, so a launch that failed before either earlier moment cannot
            // leave the call waiting out `LAUNCH_BOUND` for something that will not come.
            let _ = observer.tx.send(Progress::Finished);
        });

        let deadline = std::time::Instant::now() + LAUNCH_BOUND;
        // `prepare_watched` calls `identified` before its first side effect, so nothing but
        // minting the id itself can fail ahead of it — an unreadable entropy source.
        let agent_id = await_identified(&progress, deadline)?;
        self.file_join(&agent_id, join);
        self.await_root_started(&progress, deadline, &agent_id)?;
        self.live.refresh();
        let state = self.spawned_state(&agent_id);
        Ok((agent_id, state))
    }

    /// **Park a launch thread's handle under the node it has just identified.**
    ///
    /// Filed now rather than at `spawn`, because the table's key is the id the launch has only just
    /// learned. Nothing races: `Progress::Identified` is sent from inside `claim`, so the entry
    /// exists before this can run. A launch that gave up before this point leaves the thread
    /// detached, and that is deliberate rather than overlooked: the node is still claimed, so §5.7
    /// still refuses to exit while it runs, and the alternative — holding the handle somewhere
    /// keyed by nothing — would be a second table to keep consistent with this one.
    fn file_join(&self, agent_id: &AgentId, join: std::thread::JoinHandle<()>) {
        if let Some(node) = lock(&self.nodes).get_mut(agent_id) {
            node.join = Some(join);
        }
    }

    /// The root launch's second wait: a process exists, or the reason there is none.
    ///
    /// The launch failed between the identity and the process: a working tree marion could not
    /// snapshot, a `<state>` inside the repository, a configuration document that would not
    /// compile, an unsupported root surface.
    ///
    /// **Answered with the reason, not merely with the fact.** Every one of those is a `RootError`
    /// marion wrote as a sentence for an operator — it names the directory, the declaration and the
    /// way through — and until step 6 `marion run` printed it from the error in its own hand. The
    /// supervisor holds it now, so a client that got the generic sentence back would be told a run
    /// failed and never told what to change. [`Self::owned_failure`] is where the thread filed it,
    /// and it is filed before `Progress::Finished` is sent, so it is there by the time this reads.
    fn await_root_started(
        &self,
        progress: &std::sync::mpsc::Receiver<Progress>,
        deadline: std::time::Instant,
        agent_id: &AgentId,
    ) -> Result<(), RpcError> {
        match recv_progress(progress, deadline) {
            Ok(Progress::Started) => Ok(()),
            Ok(_) => Err(root_launch_failed(
                agent_id,
                self.owned_failure(agent_id).as_deref(),
            )),
            Err(_) => Err(launch_bound_expired(Some(agent_id))),
        }
    }

    /// **The state a launch answers with, read back off the registry rather than asserted.** The
    /// state this returns is the state the journal says, which is the point of answering at
    /// `Spawned` rather than before it: a client that renders `Running` — or a `Spawning` the
    /// registry's tail has not yet moved past — is rendering a record it could have read itself.
    pub(super) fn spawned_state(&self, agent_id: &AgentId) -> NodeState {
        self.live
            .read(|r| r.tree().get(agent_id).map(|n| n.state))
            .unwrap_or(NodeState::Spawning)
    }

    /// **`node/resume` — relaunch a lost node under its own id** (`plan-restart-resume.md` step 6).
    ///
    /// §8's rule is that a lost session is *relaunched*, never re-opened: the process died with the
    /// supervisor that held it, and every harness's resume starts a **new** process against the
    /// single-writer transcript. So this is not `node/prompt` reaching a live channel — there is
    /// none — it is `agent/spawn`'s own launch path fed a [`RootSpec`] rebuilt from the node's
    /// journal, with the node's own id and the session its stream named carried onto it.
    ///
    /// The preflight is in the order §7.2 and §6.7 impose, each refusal naming the node rather than
    /// guessing past it:
    ///
    /// 1. the node exists, and its intent says what it was — a node with no `SpawnIntent` has no
    ///    type, no harness and no depth, and a launch rebuilt from that would be a guess;
    /// 2. its process is finished — an `Orphaned`/`ReapedIdle`/exited node — never a live one;
    /// 3. its stream named a session — or, where its row lists sessions by title, the harness's own
    ///    listing names one under the node's title ([`Self::session_by_title`]) — else there is
    ///    nothing to resume and the refusal says so;
    /// 4. its recorded process is provably not still running: `AliveAndOurs` is killed first (the
    ///    same confirmed group kill `session/quit` uses), `CannotTell` refuses rather than risk a
    ///    second live process against one transcript, `Gone` proceeds.
    ///
    /// Then the node's own depth selects the launcher — [`Self::relaunch_root`] or
    /// [`Self::relaunch_child`], which carries the three further refusals a child needs and a root
    /// cannot need. Both go through the launcher `agent/spawn` uses at that depth: **one spawn
    /// path**, with the resume as a parameter to it.
    pub(super) fn node_resume(
        &self,
        p: &marion_core::proto::params::NodeResumeParams,
        peer: Peer,
    ) -> Result<marion_core::proto::result::NodeResumeResult, RpcError> {
        // A resume starts work under a root's authority — the same filesystem-permission gate
        // `agent/spawn`'s root path answers, for the same reason (a resumed root has an id, but the
        // caller asking to resume it is not that node).
        root_spawn_authorized(peer)?;
        let me = self.me.upgrade().ok_or_else(|| {
            RpcError::internal(
                "this supervisor is being dropped and will not relaunch a node it could not own",
            )
        })?;
        let Some(env) = self.spawn_env.clone() else {
            return Err(RpcError::unimplemented(
                "node/resume",
                "this handle describes nodes and owns none, so there is no environment to relaunch \
                 one in. A detached supervisor built by `RegistryHandle::owning` serves this.",
                "§2",
            ));
        };
        self.live.refresh();
        let node = self
            .live
            .read(|r| r.tree().get(&p.agent_id).cloned())
            .ok_or_else(|| {
                RpcError::refused(
                    "agent_id",
                    format!(
                        "no node `{}` is on this project's journal, so there is nothing to resume.",
                        p.agent_id.0
                    ),
                    "§2, §7.2",
                )
            })?;
        // **A node with no intent has no depth, and a relaunch of it would be a guess.** Its
        // agent type, its harness, its parent and its position are all on that one record; a
        // journal whose head was compacted away yields a node marion has records *about* and no
        // identity *for*, which is not something to rebuild a launch from.
        let Some(depth) = node.depth() else {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` has records on this journal but no `SpawnIntent`, so marion cannot say \
                     what it was — its type, its harness, or where it sat in the tree. Refused by \
                     name rather than relaunched as a guess.",
                    p.agent_id.0
                ),
                "§4.3, §7.2",
            ));
        };
        // **A live node is not resumed — it is attached to.** Only a node whose fate is decided (an
        // orphan the last restart marked, an idle node deliberately reaped, or one that exited) has
        // a process to relaunch in place of.
        let resumable = matches!(node.reap_state, ReapState::Orphaned | ReapState::ReapedIdle)
            || node.state.is_exited();
        if !resumable {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` is still live ({:?}); marion holds its channel, so a new turn is \
                     `node/prompt` and reaching it is `node/attach`. Resume relaunches a node whose \
                     process is gone, never one that is running.",
                    p.agent_id.0, node.state,
                ),
                "§8, §7.2",
            ));
        }
        // **A session the stream never named** is looked for under the title marion gave it: a node
        // whose supervisor died during its first request may have a session its harness names only
        // once a response streams. Where the harness says so, that listing also says where the
        // session was created — the tree a child's resume must run in, which only a named session
        // would otherwise have journaled.
        let mut node = node;
        let session = match node.harness_session.clone() {
            Some(session) => session,
            None => {
                let found = self.session_by_title(&node, &env).ok_or_else(|| {
                    RpcError::refused(
                        "agent_id",
                        format!(
                            "`{}` named no harness session — its stream never carried one, or it \
                             ran on a harness marion reads no session from, and no listing of its \
                             harness's sessions names one under its title — so marion cannot hand \
                             a resume back to the harness. Refused by name rather than started \
                             fresh under a resumed session's id, which would misdescribe the run.",
                            p.agent_id.0
                        ),
                        "§8",
                    )
                })?;
                if node.launch_workspace.is_none() && depth > 0 {
                    node.launch_workspace = found
                        .directory
                        .as_deref()
                        .and_then(|dir| listed_workspace(&node, &env, dir));
                }
                found.id
            }
        };
        // **The recorded process must be provably not still running before a second one is started
        // against the same single-writer transcript** (principle 8). A node with no recorded pid
        // never reached `command.spawn()` under the lost supervisor, so there is nothing to signal.
        if let Some(pid) = node.pid {
            match crate::procid::resolve(node.start_id.as_ref(), crate::procid::read(pid)) {
                crate::procid::Resolution::AliveAndOurs => {
                    self.journal_append(RecordKind::KillIntent(KillIntent {
                        agent_id: node.agent_id.clone(),
                        was: node.state,
                    }))
                    .map_err(journal_failure_before_signal)?;
                    if !self.runtime.kill_process_tree_and_wait(pid) {
                        return Err(RpcError::internal(format!(
                            "`{}` was still running its own process and marion journaled the intent \
                             to kill it before resuming, but could not observe its PID dead; the \
                             intent remains unconfirmed and marion will not start a second process \
                             against one transcript (§6.7, §8)",
                            node.agent_id.0
                        )));
                    }
                    self.journal_append(RecordKind::KillConfirmed(KillConfirmed {
                        agent_id: node.agent_id.clone(),
                        exit: ProcessExit {
                            code: None,
                            signal: Some(9),
                            description: "marion sent SIGKILL to the surviving process before \
                                          resuming its node"
                                .into(),
                        },
                    }))
                    .map_err(journal_failure_after_signal)?;
                }
                crate::procid::Resolution::CannotTell(_) => {
                    return Err(RpcError::conflict(
                        &node.agent_id.0,
                        "marion cannot prove `node`'s recorded process is gone — a process wears \
                         its PID and no recorded identity settles whether it is the same one — so \
                         it will not start a second process that might run against a transcript a \
                         first is still writing. Nothing was signalled or launched.",
                        "§6.7, §8",
                    ));
                }
                crate::procid::Resolution::Gone => {}
            }
        }
        // **The tree this node's own children branch from**, and it is the same value at every
        // depth: `marion run` gives the root the project's working tree, and every `agent/spawn`
        // hands a child its caller's, so one tree runs the whole fleet. A resumed node at any depth
        // therefore hands its own children what it would have handed them in its first life.
        let repo = resumable_root_cwd(&env.project_root);
        let (agent_id, state) = if depth == 0 {
            self.relaunch_root(me, &node, &env, &p.prompt, repo, session)?
        } else {
            self.relaunch_child(me, &node, &env, &p.prompt, repo, session)?
        };
        Ok(marion_core::proto::result::NodeResumeResult {
            agent_id,
            state,
            // The lifetime this relaunch begins: replay folds a second `Spawned` as generation two,
            // so the count the caller reads is the recorded one plus this launch.
            spawn_generation: node.spawn_generation + 1,
        })
    }

    /// **The session a node's stream never named, found under the title marion launched it with**
    /// — the row's [`marion_harness::grammar::TitleLookup`]: the harness's own read-only listing,
    /// run with the node's own environment (where the harness keeps its session store) and the
    /// row's update switch, from the project's tree, bounded. `None` where the row lists no
    /// sessions by title, the listing could not run, or it names none — or more than one — by this
    /// node's title.
    fn session_by_title(
        &self,
        node: &ReplayedNode,
        env: &crate::run::Env,
    ) -> Option<marion_harness::grammar::TitledSession> {
        let harness = node.intent.as_ref()?.harness;
        let lookup = marion_harness::adapter::adapter_for(harness)
            .ok()?
            .session_lookup()?;
        let spec = marion_harness::adapter::harness_spec(harness);
        let (vars, args) = marion_harness::probe::ProbeSwitch::bare(spec).ok()?;
        let cwd = resumable_root_cwd(&env.project_root);
        let fields = marion_harness::spec::Fields {
            cwd: cwd.clone(),
            config_dir: env.project_dir.agent(&node.agent_id).config_dir(),
            auth: env.auth,
            ..Default::default()
        };
        let set: Vec<(String, String)> = marion_harness::spec::render_env(spec.env, &fields)
            .into_iter()
            .chain(vars)
            .collect();
        let filter = marion_harness::env_filter::InheritFilter {
            login: spec.login_env,
            auth: env.auth,
            passthrough: vec![],
        };
        let mut listing = std::process::Command::new(spec.program?);
        listing
            .args(args)
            .args(lookup.argv)
            .envs(set.iter().cloned())
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null());
        for key in marion_harness::invocation::removals_for(Some(&filter), &set) {
            listing.env_remove(key);
        }
        let out = crate::run::run_bounded(&mut listing, SESSION_LISTING_BOUND).ok()?;
        if out.timed_out || out.code != Some(0) {
            return None;
        }
        marion_harness::grammar::session_by_title(
            lookup,
            &String::from_utf8_lossy(&out.stdout),
            &marion_harness::grammar::session_title(&node.agent_id),
        )
    }

    /// [`Self::node_resume`]'s root arm: [`crate::root::RootSpec`] rebuilt from the node's journal,
    /// through the same [`Self::launch_root`] `agent/spawn` uses.
    fn relaunch_root(
        &self,
        me: Arc<RegistryHandle>,
        node: &marion_core::registry::ReplayedNode,
        env: &crate::run::Env,
        prompt: &str,
        repo: PathBuf,
        session: String,
    ) -> Result<(AgentId, NodeState), RpcError> {
        // §6.1 step 5's type resolution, for the wall clock the relaunch runs under — the same
        // number `marion run` and `agent/spawn` resolve, from the node's own recorded type.
        let agent_type = node
            .intent
            .as_ref()
            .map(|i| recorded_type(Some(&env.project_dir), &repo, i))
            .transpose()?
            .ok_or_else(spawn_refused_before_the_node_existed)?;
        let spec = crate::root::RootSpec {
            os_sandbox: env.os_sandbox,
            wider_children: false,
            // A second life runs under the budget its first was spawned with.
            budget: node.intent.as_ref().and_then(|i| i.budget),
            agent_type: node.agent_type().unwrap_or_default().to_string(),
            prompt: prompt.to_string(),
            native_launch: None,
            repo: repo.clone(),
            state: env.state.clone(),
            base_url: env.base_url.clone(),
            bridge: env.bridge.clone(),
            // An endpoint root re-resolves its journaled provider (and re-reads its key).
            model: crate::endpoint::resume_model(
                node.model.as_deref(),
                node.provider.as_deref(),
                &agent_type,
            ),
            no_change_record: false,
            auth: env.auth,
            // **From the recorded value, never inferred** (`plan-restart-resume.md` step 6): the
            // shape the node's session was observed in. A pane names no session, so a resumable
            // node is always headless today; the field keeps that a checked fact.
            pane: node.harness_pane,
            resume: Some(crate::root::RootResume {
                agent_id: node.agent_id.clone(),
                session,
                usage: node.usage,
            }),
            // **The node's own bound, from its own journal** — the same discipline the two fields
            // above follow. A relaunch that re-resolved this from the agent type would put a root
            // the operator had launched with `--timeout 300` back under §3.1's 900 s, which is a
            // node quietly given a different clock than the one it was started with. `None` — an
            // intent written before the field existed — falls back to the type, which is the only
            // thing a journal that never recorded a bound can honestly say.
            bound_secs: crate::root::blocked_bound_secs(
                node.intent.as_ref().and_then(|i| i.timeout_secs),
                agent_type.timeout.0.as_secs(),
            ),
            // The account the session was recorded under, never a re-resolved one.
            profile: node.profile.clone(),
        };
        self.launch_root(me, spec, repo)
    }

    /// [`Self::node_resume`]'s child arm: a [`crate::run::SpawnRequest`] rebuilt from the node's
    /// journal, through the same [`Self::launch_child`] `agent/spawn` uses.
    ///
    /// Three more refusals than the root arm, each about something a root cannot lack, and in this
    /// order because the tree question settles whether there is a relaunch to place at all before
    /// anything asks the filesystem where it would go:
    ///
    /// 1. **its parent's fate is decided** — principle 8. A **Live** parent holds this child: its
    ///    own `spawn` is owed the outcome, and a relaunch from outside would put a second process
    ///    under one contract. A parent that is `Orphaned`, `ReapedIdle` or exited holds nothing,
    ///    and its child resumes: the child reports to the *supervisor* (the report tool writes the
    ///    contract and the journal), so a decided parent costs the report no destination;
    /// 2. **its workspace is on the journal** — a child's cwd is a linked worktree marion made or
    ///    the caller's own directory, and neither is derivable from anything else;
    /// 3. **that workspace still exists** — `run::cleanup` and `marion worktree reap` both remove a
    ///    child's tree, and a harness handed a session from a directory it has never seen starts
    ///    fresh, which marion would then have called a resume.
    ///
    /// **The design documents are silent on the third**, and this is the reading taken rather than
    /// invented: nothing in §7 or `plan-restart-resume.md` gives a child resume a parent rule at
    /// all. The alternative reading — refuse when the parent is *not* live, on the grounds that the
    /// child "reports into nothing" — was rejected because the premise is false in this codebase,
    /// and because it would make the case a supervisor SIGKILL actually produces (a whole tree
    /// orphaned at once) the one case resume could not serve.
    ///
    /// **Its `verification` comes back from the intent**, where `run_spawn` journals the lines the
    /// spawn asked for, so the second life re-runs them and its contract records their outcome as
    /// evidence. An intent written before the field replays as no lines, and that child resumes
    /// with none, as every child did before.
    ///
    /// **Two limits, stated rather than hidden.** A child's `writable_scope` and its
    /// `acceptance_criteria` are not journaled — they live on the contract, which an orphaned child
    /// never got far enough to write — so the second life runs with the empty scope, which is its
    /// agent type's ceiling (`run::requested_scope`, still checked against that ceiling), and an
    /// empty criteria list. Narrowing them again would need a second record, not a guess here.
    /// A resumed child's parent, read back and checked: on this journal, and no longer waiting on
    /// the child — so the child's outcome is owed to nobody still holding a `spawn`.
    fn decided_parent(
        &self,
        node: &marion_core::registry::ReplayedNode,
        parent_id: &AgentId,
    ) -> Result<marion_core::registry::ReplayedNode, RpcError> {
        let parent = self
            .live
            .read(|r| r.tree().get(parent_id).cloned())
            .ok_or_else(|| {
                RpcError::refused(
                    "agent_id",
                    format!(
                        "`{}` names `{}` as its parent and no such node is on this journal, so the \
                         relaunch would have no place in the tree. Refused by name.",
                        node.agent_id.0, parent_id.0,
                    ),
                    "§7.5",
                )
            })?;
        let parent_decided = matches!(
            parent.reap_state,
            ReapState::Orphaned | ReapState::ReapedIdle
        ) || parent.state.is_exited();
        if !parent_decided {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}`'s parent `{}` is still live ({:?}); marion holds its channel and its own \
                     `spawn` owns this child, so the child's outcome is owed to a call that is \
                     still waiting for it. A second process under one contract is what resume \
                     exists not to do. Resume this child once its parent's fate is decided, or \
                     reach it through its parent.",
                    node.agent_id.0, parent_id.0, parent.state,
                ),
                "§8, §7.2",
            ));
        }
        Ok(parent)
    }

    fn relaunch_child(
        &self,
        me: Arc<RegistryHandle>,
        node: &marion_core::registry::ReplayedNode,
        env: &crate::run::Env,
        prompt: &str,
        repo: PathBuf,
        session: String,
    ) -> Result<(AgentId, NodeState), RpcError> {
        // **§7.5's immutable parent link, read back.** The intent is the only record that names it:
        // a node's child names its caller, and a contracted node the operator asked for directly
        // names none and carries a contract — the operator is its requester again.
        let parent_id = node.intent.as_ref().and_then(|i| i.parent_id.clone());
        let operators = node
            .intent
            .as_ref()
            .is_some_and(|i| i.parent_id.is_none() && i.task_id.is_some());
        let parent_id = match parent_id {
            Some(p) => Some(p),
            None if operators => None,
            None => {
                return Err(RpcError::refused(
                    "agent_id",
                    format!(
                        "`{}` records a depth below the root and neither a parent nor a contract, \
                         so marion cannot say whose node it is or what gates its relaunch. Refused \
                         by name.",
                        node.agent_id.0
                    ),
                    "§7.5",
                ));
            }
        };
        let parent = match &parent_id {
            Some(parent_id) => Some(self.decided_parent(node, parent_id)?),
            None => None,
        };
        let Some(workspace) = node.launch_workspace.clone() else {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` is a child, and this journal does not record which directory it ran in \
                     — its session record predates the field, or no frame ever named a session. A \
                     harness resumes a conversation only from the tree that created it, so marion \
                     refuses by name rather than relaunching it somewhere the session has never \
                     been.",
                    node.agent_id.0
                ),
                "§6.6, §8",
            ));
        };
        if !workspace.path().is_dir() {
            return Err(RpcError::refused(
                "agent_id",
                format!(
                    "`{}` ran in {}, and that tree no longer exists — a reap or a cleanup removed \
                     it. Its session id outlived its workspace; handing the id back from anywhere \
                     else would start a fresh run wearing a resumed node's name, so marion refuses \
                     by name instead.",
                    node.agent_id.0,
                    workspace.path().display(),
                ),
                "§6.6, §8",
            ));
        }
        let parent_type = parent
            .as_ref()
            .map(|parent| {
                parent
                    .intent
                    .as_ref()
                    .map(|i| recorded_type(Some(&env.project_dir), &repo, i))
                    .transpose()?
                    .ok_or_else(spawn_refused_before_the_node_existed)
            })
            .transpose()?;
        let agent_type = node
            .intent
            .as_ref()
            .map(|i| recorded_type(Some(&env.project_dir), &repo, i))
            .transpose()?
            .ok_or_else(spawn_refused_before_the_node_existed)?;
        // **The task this run is still under**, from the node's own intent (§9: one contract per
        // run of one task). A fresh id here would file the second life's audit record under a task
        // nothing asked for.
        let task_id = node
            .intent
            .as_ref()
            .and_then(|i| i.task_id.clone())
            .ok_or_else(|| {
                RpcError::refused(
                    "agent_id",
                    format!(
                        "`{}` is a child with no task on its intent, so §9's contract for its \
                         second life would name nothing. Refused by name.",
                        node.agent_id.0
                    ),
                    "§9",
                )
            })?;
        // Held for the same window `agent/spawn` holds it: §6.1 step 2's gate evaluation and the
        // intent derived from it are one decision, and this launch is gated exactly as a fresh
        // child's is (`run_spawn_watched` calls `check_spawn_gates` with this caller).
        let decision = lock(&self.spawn_decision);
        let requester = match (parent_id, parent, parent_type) {
            (Some(parent_id), Some(parent), Some(parent_type)) => {
                crate::run::Requester::from(crate::run::Caller {
                    live_children: self.live_children_of(&parent_id),
                    agent_id: parent_id.0,
                    agent_type: parent_type,
                    depth: parent.depth().unwrap_or(0),
                })
            }
            // Its own tree's table is in its agent directory, read back by the resume.
            _ => crate::run::Requester::Operator {
                allow_wider_children: false,
            },
        };
        // A reviewer's second life is a reviewer's: read-only, against the same reviewed work.
        let review = node
            .intent
            .as_ref()
            .and_then(|i| i.review_of.as_ref())
            .map(|target| self.reviewed(env, target).map(|r| r.review))
            .transpose()?;
        let req = crate::run::SpawnRequest {
            // A second life runs under the budget its first was spawned with.
            budget: node.intent.as_ref().and_then(|i| i.budget),
            review,
            agent_type: agent_type.name.clone(),
            prompt: prompt.to_string(),
            repo: repo.clone(),
            // Not journaled; see this function's doc for why it is empty rather than invented.
            acceptance_criteria: vec![],
            // Journaled on the intent, so the second life re-runs what its spawn asked for.
            verification: node
                .intent
                .as_ref()
                .map(|i| i.verification.clone())
                .unwrap_or_default(),
            writable_scope: vec![],
            timeout_secs: agent_type.timeout.0.as_secs(),
            // An endpoint child re-resolves its journaled provider (and re-reads its key).
            model: crate::endpoint::resume_model(
                node.model.as_deref(),
                node.provider.as_deref(),
                &agent_type,
            ),
            // Read off the recorded workspace, so the answer and the directory cannot disagree.
            isolation: match workspace {
                marion_core::contract::Workspace::Worktree { .. } => Isolation::Worktree,
                marion_core::contract::Workspace::SharedCwd { .. } => Isolation::SharedCwd,
            },
            allow_concurrent_writes: false,
            resume: Some(crate::run::ChildResume {
                agent_id: node.agent_id.clone(),
                session,
                workspace,
                usage: node.usage,
            }),
            // The account the session was recorded under, never a re-resolved one.
            profile: node.profile.clone(),
            // A resumed seat keeps its seat.
            race: node.intent.as_ref().and_then(|i| i.race.clone()),
            read_only: false,
            // A resumed step node keeps its step; its recorded tree is where it resumes.
            workflow: node
                .intent
                .as_ref()
                .and_then(|i| i.workflow.clone())
                .map(|seat| crate::run::StepLaunch { seat, base: None }),
        };
        // A resumed child answers the operator's `node/resume`, not a parent's `spawn`.
        self.launch_child(
            me,
            env.clone(),
            req,
            task_id,
            requester,
            repo,
            decision,
            None,
        )
    }
}
