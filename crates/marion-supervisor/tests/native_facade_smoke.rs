//! The shipped `marion <harness> …` binary in front of a real harness, on a real terminal.
//!
//! Not the M3 E2E matrix (a TUI cell on screen, keystrokes, resize, detach — `plan-native-
//! activation.md` step 9). One smoke proof for the composition, **from a cold project**: nothing
//! serves the project when the operator types `marion codex --help`, so the shipped binary must
//! start this project's supervisor the way `marion run` does (`detach::ensure_supervisor`, whose
//! stage 3 composes the production registry, the row-derived adapter table and the production
//! factory), then bootstrap through it on its own controlling PTY; and the harness's own help text
//! lands on the operator's terminal with termios back where it started. The project is a
//! repository, so §2 keys it on `<project>/.git`, and the harness must nonetheless run where the
//! operator stood: a `codex` shim first on the client's `PATH` records `pwd -P` and hands over to
//! the real binary.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use marion_core::paths::ProjectDir;
use marion_supervisor::pty::{PtyMaster, WinSize};
use marion_supervisor::registry::Registry;
use marion_supervisor::socket::own_uid;
use marion_supervisor::socket::{
    SocketPaths, nobody_is_serving, project_root, read_identity, socket_paths,
};
use marion_testsupport::{on_path, scratch};

#[cfg(target_os = "linux")]
const TIOCSCTTY: usize = 0x540e;
#[cfg(target_os = "macos")]
const TIOCSCTTY: usize = 0x2000_7461;

/// The facade under test: the one enabled production lane whose `--help` needs no login.
const HARNESS: &str = "codex";

/// The real `codex` on this process's `PATH`, so the shim can hand over to it by absolute path.
fn real_harness() -> PathBuf {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(HARNESS))
        .find(|candidate| candidate.is_file())
        .expect("on_path already found the harness")
}

/// A `codex` that records its working directory and becomes the real one. `exec`, so the vendor
/// the supervisor reaps is the harness itself and its exit status is the harness's own.
fn write_recording_shim(bin: &Path, cwd_record: &Path, real: &Path) {
    std::fs::create_dir_all(bin).unwrap();
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let shim = bin.join(HARNESS);
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\npwd -P > {record}\nexec {real} \"$@\"\n",
            record = quote(cwd_record),
            real = quote(real),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Leave no supervisor behind, whichever way the assertions go: the one the client starts would
/// otherwise outlive a failed run by five minutes of idle grace.
struct StartedSupervisor(SocketPaths);

impl Drop for StartedSupervisor {
    fn drop(&mut self) {
        if let Some(identity) = read_identity(&self.0) {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // SAFETY: `pid` names the supervisor whose identity file was just read under its lock.
            unsafe { kill(identity.pid, 9) };
        }
    }
}

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
    let bin = work.join("bin");
    let cwd_record = work.join("vendor-cwd");
    std::fs::create_dir_all(&project).unwrap();
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .status()
        .expect("git runs");
    assert!(status.success(), "git init: {status}");
    write_recording_shim(&bin, &cwd_record, &real_harness());
    let client_path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>()),
        ),
    )
    .expect("the client's PATH joins");

    // A cold project: the socket the client will resolve for `project` under `state`, with nothing
    // serving it. The supervisor the client starts is stage 3's composition — production
    // descriptors, the harness registry's adapter table, the production factory — and its
    // stderr lands in `paths.log()`, which is read back as evidence if anything below fails.
    let key = project_root(&project);
    assert_eq!(
        key,
        project.join(".git"),
        "the project is keyed on its common dir"
    );
    let paths = socket_paths(&state, &key, own_uid());
    assert!(
        paths.overflow().is_none(),
        "this bed's socket must live under <state>; {:?} overflowed to /tmp",
        paths.socket()
    );
    assert!(
        nobody_is_serving(&paths),
        "a fresh scratch project already had a supervisor: {:?}",
        paths.socket()
    );
    let project_dir = ProjectDir::new(&state, paths.canonical_project());
    let started = StartedSupervisor(paths.clone());

    // The operator's terminal, and the shipped binary on it as a foreground session leader.
    let terminal = Arc::new(PtyMaster::open(WinSize::new(140, 45)).expect("operator PTY"));
    let baseline = termios_of(&terminal);
    let stdin: OwnedFd = terminal.open_slave().expect("stdin slave");
    let stdout: OwnedFd = terminal.open_slave().expect("stdout slave");
    let mut command = Command::new(env!("CARGO_BIN_EXE_marion"));
    command
        .args([HARNESS, "--help"])
        .current_dir(&project)
        .env("PATH", &client_path)
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
    let supervisor_log = std::fs::read_to_string(paths.log()).unwrap_or_default();
    eprintln!(
        "SMOKE status={status} screen_bytes={} stderr={stderr:?} first_line={:?} \
         supervisor_log={supervisor_log:?}",
        screen.len(),
        screen.lines().find(|line| !line.trim().is_empty())
    );

    assert!(
        !timed_out,
        "the shipped client exceeded its watchdog: {stderr}"
    );
    assert!(
        !stderr.contains("not enabled") && !stderr.contains("refused"),
        "the shipped client refused the facade instead of starting this project's supervisor: \
         {stderr}; supervisor log: {supervisor_log}"
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
    // The harness ran where the operator stood — the working tree, not `<project>/.git`, which
    // is what §2 keys this project on and what the vendor used to print as its workspace.
    let vendor_cwd =
        std::fs::read_to_string(&cwd_record).expect("the shim recorded the harness's cwd");
    assert_eq!(
        PathBuf::from(vendor_cwd.trim_end_matches('\n')),
        project,
        "the harness did not run in the operator's working directory"
    );
    assert_eq!(
        restored, baseline,
        "the operator's terminal was not restored after the relay"
    );

    // And the node is on the record: one native root, exited as the harness exited. The client's
    // exit and the lifecycle worker's `Exited` append are unordered observers of the vendor's
    // death (see `native_bootstrap.rs::replay_until_a_node_is_terminal`), so wait on the journal,
    // which is the barrier the supervisor itself exits on.
    let deadline = Instant::now() + Duration::from_secs(10);
    let replayed = loop {
        let replayed = Registry::boot(&project_dir).expect("the journal replays");
        let terminal = replayed
            .tree()
            .nodes()
            .iter()
            .any(|node| node.exit.is_some());
        if terminal || Instant::now() >= deadline {
            break replayed;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
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

    // The supervisor the client started is still serving — proof that the shipped binary, not
    // this test, brought it up. `started` stops it on the way out.
    assert!(
        read_identity(&paths).is_some(),
        "the client's supervisor does not serve this project"
    );
    drop(started);
    drop(terminal);
}
