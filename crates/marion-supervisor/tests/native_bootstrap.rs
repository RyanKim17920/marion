#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::fs::PermissionsExt;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use marion_supervisor::native_bootstrap::{DirectNativeRequestContext, context_hash};
use marion_supervisor::serve::own_uid;
use marion_supervisor::socket::{Acquired, acquire, socket_paths};
use marion_testsupport::scratch;

mod common;

fn request_context(selector: &str, tail: Vec<OsString>) -> DirectNativeRequestContext {
    DirectNativeRequestContext::new(
        PathBuf::from(OsString::from_vec(b"/canonical/project-\xff".to_vec())),
        PathBuf::from(OsString::from_vec(b"/canonical/project-\xff/work".to_vec())),
        OsString::from(selector),
        tail,
        OsString::from_vec(b"xterm-\xfe".to_vec()),
        7,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn native_bootstrap_listener_is_a_private_sibling_of_the_project_socket() {
    let work = scratch("native-bootstrap-listener");
    let state = work.join("state");
    let project = work.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let paths = socket_paths(&state, &project, own_uid());
    assert_eq!(paths.native_bootstrap().parent(), paths.socket().parent());
    assert_ne!(paths.native_bootstrap(), paths.socket());

    let Acquired::Serving(serving) = acquire(&paths).expect("bind both project listeners") else {
        panic!("fresh project unexpectedly dialed an existing supervisor")
    };
    assert_eq!(serving.native_bootstrap_path(), paths.native_bootstrap());
    assert_eq!(
        std::fs::metadata(paths.native_bootstrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
    );
    let client = UnixStream::connect(paths.native_bootstrap()).expect("dial native sibling");
    let (accepted, _) = serving
        .native_bootstrap_listener()
        .expect("supported platforms expose the native listener")
        .accept()
        .expect("accept native sibling connection");
    drop((client, accepted));
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_platform_fails_closed_without_a_native_listener_or_socket_artifact() {
    let work = scratch("native-bootstrap-disabled-macos");
    let state = work.join("state");
    let project = work.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let paths = socket_paths(&state, &project, own_uid());

    let Acquired::Serving(serving) = acquire(&paths).expect("bind the ordinary project listener")
    else {
        panic!("fresh project unexpectedly dialed an existing supervisor")
    };
    assert!(serving.native_bootstrap_listener().is_none());
    assert!(!paths.native_bootstrap().exists());
}

#[test]
fn context_hash_is_byte_safe_and_preserves_argument_boundaries() {
    let separated = request_context(
        "atlas",
        vec![OsString::from_vec(vec![b'a', 0xff]), OsString::new()],
    );
    let joined = request_context("atlas", vec![OsString::from_vec(vec![b'a', 0xff, 0])]);
    let other_selector = request_context(
        "boreal",
        vec![OsString::from_vec(vec![b'a', 0xff]), OsString::new()],
    );

    assert_eq!(context_hash(&separated), context_hash(&separated));
    assert_ne!(context_hash(&separated), context_hash(&joined));
    assert_ne!(context_hash(&separated), context_hash(&other_selector));
}

/// The enabled native bootstrap service, wired the way the detached supervisor wires it, driven
/// through the real private socket by the shipped client dispatch running on a real controlling
/// terminal.
///
/// The production descriptor slice is still empty, so the supervisor here is composed in-process
/// through the same public constructor `detach.rs` stage 3 calls, with one test-registered facade
/// and a fixture adapter. The vendor process is a `claude` shim on the **client's** `PATH` that
/// records its argv and environment byte-exact and its working directory, and exits 37.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod enabled_launch {
    use std::ffi::OsString;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitCode, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use marion_core::harness::Harness;
    use marion_core::paths::ProjectDir;
    use marion_core::{
        Lane, NativeAdapterId, NativeFacadeDescriptor, NativeFacadeRegistry, NativeLane,
        VendorIdentity,
    };
    use marion_harness::{NativeInjection, NativeInjectionAdapter, NativeInjectionError};
    use marion_supervisor::facade_cli::dispatch_native_facade_or_legacy;
    use marion_supervisor::handler::RegistryHandle;
    use marion_supervisor::native_bootstrap::NativeBootstrapClient;
    use marion_supervisor::pty::{PtyMaster, WinSize};
    use marion_supervisor::registry::{LiveRegistry, Registry};
    use marion_supervisor::serve::{NativeLaunchConfig, Server, own_uid};
    use marion_supervisor::socket::{Acquired, acquire, project_root, socket_paths};
    use marion_testsupport::scratch;

    const DESCRIPTORS: &[NativeFacadeDescriptor] = &[NativeFacadeDescriptor {
        identity: VendorIdentity::new("claude"),
        command: "claude",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("claude", "claude", NativeAdapterId::new("claude-native")),
        )),
        structured: None,
    }];

    const PROBE_ENV: &str = "MARION_NATIVE_LAUNCH_PROBE";
    const MARKER_ENV: &str = "PROBE_MARKER";
    const LEAK_ENV: &str = "MARION_PROBE_MUST_NOT_REACH_THE_VENDOR";
    const PROBE_TAIL: &[&[u8]] = &[b"--", b"", b"\xff\x80x"];
    const INJECTED_PREFIX: &str = "--marion-injected";
    /// Where the probe adapter echoes the node token marion declared, since a `MARION_*` name may
    /// not reach the vendor process at all (`assemble_native`).
    const PROBE_TOKEN_ENV: &str = "PROBE_NODE_TOKEN";
    /// What the vendor shim writes to **its** terminal — the supervisor's pane — and what must
    /// therefore appear on the **operator's** terminal only if the relay carried it there.
    const VENDOR_GREETING: &str = "VENDOR_SAYS_HELLO";

    #[cfg(target_os = "linux")]
    const TIOCSCTTY: usize = 0x540e;
    #[cfg(target_os = "macos")]
    const TIOCSCTTY: usize = 0x2000_7461;

    struct ProbeAdapter;

    impl NativeInjectionAdapter for ProbeAdapter {
        fn prepare_native(
            &self,
            context: &marion_harness::NativeNodeContext<'_>,
        ) -> Result<NativeInjection, NativeInjectionError> {
            // The repository the bridge was told, echoed where the shim's environment record can
            // show it: `MARION_REPO` itself never reaches the vendor.
            let repo = context
                .bridge
                .pairs()
                .into_iter()
                .find(|(name, _)| name == "MARION_REPO")
                .map(|(_, value)| OsString::from(value))
                .expect("the bridge declaration names a repository");
            // §5.4's capability, echoed under a name of the probe's own: `MARION_*` may not reach
            // the vendor, and the shim's environment record is the only place outside the
            // supervisor where a test can read what marion really declared.
            let mut env_overlay = vec![
                (OsString::from("PROBE_INJECTED"), OsString::from("1")),
                (OsString::from("PROBE_REPO"), repo),
            ];
            if let Some((_, token)) = context
                .bridge
                .pairs()
                .into_iter()
                .find(|(name, _)| name == marion_harness::mcp_bridge::NODE_TOKEN_ENV)
            {
                env_overlay.push((OsString::from(PROBE_TOKEN_ENV), OsString::from(token)));
            }
            Ok(NativeInjection {
                argv_prefix: vec![OsString::from(INJECTED_PREFIX)],
                env_overlay,
                documents: vec![],
            })
        }
    }

    static PROBE_ADAPTER: ProbeAdapter = ProbeAdapter;

    fn probe_adapter(harness: Harness) -> Option<&'static dyn NativeInjectionAdapter> {
        (harness == Harness::ClaudeCode).then_some(&PROBE_ADAPTER)
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    fn write_shim(bin: &Path, marker: &Path) {
        std::fs::create_dir_all(bin).unwrap();
        let shim = bin.join("claude");
        // argv byte-exact with NUL separators, then the environment, then the completion marker
        // last so a reader that sees the marker sees complete records; then one line on its own
        // terminal, which only a relay can carry to the operator's.
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\n\
                 printf '%s\\0' \"$@\" > {marker}.argv\n\
                 /usr/bin/env > {marker}.env\n\
                 pwd -P > {marker}.cwd\n\
                 : > {marker}\n\
                 printf '{greeting}\\n'\n\
                 exit 37\n",
                marker = shell_quote(marker),
                greeting = VENDOR_GREETING,
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn wait_for(path: &Path, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            match path.try_exists() {
                Ok(true) => return true,
                Ok(false) => {}
                Err(error) => panic!("could not inspect {}: {error}", path.display()),
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Replay the journal until it carries a terminal record for a node, or `bound` passes.
    ///
    /// The client's exit and the `Exited` append are two observers of one event with no order
    /// between them. The vendor's death hangs up the pty, which ends the relay and the client;
    /// the supervisor's lifecycle worker (`native_exec::prepare_lifecycle`) sees the same death
    /// through `poll_exited_unreaped`'s 25 ms poll, reaps, finishes the pane and only then
    /// appends and fsyncs `Exited`. A replay taken at the client's wait status races that append
    /// and loses under load. The supervisor itself does not exit on its threads here but on the
    /// journal (§5.7 residency reads the replayed tree, `NonTerminalNode`), so the test waits on
    /// that same predicate and lets the assertions below read the settled tree.
    fn replay_until_a_node_is_terminal(project_dir: &ProjectDir, bound: Duration) -> Registry {
        let deadline = Instant::now() + bound;
        loop {
            let replayed = Registry::boot(project_dir).expect("the journal replays");
            let terminal = replayed
                .tree()
                .nodes()
                .iter()
                .any(|node| node.exit.is_some() || node.spawn_aborted.is_some());
            if terminal || Instant::now() >= deadline {
                return replayed;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn reap_with_watchdog(
        mut child: std::process::Child,
        ceiling: Duration,
    ) -> (std::process::ExitStatus, bool) {
        let pid = child.id() as i32;
        let (status_tx, status_rx) = std::sync::mpsc::sync_channel(1);
        let waiter = std::thread::spawn(move || {
            let _ = status_tx.send(child.wait());
        });
        let outcome = match status_rx.recv_timeout(ceiling) {
            Ok(status) => (status.expect("wait for the probe"), false),
            Err(_) => {
                unsafe extern "C" {
                    fn kill(pid: i32, signal: i32) -> i32;
                }
                // SAFETY: `pid` is the exact owned child; this is only the fixture ceiling.
                assert_eq!(unsafe { kill(pid, 9) }, 0, "kill the timed-out probe");
                let status = status_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("the killed probe was reaped")
                    .expect("wait for the killed probe");
                (status, true)
            }
        };
        waiter.join().expect("join the probe waiter");
        outcome
    }

    /// The shim's record of one run, read back off disk once the completion marker exists.
    struct VendorRecord {
        argv: Vec<u8>,
        env: String,
        cwd: PathBuf,
    }

    /// What the re-executed client half did, as its parent observed it.
    struct ProbeRun {
        status: std::process::ExitStatus,
        timed_out: bool,
        stderr: String,
        screen: String,
    }

    /// One supervisor composed the way stage 3 composes it, over a scratch project, with the
    /// vendor shim on a `PATH` of its own.
    struct Bed {
        work: marion_testsupport::Scratch,
        state: PathBuf,
        project: PathBuf,
        bin: PathBuf,
        marker: PathBuf,
        project_dir: ProjectDir,
        server: Server,
    }

    impl Bed {
        /// `prepare_project` runs on the empty project directory before the project is keyed, so a
        /// test can make it a repository and have the supervisor key on its common dir.
        fn start(tag: &str, prepare_project: impl FnOnce(&Path)) -> Self {
            let work = scratch(tag);
            let state = work.join("state");
            let project = work.join("project");
            let bin = work.join("bin");
            let marker = work.join("vendor-ran");
            std::fs::create_dir_all(&project).unwrap();
            prepare_project(&project);
            write_shim(&bin, &marker);

            let key = project_root(&project);
            let paths = socket_paths(&state, &key, own_uid());
            assert!(
                paths.overflow().is_none(),
                "this bed's socket must live under <state>; {:?} overflowed to /tmp",
                paths.socket()
            );
            let Acquired::Serving(serving) = acquire(&paths).expect("bind both listeners") else {
                panic!("fresh project unexpectedly dialed an existing supervisor")
            };
            let project_dir = ProjectDir::new(&state, paths.canonical_project());
            let live = Arc::new(LiveRegistry::follow(
                Registry::boot(&project_dir).expect("an absent journal is an empty tree"),
                Duration::from_millis(10),
            ));
            let env = marion_supervisor::run::Env {
                project_dir: project_dir.clone(),
                state: state.clone(),
                project_root: paths.canonical_project().to_path_buf(),
                bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
                base_url: Some("http://127.0.0.1:8099/v1".into()),
                auth: marion_harness::Auth::Canned,
            };
            let handle = RegistryHandle::owning(live, env.clone());
            let server = Server::start_with_native_launch(
                serving,
                Arc::clone(&handle),
                NativeLaunchConfig {
                    descriptors: DESCRIPTORS,
                    adapter_for: probe_adapter,
                    env,
                },
                Duration::from_secs(300),
            )
            .expect("the enabled native bootstrap service installs once");
            Self {
                work,
                state,
                project,
                bin,
                marker,
                project_dir,
                server,
            }
        }

        /// The client's `PATH`: the shim's directory first, then the directory `git` lives in,
        /// because resolving the project from a working directory asks git for the common dir and
        /// a client that cannot find git would key a repository's subdirectory as a project of its
        /// own. Nothing else — the supervisor process has a PATH this one must not be confused with.
        fn client_path(&self) -> std::ffi::OsString {
            let git_dir = std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .find(|dir| dir.join("git").is_file())
                .expect("git is on this test's PATH");
            std::env::join_paths([self.bin.as_path(), git_dir.as_path()]).unwrap()
        }

        /// Run the re-executed client half from `cwd`, on its own controlling terminal, with a
        /// `PATH` that names the shim and an environment the supervisor process does not have.
        fn run_probe(&self, cwd: &Path) -> ProbeRun {
            let terminal = PtyMaster::open(WinSize::new(117, 43)).expect("client PTY");
            let stdin: OwnedFd = terminal.open_slave().expect("stdin slave");
            let stdout: OwnedFd = terminal.open_slave().expect("stdout slave");
            let mut command = Command::new(std::env::current_exe().expect("test binary"));
            command
                .args([
                    "--exact",
                    "enabled_launch::native_launch_probe",
                    "--nocapture",
                ])
                .current_dir(cwd)
                .env_clear()
                .env(PROBE_ENV, "1")
                .env(MARKER_ENV, &self.marker)
                .env(LEAK_ENV, "leaked")
                .env("MARION_STATE_DIR", &self.state)
                .env("TERM", "xterm-256color")
                .env("HOME", &*self.work)
                .env("PATH", self.client_path())
                .stdin(Stdio::from(stdin))
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::piped());
            unsafe extern "C" {
                fn setsid() -> i32;
                fn ioctl(fd: i32, request: usize, ...) -> i32;
            }
            // SAFETY: runs after fork and before exec, performs only async-signal-safe session and
            // ioctl syscalls, and reports refusal as the OS error.
            unsafe {
                command.pre_exec(|| {
                    if setsid() < 0 || ioctl(0, TIOCSCTTY, 0_i32) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().expect("spawn the native launch probe");
            drop(command);
            let stderr = child.stderr.take().expect("probe stderr pipe");
            let reader = std::thread::spawn(move || {
                let mut output = Vec::new();
                let _ =
                    std::io::Read::read_to_end(&mut std::io::BufReader::new(stderr), &mut output);
                output
            });
            // The operator's screen: whatever the client process writes to its terminal. Read
            // until the last slave closes (the child has exited) or the watchdog ceiling.
            let terminal = Arc::new(terminal);
            let screen = {
                let terminal = Arc::clone(&terminal);
                std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(35);
                    let mut screen = Vec::new();
                    let mut bytes = [0u8; 4096];
                    while Instant::now() < deadline {
                        match terminal.read(&mut bytes) {
                            Ok(0) => break,
                            Ok(count) => screen.extend_from_slice(&bytes[..count]),
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                    screen
                })
            };
            let (status, timed_out) = reap_with_watchdog(child, Duration::from_secs(30));
            let stderr =
                String::from_utf8_lossy(&reader.join().expect("join stderr reader")).into_owned();
            let screen = String::from_utf8_lossy(&screen.join().expect("join the screen reader"))
                .into_owned();
            drop(terminal);
            ProbeRun {
                status,
                timed_out,
                stderr,
                screen,
            }
        }

        /// The probe relayed a native launch to completion and the vendor's own output crossed to
        /// the operator's terminal; then what the shim recorded.
        fn assert_relayed(&self, run: &ProbeRun) -> VendorRecord {
            let ProbeRun {
                status,
                timed_out,
                stderr,
                screen,
            } = run;
            assert!(!timed_out, "the probe exceeded its watchdog: {stderr}");
            assert!(status.success(), "the probe failed: {stderr}");
            assert!(
                stderr.contains("NATIVE_HANDOFF"),
                "the client never received a native handoff: {stderr}"
            );
            assert!(
                !stderr.contains("not enabled"),
                "the shipped seam still refuses the relay: {stderr}"
            );
            assert!(
                screen.contains(VENDOR_GREETING),
                "the vendor's own output never reached the operator's terminal; screen: \
                 {screen:?}; stderr: {stderr}"
            );
            assert!(
                wait_for(&self.marker, Duration::from_secs(5)),
                "the shim on the client's PATH never ran: {stderr}"
            );
            VendorRecord {
                argv: std::fs::read(self.marker.with_extension("argv")).unwrap(),
                env: std::fs::read_to_string(self.marker.with_extension("env")).unwrap(),
                cwd: PathBuf::from(
                    std::fs::read_to_string(self.marker.with_extension("cwd"))
                        .unwrap()
                        .trim_end_matches('\n'),
                ),
            }
        }

        /// The settled journal, once it carries the native node's terminal record.
        fn replayed(&self) -> Registry {
            replay_until_a_node_is_terminal(&self.project_dir, Duration::from_secs(10))
        }
    }

    #[test]
    fn a_detached_supervisor_authorizes_a_native_launch_for_a_registered_facade() {
        let bed = Bed::start("native-enabled-launch", |_| {});
        let run = bed.run_probe(&bed.project);
        let vendor = bed.assert_relayed(&run);

        let mut expected_argv = Vec::new();
        expected_argv.extend_from_slice(INJECTED_PREFIX.as_bytes());
        expected_argv.push(0);
        for argument in PROBE_TAIL {
            expected_argv.extend_from_slice(argument);
            expected_argv.push(0);
        }
        assert_eq!(
            vendor.argv, expected_argv,
            "the vendor argv is not program + prefix + opaque tail, byte for byte"
        );
        let vendor_env = &vendor.env;
        let client_path = bed.client_path();
        assert!(
            vendor_env
                .lines()
                .any(|line| line == format!("PATH={}", client_path.to_string_lossy())),
            "the vendor did not inherit the client's PATH: {vendor_env}"
        );
        assert!(
            vendor_env.lines().any(|line| line == "PROBE_INJECTED=1"),
            "the adapter overlay did not reach the vendor: {vendor_env}"
        );
        assert!(
            !vendor_env.lines().any(|line| line.starts_with("MARION_")),
            "reserved MARION_* identity reached the vendor process: {vendor_env}"
        );

        // The native node is a node: §6.1 step 7's intent, confirmation and terminal record are
        // in this project's journal, so the tree, restart and `node/attach` all know it.
        let replayed = bed.replayed();
        let nodes = replayed.tree().nodes().to_vec();
        assert_eq!(nodes.len(), 1, "exactly one node was journaled: {nodes:?}");
        let node = &nodes[0];
        let intent = node
            .intent
            .as_ref()
            .expect("a SpawnIntent names the native root");
        assert_eq!(intent.agent_type, "claude");
        assert_eq!(intent.harness, Harness::ClaudeCode);
        assert_eq!(intent.depth, 0);
        assert_eq!(intent.parent_id, None);
        assert_eq!(intent.task_id, None, "a root has no contract");
        assert!(node.spawn_confirmed, "no Spawned record: {node:?}");
        assert!(node.pid.is_some(), "Spawned carries no pid: {node:?}");
        assert_eq!(node.spawn_aborted, None);
        let exit = node
            .exit
            .as_ref()
            .expect("an Exited record closes the node");
        assert_eq!(exit.code, Some(37), "{exit:?}");
        assert!(node.state.is_exited(), "{:?}", node.state);
        assert!(
            bed.project_dir.agents_dir().read_dir().unwrap().count() == 1,
            "exactly one agent directory holds the native session's cast"
        );

        bed.server.stop();
    }

    /// In a repository §2 keys the project on the git common dir, so the canonical project the
    /// bootstrap authenticates is `<repo>/.git` — a key, not a place. The vendor runs where the
    /// operator invoked the facade, and the bridge is told the working tree that directory is in.
    ///
    /// Mutation: run the vendor in the canonical project (the demo saw Claude Code, Codex and
    /// copilot each print `.git` as their workspace), or hand the bridge the key or the
    /// subdirectory as the repository.
    #[test]
    fn the_native_command_runs_in_the_clients_working_directory_not_the_common_dir() {
        let bed = Bed::start("native-launch-cwd", |project| {
            let status = Command::new("git")
                .args(["init", "-q"])
                .current_dir(project)
                .status()
                .expect("git runs");
            assert!(status.success(), "git init: {status}");
        });
        let key = project_root(&bed.project);
        assert_eq!(
            key,
            bed.project.join(".git"),
            "the project is keyed on its common dir"
        );
        let cwd = bed.project.join("crates").join("deep");
        std::fs::create_dir_all(&cwd).unwrap();

        let run = bed.run_probe(&cwd);
        let vendor = bed.assert_relayed(&run);

        assert_eq!(
            vendor.cwd, cwd,
            "the vendor did not run where the operator invoked the facade"
        );
        assert!(
            vendor
                .env
                .lines()
                .any(|line| line == format!("PROBE_REPO={}", bed.project.display())),
            "the bridge was not told the working tree as the repository: {}",
            vendor.env
        );

        let replayed = bed.replayed();
        let nodes = replayed.tree().nodes().to_vec();
        assert_eq!(nodes.len(), 1, "exactly one node was journaled: {nodes:?}");
        assert_eq!(
            nodes[0].exit.as_ref().and_then(|exit| exit.code),
            Some(37),
            "{:?}",
            nodes[0].exit
        );

        bed.server.stop();
    }

    /// **A native root is a journaled node, so it can delegate** — §5.4's capability, minted for
    /// the native root at the instant its `SpawnIntent` became durable and carried in the
    /// declaration marion writes for the operator's own session.
    ///
    /// The whole point of putting marion's MCP server in front of the operator's harness is that
    /// the harness can call `spawn`. Before this, every `spawn` from a native root was refused by
    /// its own bridge — `MARION_NODE_TOKEN` was never declared, so `mcp::node_identity` refused
    /// before a frame was sent — and the two halves agreed with each other, which is why nothing
    /// was red. This asserts the far end instead: the token the vendor really received is
    /// presented on the project socket as the bridge would present it, and the supervisor journals
    /// a child under the native root.
    ///
    /// Mutation: `native_launch.rs`'s `bridge_for` back to `node_token: None`, or the claim moved
    /// after the declaration is written. The shim's environment record loses the key and this test
    /// names it; drop only the `RegistryHandle::claim` and the spawn is refused with §5.4's
    /// sentence instead.
    #[test]
    fn a_native_roots_declaration_carries_a_token_that_authorizes_a_spawn_under_it() {
        use marion_core::proto::Call;
        use marion_core::proto::params::AgentSpawnParams;

        let bed = Bed::start("native-launch-delegates", |project| {
            let status = Command::new("git")
                .args(["init", "-q"])
                .current_dir(project)
                .status()
                .expect("git runs");
            assert!(status.success(), "git init: {status}");
        });
        let run = bed.run_probe(&bed.project);
        let vendor = bed.assert_relayed(&run);

        let token = vendor
            .env
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{PROBE_TOKEN_ENV}=")))
            .unwrap_or_else(|| {
                panic!(
                    "the native root's declaration carries no {}, so its bridge can never prove \
                     which node it serves and every `spawn` from the operator's own session is \
                     refused by §5.4 before it reaches the socket:\n{}",
                    marion_harness::mcp_bridge::NODE_TOKEN_ENV,
                    vendor.env
                )
            })
            .to_string();
        assert!(
            !token.trim().is_empty(),
            "an empty capability is one every process on this machine already has"
        );

        let replayed = bed.replayed();
        let nodes = replayed.tree().nodes().to_vec();
        assert_eq!(nodes.len(), 1, "exactly one node was journaled: {nodes:?}");
        let root = nodes[0].agent_id.clone();

        // The frame the node's own bridge would send, on the socket the declaration names.
        let paths = socket_paths(&bed.state, &project_root(&bed.project), own_uid());
        let mut client = crate::common::client::Client::dial(&paths);
        client.send(Call::AgentSpawn(AgentSpawnParams {
            agent_type: "claude".into(),
            prompt: "a child of the operator's own native session".into(),
            native_launch: None,
            caller: Some(marion_core::proto::SpawnCaller {
                agent_id: root.clone(),
                node_token: token,
            }),
            // Forbidden beside a caller: the supervisor knows which tree this node lives in.
            repo: None,
            acceptance_criteria: vec![],
            writable_scope: vec![],
            timeout_secs: Some(5),
            model: None,
            no_change_record: None,
            pane: None,
            isolation: None,
            allow_concurrent_writes: None,
        }));

        // `SpawnIntent` is journaled before the spawn's first side effect, so the child is in the
        // tree from the first instant it exists at all — whatever the harness does afterwards.
        // The response is deliberately not read: what is asserted is that authorization passed,
        // not what a child marion could not run on this machine went on to do.
        let mut child = None;
        let found = marion_testsupport::until_within(
            Duration::from_secs(30),
            Duration::from_millis(25),
            || {
                let replayed = Registry::boot(&bed.project_dir).expect("the journal replays");
                child = replayed
                    .tree()
                    .nodes()
                    .iter()
                    .find(|node| node.parent_id() == Some(&root))
                    .map(|node| (node.agent_id.clone(), node.intent.clone()));
                child.is_some()
            },
        );
        assert!(
            found,
            "no child was journaled under the native root {}: the supervisor refused the spawn \
             the operator's own session asked for",
            root.0
        );
        let (_, intent) = child.expect("a child under the native root");
        let intent = intent.expect("a SpawnIntent names the child");
        assert_eq!(
            intent.parent_id,
            Some(root),
            "§7.5: written once, immutable"
        );
        assert_eq!(intent.depth, 1, "one level below the native root");

        bed.server.stop();
    }

    /// The re-executed client half of the test above; a no-op unless it was spawned as one.
    #[test]
    fn native_launch_probe() {
        if std::env::var_os(PROBE_ENV).is_none() {
            return;
        }
        unsafe extern "C" {
            fn _exit(status: i32) -> !;
        }
        let marker = PathBuf::from(std::env::var_os(MARKER_ENV).expect("the marker path"));
        let registry = NativeFacadeRegistry::new(DESCRIPTORS).expect("probe registry");
        let mut argv = vec![OsString::from("claude")];
        argv.extend(
            PROBE_TAIL
                .iter()
                .map(|argument| OsString::from_vec(argument.to_vec())),
        );
        let status = dispatch_native_facade_or_legacy(
            argv,
            &registry,
            NativeBootstrapClient::connect_for_cwd,
            |handoff| {
                eprintln!("NATIVE_HANDOFF");
                // The shipped continuation: claim, relay until the vendor's pane ends, restore.
                let status = marion_supervisor::facade_cli::relay_native_facade(handoff);
                if !wait_for(&marker, Duration::from_secs(10)) {
                    eprintln!("VENDOR_NEVER_RAN");
                    return ExitCode::FAILURE;
                }
                status
            },
            std::io::stderr(),
            || panic!("the registered selector reached the legacy CLI"),
        );
        let code = i32::from(status != ExitCode::SUCCESS);
        // SAFETY: this re-exec helper has emitted its complete result and must not re-enter libtest.
        unsafe { _exit(code) }
    }
}
