//! The native facade seam is wired into the user-facing binary, but this slice advertises none.
//! These selectors therefore retain the legacy usage refusal until a later transport slice makes
//! one production descriptor ready.

use std::cell::Cell;
use std::ffi::OsString;
use std::process::{Command, ExitCode};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::OwnedFd;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::process::CommandExt;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Stdio;

use marion_core::{
    Lane, NativeAdapterId, NativeFacadeDescriptor, NativeFacadeRegistry, NativeLane,
    VendorIdentity, production_native_facades,
};
use marion_supervisor::facade_cli::dispatch_native_facade_or_legacy;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use marion_supervisor::pty::{PtyMaster, WinSize};

#[test]
fn a_synthetic_ready_facade_requires_foreground_stdio_before_the_legacy_cli_runs() {
    const DESCRIPTORS: &[NativeFacadeDescriptor] = &[NativeFacadeDescriptor {
        identity: VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &[],
        native: Some(Lane::new(
            true,
            NativeLane::new("atlas-cli", "codex", NativeAdapterId::new("atlas-native")),
        )),
        structured: None,
    }];
    let registry = NativeFacadeRegistry::new(DESCRIPTORS).expect("synthetic registry is valid");
    let legacy_called = Cell::new(false);
    let mut stderr = Vec::new();

    let status = dispatch_native_facade_or_legacy(
        [OsString::from("atlas"), OsString::from("--help")],
        &registry,
        || unreachable!("non-TTY test process must refuse before bootstrap"),
        &mut stderr,
        || {
            legacy_called.set(true);
            ExitCode::SUCCESS
        },
    );

    assert_eq!(status, ExitCode::FAILURE);
    assert_eq!(
        stderr,
        b"marion: native facade \"atlas\" requires stdin and stdout on the same foreground controlling terminal; use 'marion run <agent-type> --prompt <text>' for structured execution\n"
    );
    assert!(
        !legacy_called.get(),
        "matched facade reached the legacy CLI"
    );
}

#[test]
fn registered_selector_without_foreground_stdio_refuses_before_bootstrap_connection() {
    const DESCRIPTORS: &[NativeFacadeDescriptor] = &[NativeFacadeDescriptor {
        identity: VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &["at"],
        native: Some(Lane::new(
            true,
            NativeLane::new("atlas-cli", "codex", NativeAdapterId::new("atlas-native")),
        )),
        structured: None,
    }];
    let registry = NativeFacadeRegistry::new(DESCRIPTORS).expect("synthetic registry is valid");
    let bootstrap_connections = Cell::new(0_u64);
    let legacy_called = Cell::new(false);
    let mut stderr = Vec::new();

    let status = dispatch_native_facade_or_legacy(
        [OsString::from("at"), OsString::from("--opaque")],
        &registry,
        || {
            bootstrap_connections.set(bootstrap_connections.get() + 1);
            unreachable!("client TTY refusal must precede bootstrap connection")
        },
        &mut stderr,
        || {
            legacy_called.set(true);
            ExitCode::SUCCESS
        },
    );

    assert_eq!(status, ExitCode::FAILURE);
    assert_eq!(bootstrap_connections.get(), 0);
    assert!(
        !legacy_called.get(),
        "matched facade reached the legacy CLI"
    );
    assert_eq!(
        stderr,
        b"marion: native facade \"at\" requires stdin and stdout on the same foreground controlling terminal; use 'marion run <agent-type> --prompt <text>' for structured execution\n"
    );
}

#[test]
fn registered_disabled_or_native_absent_facades_never_fall_through_to_legacy() {
    const DISABLED: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("disabled"),
        command: "disabled",
        aliases: &[],
        native: Some(Lane::new(
            false,
            NativeLane::new(
                "disabled-cli",
                "codex",
                NativeAdapterId::new("disabled-native"),
            ),
        )),
        structured: None,
    };
    const ABSENT: NativeFacadeDescriptor = NativeFacadeDescriptor {
        identity: VendorIdentity::new("absent"),
        command: "absent",
        aliases: &[],
        native: None,
        structured: None,
    };
    let descriptors = [DISABLED, ABSENT];
    let registry = NativeFacadeRegistry::new(&descriptors).unwrap();

    for selector in ["disabled", "absent"] {
        let legacy_called = Cell::new(false);
        let mut stderr = Vec::new();

        let status = dispatch_native_facade_or_legacy(
            [OsString::from(selector)],
            &registry,
            || unreachable!("non-TTY test process must refuse before bootstrap"),
            &mut stderr,
            || {
                legacy_called.set(true);
                ExitCode::SUCCESS
            },
        );

        assert_eq!(status, ExitCode::FAILURE);
        assert_eq!(
            stderr,
            format!(
                "marion: native facade \"{selector}\" requires stdin and stdout on the same foreground controlling terminal; use 'marion run <agent-type> --prompt <text>' for structured execution\n"
            )
            .as_bytes()
        );
        assert!(!legacy_called.get(), "{selector:?} fell through to legacy");
    }
}

#[test]
fn production_exposes_no_facade_and_known_or_arbitrary_names_keep_legacy_usage() {
    assert!(
        production_native_facades()
            .enabled_native_commands()
            .is_empty(),
        "a native facade became public before its transport exists"
    );

    let canonical_help = Command::new(env!("CARGO_BIN_EXE_marion"))
        .arg("--help")
        .output()
        .expect("the marion binary runs");
    assert!(
        canonical_help.status.success(),
        "the canonical marion help command no longer succeeds"
    );
    assert!(
        canonical_help.stderr.is_empty(),
        "the canonical marion help command printed to stderr"
    );

    for selector in ["claude", "codex", "definitely-not-a-facade"] {
        let output = Command::new(env!("CARGO_BIN_EXE_marion"))
            .arg(selector)
            .output()
            .expect("the marion binary runs");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{selector:?} no longer takes the legacy refusal path; stderr: {stderr}"
        );
        assert_eq!(
            output.stderr, canonical_help.stdout,
            "{selector:?} no longer prints byte-exact legacy usage; stderr: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "{selector:?} unexpectedly produced facade output: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );

        let help = Command::new(env!("CARGO_BIN_EXE_marion"))
            .args([selector, "--help"])
            .output()
            .expect("the marion binary runs");
        assert!(
            help.status.success(),
            "{selector:?} no longer retains the legacy all-argument help scan"
        );
        assert_eq!(
            help.stdout, canonical_help.stdout,
            "{selector:?} no longer prints byte-exact legacy help"
        );
        assert!(
            help.stderr.is_empty(),
            "{selector:?} printed help to stderr"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const TTY_REFUSAL: &str = "marion: native facade \"at\" requires stdin and stdout on the same foreground controlling terminal; use 'marion run <agent-type> --prompt <text>' for structured execution";

#[cfg(target_os = "linux")]
const TIOCSCTTY: usize = 0x540e;
#[cfg(target_os = "macos")]
const TIOCSCTTY: usize = 0x2000_7461;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reexec_command(
    test_name: &str,
    role_env: &str,
    stdin: Stdio,
    stdout: Stdio,
    controlling_fd: Option<i32>,
) -> Command {
    unsafe extern "C" {
        fn setsid() -> i32;
        fn ioctl(fd: i32, request: usize, ...) -> i32;
        fn write(fd: i32, bytes: *const u8, len: usize) -> isize;
    }

    let mut command = Command::new(std::env::current_exe().expect("integration test path"));
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(role_env, "1")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(Stdio::piped());
    if let Some(fd) = controlling_fd {
        // SAFETY: this closure runs after fork and before exec, performs only async-signal-safe
        // session/ioctl syscalls, and returns an OS error directly on refusal.
        unsafe {
            command.pre_exec(move || {
                let marker = b"CHILD pre_exec entered\n";
                let _ = write(2, marker.as_ptr(), marker.len());
                if setsid() < 0 || ioctl(fd, TIOCSCTTY, 0_i32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let marker = b"CHILD pre_exec ctty-ready\n";
                let _ = write(2, marker.as_ptr(), marker.len());
                Ok(())
            });
        }
    }
    command
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn probe_command(stdin: Stdio, stdout: Stdio, controlling_fd: Option<i32>) -> Command {
    reexec_command(
        "nested_tty_dispatch_probe",
        "MARION_NATIVE_TTY_PROBE",
        stdin,
        stdout,
        controlling_fd,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn session_command(stdin: Stdio, stdout: Stdio) -> Command {
    reexec_command(
        "nested_tty_session_helper",
        "MARION_NATIVE_TTY_SESSION",
        stdin,
        stdout,
        Some(0),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn slave_stdio(master: &PtyMaster) -> (Stdio, Stdio) {
    let stdin: OwnedFd = master.open_slave().expect("open stdin slave");
    let stdout: OwnedFd = master.open_slave().expect("open stdout slave");
    (Stdio::from(stdin), Stdio::from(stdout))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ChildWatchdogOutcome {
    status: std::process::ExitStatus,
    timed_out: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_child_with_watchdog(
    mut child: std::process::Child,
    deadlock_ceiling: std::time::Duration,
) -> ChildWatchdogOutcome {
    let pid = child.id() as i32;
    let (status_tx, status_rx) = std::sync::mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let _ = status_tx.send(child.wait());
    });
    let (status, timed_out) = match status_rx.recv_timeout(deadlock_ceiling) {
        Ok(status) => (status.expect("wait for nested TTY child"), false),
        Err(error) => {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            const SIGKILL: i32 = 9;
            // SAFETY: `pid` is the exact owned child; this is only the fixture deadlock ceiling.
            let killed = unsafe { kill(pid, SIGKILL) };
            assert_eq!(killed, 0, "kill timed-out nested child: {error}");
            let status = status_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("timed-out nested child was not reaped")
                .expect("wait for killed nested child");
            (status, true)
        }
    };
    waiter.join().expect("join bounded nested-child waiter");
    ChildWatchdogOutcome { status, timed_out }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_probe(mut command: Command) -> String {
    use std::io::BufRead;

    let mut child = command.spawn().expect("spawn nested TTY probe");
    drop(command);
    eprintln!("PARENT spawned probe pid={}", child.id());
    let stderr = child.stderr.take().expect("probe stderr control pipe");
    let (output_tx, output_rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut output = String::new();
        for line in std::io::BufReader::new(stderr).lines() {
            let line = line.expect("read probe control event");
            eprintln!("CONTROL {line}");
            output.push_str(&line);
            output.push('\n');
        }
        let _ = output_tx.send(output);
    });
    let outcome = wait_for_child_with_watchdog(child, std::time::Duration::from_secs(10));
    eprintln!("PARENT reaped probe status={}", outcome.status);
    let stderr = output_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("probe control reader exceeded watchdog");
    reader.join().expect("join bounded probe control reader");
    assert!(
        !outcome.timed_out,
        "nested probe exceeded watchdog: {stderr}"
    );
    assert!(outcome.status.success(), "nested probe failed: {stderr}");
    stderr
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn nested_tty_fixture_watchdog_reaps_a_probe_that_never_completes() {
    use std::time::Duration;

    let mut command = Command::new(std::env::current_exe().expect("integration test path"));
    command
        .args(["--exact", "nested_tty_watchdog_probe", "--nocapture"])
        .env("MARION_NATIVE_TTY_WATCHDOG_PROBE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().expect("spawn watchdog probe");
    drop(command);

    let outcome = wait_for_child_with_watchdog(child, Duration::from_millis(25));
    assert!(
        outcome.timed_out,
        "non-completing probe escaped its watchdog"
    );
    assert!(
        !outcome.status.success(),
        "watchdog did not kill its owned probe"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn nested_tty_watchdog_probe() {
    if std::env::var_os("MARION_NATIVE_TTY_WATCHDOG_PROBE").is_none() {
        return;
    }
    loop {
        std::thread::park();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn nested_tty_capture_matrix_requires_actual_same_foreground_stdio() {
    // This matrix isolates the client capture boundary. The real socket-backed positive route,
    // including server verification/consume/selection, lives in native_bootstrap's unit fixture.
    eprintln!("CASE foreground start");
    let foreground = PtyMaster::open(WinSize::new(117, 43)).expect("foreground PTY");
    let (stdin, stdout) = slave_stdio(&foreground);
    let foreground_output = run_probe(probe_command(stdin, stdout, Some(0)));
    eprintln!("CASE foreground done");
    assert!(
        foreground_output.contains("TTY_CAPTURE_CONNECTOR"),
        "valid foreground stdio never reached bootstrap: {foreground_output}"
    );
    assert!(!foreground_output.contains(TTY_REFUSAL));

    let redirected_input = PtyMaster::open(WinSize::new(117, 43)).expect("pipe PTY");
    let output = redirected_input.open_slave().expect("stdout slave");
    eprintln!("CASE pipe start");
    let piped = probe_command(Stdio::piped(), Stdio::from(output), Some(1));
    let piped_output = run_probe(piped);
    eprintln!("CASE pipe done");
    assert!(piped_output.contains(TTY_REFUSAL));
    assert!(!piped_output.contains("TTY_CAPTURE_CONNECTOR"));

    let redirected_output = PtyMaster::open(WinSize::new(117, 43)).expect("redirect PTY");
    let input = redirected_output.open_slave().expect("stdin slave");
    let redirected = run_probe(probe_command(Stdio::from(input), Stdio::null(), Some(0)));
    eprintln!("CASE redirect done");
    assert!(redirected.contains(TTY_REFUSAL));
    assert!(!redirected.contains("TTY_CAPTURE_CONNECTOR"));

    let dev_tty = PtyMaster::open(WinSize::new(117, 43)).expect("/dev/tty PTY");
    let input = dev_tty.open_slave().expect("/dev/tty stdin slave");
    let mut dev_tty_command = probe_command(Stdio::from(input), Stdio::null(), Some(0));
    dev_tty_command.env("MARION_PROBE_OPEN_DEV_TTY", "1");
    let dev_tty_output = run_probe(dev_tty_command);
    assert!(
        dev_tty_output.contains("DEV_TTY_OPEN"),
        "fixture did not prove that /dev/tty was available: {dev_tty_output}"
    );
    assert!(dev_tty_output.contains(TTY_REFUSAL));
    assert!(!dev_tty_output.contains("TTY_CAPTURE_CONNECTOR"));

    let no_controlling = PtyMaster::open(WinSize::new(117, 43)).expect("unclaimed PTY");
    let (stdin, stdout) = slave_stdio(&no_controlling);
    let no_controlling_output = run_probe(probe_command(stdin, stdout, None));
    eprintln!("CASE no-ctty done");
    assert!(no_controlling_output.contains(TTY_REFUSAL));
    assert!(!no_controlling_output.contains("TTY_CAPTURE_CONNECTOR"));

    let background = PtyMaster::open(WinSize::new(117, 43)).expect("background PTY");
    let (stdin, stdout) = slave_stdio(&background);
    let background_output = run_probe(session_command(stdin, stdout));
    assert!(background_output.contains("SESSION_PROBE_REAPED"));
    assert!(background_output.contains(TTY_REFUSAL));
    assert!(!background_output.contains("TTY_CAPTURE_CONNECTOR"));

    let first = PtyMaster::open(WinSize::new(117, 43)).expect("first PTY");
    let second = PtyMaster::open(WinSize::new(91, 29)).expect("second PTY");
    let stdin = first.open_slave().expect("first slave");
    let stdout = second.open_slave().expect("second slave");
    let mismatched = run_probe(probe_command(
        Stdio::from(stdin),
        Stdio::from(stdout),
        Some(0),
    ));
    eprintln!("CASE mismatch done");
    assert!(mismatched.contains(TTY_REFUSAL));
    assert!(!mismatched.contains("TTY_CAPTURE_CONNECTOR"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn redirected_process_cannot_repair_stdio_by_opening_dev_tty() {
    let terminal = PtyMaster::open(WinSize::new(117, 43)).expect("/dev/tty PTY");
    let input = terminal.open_slave().expect("/dev/tty stdin slave");
    let mut command = probe_command(Stdio::from(input), Stdio::null(), Some(0));
    command.env("MARION_PROBE_OPEN_DEV_TTY", "1");
    let output = run_probe(command);

    assert!(output.contains("DEV_TTY_OPEN"));
    assert!(output.contains(TTY_REFUSAL));
    assert!(!output.contains("TTY_CAPTURE_CONNECTOR"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn nested_tty_dispatch_probe() {
    unsafe extern "C" {
        fn _exit(status: i32) -> !;
        fn getpid() -> i32;
        fn write(fd: i32, bytes: *const u8, len: usize) -> isize;
    }
    if std::env::var_os("MARION_NATIVE_TTY_PROBE").is_none() {
        return;
    }
    eprintln!("PROBE helper entered");
    if std::env::var_os("MARION_PROBE_OPEN_DEV_TTY").is_some() {
        std::fs::File::open("/dev/tty").expect("probe can open its controlling terminal");
        eprintln!("DEV_TTY_OPEN");
    }

    const DESCRIPTORS: &[NativeFacadeDescriptor] = &[NativeFacadeDescriptor {
        identity: VendorIdentity::new("atlas"),
        command: "atlas",
        aliases: &["at"],
        native: Some(Lane::new(
            true,
            NativeLane::new("atlas-cli", "codex", NativeAdapterId::new("atlas-native")),
        )),
        structured: None,
    }];
    let registry = NativeFacadeRegistry::new(DESCRIPTORS).expect("probe registry");
    eprintln!("PROBE before dispatch");
    let _ = dispatch_native_facade_or_legacy(
        [OsString::from("at"), OsString::from("--opaque")],
        &registry,
        || {
            eprintln!("TTY_CAPTURE_CONNECTOR");
            Err(marion_supervisor::native_bootstrap::BootstrapError::AuthorizationRefused)
        },
        std::io::stderr(),
        || panic!("registered selector reached legacy"),
    );
    eprintln!("PROBE after dispatch pid={}", unsafe { getpid() });
    let marker = b"BEFORE_RAW_EXIT\n";
    // SAFETY: fd 2 is the live control-pipe writer and `marker` is valid for its full length.
    let _ = unsafe { write(2, marker.as_ptr(), marker.len()) };
    // SAFETY: this re-exec helper has emitted its complete result and must not re-enter libtest.
    unsafe { _exit(0) }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn nested_tty_session_helper() {
    if std::env::var_os("MARION_NATIVE_TTY_SESSION").is_none() {
        return;
    }

    unsafe extern "C" {
        fn _exit(status: i32) -> !;
    }

    let mut probe = Command::new(std::env::current_exe().expect("integration test path"));
    probe
        .args(["--exact", "nested_tty_dispatch_probe", "--nocapture"])
        .env("MARION_NATIVE_TTY_PROBE", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    probe.process_group(0);
    let child = probe.spawn().expect("spawn background probe");
    drop(probe);
    let outcome = wait_for_child_with_watchdog(child, std::time::Duration::from_secs(5));
    eprintln!("SESSION_PROBE_REAPED {}", outcome.status);
    assert!(!outcome.timed_out, "background probe exceeded watchdog");
    // SAFETY: this re-exec helper has emitted its complete result and must not re-enter libtest.
    unsafe { _exit(i32::from(!outcome.status.success())) }
}
