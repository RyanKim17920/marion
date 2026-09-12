//! Tests for the native relay terminal: raw-mode entry, suspend/resume restoration, and the
//! retrying restore guard.

use std::fs::OpenOptions;

use super::*;
use crate::pty::{PtyMaster, WinSize};

#[test]
fn native_relay_terminal_restores_for_suspend_and_reenters_raw_mode() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("suspend/resume PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let baseline = tcgetattr(&stdin).expect("capture terminal baseline");
    let baseline_stdin_flags = fcntl_getfl(&stdin).expect("capture stdin flags");
    let baseline_stdout_flags = fcntl_getfl(&stdout).expect("capture stdout flags");
    let witness = ClientTtyWitness {
        stdin,
        stdout,
        fingerprint: TerminalFingerprint {
            st_dev: 0,
            st_ino: 0,
            st_rdev: 0,
        },
        baseline: TerminalBaseline {
            termios: baseline.clone(),
            stdin_flags: baseline_stdin_flags,
            stdout_flags: baseline_stdout_flags,
        },
        session_id: getpgrp(),
        foreground_pgid: getpgrp(),
        observed_geometry: TerminalGeometryV1 {
            cols: 91,
            rows: 29,
            xpixel: 0,
            ypixel: 0,
        },
    };
    let mut terminal = witness.enter_native_relay().expect("enter raw relay mode");
    assert_ne!(
        tcgetattr(terminal.stdin()).unwrap().local_modes,
        baseline.local_modes
    );

    terminal
        .restore_for_suspend()
        .expect("restore terminal before suspension");
    let restored = tcgetattr(terminal.stdin()).unwrap();
    assert_eq!(
        restored.local_modes & !rustix::termios::LocalModes::PENDIN,
        baseline.local_modes
    );
    assert_eq!(fcntl_getfl(terminal.stdin()).unwrap(), baseline_stdin_flags);

    terminal
        .reenter_after_continue()
        .expect("re-enter raw relay mode after continuation");
    assert_ne!(
        tcgetattr(terminal.stdin()).unwrap().local_modes,
        baseline.local_modes
    );
    assert!(
        fcntl_getfl(terminal.stdin())
            .unwrap()
            .contains(OFlags::NONBLOCK)
    );
}

#[test]
fn native_relay_terminal_rolls_back_a_failed_raw_reentry_after_continue() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("failed re-entry PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let baseline = tcgetattr(&stdin).expect("capture terminal baseline");
    let baseline_stdin_flags = fcntl_getfl(&stdin).expect("capture stdin flags");
    let baseline_stdout_flags = fcntl_getfl(&stdout).expect("capture stdout flags");
    let witness = ClientTtyWitness {
        stdin,
        stdout,
        fingerprint: TerminalFingerprint {
            st_dev: 0,
            st_ino: 0,
            st_rdev: 0,
        },
        baseline: TerminalBaseline {
            termios: baseline.clone(),
            stdin_flags: baseline_stdin_flags,
            stdout_flags: baseline_stdout_flags,
        },
        session_id: getpgrp(),
        foreground_pgid: getpgrp(),
        observed_geometry: TerminalGeometryV1 {
            cols: 91,
            rows: 29,
            xpixel: 0,
            ypixel: 0,
        },
    };
    let mut terminal = witness.enter_native_relay().expect("enter raw relay mode");
    terminal
        .restore_for_suspend()
        .expect("restore terminal before suspension");
    F_SETFL_RESULTS.with(|results| {
        results
            .borrow_mut()
            .push_back(Some(rustix::io::Errno::BADF));
    });

    let error = terminal
        .reenter_after_continue()
        .expect_err("post-termios flag failure must refuse raw re-entry");
    assert!(error.to_string().contains("Bad file descriptor"), "{error}");
    let restored = tcgetattr(terminal.stdin()).unwrap();
    assert_eq!(restored.input_modes, baseline.input_modes);
    assert_eq!(restored.output_modes, baseline.output_modes);
    assert_eq!(restored.control_modes, baseline.control_modes);
    assert_eq!(
        restored.local_modes & !rustix::termios::LocalModes::PENDIN,
        baseline.local_modes
    );
    assert_eq!(fcntl_getfl(terminal.stdin()).unwrap(), baseline_stdin_flags);
    assert_eq!(
        fcntl_getfl(terminal.stdout()).unwrap(),
        baseline_stdout_flags
    );

    terminal
        .reenter_after_continue()
        .expect("a rolled-back re-entry remains retryable");
    assert_ne!(
        tcgetattr(terminal.stdin()).unwrap().local_modes,
        baseline.local_modes
    );
}

#[test]
fn continue_refuses_while_suspend_restoration_is_pending_then_retries_cleanly() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("partial suspend PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let baseline = tcgetattr(&stdin).expect("capture terminal baseline");
    let baseline_stdin_flags = fcntl_getfl(&stdin).expect("capture stdin flags");
    let baseline_stdout_flags = fcntl_getfl(&stdout).expect("capture stdout flags");
    let witness = ClientTtyWitness {
        stdin,
        stdout,
        fingerprint: TerminalFingerprint {
            st_dev: 0,
            st_ino: 0,
            st_rdev: 0,
        },
        baseline: TerminalBaseline {
            termios: baseline.clone(),
            stdin_flags: baseline_stdin_flags,
            stdout_flags: baseline_stdout_flags,
        },
        session_id: getpgrp(),
        foreground_pgid: getpgrp(),
        observed_geometry: TerminalGeometryV1 {
            cols: 91,
            rows: 29,
            xpixel: 0,
            ypixel: 0,
        },
    };
    let mut terminal = witness.enter_native_relay().expect("enter raw relay mode");
    F_SETFL_RESULTS.with(|results| {
        results
            .borrow_mut()
            .extend([Some(rustix::io::Errno::BADF), None]);
    });

    terminal
        .restore_for_suspend()
        .expect_err("partial suspend restoration must remain pending");
    let error = terminal
        .reenter_after_continue()
        .expect_err("continue cannot treat pending restoration as already raw");
    assert!(
        error.to_string().contains("restoration is pending"),
        "{error}"
    );

    terminal
        .restore_for_suspend()
        .expect("retry completes pending suspend restoration");
    terminal
        .restore_for_suspend()
        .expect("duplicate suspend restoration is idempotent");
    terminal
        .reenter_after_continue()
        .expect("continue re-enters raw mode from fully restored state");
    terminal
        .reenter_after_continue()
        .expect("duplicate continue is idempotent while already raw");
    assert_ne!(
        tcgetattr(terminal.stdin()).unwrap().local_modes,
        baseline.local_modes
    );
    assert!(
        fcntl_getfl(terminal.stdin())
            .unwrap()
            .contains(OFlags::NONBLOCK)
    );
}

#[test]
fn production_verifier_refuses_swapped_or_insufficient_terminal_roles_before_session_detail() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("test PTY");

    let invalid_pairs = [
        (
            OpenOptions::new()
                .write(true)
                .open(master.slave_path())
                .expect("write-only stdin"),
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(master.slave_path())
                .expect("read-write stdout"),
        ),
        (
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(master.slave_path())
                .expect("read-write stdin"),
            OpenOptions::new()
                .read(true)
                .open(master.slave_path())
                .expect("read-only stdout"),
        ),
    ];

    for (stdin, stdout) in invalid_pairs {
        let error = verify_bootstrap_tty(
            PeerIdentity::current_for_tty_test(),
            [OwnedFd::from(stdin), OwnedFd::from(stdout)],
        )
        .expect_err("invalid descriptor roles must fail before terminal-session disclosure");
        assert_eq!(error.to_string(), "terminal descriptor roles are invalid");
    }
}

#[cfg(target_os = "linux")]
const TIOCSCTTY: std::ffi::c_ulong = 0x540e;
#[cfg(target_os = "macos")]
const TIOCSCTTY: std::ffi::c_ulong = 0x2000_7461;

/// A `sleep` in its own session, optionally holding the PTY as its controlling terminal.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn session_child(master: &PtyMaster, claim_terminal: bool) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    unsafe extern "C" {
        fn setsid() -> i32;
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }
    let stdin = master.open_slave().expect("child stdin");
    let stdout = master.open_slave().expect("child stdout");
    let mut command = std::process::Command::new("/bin/sleep");
    command
        .arg("30")
        .stdin(std::process::Stdio::from(stdin))
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::null());
    // SAFETY: only async-signal-safe session/ioctl syscalls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if claim_terminal && ioctl(0, TIOCSCTTY, 0_i32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("spawn the session child")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_session(child: &std::process::Child) {
    let pid = Pid::from_raw(child.id() as i32).expect("live child pid");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while getsid(Some(pid)).expect("child session") != pid {
        assert!(
            std::time::Instant::now() < deadline,
            "the child never became a session leader"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Mutation: verify the peer with `tcgetsid`/`tcgetpgrp`. Both answer `ENOTTY` from a process
/// outside the terminal's session, which a detached supervisor always is; the process table
/// answers for any same-uid peer and still refuses a peer that merely holds the descriptors.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn production_verifier_proves_the_peer_terminal_from_another_session() {
    let master = PtyMaster::open(WinSize::new(101, 37)).expect("verifier PTY");

    let mut foreground = session_child(&master, true);
    wait_for_session(&foreground);
    let peer = PeerIdentity::child_for_tty_test(foreground.id());
    let child_pid = Pid::from_raw(foreground.id() as i32).unwrap();
    assert_ne!(
        getsid(Some(child_pid)).unwrap(),
        getsid(None).unwrap(),
        "the fixture must verify across sessions"
    );
    let witness = verify_bootstrap_tty(
        peer,
        [
            master.open_slave().expect("verifier stdin"),
            master.open_slave().expect("verifier stdout"),
        ],
    )
    .expect("a foreground peer on its controlling terminal verifies from another session");
    assert_eq!(witness.foreground_pgid, child_pid);
    assert_eq!(witness.peer_session_id, child_pid);
    assert_eq!(witness.initial_geometry.cols, 101);
    witness
        .revalidate_peer(peer)
        .expect("the same peer revalidates");
    foreground.kill().unwrap();
    foreground.wait().unwrap();

    let mut detached = session_child(&master, false);
    wait_for_session(&detached);
    let error = verify_bootstrap_tty(
        PeerIdentity::child_for_tty_test(detached.id()),
        [
            master.open_slave().expect("verifier stdin"),
            master.open_slave().expect("verifier stdout"),
        ],
    )
    .expect_err("a peer that only holds the descriptors is not on its controlling terminal");
    assert!(
        matches!(error, NativeTtyError::NotControllingTerminal),
        "{error}"
    );
    detached.kill().unwrap();
    detached.wait().unwrap();
}

/// A shell that is the session leader on `master`, with job control on, holding one background
/// job whose pid it prints on its terminal. Returns the shell and the job's pid.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn leader_with_background_job(master: &PtyMaster) -> (std::process::Child, i32) {
    use std::os::unix::process::CommandExt;
    unsafe extern "C" {
        fn setsid() -> i32;
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }
    let stdin = master.open_slave().expect("leader stdin");
    let stdout = master.open_slave().expect("leader stdout");
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", "set -m; sleep 30 & echo $!; wait"])
        .stdin(std::process::Stdio::from(stdin))
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::null());
    // SAFETY: only async-signal-safe session/ioctl syscalls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if setsid() < 0 || ioctl(0, TIOCSCTTY, 0_i32) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let leader = command.spawn().expect("spawn the job-control leader");
    // The job's pid arrives on the terminal as `<digits>\r\n`; read until the line completes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut line = Vec::new();
    let mut byte = [0u8; 64];
    let job = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the leader never printed its job's pid: {:?}",
            String::from_utf8_lossy(&line)
        );
        match master.read(&mut byte) {
            Ok(0) => panic!("the leader's terminal closed before it printed a pid"),
            Ok(n) => line.extend_from_slice(&byte[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("reading the leader's terminal: {error}"),
        }
        let text = String::from_utf8_lossy(&line);
        if let Some((digits, _)) = text.split_once('\n') {
            break digits.trim().parse::<i32>().unwrap_or_else(|_| {
                panic!("the leader printed something other than a pid: {text:?}")
            });
        }
    };
    (leader, job)
}

/// Two causal negatives for the production verifier, on real processes: descriptors from a
/// **different** terminal than the peer's controlling one are `NotControllingTerminal` even
/// though the peer is a foreground session leader on its own; and a process that shares the
/// peer's controlling terminal but sits in a **background** job's process group is
/// `BackgroundProcessGroup`.
///
/// Mutation: compare the terminal by `fstat` of the descriptors alone (the first case passes),
/// or read `foreground_pgid` off the caller's own terminal (the second case passes).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn production_verifier_refuses_a_different_terminal_and_a_background_peer() {
    let terminal_a = PtyMaster::open(WinSize::new(101, 37)).expect("terminal A");
    let terminal_b = PtyMaster::open(WinSize::new(91, 29)).expect("terminal B");
    let (mut leader, job) = leader_with_background_job(&terminal_a);
    wait_for_session(&leader);
    let leader_pid = Pid::from_raw(leader.id() as i32).unwrap();
    let job_pid = Pid::from_raw(job).expect("a background job pid");
    assert_ne!(
        getpgid(Some(job_pid)).expect("the job's process group"),
        leader_pid,
        "`set -m` must give the background job its own process group"
    );

    let different = verify_bootstrap_tty(
        PeerIdentity::child_for_tty_test(leader.id()),
        [
            terminal_b.open_slave().expect("B stdin"),
            terminal_b.open_slave().expect("B stdout"),
        ],
    )
    .expect_err("descriptors on another terminal are not the peer's controlling terminal");
    assert!(
        matches!(different, NativeTtyError::NotControllingTerminal),
        "{different}"
    );

    let background = verify_bootstrap_tty(
        PeerIdentity::child_for_tty_test(job as u32),
        [
            terminal_a.open_slave().expect("A stdin"),
            terminal_a.open_slave().expect("A stdout"),
        ],
    )
    .expect_err("a background job on the peer's terminal is not in its foreground group");
    assert!(
        matches!(background, NativeTtyError::BackgroundProcessGroup),
        "{background}"
    );

    // And the positive control on the same fixture: the leader itself, on A, verifies.
    let witness = verify_bootstrap_tty(
        PeerIdentity::child_for_tty_test(leader.id()),
        [
            terminal_a.open_slave().expect("A stdin"),
            terminal_a.open_slave().expect("A stdout"),
        ],
    )
    .expect("the foreground leader on its own terminal verifies");
    assert_eq!(witness.foreground_pgid, leader_pid);

    leader.kill().unwrap();
    leader.wait().unwrap();
    // The job outlives its shell only until this; it is in its own group, so kill it directly.
    let _ = rustix::process::kill_process(job_pid, rustix::process::Signal::KILL);
}

#[test]
fn native_relay_terminal_restores_the_retained_termios_and_flags_after_failure() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("test PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let baseline_termios = tcgetattr(&stdin).unwrap();
    let baseline_stdin_flags = fcntl_getfl(&stdin).unwrap();
    let baseline_stdout_flags = fcntl_getfl(&stdout).unwrap();
    let witness = ClientTtyWitness {
        stdin,
        stdout,
        fingerprint: TerminalFingerprint {
            st_dev: 0,
            st_ino: 0,
            st_rdev: 0,
        },
        baseline: TerminalBaseline {
            termios: baseline_termios.clone(),
            stdin_flags: baseline_stdin_flags,
            stdout_flags: baseline_stdout_flags,
        },
        session_id: getpgrp(),
        foreground_pgid: getpgrp(),
        observed_geometry: TerminalGeometryV1 {
            cols: 91,
            rows: 29,
            xpixel: 0,
            ypixel: 0,
        },
    };
    let observed_stdin = fcntl_dupfd_cloexec(witness.stdin(), 3).unwrap();
    let observed_stdout = fcntl_dupfd_cloexec(witness.stdout(), 3).unwrap();

    let terminal = witness.enter_native_relay().expect("enter raw mode");
    assert_ne!(
        tcgetattr(&observed_stdin).unwrap().local_modes,
        baseline_termios.local_modes
    );
    assert!(
        fcntl_getfl(&observed_stdin)
            .unwrap()
            .contains(OFlags::NONBLOCK)
    );
    drop(terminal); // models any protocol, socket, or rendering failure

    let restored = tcgetattr(&observed_stdin).unwrap();
    assert_eq!(restored.input_modes, baseline_termios.input_modes);
    assert_eq!(restored.output_modes, baseline_termios.output_modes);
    assert_eq!(restored.control_modes, baseline_termios.control_modes);
    // macOS may expose PENDIN as transient kernel state when canonical mode is restored over
    // unread bytes. It is not part of the configured baseline and must not justify flushing
    // the operator's input merely to make a byte-for-byte observation match.
    assert_eq!(
        restored.local_modes & !rustix::termios::LocalModes::PENDIN,
        baseline_termios.local_modes
    );
    assert_eq!(fcntl_getfl(&observed_stdin).unwrap(), baseline_stdin_flags);
    assert_eq!(
        fcntl_getfl(&observed_stdout).unwrap(),
        baseline_stdout_flags
    );
}

#[test]
fn entering_native_relay_preserves_input_already_queued_on_the_terminal() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("test PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let baseline_termios = tcgetattr(&stdin).unwrap();
    let baseline_stdin_flags = fcntl_getfl(&stdin).unwrap();
    let baseline_stdout_flags = fcntl_getfl(&stdout).unwrap();
    let witness = ClientTtyWitness {
        stdin,
        stdout,
        fingerprint: TerminalFingerprint {
            st_dev: 0,
            st_ino: 0,
            st_rdev: 0,
        },
        baseline: TerminalBaseline {
            termios: baseline_termios,
            stdin_flags: baseline_stdin_flags,
            stdout_flags: baseline_stdout_flags,
        },
        session_id: getpgrp(),
        foreground_pgid: getpgrp(),
        observed_geometry: TerminalGeometryV1 {
            cols: 91,
            rows: 29,
            xpixel: 0,
            ypixel: 0,
        },
    };

    let queued = b"typed-before-relay\n";
    master
        .write_all(queued)
        .expect("queue terminal input before raw mode");
    let terminal = witness.enter_native_relay().expect("enter raw mode");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut received = Vec::new();
    while received.len() < queued.len() {
        let mut bytes = [0; 64];
        match rustix::io::read(terminal.stdin(), &mut bytes) {
            Ok(0) => panic!("the terminal ended before yielding its queued input"),
            Ok(count) => received.extend_from_slice(&bytes[..count]),
            Err(error)
                if error == rustix::io::Errno::AGAIN && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("reading queued terminal input: {error}"),
        }
    }

    assert_eq!(received, queued);

    let queued_during_raw = b"typed-during-relay\n";
    master
        .write_all(queued_during_raw)
        .expect("queue unread terminal input during raw mode");
    let restored_input = fcntl_dupfd_cloexec(terminal.stdin(), 3).unwrap();
    drop(terminal);
    fcntl_setfl(
        &restored_input,
        fcntl_getfl(&restored_input).unwrap() | OFlags::NONBLOCK,
    )
    .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut restored = Vec::new();
    while restored.len() < queued_during_raw.len() {
        let mut bytes = [0; 64];
        match rustix::io::read(&restored_input, &mut bytes) {
            Ok(0) => panic!("the restored terminal ended before yielding its queued input"),
            Ok(count) => restored.extend_from_slice(&bytes[..count]),
            Err(error)
                if error == rustix::io::Errno::AGAIN && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("reading input queued during raw mode: {error}"),
        }
    }
    assert_eq!(restored, queued_during_raw);
}

#[test]
fn native_relay_terminal_restores_after_every_terminal_relay_exit_class() {
    #[derive(Clone, Copy)]
    enum ExitClass {
        End,
        Detach,
        ReadFailure,
        WriteFailure,
        ResizeFailure,
    }

    for exit in [
        ExitClass::End,
        ExitClass::Detach,
        ExitClass::ReadFailure,
        ExitClass::WriteFailure,
        ExitClass::ResizeFailure,
    ] {
        let master = PtyMaster::open(WinSize::new(91, 29)).expect("test PTY");
        let stdin = master.open_slave().expect("relay stdin");
        let stdout = master.open_slave().expect("relay stdout");
        let baseline_termios = tcgetattr(&stdin).unwrap();
        let mut baseline_stdin_flags = fcntl_getfl(&stdin).unwrap();
        baseline_stdin_flags.remove(OFlags::NONBLOCK);
        fcntl_setfl(&stdin, baseline_stdin_flags).unwrap();
        let mut baseline_stdout_flags = fcntl_getfl(&stdout).unwrap();
        // Make the two descriptor baselines deliberately differ: restoring stdin flags onto
        // both descriptors would otherwise pass when a PTY slave begins with matching flags.
        baseline_stdout_flags.insert(OFlags::NONBLOCK);
        fcntl_setfl(&stdout, baseline_stdout_flags).unwrap();
        let witness = ClientTtyWitness {
            stdin,
            stdout,
            fingerprint: TerminalFingerprint {
                st_dev: 0,
                st_ino: 0,
                st_rdev: 0,
            },
            baseline: TerminalBaseline {
                termios: baseline_termios.clone(),
                stdin_flags: baseline_stdin_flags,
                stdout_flags: baseline_stdout_flags,
            },
            session_id: getpgrp(),
            foreground_pgid: getpgrp(),
            observed_geometry: TerminalGeometryV1 {
                cols: 91,
                rows: 29,
                xpixel: 0,
                ypixel: 0,
            },
        };
        let observed_stdin = fcntl_dupfd_cloexec(witness.stdin(), 3).unwrap();
        let observed_stdout = fcntl_dupfd_cloexec(witness.stdout(), 3).unwrap();

        let terminal = witness.enter_native_relay().expect("enter raw mode");
        TCSETATTR_RESULTS.with(|results| {
            results
                .borrow_mut()
                .extend([Some(rustix::io::Errno::INTR), None])
        });
        // Each is a distinct return edge in the relay; the guard must not rely on which error
        // value caused the stack to unwind normally.
        let _: Result<(), &'static str> = match exit {
            ExitClass::End | ExitClass::Detach => Ok(()),
            ExitClass::ReadFailure => Err("read"),
            ExitClass::WriteFailure => Err("write"),
            ExitClass::ResizeFailure => Err("resize"),
        };
        drop(terminal);

        let restored = tcgetattr(&observed_stdin).unwrap();
        assert_eq!(restored.input_modes, baseline_termios.input_modes);
        assert_eq!(restored.output_modes, baseline_termios.output_modes);
        assert_eq!(restored.control_modes, baseline_termios.control_modes);
        assert_eq!(
            restored.local_modes & !rustix::termios::LocalModes::PENDIN,
            baseline_termios.local_modes
        );
        assert_eq!(fcntl_getfl(&observed_stdin).unwrap(), baseline_stdin_flags);
        assert_eq!(
            fcntl_getfl(&observed_stdout).unwrap(),
            baseline_stdout_flags
        );
    }
}

#[test]
fn entry_failure_retries_interrupted_termios_cleanup_before_returning() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("test PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let baseline_termios = tcgetattr(&stdin).unwrap();
    let baseline_stdin_flags = fcntl_getfl(&stdin).unwrap();
    let baseline_stdout_flags = fcntl_getfl(&stdout).unwrap();
    let witness = ClientTtyWitness {
        stdin,
        stdout,
        fingerprint: TerminalFingerprint {
            st_dev: 0,
            st_ino: 0,
            st_rdev: 0,
        },
        baseline: TerminalBaseline {
            termios: baseline_termios.clone(),
            stdin_flags: baseline_stdin_flags,
            stdout_flags: baseline_stdout_flags,
        },
        session_id: getpgrp(),
        foreground_pgid: getpgrp(),
        observed_geometry: TerminalGeometryV1 {
            cols: 91,
            rows: 29,
            xpixel: 0,
            ypixel: 0,
        },
    };
    let observed_stdin = fcntl_dupfd_cloexec(witness.stdin(), 3).unwrap();

    TCSETATTR_RESULTS.with(|results| {
        results
            .borrow_mut()
            .extend([None, Some(rustix::io::Errno::INTR)])
    });
    F_SETFL_RESULTS.with(|results| {
        results
            .borrow_mut()
            .push_back(Some(rustix::io::Errno::BADF))
    });

    let error = match witness.enter_native_relay() {
        Ok(_) => panic!("injected F_SETFL failure unexpectedly entered raw mode"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("Bad file descriptor"), "{error}");
    let observed = tcgetattr(&observed_stdin).unwrap();
    assert_eq!(observed.input_modes, baseline_termios.input_modes);
    assert_eq!(observed.output_modes, baseline_termios.output_modes);
    assert_eq!(observed.control_modes, baseline_termios.control_modes);
    assert_eq!(
        observed.local_modes & !rustix::termios::LocalModes::PENDIN,
        baseline_termios.local_modes,
        "entry returned while the retained terminal was still raw"
    );
}

#[test]
fn entry_reports_both_the_primary_and_terminal_cleanup_failures() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("test PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let baseline_termios = tcgetattr(&stdin).unwrap();
    let baseline_stdin_flags = fcntl_getfl(&stdin).unwrap();
    let baseline_stdout_flags = fcntl_getfl(&stdout).unwrap();
    let witness = ClientTtyWitness {
        stdin,
        stdout,
        fingerprint: TerminalFingerprint {
            st_dev: 0,
            st_ino: 0,
            st_rdev: 0,
        },
        baseline: TerminalBaseline {
            termios: baseline_termios,
            stdin_flags: baseline_stdin_flags,
            stdout_flags: baseline_stdout_flags,
        },
        session_id: getpgrp(),
        foreground_pgid: getpgrp(),
        observed_geometry: TerminalGeometryV1 {
            cols: 91,
            rows: 29,
            xpixel: 0,
            ypixel: 0,
        },
    };

    TCSETATTR_RESULTS.with(|results| results.borrow_mut().push_back(None));
    F_SETFL_RESULTS.with(|results| {
        results.borrow_mut().extend([
            Some(rustix::io::Errno::BADF),
            Some(rustix::io::Errno::IO),
            None,
        ])
    });
    let error = match witness.enter_native_relay() {
        Ok(_) => panic!("injected entry and cleanup failures unexpectedly succeeded"),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("Bad file descriptor"), "{error}");
    assert!(error.contains("terminal restoration"), "{error}");
    assert!(error.contains("Input/output error"), "{error}");
}

#[test]
fn restoration_attempts_every_resource_and_remains_armed_after_partial_failure() {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("test PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let baseline_termios = tcgetattr(&stdin).unwrap();
    let baseline_stdin_flags = fcntl_getfl(&stdin).unwrap();
    let mut baseline_stdout_flags = fcntl_getfl(&stdout).unwrap();
    baseline_stdout_flags.insert(OFlags::NONBLOCK);
    fcntl_setfl(&stdout, baseline_stdout_flags).unwrap();
    let witness = ClientTtyWitness {
        stdin,
        stdout,
        fingerprint: TerminalFingerprint {
            st_dev: 0,
            st_ino: 0,
            st_rdev: 0,
        },
        baseline: TerminalBaseline {
            termios: baseline_termios.clone(),
            stdin_flags: baseline_stdin_flags,
            stdout_flags: baseline_stdout_flags,
        },
        session_id: getpgrp(),
        foreground_pgid: getpgrp(),
        observed_geometry: TerminalGeometryV1 {
            cols: 91,
            rows: 29,
            xpixel: 0,
            ypixel: 0,
        },
    };
    let observed_stdin = fcntl_dupfd_cloexec(witness.stdin(), 3).unwrap();
    let observed_stdout = fcntl_dupfd_cloexec(witness.stdout(), 3).unwrap();
    let mut terminal = witness.enter_native_relay().expect("enter raw mode");

    // Fail only stdin restoration. The implementation must still attempt stdout and termios,
    // report the first error, and retain the armed state for a later retry.
    F_SETFL_RESULTS.with(|results| {
        results
            .borrow_mut()
            .extend([Some(rustix::io::Errno::BADF), None])
    });
    let error = terminal.restore_result().unwrap_err();
    assert!(error.to_string().contains("Bad file descriptor"), "{error}");
    assert!(
        fcntl_getfl(&observed_stdin)
            .unwrap()
            .contains(OFlags::NONBLOCK)
    );
    assert_eq!(
        fcntl_getfl(&observed_stdout).unwrap(),
        baseline_stdout_flags
    );
    let restored = tcgetattr(&observed_stdin).unwrap();
    assert_eq!(restored.input_modes, baseline_termios.input_modes);
    assert_eq!(restored.output_modes, baseline_termios.output_modes);
    assert_eq!(restored.control_modes, baseline_termios.control_modes);
    assert_eq!(
        restored.local_modes & !rustix::termios::LocalModes::PENDIN,
        baseline_termios.local_modes
    );

    terminal
        .restore_result()
        .expect("the armed guard retries the failed resource");
    assert_eq!(fcntl_getfl(&observed_stdin).unwrap(), baseline_stdin_flags);
}
