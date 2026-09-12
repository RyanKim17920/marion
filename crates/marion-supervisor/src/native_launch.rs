//! Authenticated native selection composed with the existing pane lifecycle.
//!
//! The detached supervisor installs this handler through `serve::Server::start_with_native_launch`.
//! The handler is generic over the descriptor slice and the command factory; the production
//! factory below resolves and assembles, while vendor injection stays behind the opaque
//! `NativeInjectionAdapter` boundary the factory is handed.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use marion_core::agent_type::AgentType;
use marion_core::harness::Harness;
use marion_core::journal::{Exited, RecordKind, SpawnAborted, SpawnIntent, Spawned, StateChanged};
use marion_core::node::NodeState;
use marion_core::{NativeFacadeDescriptor, contract::AgentId};
use marion_harness::mcp_bridge::BridgeEnv;
use marion_harness::{
    NativeEnvironmentView, NativeInjectionAdapter, NativeInvocation, NativeNodeContext,
    NativeProcessBase, NativeTerminalGeometry, assemble_native,
};

use crate::native_binding::resolve_declared_executable;
use crate::native_bootstrap::{
    BootstrapError, ConsumedNativeRequest, DirectNativeRequestContext, NativeBootstrapHandler,
    NativeClaimant, NativeLaunchDeadline, NativeLaunchTicket, PendingNativeLaunchReceipt,
    PendingNativeLaunches, PreparedNativeRelay, TerminalGeometryObservation,
};
use crate::native_exec::{
    LaunchedNativeCommand, NativeCommandLauncher, NativeCommandSpec, NativeLifecycleSpawner,
    NativeNodeRecorder, PreparedNativeLifecycle, ThreadNativeLifecycleSpawner,
};

pub(crate) struct PreparedNativeCommand {
    pub(crate) invocation: NativeInvocation,
    pub(crate) cast_path: PathBuf,
    pub(crate) terminal_profile: OsString,
    /// What the resolved executable answered to `--version`, or `"unknown"` — the same probe,
    /// on the same pre-launch path, that `run_spawn` gives a child (§6.1 step 3).
    pub(crate) harness_version: String,
}

/// **One native root's identity and its §5.4 capability**, decided together and handed to the
/// factory as one value.
///
/// They travel together because they are minted together and are worthless apart: an `AgentId`
/// with no token is a bridge that can name a node and prove nothing about it, and a token with no
/// id names nobody. The pairing is the same one [`crate::handler::RegistryHandle::claim`] returns
/// for a managed node, arriving here rather than through `SpawnCtx` because a native root has no
/// `run_spawn` to carry it.
pub(crate) struct NativeNodeClaim {
    /// The node this launch is, in the journal, the registry, its agent directory and its bridge.
    pub(crate) agent_id: AgentId,
    /// §5.4's per-node capability, or `None` where the supervisor could mint none — a native
    /// session that runs and cannot delegate, never one with a guessable capability.
    pub(crate) node_token: Option<String>,
}

/// Descriptor-specific assembly, injected so this generic executor never guesses vendor flags,
/// readiness, environment injection, or executable resolution.
pub(crate) trait NativeCommandFactory: Send + Sync + 'static {
    fn prepare(
        &self,
        selected: &crate::native_intent::SelectedNativeFacade<'_>,
        context: &DirectNativeRequestContext,
        geometry: NativeTerminalGeometry,
        claim: &NativeNodeClaim,
    ) -> Result<PreparedNativeCommand, BootstrapError>;
}

/// The marion tools a native root may call through its injected MCP server, in marion's own
/// spelling. Each vendor adapter maps them to that harness's spelling; the registry-driven
/// adapters own that translation, not this factory.
pub const NATIVE_ROOT_MARION_TOOLS: &[&str] = &["spawn", "wait", "status"];

/// Where the factory finds the vendor injection adapter for a selected harness.
///
/// A plain function pointer rather than a registry method, so the lookup is data the composition
/// root supplies: production hands in `marion_harness::native_adapter` (the row-derived table),
/// the integration beds hand in a fixture, and `None` refuses every launch on that harness by name.
pub type NativeAdapterLookup = fn(Harness) -> Option<&'static dyn NativeInjectionAdapter>;

/// The production [`NativeCommandFactory`]: resolve the declared executable on the **client's**
/// `PATH`, ask the opaque adapter for its injection, and let the generic assembler place
/// `program + prefix + opaque_tail`. This type never inspects the tail.
pub(crate) struct ProductionNativeCommandFactory {
    env: crate::run::Env,
    adapter_for: NativeAdapterLookup,
}

impl ProductionNativeCommandFactory {
    pub(crate) fn new(env: crate::run::Env, adapter_for: NativeAdapterLookup) -> Self {
        Self { env, adapter_for }
    }

    /// The declaration of marion's own MCP server for this node — the same keys a managed node's
    /// declaration carries (`claude_code::mcp_config_json`), minus only the readiness file a
    /// native session has no use for.
    ///
    /// **The node token is here for the same reason it is in a managed root's declaration**, and
    /// its absence was the whole of why `marion codex` could never delegate: `marion-supervisor
    /// mcp` is [`crate::mcp::Principal::Node`], and a `Node` that cannot present §5.4's capability
    /// refuses its own `spawn` before a frame reaches the socket. A native root **is** a journaled
    /// node — `NativeNodeJournal` writes its `SpawnIntent` and `Spawned` — so the owner that
    /// claimed it mints the same capability it mints for every other node it owns.
    ///
    /// `repo` is the working tree the operator stood in, which is what `marion run` records for a
    /// managed root (`default_repo`) — never §2's key, which is the git common dir.
    fn bridge_for(
        &self,
        claim: &NativeNodeClaim,
        agent_type: &AgentType,
        repo: &std::path::Path,
    ) -> BridgeEnv {
        BridgeEnv {
            bridge: self.env.bridge.clone(),
            args: vec!["mcp".into()],
            repo: repo.to_path_buf(),
            state: self.env.state.clone(),
            base_url: self.env.base_url.clone(),
            auth: self.env.auth,
            agent_id: claim.agent_id.clone(),
            agent_type: agent_type.name.clone(),
            // A native session is a root.
            depth: 0,
            node_token: claim.node_token.clone(),
            ready_file: None,
        }
    }
}

fn native_command_error(error: impl std::fmt::Display) -> BootstrapError {
    BootstrapError::NativeCommand(error.to_string())
}

/// The working tree `cwd` is in: its nearest ancestor holding a `.git` entry — a directory in the
/// main worktree, a file in a linked one — or `cwd` itself outside git. The same walk `marion run`
/// takes for a managed root's repository, so the two kinds of root record the same thing.
fn working_tree_root(cwd: &std::path::Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map_or_else(|| cwd.to_path_buf(), std::path::Path::to_path_buf)
}

/// Write a native launch's declaration documents **under the node's own directory and nowhere
/// else**.
///
/// Every path is checked to be a direct child of `agent_dir` before anything is created, and the
/// write itself goes through a descriptor on that directory rather than the absolute path, so a
/// component swapped underneath between check and open cannot redirect it (`O_NOFOLLOW` on both
/// opens). `O_EXCL` because a declaration names one node: a name that already exists is a second
/// launch under the same id, which is a refusal, not an overwrite. `0600` because the document
/// carries the node's identity and, under a canned supervisor, its provider endpoint.
fn materialize_documents(
    agent_dir: &std::path::Path,
    documents: &[marion_harness::NativeDocument],
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt as _;

    use rustix::fs::{Mode, OFlags};

    let mut names = Vec::with_capacity(documents.len());
    for document in documents {
        let name = document
            .path
            .strip_prefix(agent_dir)
            .ok()
            .filter(|rel| {
                let mut components = rel.components();
                matches!(
                    (components.next(), components.next()),
                    (Some(std::path::Component::Normal(_)), None)
                )
            })
            .ok_or_else(|| {
                std::io::Error::other(format!(
                    "native document {} is not a direct child of the node directory",
                    document.path.display()
                ))
            })?;
        names.push((name, &document.contents));
    }
    if names.is_empty() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(agent_dir)?;
    let dir = rustix::fs::open(
        agent_dir,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    for (name, contents) in names {
        let fd = rustix::fs::openat(
            &dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut file = std::fs::File::from(fd);
        file.write_all(contents)?;
        file.sync_all()?;
    }
    rustix::fs::fsync(&dir)?;
    Ok(())
}

impl NativeCommandFactory for ProductionNativeCommandFactory {
    fn prepare(
        &self,
        selected: &crate::native_intent::SelectedNativeFacade<'_>,
        context: &DirectNativeRequestContext,
        geometry: NativeTerminalGeometry,
        claim: &NativeNodeClaim,
    ) -> Result<PreparedNativeCommand, BootstrapError> {
        let lane = selected.native_lane();
        let environment = context.environment();
        // Where the operator invoked the facade, never the canonical project: that is §2's key —
        // the git common dir — and a harness run there prints `.git` as its workspace. The
        // bootstrap service has already checked this directory belongs to the project.
        let cwd = context.client_cwd().to_path_buf();
        // The client's PATH, never this detached process's: the operator's shell decides which
        // `claude` they get, exactly as it would without marion in front.
        let program = resolve_declared_executable(lane.executable(), &cwd, environment)
            .map_err(native_command_error)?;
        // **Probed here, where the binary was just resolved on the operator's own PATH**, so the
        // version on the node's `Spawned` is the version of the process about to run and not of
        // whatever this detached supervisor's PATH would have found. Bounded and never a refusal,
        // exactly as for a child: `"unknown"` is what a silent or hanging binary records.
        let harness_version = crate::run::harness_version(&program.to_string_lossy());
        let agent_type = lane.agent_type();
        let adapter = (self.adapter_for)(agent_type.harness).ok_or_else(|| {
            native_command_error(format!(
                "no native injection adapter is registered for harness {:?}",
                agent_type.harness.as_str()
            ))
        })?;
        let agent_dir = self.env.project_dir.agent(&claim.agent_id);
        let bridge = self.bridge_for(claim, agent_type, &working_tree_root(&cwd));
        let injection = adapter
            .prepare_native(&NativeNodeContext {
                bridge: &bridge,
                document_dir: agent_dir.path(),
                allowed_marion_tools: NATIVE_ROOT_MARION_TOOLS,
                environment: NativeEnvironmentView::validate(environment)
                    .map_err(native_command_error)?,
            })
            .map_err(native_command_error)?;
        let prepared = assemble_native(
            NativeProcessBase {
                program,
                user_argv: context.opaque_tail().to_vec(),
                env: environment.to_vec(),
                cwd,
                geometry,
            },
            injection,
        )
        .map_err(native_command_error)?;
        materialize_documents(agent_dir.path(), &prepared.documents)
            .map_err(native_command_error)?;
        Ok(PreparedNativeCommand {
            invocation: prepared.invocation,
            cast_path: agent_dir.pty_cast(),
            terminal_profile: context.terminal_profile().to_owned(),
            harness_version,
        })
    }
}

pub(crate) struct NativeLaunchHandler {
    descriptors: &'static [NativeFacadeDescriptor],
    handle: Arc<crate::handler::RegistryHandle>,
    factory: Arc<dyn NativeCommandFactory>,
    mint_agent: Arc<dyn Fn() -> std::io::Result<AgentId> + Send + Sync>,
    pending: Arc<Mutex<std::collections::HashMap<AgentId, LaunchedNativeCommand>>>,
    lifecycle_spawner: Arc<dyn NativeLifecycleSpawner>,
}

impl NativeLaunchHandler {
    pub(crate) fn new(
        descriptors: &'static [NativeFacadeDescriptor],
        handle: Arc<crate::handler::RegistryHandle>,
        authority: Arc<PendingNativeLaunches>,
        factory: Arc<dyn NativeCommandFactory>,
        mint_agent: Arc<dyn Fn() -> std::io::Result<AgentId> + Send + Sync>,
    ) -> Result<Self, BootstrapError> {
        handle
            .install_pending_native_launches(authority)
            .map_err(|_| {
                BootstrapError::NativeClaim("native authority already installed".into())
            })?;
        Ok(Self {
            descriptors,
            handle,
            factory,
            mint_agent,
            pending: Arc::new(Mutex::new(std::collections::HashMap::new())),
            lifecycle_spawner: Arc::new(ThreadNativeLifecycleSpawner),
        })
    }

    fn lock_pending(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<AgentId, LaunchedNativeCommand>> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(test)]
    fn for_claim_test(
        handle: Arc<crate::handler::RegistryHandle>,
        lifecycle_spawner: Arc<dyn NativeLifecycleSpawner>,
    ) -> Self {
        struct UnusedFactory;
        impl NativeCommandFactory for UnusedFactory {
            fn prepare(
                &self,
                _: &crate::native_intent::SelectedNativeFacade<'_>,
                _: &DirectNativeRequestContext,
                _: NativeTerminalGeometry,
                _: &NativeNodeClaim,
            ) -> Result<PreparedNativeCommand, BootstrapError> {
                unreachable!("claim test bypasses authorization")
            }
        }
        Self {
            descriptors: &[],
            handle,
            factory: Arc::new(UnusedFactory),
            mint_agent: Arc::new(|| unreachable!("claim test bypasses authorization")),
            pending: Arc::new(Mutex::new(std::collections::HashMap::new())),
            lifecycle_spawner,
        }
    }

    #[cfg(test)]
    fn insert_pending_for_test(&self, agent_id: AgentId, launch: LaunchedNativeCommand) {
        assert!(self.lock_pending().insert(agent_id, launch).is_none());
    }
}

/// A native node's journal records — intent, `Spawned` and the `Running` beside it, and the
/// exit or abort — written through the supervisor's own handle so the live tree sees each before
/// the next connection can ask about it.
///
/// `harness_version` is what the factory's probe of the operator's own binary answered
/// ([`PreparedNativeCommand::harness_version`]), and `"unknown"` before the factory has run — an
/// abort ahead of `prepare` names no version because none was measured. `model` is `None` for the
/// same reason a codex `exec` records none — marion placed no model on this argv.
struct NativeNodeJournal {
    handle: Arc<crate::handler::RegistryHandle>,
    agent_id: AgentId,
    harness_version: String,
}

impl NativeNodeJournal {
    fn intent(&self, agent_type: &AgentType) -> Result<(), crate::journal::JournalError> {
        self.handle
            .journal_now(RecordKind::SpawnIntent(SpawnIntent {
                agent_id: self.agent_id.clone(),
                parent_id: None,
                agent_type: agent_type.name.clone(),
                harness: agent_type.harness,
                depth: 0,
                // A root has no contract (§9).
                task_id: None,
                // **No bound, and that is the record rather than a gap.** A native launch is the
                // operator's own session in their own terminal: marion starts it and never holds a
                // clock over it, so there is no resolved bound to record. `None` reads back as
                // §3.1's agent-type default in the tree, which is the honest answer for a node
                // marion is not timing.
                timeout_secs: None,
            }))
    }

    fn record(&self, kind: RecordKind) {
        // The intent is durable, so the node is named either way; a lost confirmation replays as
        // an intent with no pid, which is the honest state. `journal::record`'s policy.
        if let Err(error) = self.handle.journal_now(kind) {
            eprintln!(
                "marion: journal write failed for native node {}: {error}",
                self.agent_id.0
            );
        }
    }
}

impl NativeNodeRecorder for NativeNodeJournal {
    fn spawned(&self, pid: i32) {
        self.record(RecordKind::Spawned(Spawned {
            agent_id: self.agent_id.clone(),
            harness_version: self.harness_version.clone(),
            model: None,
            pid: Some(pid),
            // Read here, while marion holds the child, or not at all (`run.rs`'s reasoning).
            start_id: match crate::procid::read(pid) {
                crate::procid::Read::Id(id) => Some(id),
                crate::procid::Read::NoSuchProcess | crate::procid::Read::Unavailable(_) => None,
            },
        }));
        // **`Running` from the instant the process holds its pane.** A managed node's turn has
        // boundaries marion can see on its stream; a native session has none — its pty session
        // *is* its turn, open from `spawn()` until the pane ends — so the only honest reading
        // between `Spawned` and `Exited` is `Running`. Without this record the node replayed as
        // `Spawning` for its whole life, and `marion tree` said so to the operator sitting in it.
        self.record(RecordKind::StateChanged(StateChanged {
            agent_id: self.agent_id.clone(),
            state: NodeState::Running,
        }));
    }

    fn exited(&self, status: Option<std::process::ExitStatus>) {
        use std::os::unix::process::ExitStatusExt as _;
        let code = status.and_then(|s| s.code());
        let signal = status.and_then(|s| s.signal());
        let description = match (code, signal) {
            (Some(code), _) => format!("native node exited with code {code}"),
            (None, Some(signal)) => format!("native node was terminated by signal {signal}"),
            (None, None) => "native node exit status was unavailable".to_string(),
        };
        self.record(RecordKind::Exited(Exited {
            agent_id: self.agent_id.clone(),
            status: if code == Some(0) {
                marion_core::contract::ExitStatus::Ok
            } else {
                marion_core::contract::ExitStatus::Failed
            },
            exit: marion_core::contract::ProcessExit {
                code,
                signal,
                description: description.clone(),
            },
        }));
        // The registry entry the claim opened, closed at the same instant. §5.7's second guard
        // reads liveness off this table, and nothing else would ever set it for a native node.
        self.handle.finished_native(
            &self.agent_id,
            if code == Some(0) {
                Ok(())
            } else {
                Err(description)
            },
        );
    }

    fn aborted(&self, reason: &str) {
        self.record(RecordKind::SpawnAborted(SpawnAborted {
            agent_id: self.agent_id.clone(),
            reason: reason.to_string(),
        }));
        self.handle
            .finished_native(&self.agent_id, Err(reason.to_string()));
    }
}

struct NativePaneOwner(Arc<crate::handler::RegistryHandle>);

impl crate::root::PaneOwner for NativePaneOwner {
    fn opened(&self, id: &AgentId, host: Arc<crate::pty::PtyHost>) {
        self.0.register_pane(id, host);
    }

    fn closing(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.0.closing_pane(id, host);
    }

    fn completed(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>, charge: usize) {
        self.0.completed_pane(id, host, charge);
    }

    fn failed(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
        self.0.failed_pane(id, host);
    }
}

struct PreparedClaimRelay {
    lifecycle: PreparedNativeLifecycle,
    handle: Arc<crate::handler::RegistryHandle>,
}

impl PreparedNativeRelay for PreparedClaimRelay {
    fn handle(&self) -> Arc<dyn crate::serve::Handle> {
        self.handle.clone()
    }

    fn commit(self: Box<Self>) {
        let Self { lifecycle, handle } = *self;
        let _running = lifecycle.commit();
        drop(handle);
    }
}

impl NativeBootstrapHandler for NativeLaunchHandler {
    fn verify_terminal(
        &self,
        _peer: crate::native_bootstrap::PeerIdentity,
        stdin: std::os::fd::BorrowedFd<'_>,
        _stdout: std::os::fd::BorrowedFd<'_>,
    ) -> Result<TerminalGeometryObservation, BootstrapError> {
        let size = rustix::termios::tcgetwinsize(stdin)
            .map_err(|error| BootstrapError::TerminalVerification(error.to_string()))?;
        if size.ws_col == 0 || size.ws_row == 0 {
            return Err(BootstrapError::TerminalVerification(
                "terminal geometry contains a zero axis".into(),
            ));
        }
        Ok(TerminalGeometryObservation::new(
            crate::native_bootstrap::TerminalGeometry {
                cols: size.ws_col,
                rows: size.ws_row,
                xpixel: size.ws_xpixel,
                ypixel: size.ws_ypixel,
            },
        ))
    }

    fn authorized(
        &self,
        request: ConsumedNativeRequest<'_>,
        deadline: &NativeLaunchDeadline,
    ) -> Result<PendingNativeLaunchReceipt, BootstrapError> {
        deadline.require_remaining()?;
        let context = request.context().clone();
        let registry = marion_core::NativeFacadeRegistry::new(self.descriptors)
            .map_err(|error| BootstrapError::NativeClaim(error.to_string()))?;
        let selected =
            crate::native_bootstrap::select_authenticated_native_launch(&registry, request)
                .ok_or(BootstrapError::AuthorizationRefused)?;
        let agent_id = (self.mint_agent)().map_err(BootstrapError::NativeTransportIo)?;
        let (selected, terminal, binding) = selected.into_parts(agent_id.clone());
        let geometry = terminal.initial_geometry();
        let agent_type = selected.native_lane().agent_type();

        // §6.1 step 7, first half, at the first instant the node has an identity and before any
        // side effect: a native node is a root in this project's tree, and `node/attach` — which
        // the relay is about to send — resolves it from the journal before it looks for a pane.
        let mut journal = NativeNodeJournal {
            handle: Arc::clone(&self.handle),
            agent_id: agent_id.clone(),
            harness_version: "unknown".into(),
        };
        journal
            .intent(agent_type)
            .map_err(|error| BootstrapError::NativeClaim(error.to_string()))?;
        // **§6.1 step 7's window, and the only instant §5.4's capability can be decided in.** The
        // intent is durable, so the node exists and the registry can read its depth and agent type
        // back; nothing irreversible has happened, so the token is decided before the declaration
        // that carries it is written. This is `run_spawn`'s `observer.identified` hook, arriving on
        // the one path that has no `run_spawn` — which is why a native root used to be the single
        // journaled node marion never claimed, and why its bridge refused every `spawn`.
        //
        // `working_tree_root` for the same reason the factory calls it for `MARION_REPO`: the tree
        // this node lives in is the tree its own children branch from, and a native root lives
        // where the operator stood, never in §2's key.
        let node_token = self.handle.claim(
            &agent_id,
            // A root has no contract (§9).
            None,
            working_tree_root(context.client_cwd()),
        );
        let claim = NativeNodeClaim {
            agent_id: agent_id.clone(),
            // Present or absent, never empty: an empty token is a secret every process on this
            // machine already has (`mcp_bridge::BridgeEnv`).
            node_token: (!node_token.is_empty()).then_some(node_token),
        };
        // Every refusal from here until the launch owns the record resolves the intent.
        let prepared = (|| {
            // Authority exists before the PaneOwner can make a host visible.
            let receipt = self
                .handle
                .reserve_pending_native_launch(binding)
                .map_err(|error| BootstrapError::NativeClaim(error.to_string()))?;
            let prepared = self.factory.prepare(
                &selected,
                &context,
                NativeTerminalGeometry {
                    cols: geometry.cols,
                    rows: geometry.rows,
                    xpixel: geometry.xpixel,
                    ypixel: geometry.ypixel,
                },
                &claim,
            )?;
            deadline.require_remaining()?;
            Ok::<_, BootstrapError>((receipt, prepared))
        })();
        let (mut receipt, prepared) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                journal.aborted(&format!("the native launch was refused: {error}"));
                return Err(error);
            }
        };
        // Known only now: the factory resolved and probed the binary. Set before the launcher
        // owns the recorder, so the `Spawned` it writes at `spawn()` carries it.
        journal.harness_version = prepared.harness_version.clone();
        let owner: Arc<dyn crate::root::PaneOwner> =
            Arc::new(NativePaneOwner(Arc::clone(&self.handle)));
        let launch = match NativeCommandLauncher::launch(
            NativeCommandSpec {
                agent_id: agent_id.clone(),
                invocation: prepared.invocation,
                cast_path: prepared.cast_path,
                terminal_profile: prepared.terminal_profile,
            },
            owner,
            Arc::new(journal),
        ) {
            Ok(launch) => launch,
            Err(error) => {
                // The launcher owns the record from `spawn()` on; a launch that never got there
                // resolves the intent here.
                NativeNodeJournal {
                    handle: Arc::clone(&self.handle),
                    agent_id: agent_id.clone(),
                    harness_version: prepared.harness_version.clone(),
                }
                .aborted(&format!("the native process could not be started: {error}"));
                return Err(BootstrapError::NativeTransportIo(error));
            }
        };
        self.handle
            .publish_pending_native_launch(receipt.receipt())
            .map_err(|error| BootstrapError::NativeClaim(error.to_string()))?;

        self.lock_pending().insert(agent_id.clone(), launch);
        let pending = Arc::clone(&self.pending);
        receipt.on_cancel(move || {
            let launch = pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&agent_id);
            // Dropping a launch shuts down and reaps its child, so never do that while holding the
            // global pending-launch map. An unrelated authorization/claim must not wait on it.
            drop(launch);
        });
        // Returning is the receipt-publication boundary; no response byte exists before this.
        Ok(receipt)
    }

    fn requires_claim_transport(&self) -> bool {
        true
    }

    fn prepare_native_claim(
        &self,
        ticket: &NativeLaunchTicket,
        agent_id: &AgentId,
        claimant: NativeClaimant,
    ) -> Result<Box<dyn PreparedNativeRelay>, BootstrapError> {
        let writer = self
            .handle
            .prepare_pending_native_writer(ticket, agent_id, claimant)
            .map_err(|error| BootstrapError::NativeClaim(error.to_string()))?;
        let launch = self
            .lock_pending()
            .remove(agent_id)
            .ok_or_else(|| BootstrapError::NativeClaim("native launch is not pending".into()))?;
        let lifecycle = match launch.prepare_lifecycle(self.lifecycle_spawner.as_ref()) {
            Ok(lifecycle) => lifecycle,
            Err(failure) => {
                let (error, launch) = failure.into_parts();
                self.lock_pending().insert(agent_id.clone(), launch);
                return Err(BootstrapError::NativeTransportIo(error));
            }
        };
        if let Err(error) = writer.commit() {
            let launch = lifecycle.into_launch();
            self.lock_pending().insert(agent_id.clone(), launch);
            return Err(BootstrapError::NativeClaim(error.to_string()));
        }
        Ok(Box::new(PreparedClaimRelay {
            lifecycle,
            handle: Arc::clone(&self.handle),
        }))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use marion_core::contract::AgentId;
    use marion_harness::{
        NativeInjection, NativeProcessBase, NativeTerminalGeometry, assemble_native,
    };

    use super::*;
    use crate::native_bootstrap::fakes::{ManualClock, SequenceRng};
    use crate::native_bootstrap::{
        DirectNativeRequestContext, NativeLaunchBinding, NativeLaunchDescriptor,
        NativeLaunchTicket, PeerIdentity, TerminalFingerprint, TerminalGeometry, context_hash,
    };
    use crate::native_exec::{NativeLifecycleSpawner, NativeLifecycleTask};
    use crate::registry::{LiveRegistry, Registry};
    use crate::serve::ConnId;

    fn invocation(work: &std::path::Path) -> NativeInvocation {
        assemble_native(
            NativeProcessBase {
                program: OsString::from("/bin/sh"),
                user_argv: vec![OsString::from("-c"), OsString::from("exit 0")],
                env: vec![],
                cwd: work.to_path_buf(),
                geometry: NativeTerminalGeometry {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                },
            },
            NativeInjection {
                argv_prefix: vec![],
                env_overlay: vec![],
                documents: vec![],
            },
        )
        .unwrap()
        .invocation
    }

    fn binding(agent_id: AgentId) -> NativeLaunchBinding {
        NativeLaunchBinding::new(
            agent_id,
            PathBuf::from("/project"),
            PeerIdentity::current_for_tty_test(),
            ConnId(900),
            TerminalFingerprint::new(1, 2, 3),
            TerminalGeometry {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            },
            NativeLaunchDescriptor::new("atlas", "atlas", "atlas-native"),
            context_hash(&DirectNativeRequestContext::new(
                PathBuf::from("/project"),
                PathBuf::from("/project"),
                OsString::from("atlas"),
                vec![],
                OsString::from("xterm-256color"),
                crate::native_bootstrap::NATIVE_WIRE_VERSION,
            )),
        )
    }

    struct FailOnceLifecycleSpawner {
        fail: AtomicBool,
        finished: Mutex<Option<mpsc::SyncSender<()>>>,
    }

    impl NativeLifecycleSpawner for FailOnceLifecycleSpawner {
        fn spawn(
            &self,
            name: String,
            task: NativeLifecycleTask,
        ) -> std::io::Result<
            std::thread::JoinHandle<std::io::Result<Option<std::process::ExitStatus>>>,
        > {
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::other(
                    "injected lifecycle thread spawn failure",
                ));
            }
            let finished = self
                .finished
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .expect("the retry spawns one lifecycle worker");
            std::thread::Builder::new().name(name).spawn(move || {
                let result = task();
                let _ = finished.send(());
                result
            })
        }
    }

    const ATLAS: marion_core::NativeFacadeDescriptor = marion_core::NativeFacadeDescriptor {
        identity: marion_core::VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &[],
        native: Some(marion_core::Lane::new(
            true,
            marion_core::NativeLane::new(
                "atlas-cli",
                "codex",
                marion_core::NativeAdapterId::new("atlas-native"),
            ),
        )),
        structured: None,
    };
    const ATLAS_DESCRIPTORS: &[marion_core::NativeFacadeDescriptor] = &[ATLAS];

    /// Stands in for the 32 hex characters `RegistryHandle::claim` mints. A literal, because what
    /// these tests assert is that the factory carries **the owner's** value into the declaration
    /// unchanged, and a value the test could not have predicted proves nothing about that.
    const FIXTURE_NODE_TOKEN: &str = "MARION-NATIVE-TOKEN-VALUE-9c2f";

    fn fixture_claim(agent_id: &AgentId) -> NativeNodeClaim {
        NativeNodeClaim {
            agent_id: agent_id.clone(),
            node_token: Some(FIXTURE_NODE_TOKEN.to_string()),
        }
    }

    /// The adapter sees only the semantic node context. It proves that by echoing the bridge
    /// program it was handed into its prefix, which the factory must place before the tail.
    struct FixtureAdapter;

    impl marion_harness::NativeInjectionAdapter for FixtureAdapter {
        fn prepare_native(
            &self,
            context: &marion_harness::NativeNodeContext<'_>,
        ) -> Result<NativeInjection, marion_harness::NativeInjectionError> {
            assert_eq!(context.allowed_marion_tools, NATIVE_ROOT_MARION_TOOLS);
            assert!(
                context
                    .environment
                    .get(std::ffi::OsStr::new("PATH"))
                    .is_some(),
                "the adapter must see the client's PATH through the validated view"
            );
            let pairs = context.bridge.pairs();
            let declared = |name: &str| {
                pairs
                    .iter()
                    .find(|(candidate, _)| candidate == name)
                    .map(|(_, value)| OsString::from(value))
                    .unwrap_or_else(|| panic!("the bridge declaration carries {name}"))
            };
            assert_eq!(context.bridge.args, ["mcp".to_string()]);
            assert_eq!(declared("MARION_DEPTH"), "0");
            assert_eq!(declared("MARION_AUTH"), "canned");
            assert_eq!(declared("MARION_BASE_URL"), "http://127.0.0.1:8099/v1");
            assert!(
                !pairs.iter().any(|(name, _)| name == "MARION_READY_FILE"),
                "a native root has no readiness file: marion never waits on the operator's own \
                 session to announce itself"
            );
            // §5.4, carried exactly as a managed root's declaration carries it: the bridge in
            // front of the operator's harness is a node's bridge, and a node's bridge must be able
            // to prove which node it serves or it can never spawn.
            assert_eq!(
                declared("MARION_NODE_TOKEN"),
                OsString::from(FIXTURE_NODE_TOKEN,),
                "the declaration must carry the token its owner minted"
            );
            Ok(NativeInjection {
                argv_prefix: vec![
                    OsString::from("--marion-mcp"),
                    context.bridge.bridge.as_os_str().to_owned(),
                    OsString::from("--marion-documents"),
                    context.document_dir.as_os_str().to_owned(),
                    OsString::from("--marion-agent"),
                    declared(marion_harness::AGENT_ID_ENV),
                    OsString::from("--marion-repo"),
                    declared("MARION_REPO"),
                    OsString::from("--marion-agent-type"),
                    declared(marion_harness::AGENT_TYPE_ENV),
                ],
                env_overlay: vec![(OsString::from("ATLAS_INJECTED"), OsString::from("1"))],
                documents: vec![],
            })
        }
    }

    static FIXTURE_ADAPTER: FixtureAdapter = FixtureAdapter;

    fn fixture_adapter(
        harness: marion_core::Harness,
    ) -> Option<&'static dyn marion_harness::NativeInjectionAdapter> {
        (harness == marion_core::Harness::Codex).then_some(&FIXTURE_ADAPTER)
    }

    fn no_adapter(
        _: marion_core::Harness,
    ) -> Option<&'static dyn marion_harness::NativeInjectionAdapter> {
        None
    }

    fn factory_env(work: &std::path::Path) -> crate::run::Env {
        crate::run::Env {
            project_dir: marion_core::paths::ProjectDir::new(
                &work.join("state"),
                &work.join("project"),
            ),
            state: work.join("state"),
            project_root: work.join("project"),
            bridge: PathBuf::from("/opt/marion/bin/marion-supervisor"),
            base_url: Some("http://127.0.0.1:8099/v1".into()),
            auth: marion_harness::Auth::Canned,
        }
    }

    fn fixture_executable(work: &std::path::Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let bin = work.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("atlas-cli");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        (bin, executable)
    }

    fn atlas_context(
        project: &std::path::Path,
        tail: Vec<OsString>,
        environment: Vec<(OsString, OsString)>,
    ) -> DirectNativeRequestContext {
        DirectNativeRequestContext::new(
            project.to_path_buf(),
            project.to_path_buf(),
            OsString::from("atlas"),
            tail,
            OsString::from_vec(b"xterm-\xf0".to_vec()),
            crate::native_bootstrap::NATIVE_WIRE_VERSION,
        )
        .with_environment(environment)
    }

    /// Mutation: run the vendor in the canonical project (§2's key, `<repo>/.git`) instead of
    /// where the operator invoked the facade, or tell the bridge that key — or the subdirectory —
    /// is the repository.
    #[test]
    fn production_factory_runs_the_vendor_where_the_client_stood_and_names_its_working_tree() {
        let work = marion_testsupport::scratch("native-factory-cwd");
        let (bin, _) = fixture_executable(&work);
        let env = factory_env(&work);
        let project = work.join("project");
        let cwd = project.join("crates").join("deep");
        std::fs::create_dir_all(&cwd).unwrap();
        // A linked worktree's `.git` is a file; the walk must stop at either.
        std::fs::write(
            project.join(".git"),
            b"gitdir: /elsewhere/.git/worktrees/x\n",
        )
        .unwrap();
        let key = project.join(".git");
        let agent_id = AgentId("native-cwd".into());
        let registry = marion_core::NativeFacadeRegistry::new(ATLAS_DESCRIPTORS).unwrap();
        let selected = crate::native_intent::select_test_native(&registry, "atlas").unwrap();
        let context = DirectNativeRequestContext::new(
            key.clone(),
            cwd.clone(),
            OsString::from("atlas"),
            vec![OsString::from("--tail")],
            OsString::from("xterm"),
            crate::native_bootstrap::NATIVE_WIRE_VERSION,
        )
        .with_environment(vec![(OsString::from("PATH"), bin.as_os_str().to_owned())]);

        let prepared = ProductionNativeCommandFactory::new(env, fixture_adapter)
            .prepare(
                &selected,
                &context,
                NativeTerminalGeometry {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                },
                &fixture_claim(&agent_id),
            )
            .expect("the fixture facade assembles");

        assert_eq!(prepared.invocation.cwd, cwd);
        let repo_flag = prepared
            .invocation
            .args
            .iter()
            .position(|argument| argument == "--marion-repo")
            .expect("the adapter echoed the repository it was told");
        assert_eq!(prepared.invocation.args[repo_flag + 1], project.as_os_str());
        assert_eq!(
            working_tree_root(&work.join("nowhere")),
            work.join("nowhere")
        );
    }

    /// Mutation: reorder prefix and tail, parse or normalize the tail, resolve the executable on
    /// the supervisor's own PATH, or let the adapter see anything but the node context.
    #[test]
    fn production_factory_assembles_program_prefix_then_opaque_tail_byte_exact() {
        let work = marion_testsupport::scratch("native-factory-byte-exact");
        let (bin, executable) = fixture_executable(&work);
        let project = work.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let env = factory_env(&work);
        let agent_id = AgentId("019f81eb-36a4-7000-8000-000000000101".into());
        let registry = marion_core::NativeFacadeRegistry::new(ATLAS_DESCRIPTORS).unwrap();
        let selected = crate::native_intent::select_test_native(&registry, "atlas").unwrap();
        let tail = vec![
            OsString::from("--"),
            OsString::new(),
            OsString::from_vec(vec![0xff, 0x80, b'x']),
            OsString::from("--marion-mcp"),
        ];
        let context = atlas_context(
            &project,
            tail.clone(),
            vec![
                (OsString::from("PATH"), bin.as_os_str().to_owned()),
                (OsString::from("SHELL"), OsString::from("/bin/zsh")),
            ],
        );
        let geometry = NativeTerminalGeometry {
            cols: 103,
            rows: 41,
            xpixel: 7,
            ypixel: 11,
        };

        let prepared = ProductionNativeCommandFactory::new(env.clone(), fixture_adapter)
            .prepare(&selected, &context, geometry, &fixture_claim(&agent_id))
            .expect("the fixture facade assembles");

        let agent_dir = env.project_dir.agent(&agent_id);
        assert_eq!(prepared.invocation.program, executable.as_os_str());
        let mut expected_args = vec![
            OsString::from("--marion-mcp"),
            OsString::from("/opt/marion/bin/marion-supervisor"),
            OsString::from("--marion-documents"),
            agent_dir.path().as_os_str().to_owned(),
            OsString::from("--marion-agent"),
            OsString::from(agent_id.0.as_str()),
            OsString::from("--marion-repo"),
            project.as_os_str().to_owned(),
            OsString::from("--marion-agent-type"),
            OsString::from("codex-impl"),
        ];
        expected_args.extend(tail);
        assert_eq!(prepared.invocation.args, expected_args);
        assert_eq!(prepared.invocation.cwd, project);
        assert_eq!(prepared.invocation.geometry, geometry);
        assert!(prepared.invocation.requires_env_clear());
        assert_eq!(
            prepared.invocation.env,
            vec![
                (OsString::from("PATH"), bin.as_os_str().to_owned()),
                (OsString::from("SHELL"), OsString::from("/bin/zsh")),
                (OsString::from("ATLAS_INJECTED"), OsString::from("1")),
            ],
            "the child environment is the client's plus the adapter overlay, nothing else"
        );
        assert_eq!(prepared.cast_path, agent_dir.pty_cast());
        assert_eq!(
            prepared.terminal_profile,
            OsString::from_vec(b"xterm-\xf0".to_vec())
        );
    }

    /// Mutation: fall back to the supervisor's PATH, or launch without an adapter. Both refusals
    /// must happen before any filesystem artifact appears under the agent directory.
    #[test]
    fn production_factory_refuses_without_a_client_resolvable_executable_or_an_adapter() {
        let work = marion_testsupport::scratch("native-factory-refusals");
        let (bin, _executable) = fixture_executable(&work);
        let project = work.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let env = factory_env(&work);
        let agent_id = AgentId("019f81eb-36a4-7000-8000-000000000102".into());
        let registry = marion_core::NativeFacadeRegistry::new(ATLAS_DESCRIPTORS).unwrap();
        let selected = crate::native_intent::select_test_native(&registry, "atlas").unwrap();
        let geometry = NativeTerminalGeometry {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        };

        let unresolvable = atlas_context(
            &project,
            vec![],
            vec![(OsString::from("PATH"), work.join("empty").into_os_string())],
        );
        assert!(matches!(
            ProductionNativeCommandFactory::new(env.clone(), fixture_adapter).prepare(
                &selected,
                &unresolvable,
                geometry,
                &fixture_claim(&agent_id)
            ),
            Err(BootstrapError::NativeCommand(_))
        ));

        let resolvable = atlas_context(
            &project,
            vec![],
            vec![(OsString::from("PATH"), bin.as_os_str().to_owned())],
        );
        assert!(matches!(
            ProductionNativeCommandFactory::new(env.clone(), no_adapter).prepare(
                &selected,
                &resolvable,
                geometry,
                &fixture_claim(&agent_id)
            ),
            Err(BootstrapError::NativeCommand(_))
        ));

        assert!(
            matches!(
                std::fs::metadata(env.project_dir.agent(&agent_id).path()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
            "a refused preparation created the agent directory"
        );
    }

    /// An adapter whose row carries its declaration in a document: the file it names.
    struct DocumentAdapter(fn(&std::path::Path) -> PathBuf);

    impl marion_harness::NativeInjectionAdapter for DocumentAdapter {
        fn prepare_native(
            &self,
            context: &marion_harness::NativeNodeContext<'_>,
        ) -> Result<NativeInjection, marion_harness::NativeInjectionError> {
            let path = (self.0)(context.document_dir);
            Ok(NativeInjection {
                argv_prefix: vec![OsString::from("--mcp-config"), path.as_os_str().to_owned()],
                env_overlay: vec![],
                documents: vec![marion_harness::NativeDocument {
                    path,
                    contents: b"{\"mcpServers\":{\"marion\":{}}}\n".to_vec(),
                }],
            })
        }
    }

    static ROW_DOCUMENT: DocumentAdapter = DocumentAdapter(|dir| dir.join("mcp.json"));
    static ESCAPING_DOCUMENT: DocumentAdapter = DocumentAdapter(|dir| dir.join("../escape.json"));
    static NESTED_DOCUMENT: DocumentAdapter = DocumentAdapter(|dir| dir.join("nested/mcp.json"));

    fn row_document_adapter(
        _: marion_core::Harness,
    ) -> Option<&'static dyn marion_harness::NativeInjectionAdapter> {
        Some(&ROW_DOCUMENT)
    }

    fn escaping_document_adapter(
        _: marion_core::Harness,
    ) -> Option<&'static dyn marion_harness::NativeInjectionAdapter> {
        Some(&ESCAPING_DOCUMENT)
    }

    fn nested_document_adapter(
        _: marion_core::Harness,
    ) -> Option<&'static dyn marion_harness::NativeInjectionAdapter> {
        Some(&NESTED_DOCUMENT)
    }

    /// A declaration carried by a document is written **under the node's own directory, as a
    /// direct child, private to the operator, never over an existing file** — and any other path
    /// the adapter names is a refusal that writes nothing.
    ///
    /// Mutation: write through the absolute path instead of the directory descriptor, drop
    /// `O_EXCL`, widen the mode, or accept a name with a parent component.
    #[test]
    fn production_factory_materializes_a_row_document_under_the_node_dir_only() {
        use std::os::unix::fs::PermissionsExt;

        let work = marion_testsupport::scratch("native-factory-documents");
        let (bin, _executable) = fixture_executable(&work);
        let project = work.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let env = factory_env(&work);
        let registry = marion_core::NativeFacadeRegistry::new(ATLAS_DESCRIPTORS).unwrap();
        let selected = crate::native_intent::select_test_native(&registry, "atlas").unwrap();
        let geometry = NativeTerminalGeometry {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        };
        let context = atlas_context(
            &project,
            vec![OsString::from("--help")],
            vec![(OsString::from("PATH"), bin.as_os_str().to_owned())],
        );

        let agent_id = AgentId("019f81eb-36a4-7000-8000-000000000103".into());
        let prepared = ProductionNativeCommandFactory::new(env.clone(), row_document_adapter)
            .prepare(&selected, &context, geometry, &fixture_claim(&agent_id))
            .expect("a direct-child document materializes");
        let agent_dir = env.project_dir.agent(&agent_id);
        let document = agent_dir.path().join("mcp.json");
        assert_eq!(
            prepared.invocation.args,
            vec![
                OsString::from("--mcp-config"),
                document.as_os_str().to_owned(),
                OsString::from("--help"),
            ]
        );
        assert_eq!(
            std::fs::read(&document).unwrap(),
            b"{\"mcpServers\":{\"marion\":{}}}\n"
        );
        assert_eq!(
            std::fs::metadata(&document).unwrap().permissions().mode() & 0o777,
            0o600,
            "the declaration names the node and is the operator's alone"
        );
        assert!(
            matches!(
                ProductionNativeCommandFactory::new(env.clone(), row_document_adapter).prepare(
                    &selected,
                    &context,
                    geometry,
                    &fixture_claim(&agent_id)
                ),
                Err(BootstrapError::NativeCommand(_))
            ),
            "a second launch under the same node id must not overwrite the first's declaration"
        );

        for (label, table, id) in [
            (
                "escaping",
                escaping_document_adapter as NativeAdapterLookup,
                "019f81eb-36a4-7000-8000-000000000104",
            ),
            (
                "nested",
                nested_document_adapter as NativeAdapterLookup,
                "019f81eb-36a4-7000-8000-000000000105",
            ),
        ] {
            let agent_id = AgentId(id.into());
            assert!(
                matches!(
                    ProductionNativeCommandFactory::new(env.clone(), table).prepare(
                        &selected,
                        &context,
                        geometry,
                        &fixture_claim(&agent_id)
                    ),
                    Err(BootstrapError::NativeCommand(_))
                ),
                "{label}: a document outside the node's own directory was accepted"
            );
            assert!(
                matches!(
                    std::fs::metadata(env.project_dir.agent(&agent_id).path()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ),
                "{label}: a refused document created the agent directory"
            );
        }
        assert!(
            !env.project_dir.agents_dir().join("escape.json").exists()
                && !work.join("escape.json").exists(),
            "an escaping document was written"
        );
    }

    /// Mutation: omit writer commit or let the prepared lifecycle start before the ACK boundary.
    /// This uses the real launch map, authority, registry lease, child PTY, and gated worker.
    #[test]
    fn real_claim_preparation_reserves_writer_and_abort_reaps_without_starting_lifecycle() {
        let work = marion_testsupport::scratch("native-launch-real-claim-abort");
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot_path(&work.join("journal.jsonl")).unwrap(),
            Duration::from_millis(2),
        ));
        let handle = crate::handler::RegistryHandle::new(live);
        let authority = Arc::new(
            crate::native_bootstrap::PendingNativeLaunches::with_sources(
                Arc::new(SequenceRng::default()),
                Arc::new(ManualClock::default()),
                Duration::from_secs(60),
            ),
        );
        handle
            .install_pending_native_launches(Arc::clone(&authority))
            .unwrap_or_else(|_| panic!("authority installs once"));
        let agent_id = AgentId("019f81eb-36a4-7000-8000-000000000091".into());
        let pending = handle
            .reserve_pending_native_launch(binding(agent_id.clone()))
            .unwrap();
        let ticket = NativeLaunchTicket::for_test([
            0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ]);
        let owner: Arc<dyn crate::root::PaneOwner> = Arc::new(NativePaneOwner(Arc::clone(&handle)));
        let launch = NativeCommandLauncher::launch(
            NativeCommandSpec {
                agent_id: agent_id.clone(),
                invocation: invocation(&work),
                cast_path: work.join("pty.cast"),
                terminal_profile: OsString::from("xterm-256color"),
            },
            owner,
            Arc::new(NativeNodeJournal {
                handle: Arc::clone(&handle),
                agent_id: agent_id.clone(),
                harness_version: "unknown".into(),
            }),
        )
        .unwrap();
        handle
            .publish_pending_native_launch(pending.receipt())
            .unwrap();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let handler = NativeLaunchHandler::for_claim_test(
            Arc::clone(&handle),
            Arc::new(FailOnceLifecycleSpawner {
                fail: AtomicBool::new(true),
                finished: Mutex::new(Some(finished_tx)),
            }),
        );
        handler.insert_pending_for_test(agent_id.clone(), launch);

        assert!(
            handler
                .prepare_native_claim(
                    &ticket,
                    &agent_id,
                    NativeClaimant::new(ConnId(901), PeerIdentity::current_for_tty_test()),
                )
                .is_err(),
            "real lifecycle spawn failure must refuse before ACK"
        );
        assert!(
            authority.has_pending(&agent_id),
            "spawn failure consumed the ticket instead of restoring retry"
        );
        assert_eq!(
            handle.panes(),
            1,
            "spawn failure killed the retryable launch"
        );

        let prepared = handler
            .prepare_native_claim(
                &ticket,
                &agent_id,
                NativeClaimant::new(ConnId(901), PeerIdentity::current_for_tty_test()),
            )
            .expect("real claim prepares");
        assert!(
            !authority.has_pending(&agent_id),
            "omitting writer commit leaves the ticket retryable after preparation"
        );
        assert_eq!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "the lifecycle worker ran while the claim ACK remained blocked"
        );
        assert_eq!(handle.panes(), 1, "gated lifecycle ran before ACK");
        drop(prepared);
        finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("aborting the blocked ACK joins the gated worker");
        assert_eq!(handle.panes(), 0, "ACK failure leaked the real child pane");
    }

    /// **The factory probes the executable it resolved, on the pre-launch path** — the same
    /// `<program> --version` `run_spawn` asks a child, so a native root's `Spawned` carries a
    /// measured version rather than `"unknown"`. A binary that answers nothing is still
    /// `"unknown"`, never a refusal: the version is an audit field, not a launch gate.
    ///
    /// Mutation: skip the probe in `ProductionNativeCommandFactory::prepare` and the first
    /// assertion reads `"unknown"`.
    #[test]
    fn production_factory_probes_the_resolved_executables_version() {
        use std::os::unix::fs::PermissionsExt;
        let work = marion_testsupport::scratch("native-factory-version");
        let (bin, executable) = fixture_executable(&work);
        std::fs::write(
            &executable,
            b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 4.2.0; fi\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = factory_env(&work);
        let project = work.join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let agent_id = AgentId("native-version".into());
        let registry = marion_core::NativeFacadeRegistry::new(ATLAS_DESCRIPTORS).unwrap();
        let selected = crate::native_intent::select_test_native(&registry, "atlas").unwrap();
        let context = DirectNativeRequestContext::new(
            project.join(".git"),
            project.clone(),
            OsString::from("atlas"),
            vec![],
            OsString::from("xterm"),
            crate::native_bootstrap::NATIVE_WIRE_VERSION,
        )
        .with_environment(vec![(OsString::from("PATH"), bin.as_os_str().to_owned())]);
        let geometry = NativeTerminalGeometry {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        };
        let factory = ProductionNativeCommandFactory::new(env, fixture_adapter);

        let prepared = factory
            .prepare(&selected, &context, geometry, &fixture_claim(&agent_id))
            .expect("the fixture facade assembles");
        assert_eq!(prepared.harness_version, "4.2.0");

        // The same binary, now silent on `--version`: the honest placeholder, and still a launch.
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        let prepared = factory
            .prepare(&selected, &context, geometry, &fixture_claim(&agent_id))
            .expect("a binary that names no version still assembles");
        assert_eq!(prepared.harness_version, "unknown");
    }

    /// **A native root is `Running` while its pty session is up, and its `Spawned` names the
    /// harness version the launch probed** — the two facts `marion tree` showed as `spawning`
    /// and `unknown` for the node's whole life.
    ///
    /// Mutations: drop the `StateChanged(Running)` from `NativeNodeJournal::spawned` and the
    /// summary reads `Spawning` until the exit; write `"unknown"` for the version and the
    /// summary shows no version.
    #[test]
    fn a_native_root_replays_running_with_its_probed_version_once_spawned() {
        let work = marion_testsupport::scratch("native-launch-running-summary");
        let journal_path = work.join("journal.jsonl");
        let live = Arc::new(LiveRegistry::follow(
            Registry::boot_path(&journal_path).unwrap(),
            Duration::from_millis(2),
        ));
        let handle = crate::handler::RegistryHandle::new(live);
        let agent_id = AgentId("019f81eb-36a4-7000-8000-0000000000a1".into());
        let journal = NativeNodeJournal {
            handle: Arc::clone(&handle),
            agent_id: agent_id.clone(),
            harness_version: "9.9.9-probed".into(),
        };
        let agent_type = marion_core::agent_type::builtin("claude-impl").unwrap();
        journal.intent(&agent_type).unwrap();
        journal.spawned(std::process::id() as i32);

        let replayed = Registry::boot_path(&journal_path).unwrap();
        let node = replayed.tree().get(&agent_id).expect("the node replays");
        let summary = crate::handler::summarize(node, true).expect("a native root projects");
        assert_eq!(
            summary.state,
            NodeState::Running,
            "a native root whose pty session is up is running, not still spawning"
        );
        assert_eq!(
            summary.harness_version.as_deref(),
            Some("9.9.9-probed"),
            "the summary carries the version the launch probed, not a placeholder"
        );

        journal.exited(None);
        let replayed = Registry::boot_path(&journal_path).unwrap();
        let node = replayed.tree().get(&agent_id).expect("the node replays");
        assert!(
            node.state.is_exited(),
            "the exit still lands on the running node: {:?}",
            node.state
        );
    }
}
