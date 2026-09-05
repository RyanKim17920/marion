use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Condvar, Mutex};

use marion_core::contract::AgentId;
use marion_harness::{ExecutionSurfaces, NativeInvocation};

/// One fully assembled native process, ready for the supervisor-owned PTY.
pub(crate) struct NativeCommandSpec {
    pub(crate) agent_id: AgentId,
    pub(crate) invocation: NativeInvocation,
    pub(crate) cast_path: PathBuf,
    pub(crate) terminal_profile: OsString,
}

pub(crate) struct NativeCommandLauncher;

pub(crate) type NativeLifecycleTask =
    Box<dyn FnOnce() -> std::io::Result<Option<ExitStatus>> + Send + 'static>;

pub(crate) trait NativeLifecycleSpawner: Send + Sync + 'static {
    fn spawn(
        &self,
        name: String,
        task: NativeLifecycleTask,
    ) -> std::io::Result<std::thread::JoinHandle<std::io::Result<Option<ExitStatus>>>>;
}

pub(crate) struct ThreadNativeLifecycleSpawner;

impl NativeLifecycleSpawner for ThreadNativeLifecycleSpawner {
    fn spawn(
        &self,
        name: String,
        task: NativeLifecycleTask,
    ) -> std::io::Result<std::thread::JoinHandle<std::io::Result<Option<ExitStatus>>>> {
        std::thread::Builder::new().name(name).spawn(task)
    }
}

/// A launched and published native process which has not yet crossed the response handoff.
///
/// Drop is rollback: callers must [`Self::commit`] only after every fallible step needed to hand
/// the launch to its client has succeeded. This keeps a failed bootstrap from leaving either a
/// registered pane or a child whose only owner was the failed request.
pub(crate) struct LaunchedNativeCommand {
    pending: Option<PendingNativeCommand>,
}

/// A lifecycle worker which exists but cannot observe or finish the pane until the claim ACK has
/// been flushed. Drop is the pre-ACK rollback boundary: wake the worker with Abort, join it, then
/// tear down the still-request-owned launch.
pub(crate) struct PreparedNativeLifecycle {
    pending: Option<PendingNativeCommand>,
    lifecycle: Option<std::thread::JoinHandle<std::io::Result<Option<ExitStatus>>>>,
    gate: Arc<LifecycleGate>,
}

pub(crate) struct NativeLifecycleSpawnError {
    source: std::io::Error,
    launch: LaunchedNativeCommand,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LifecycleDecision {
    Pending,
    Start,
    Abort,
}

struct LifecycleGate {
    decision: Mutex<LifecycleDecision>,
    changed: Condvar,
}

struct PendingNativeCommand {
    agent_id: AgentId,
    owner: Arc<dyn crate::root::PaneOwner>,
    host: Arc<crate::pty::PtyHost>,
}

/// The supervisor-owned lifecycle after the bootstrap handoff committed.
pub(crate) struct RunningNativeCommand {
    /// Production detaches the lifecycle worker once the claim has committed; only tests join it
    /// to read the exit status the pane-v1 relay otherwise delivers.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "joined only by tests; the committed lifecycle is detached in production"
        )
    )]
    lifecycle: std::thread::JoinHandle<std::io::Result<Option<ExitStatus>>>,
}

impl NativeCommandLauncher {
    pub(crate) fn launch(
        spec: NativeCommandSpec,
        owner: Arc<dyn crate::root::PaneOwner>,
    ) -> std::io::Result<LaunchedNativeCommand> {
        let NativeCommandSpec {
            agent_id,
            invocation,
            cast_path,
            terminal_profile,
        } = spec;
        let size = crate::pty::WinSize::new(invocation.geometry.cols, invocation.geometry.rows);
        if size.cols == 0 || size.rows == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native terminal geometry requires non-zero rows and columns",
            ));
        }
        let term = terminal_profile.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native terminal profile is not valid UTF-8",
            )
        })?;
        let master = crate::pty::PtyMaster::open(size)?;
        let host = Arc::new(crate::pty::PtyHost::start(
            agent_id.clone(),
            master,
            &cast_path,
            size,
            term,
            std::time::Instant::now(),
        )?);
        let witness = ExecutionSurfaces::opaque()
            .display_plane()
            .expect("opaque native execution always declares a PTY");
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .env_clear()
            .envs(invocation.env.iter().cloned())
            .current_dir(&invocation.cwd);
        host.adopt(crate::pty::spawn_pty(
            witness,
            &mut command,
            host.master(),
            crate::pty::StdinPlan::TerminalSlave,
            None,
        )?);

        let mut launched = LaunchedNativeCommand {
            pending: Some(PendingNativeCommand {
                agent_id: agent_id.clone(),
                owner: Arc::clone(&owner),
                host: Arc::clone(&host),
            }),
        };
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.opened(&agent_id, Arc::clone(&host));
        })) {
            launched.rollback();
            std::panic::resume_unwind(panic);
        }

        Ok(launched)
    }
}

impl LaunchedNativeCommand {
    /// One-step handoff for tests which have no external acknowledgement boundary. Native claim
    /// transport uses `prepare_lifecycle` directly so thread creation stays pre-ACK.
    #[cfg(test)]
    pub(crate) fn commit(self) -> std::io::Result<RunningNativeCommand> {
        match self.prepare_lifecycle(&ThreadNativeLifecycleSpawner) {
            Ok(prepared) => Ok(prepared.commit()),
            Err(failure) => {
                let (error, launch) = failure.into_parts();
                drop(launch);
                Err(error)
            }
        }
    }

    /// Create the lifecycle thread while the request can still refuse the claim. The worker is
    /// gated, so spawning it has no pane-lifecycle effects before a later infallible commit.
    pub(crate) fn prepare_lifecycle(
        mut self,
        spawner: &dyn NativeLifecycleSpawner,
    ) -> Result<PreparedNativeLifecycle, NativeLifecycleSpawnError> {
        let pending = self
            .pending
            .as_ref()
            .expect("a native launch can be prepared only once");
        let lifecycle_owner = Arc::clone(&pending.owner);
        let lifecycle_host = Arc::clone(&pending.host);
        let lifecycle_agent = pending.agent_id.clone();
        let gate = Arc::new(LifecycleGate::new());
        let worker_gate = Arc::clone(&gate);
        match spawner.spawn(
            format!("marion-native-{}", pending.agent_id.0),
            Box::new(move || {
                if worker_gate.wait() == LifecycleDecision::Abort {
                    return Ok(None);
                }
                let waited = crate::root::wait_for_the_pane_to_end(&lifecycle_host, None);
                crate::root::finish_terminal_pane(
                    Some(lifecycle_owner.as_ref()),
                    &lifecycle_agent,
                    &lifecycle_host,
                    waited,
                    |timed_out| lifecycle_host.shutdown_with_timeout_outcome(timed_out),
                    || lifecycle_host.completed_replay_charge(),
                )
                .map(|(status, _)| status)
            }),
        ) {
            Ok(lifecycle) => Ok(PreparedNativeLifecycle {
                pending: self.pending.take(),
                lifecycle: Some(lifecycle),
                gate,
            }),
            Err(source) => Err(NativeLifecycleSpawnError {
                source,
                launch: self,
            }),
        }
    }

    fn rollback(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.rollback();
        }
    }
}

impl NativeLifecycleSpawnError {
    pub(crate) fn into_parts(self) -> (std::io::Error, LaunchedNativeCommand) {
        (self.source, self.launch)
    }
}

impl PreparedNativeLifecycle {
    /// Release the already-created worker only after the wire acknowledgement is durable. This
    /// operation cannot fail: all allocation and thread creation happened in `prepare_lifecycle`.
    pub(crate) fn commit(mut self) -> RunningNativeCommand {
        let lifecycle = self
            .lifecycle
            .take()
            .expect("a prepared native lifecycle commits once");
        self.gate.decide(LifecycleDecision::Start);
        self.pending.take();
        RunningNativeCommand { lifecycle }
    }

    /// Undo preparation without killing the launch, for a failure which happened before the ACK
    /// and therefore remains safe to retry with the same ticket.
    pub(crate) fn into_launch(mut self) -> LaunchedNativeCommand {
        self.abort_and_join();
        LaunchedNativeCommand {
            pending: self.pending.take(),
        }
    }

    fn abort_and_join(&mut self) {
        self.gate.decide(LifecycleDecision::Abort);
        if let Some(lifecycle) = self.lifecycle.take() {
            let _ = lifecycle.join();
        }
    }
}

impl Drop for PreparedNativeLifecycle {
    fn drop(&mut self) {
        self.abort_and_join();
        if let Some(pending) = self.pending.take() {
            pending.rollback();
        }
    }
}

impl LifecycleGate {
    fn new() -> Self {
        Self {
            decision: Mutex::new(LifecycleDecision::Pending),
            changed: Condvar::new(),
        }
    }

    fn decide(&self, decision: LifecycleDecision) {
        let mut current = self
            .decision
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *current == LifecycleDecision::Pending {
            *current = decision;
            self.changed.notify_one();
        }
    }

    fn wait(&self) -> LifecycleDecision {
        let mut decision = self
            .decision
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while *decision == LifecycleDecision::Pending {
            decision = self
                .changed
                .wait(decision)
                .unwrap_or_else(|error| error.into_inner());
        }
        *decision
    }
}

impl Drop for LaunchedNativeCommand {
    fn drop(&mut self) {
        self.rollback();
    }
}

impl PendingNativeCommand {
    fn rollback(self) {
        // A callback is a boundary owned by another subsystem. Even a panic there must not skip
        // process teardown or the final invalidation callback.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.owner.closing(&self.agent_id, &self.host);
        }));
        let _ = self.host.shutdown();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.owner.failed(&self.agent_id, &self.host);
        }));
    }
}

impl RunningNativeCommand {
    #[cfg(test)]
    fn wait(self) -> std::io::Result<ExitStatus> {
        self.lifecycle
            .join()
            .map_err(|_| std::io::Error::other("native lifecycle thread panicked"))??
            .ok_or_else(|| std::io::Error::other("native child produced no exit status"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    use std::panic::AssertUnwindSafe;
    use std::sync::{Arc, Mutex};

    use marion_core::contract::AgentId;
    use marion_harness::{
        NativeInjection, NativeProcessBase, NativeTerminalGeometry, assemble_native,
    };
    use marion_testsupport::scratch;

    use super::{
        NativeCommandLauncher, NativeCommandSpec, NativeLifecycleSpawner, NativeLifecycleTask,
        ThreadNativeLifecycleSpawner,
    };

    struct FailingLifecycleSpawner;

    impl NativeLifecycleSpawner for FailingLifecycleSpawner {
        fn spawn(
            &self,
            _name: String,
            _task: NativeLifecycleTask,
        ) -> std::io::Result<
            std::thread::JoinHandle<std::io::Result<Option<std::process::ExitStatus>>>,
        > {
            Err(std::io::Error::other(
                "injected lifecycle thread spawn failure",
            ))
        }
    }

    #[derive(Default)]
    struct RecordingOwner {
        opened: Mutex<Vec<AgentId>>,
        events: Mutex<Vec<&'static str>>,
        host: Mutex<Option<Arc<crate::pty::PtyHost>>>,
    }

    impl crate::root::PaneOwner for RecordingOwner {
        fn opened(&self, agent_id: &AgentId, host: Arc<crate::pty::PtyHost>) {
            self.opened.lock().unwrap().push(agent_id.clone());
            self.events.lock().unwrap().push("opened");
            *self.host.lock().unwrap() = Some(host);
        }

        fn closing(&self, _: &AgentId, _: &Arc<crate::pty::PtyHost>) {
            self.events.lock().unwrap().push("closing");
        }

        fn completed(&self, _: &AgentId, _: &Arc<crate::pty::PtyHost>, _: usize) {
            self.events.lock().unwrap().push("completed");
        }

        fn failed(&self, _: &AgentId, _: &Arc<crate::pty::PtyHost>) {
            self.events.lock().unwrap().push("failed");
        }
    }

    fn invocation(
        program: OsString,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        cwd: &std::path::Path,
    ) -> marion_harness::NativeInvocation {
        assemble_native(
            NativeProcessBase {
                program,
                user_argv: args,
                env,
                cwd: cwd.to_path_buf(),
                geometry: NativeTerminalGeometry {
                    cols: 97,
                    rows: 31,
                    xpixel: 0,
                    ypixel: 0,
                },
            },
            NativeInjection {
                argv_prefix: Vec::new(),
                env_overlay: Vec::new(),
                documents: Vec::new(),
            },
        )
        .unwrap()
        .invocation
    }

    fn spec(
        work: &std::path::Path,
        invocation: marion_harness::NativeInvocation,
    ) -> NativeCommandSpec {
        NativeCommandSpec {
            agent_id: AgentId("019f81eb-36a4-7000-8000-000000000001".into()),
            invocation,
            cast_path: work.join("pty.cast"),
            terminal_profile: OsString::from("xterm-256color"),
        }
    }

    #[test]
    fn selected_native_command_is_spawned_byte_exact_and_published_before_return() {
        let work = scratch("native-exec-selected-command");
        let observed = work.join("observed");
        let script = work.join("native-fixture");
        std::fs::write(&script, b"#!/bin/sh\nprintf '%s' \"$1\" > \"$OBSERVED\"\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let argument = OsString::from_vec(vec![b'n', b'a', b't', b'i', b'v', b'e', 0xf6]);
        let owner = Arc::new(RecordingOwner::default());
        let agent_id = AgentId("019f81eb-36a4-7000-8000-000000000001".into());
        let invocation = invocation(
            script.into_os_string(),
            vec![argument.clone()],
            vec![(
                OsString::from("OBSERVED"),
                observed.clone().into_os_string(),
            )],
            &work,
        );

        let launched = NativeCommandLauncher::launch(
            NativeCommandSpec {
                agent_id: agent_id.clone(),
                invocation,
                cast_path: work.join("pty.cast"),
                terminal_profile: OsString::from("xterm-256color"),
            },
            owner.clone(),
        )
        .expect("the exact selected command launches");

        assert_eq!(
            owner.opened.lock().unwrap().as_slice(),
            &[agent_id],
            "pane publication must precede the launch handoff"
        );
        let status = launched
            .commit()
            .expect("the published launch commits")
            .wait()
            .expect("the native command is reaped");
        assert!(status.success());
        assert_eq!(
            std::fs::read(observed).unwrap().as_slice(),
            argument.as_os_str().as_bytes()
        );
    }

    #[test]
    fn spawn_failure_never_publishes_a_pane() {
        let work = scratch("native-exec-spawn-failure");
        let owner = Arc::new(RecordingOwner::default());
        let result = NativeCommandLauncher::launch(
            spec(
                &work,
                invocation(
                    work.join("missing-native-command").into_os_string(),
                    Vec::new(),
                    Vec::new(),
                    &work,
                ),
            ),
            owner.clone(),
        );

        assert!(result.is_err());
        assert!(owner.events.lock().unwrap().is_empty());
    }

    #[test]
    fn opened_callback_panic_revokes_and_reaps_the_published_pane() {
        struct PanickingOwner(RecordingOwner);

        impl crate::root::PaneOwner for PanickingOwner {
            fn opened(&self, id: &AgentId, host: Arc<crate::pty::PtyHost>) {
                self.0.opened(id, host);
                panic!("injected opened callback failure");
            }

            fn closing(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
                self.0.closing(id, host);
            }

            fn completed(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>, charge: usize) {
                self.0.completed(id, host, charge);
            }

            fn failed(&self, id: &AgentId, host: &Arc<crate::pty::PtyHost>) {
                self.0.failed(id, host);
            }
        }

        let work = scratch("native-exec-opened-panic");
        let owner = Arc::new(PanickingOwner(RecordingOwner::default()));
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = NativeCommandLauncher::launch(
                spec(
                    &work,
                    invocation(
                        OsString::from("/bin/sh"),
                        vec![OsString::from("-c"), OsString::from("sleep 30")],
                        Vec::new(),
                        &work,
                    ),
                ),
                owner.clone(),
            );
        }));

        assert!(panic.is_err());
        assert_eq!(
            owner.0.events.lock().unwrap().as_slice(),
            &["opened", "closing", "failed"]
        );
    }

    #[test]
    fn uncommitted_launch_drop_revokes_and_reaps_the_published_pane() {
        let work = scratch("native-exec-uncommitted-drop");
        let owner = Arc::new(RecordingOwner::default());
        let launched = NativeCommandLauncher::launch(
            spec(
                &work,
                invocation(
                    OsString::from("/bin/sh"),
                    vec![OsString::from("-c"), OsString::from("sleep 30")],
                    Vec::new(),
                    &work,
                ),
            ),
            owner.clone(),
        )
        .expect("child and pane are pending");

        drop(launched);

        assert_eq!(
            owner.events.lock().unwrap().as_slice(),
            &["opened", "closing", "failed"]
        );
    }

    /// Mutation: consume or roll back the launch when lifecycle thread creation fails. The second
    /// preparation would then be impossible, or owner callbacks would reveal premature teardown.
    #[test]
    fn lifecycle_spawn_failure_preserves_launch_for_retry_without_lifecycle_effects() {
        let work = scratch("native-exec-lifecycle-spawn-retry");
        let owner = Arc::new(RecordingOwner::default());
        let launched = NativeCommandLauncher::launch(
            spec(
                &work,
                invocation(
                    OsString::from("/bin/sh"),
                    vec![OsString::from("-c"), OsString::from("sleep 30")],
                    Vec::new(),
                    &work,
                ),
            ),
            owner.clone(),
        )
        .expect("child and pane are pending");

        let failure = match launched.prepare_lifecycle(&FailingLifecycleSpawner) {
            Ok(_) => panic!("injected lifecycle spawn failure unexpectedly succeeded"),
            Err(failure) => failure,
        };
        let (error, launched) = failure.into_parts();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            owner.events.lock().unwrap().as_slice(),
            &["opened"],
            "failed thread creation ran pane lifecycle or rollback callbacks"
        );

        let prepared = launched
            .prepare_lifecycle(&ThreadNativeLifecycleSpawner)
            .unwrap_or_else(|_| panic!("the intact launch remains lifecycle-preparable"));
        assert_eq!(
            owner.events.lock().unwrap().as_slice(),
            &["opened"],
            "the pre-spawned worker crossed its Start gate"
        );
        drop(prepared);
        assert_eq!(
            owner.events.lock().unwrap().as_slice(),
            &["opened", "closing", "failed"],
            "Abort did not join and roll back the request-owned launch"
        );
    }
}
