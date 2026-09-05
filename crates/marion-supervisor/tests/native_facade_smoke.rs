//! The shipped `marion <harness> …` binary in front of a real harness, on a real terminal.
//!
//! Not the M3 E2E matrix (a TUI cell on screen, keystrokes, resize, detach — `plan-native-
//! activation.md` step 9). One smoke proof for the composition: the production registry, the
//! row-derived adapter table and the production factory in a supervisor wired the way
//! `detach.rs` stage 3 wires it; the **shipped** `marion` binary as the client, on its own
//! controlling PTY, running `codex --help` through the facade; and the harness's own help text on
//! the operator's terminal with termios back where it started.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_core::paths::ProjectDir;
use marion_supervisor::handler::RegistryHandle;
use marion_supervisor::pty::{PtyMaster, WinSize};
use marion_supervisor::registry::{LiveRegistry, Registry};
use marion_supervisor::serve::{NativeLaunchConfig, Server, own_uid};
use marion_supervisor::socket::{Acquired, acquire, project_root, socket_paths};
use marion_testsupport::{on_path, scratch};

#[cfg(target_os = "linux")]
const TIOCSCTTY: usize = 0x540e;
#[cfg(target_os = "macos")]
const TIOCSCTTY: usize = 0x2000_7461;

/// The facade under test: the one enabled production lane whose `--help` needs no login.
const HARNESS: &str = "codex";

/// The terminal's line discipline, read through a slave opened for the read and closed again, so
/// no extra slave outlives the client (a lingering one would keep the master from seeing EOF).
fn termios_of(master: &PtyMaster) -> String {
    let probe: OwnedFd = master.open_slave().expect("termios probe slave");
    format!("{:?}", rustix::termios::tcgetattr(probe.as_fd()).unwrap())
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
        Ok(status) => (status.expect("wait for the shipped client"), false),
        Err(_) => {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // SAFETY: `pid` is the exact owned child; this is only the fixture ceiling.
            assert_eq!(unsafe { kill(pid, 9) }, 0, "kill the timed-out client");
            let status = status_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("the killed client was reaped")
                .expect("wait for the killed client");
            (status, true)
        }
    };
    waiter.join().expect("join the client waiter");
    outcome
}

#[test]
fn the_shipped_binary_relays_a_real_harness_help_screen_and_restores_the_terminal() {
    if !on_path(HARNESS) {
        eprintln!("SKIP: `{HARNESS}` is not on PATH, so the native facade smoke proof did not run");
        return;
    }
    let work = scratch("native-facade-smoke");
    let state = work.join("state");
    let project = work.join("project");
    std::fs::create_dir_all(&project).unwrap();

    // The supervisor, composed exactly as `detach::run_stage_three` composes it: production
    // descriptors, the harness registry's adapter table, the production factory.
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
        bridge: PathBuf::from(env!("CARGO_BIN_EXE_marion-supervisor")),
        base_url: Some("http://127.0.0.1:8099/v1".into()),
        auth: marion_harness::Auth::Canned,
    };
    let handle = RegistryHandle::owning(live, env.clone());
    let server = Server::start_with_native_launch(
        serving,
        Arc::clone(&handle),
        NativeLaunchConfig {
            descriptors: marion_core::PRODUCTION_NATIVE_FACADES,
            adapter_for: marion_harness::native_adapter,
            env,
        },
        Duration::from_secs(300),
    )
    .expect("the enabled native bootstrap service installs once");

    // The operator's terminal, and the shipped binary on it as a foreground session leader.
    let terminal = Arc::new(PtyMaster::open(WinSize::new(140, 45)).expect("operator PTY"));
    let baseline = termios_of(&terminal);
    let stdin: OwnedFd = terminal.open_slave().expect("stdin slave");
    let stdout: OwnedFd = terminal.open_slave().expect("stdout slave");
    let mut command = Command::new(env!("CARGO_BIN_EXE_marion"));
    command
        .args([HARNESS, "--help"])
        .current_dir(&project)
        .env("MARION_STATE_DIR", &state)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::piped());
    unsafe extern "C" {
        fn setsid() -> i32;
        fn ioctl(fd: i32, request: usize, ...) -> i32;
    }
    // SAFETY: runs after fork and before exec, performs only async-signal-safe session and ioctl
    // syscalls, and reports refusal as the OS error.
    unsafe {
        command.pre_exec(|| {
            if setsid() < 0 || ioctl(0, TIOCSCTTY, 0_i32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn the shipped marion binary");
    drop(command);
    let stderr = child.stderr.take().expect("client stderr pipe");
    let stderr_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(stderr), &mut output);
        output
    });
    let screen = {
        let terminal = Arc::clone(&terminal);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(65);
            let mut screen = Vec::new();
            let mut bytes = [0u8; 8192];
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
    let (status, timed_out) = reap_with_watchdog(child, Duration::from_secs(60));
    let stderr = String::from_utf8_lossy(&stderr_reader.join().expect("join stderr")).into_owned();
    let restored = termios_of(&terminal);
    let screen =
        String::from_utf8_lossy(&screen.join().expect("join the screen reader")).into_owned();
    // Evidence for the run log, whichever way the assertions below go.
    eprintln!(
        "SMOKE status={status} screen_bytes={} stderr={stderr:?} first_line={:?}",
        screen.len(),
        screen.lines().find(|line| !line.trim().is_empty())
    );

    assert!(
        !timed_out,
        "the shipped client exceeded its watchdog: {stderr}"
    );
    assert!(
        !stderr.contains("not enabled") && !stderr.contains("refused"),
        "the shipped client refused the facade: {stderr}"
    );
    assert!(
        status.success(),
        "the shipped client did not exit cleanly: {status}; stderr: {stderr}; screen: {screen:?}"
    );
    // The harness's own words, on the operator's terminal, through the relay. `codex --help`
    // prints a clap usage block; its first line names the program.
    assert!(
        screen.contains("Usage: codex") || screen.contains("Usage:") && screen.contains("codex"),
        "the harness's help text never reached the operator's terminal; screen: {screen:?}; \
         stderr: {stderr}"
    );
    assert_eq!(
        restored, baseline,
        "the operator's terminal was not restored after the relay"
    );

    // And the node is on the record: one native root, exited as the harness exited.
    let replayed = Registry::boot(&project_dir).expect("the journal replays");
    let nodes = replayed.tree().nodes().to_vec();
    assert_eq!(
        nodes.len(),
        1,
        "exactly one native node was journaled: {nodes:?}"
    );
    let node = &nodes[0];
    assert_eq!(
        node.intent.as_ref().map(|intent| intent.harness),
        Some(marion_core::harness::Harness::Codex)
    );
    assert!(node.spawn_confirmed && node.pid.is_some(), "{node:?}");
    assert_eq!(
        node.exit.as_ref().and_then(|exit| exit.code),
        Some(0),
        "{:?}",
        node.exit
    );

    server.stop();
    drop(terminal);
}
