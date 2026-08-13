//! Authenticated native selection composed with the existing pane lifecycle.
//!
//! This module is intentionally not installed in the production server yet. It is the generic
//! executor/claim substrate exercised by synthetic descriptors; the first real facade supplies
//! its independently verified invocation factory and readiness contract in the next slice.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use marion_core::NativeFacadeDescriptor;
use marion_core::contract::AgentId;
use marion_harness::{NativeInvocation, NativeTerminalGeometry};

use crate::native_bootstrap::{
    BootstrapError, ConsumedNativeRequest, DirectNativeRequestContext, NativeBootstrapHandler,
    NativeClaimant, NativeLaunchDeadline, NativeLaunchTicket, PendingNativeLaunchReceipt,
    PendingNativeLaunches, PreparedNativeRelay, TerminalGeometryObservation,
};
use crate::native_exec::{
    LaunchedNativeCommand, NativeCommandLauncher, NativeCommandSpec, NativeLifecycleSpawner,
    PreparedNativeLifecycle, ThreadNativeLifecycleSpawner,
};

pub(crate) struct PreparedNativeCommand {
    pub(crate) invocation: NativeInvocation,
    pub(crate) cast_path: PathBuf,
    pub(crate) terminal_profile: OsString,
}

/// Descriptor-specific assembly, injected so this generic executor never guesses vendor flags,
/// readiness, environment injection, or executable resolution.
pub(crate) trait NativeCommandFactory: Send + Sync + 'static {
    fn prepare(
        &self,
        selected: &crate::native_intent::SelectedNativeFacade<'_>,
        context: &DirectNativeRequestContext,
        geometry: NativeTerminalGeometry,
        agent_id: &AgentId,
    ) -> Result<PreparedNativeCommand, BootstrapError>;
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
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "unit tests construct the handler through their injected claim seam"
        )
    )]
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
                _: &AgentId,
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

        // Authority exists before the PaneOwner can make a host visible.
        let mut receipt = self
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
            &agent_id,
        )?;
        deadline.require_remaining()?;
        let owner: Arc<dyn crate::root::PaneOwner> =
            Arc::new(NativePaneOwner(Arc::clone(&self.handle)));
        let launch = NativeCommandLauncher::launch(
            NativeCommandSpec {
                agent_id: agent_id.clone(),
                invocation: prepared.invocation,
                cast_path: prepared.cast_path,
                terminal_profile: prepared.terminal_profile,
            },
            owner,
        )
        .map_err(BootstrapError::NativeTransportIo)?;
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use marion_core::contract::AgentId;
    use marion_harness::{
        NativeInjection, NativeProcessBase, NativeTerminalGeometry, assemble_native,
    };

    use super::*;
    use crate::native_bootstrap::{
        DirectNativeRequestContext, NativeLaunchBinding, NativeLaunchDescriptor,
        NativeLaunchTicket, PeerIdentity, TerminalFingerprint, TerminalGeometry, context_hash,
    };
    use crate::native_exec::{NativeLifecycleSpawner, NativeLifecycleTask};
    use crate::registry::{LiveRegistry, Registry};
    use crate::serve::ConnId;

    #[derive(Default)]
    struct TestClock;
    impl crate::native_bootstrap::MonotonicClock for TestClock {
        fn now(&self) -> Duration {
            Duration::ZERO
        }
    }

    #[derive(Default)]
    struct TestRng(std::sync::atomic::AtomicU64);
    impl crate::native_bootstrap::CapabilityRng for TestRng {
        fn fill(&self, bytes: &mut [u8]) -> Result<(), BootstrapError> {
            let value = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            bytes.fill(0);
            bytes[..8].copy_from_slice(&value.to_be_bytes());
            Ok(())
        }
    }

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
                Arc::new(TestRng::default()),
                Arc::new(TestClock),
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
}
