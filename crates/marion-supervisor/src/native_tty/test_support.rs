//! Test-only fixtures for callers that drive a [`NativeRelayTerminal`] on a fresh PTY.

use rustix::fs::{OFlags, fcntl_getfl};
use rustix::io::fcntl_dupfd_cloexec;
use rustix::process::getpgrp;
use rustix::termios::tcgetattr;

use super::{
    ClientTtyWitness, F_SETFL_RESULTS, NativeRelayTerminal, TerminalBaseline, TerminalFingerprint,
};
use crate::pty::{PtyMaster, WinSize};
use marion_core::proto::TerminalGeometryV1;

/// A relay terminal already in raw mode on a test PTY, plus what a test needs to prove it
/// was restored: a retained duplicate of stdin and the cooked flags it started with.
pub(crate) struct RawRelayTerminal {
    pub(crate) master: PtyMaster,
    pub(crate) terminal: NativeRelayTerminal,
    pub(crate) observed_stdin: std::os::fd::OwnedFd,
    pub(crate) baseline_stdin_flags: OFlags,
    cooked_termios: rustix::termios::Termios,
}

impl RawRelayTerminal {
    /// The cooked terminal attributes captured before the relay entered raw mode.
    pub(crate) fn baseline_termios(&self) -> rustix::termios::Termios {
        self.cooked_termios.clone()
    }
}

pub(crate) fn raw_relay_terminal_on_test_pty() -> RawRelayTerminal {
    let master = PtyMaster::open(WinSize::new(91, 29)).expect("test PTY");
    let stdin = master.open_slave().expect("relay stdin");
    let stdout = master.open_slave().expect("relay stdout");
    let cooked_termios = tcgetattr(&stdin).unwrap();
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
            termios: cooked_termios.clone(),
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
    let terminal = witness.enter_native_relay().expect("enter raw mode");
    RawRelayTerminal {
        master,
        terminal,
        observed_stdin,
        baseline_stdin_flags,
        cooked_termios,
    }
}

/// Make the next stdin status-flag restoration fail with `EBADF` on this thread; the retry
/// after it succeeds.
pub(crate) fn fail_next_stdin_flag_restore() {
    queue_status_flag_results([Some(rustix::io::Errno::BADF), None]);
}

/// Script the outcome of the next status-flag changes on this thread, in call order: `None`
/// runs the real `fcntl`, `Some(errno)` fails that one call instead.
pub(crate) fn queue_status_flag_results(
    results: impl IntoIterator<Item = Option<rustix::io::Errno>>,
) {
    F_SETFL_RESULTS.with(|queue| queue.borrow_mut().extend(results));
}
