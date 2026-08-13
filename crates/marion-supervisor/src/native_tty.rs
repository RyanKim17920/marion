//! Foreground controlling-terminal authority for direct native CLI requests.

use std::io::{stdin, stdout};
use std::os::fd::{AsFd, OwnedFd};

use marion_proto::TerminalGeometryV1;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl, fstat};
use rustix::io::fcntl_dupfd_cloexec;
use rustix::process::{Pid, getpgid, getpgrp, getsid};
use rustix::termios::{
    OptionalActions, Termios, isatty, tcgetattr, tcgetpgrp, tcgetsid, tcgetwinsize, tcsetattr,
};

use crate::native_bootstrap::{
    BootstrapError, DirectNativeRequestContext, NativeBootstrapClient,
    NativeBootstrapClientSession, PeerIdentity,
};

#[cfg(test)]
thread_local! {
    static TCSETATTR_RESULTS: std::cell::RefCell<std::collections::VecDeque<Option<rustix::io::Errno>>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    static F_SETFL_RESULTS: std::cell::RefCell<std::collections::VecDeque<Option<rustix::io::Errno>>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

fn set_terminal_attr(
    fd: std::os::fd::BorrowedFd<'_>,
    termios: &Termios,
) -> Result<(), rustix::io::Errno> {
    #[cfg(test)]
    if let Some(Some(error)) = TCSETATTR_RESULTS.with(|results| results.borrow_mut().pop_front()) {
        return Err(error);
    }
    tcsetattr(fd, OptionalActions::Now, termios)
}

fn set_status_flags(
    fd: std::os::fd::BorrowedFd<'_>,
    flags: OFlags,
) -> Result<(), rustix::io::Errno> {
    #[cfg(test)]
    if let Some(Some(error)) = F_SETFL_RESULTS.with(|results| results.borrow_mut().pop_front()) {
        return Err(error);
    }
    fcntl_setfl(fd, flags)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalFingerprint {
    st_dev: u64,
    st_ino: u64,
    st_rdev: u64,
}

impl TerminalFingerprint {
    pub(crate) const fn components(self) -> (u64, u64, u64) {
        (self.st_dev, self.st_ino, self.st_rdev)
    }
}

#[derive(Debug)]
pub(crate) struct TerminalBaseline {
    #[allow(
        dead_code,
        reason = "retained for the later transparent relay restoration guard"
    )]
    termios: Termios,
    #[allow(
        dead_code,
        reason = "retained for the later transparent relay restoration guard"
    )]
    stdin_flags: OFlags,
    #[allow(
        dead_code,
        reason = "retained for the later transparent relay restoration guard"
    )]
    stdout_flags: OFlags,
}

#[derive(Debug)]
pub(crate) struct ClientTtyWitness {
    stdin: OwnedFd,
    stdout: OwnedFd,
    #[allow(
        dead_code,
        reason = "client observation is diagnostic; the server observation is authoritative"
    )]
    fingerprint: TerminalFingerprint,
    #[allow(
        dead_code,
        reason = "retained until ownership transfers to the later relay"
    )]
    baseline: TerminalBaseline,
    #[allow(
        dead_code,
        reason = "client observation is diagnostic; the server observation is authoritative"
    )]
    session_id: Pid,
    #[allow(
        dead_code,
        reason = "client observation is diagnostic; the server observation is authoritative"
    )]
    foreground_pgid: Pid,
    #[allow(
        dead_code,
        reason = "client observation is diagnostic; the server observation is authoritative"
    )]
    observed_geometry: TerminalGeometryV1,
}

/// The retained client terminal in raw relay mode. Its baseline is the one captured before native
/// bootstrap and descriptor passing, not a second observation made after the handoff.
pub(crate) struct NativeRelayTerminal {
    stdin: OwnedFd,
    stdout: OwnedFd,
    baseline: TerminalBaseline,
    armed: bool,
}

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("restoring native terminal state failed: {0}")]
pub(crate) struct NativeTtyRestoreError(rustix::io::Errno);

#[derive(Debug)]
pub(crate) struct ControllingTtyWitness {
    stdin: OwnedFd,
    stdout: OwnedFd,
    fingerprint: TerminalFingerprint,
    #[allow(
        dead_code,
        reason = "retained for the later transparent relay restoration guard"
    )]
    baseline: TerminalBaseline,
    peer_session_id: Pid,
    foreground_pgid: Pid,
    initial_geometry: TerminalGeometryV1,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeTtyError {
    #[error("stdin or stdout is not a terminal")]
    NotATerminal,
    #[error("stdin and stdout name different terminals")]
    DifferentTerminals,
    #[error("stdin or stdout is not the process controlling terminal")]
    NotControllingTerminal,
    #[error("the process is not in the terminal foreground process group")]
    BackgroundProcessGroup,
    #[error("the terminal reported zero rows or columns")]
    InvalidGeometry,
    #[error("terminal descriptor roles are invalid")]
    InvalidDescriptorRoles,
    #[error("the authenticated peer pid was invalid")]
    InvalidPeer,
    #[error(
        "terminal relay entry failed: {entry}; cleanup stage `terminal restoration` also failed: {cleanup}"
    )]
    EntryCleanup {
        entry: rustix::io::Errno,
        cleanup: rustix::io::Errno,
    },
    #[error("terminal inspection failed: {0}")]
    Inspection(#[from] rustix::io::Errno),
}

fn descriptor_roles_allow(stdin_flags: OFlags, stdout_flags: OFlags) -> bool {
    matches!(stdin_flags & OFlags::ACCMODE, OFlags::RDONLY | OFlags::RDWR)
        && matches!(
            stdout_flags & OFlags::ACCMODE,
            OFlags::WRONLY | OFlags::RDWR
        )
}

fn retry_interrupted(
    mut operation: impl FnMut() -> Result<(), rustix::io::Errno>,
) -> Result<(), rustix::io::Errno> {
    loop {
        match operation() {
            Err(rustix::io::Errno::INTR) => {}
            result => return result,
        }
    }
}

/// Duplicate and verify the process's actual standard descriptors. This deliberately never opens
/// `/dev/tty`: redirected stdio cannot be repaired into native eligibility.
pub(crate) fn capture_process_stdio() -> Result<ClientTtyWitness, NativeTtyError> {
    let stdin = fcntl_dupfd_cloexec(stdin().as_fd(), 3)?;
    let stdout = fcntl_dupfd_cloexec(stdout().as_fd(), 3)?;
    if !isatty(&stdin) || !isatty(&stdout) {
        return Err(NativeTtyError::NotATerminal);
    }

    let stdin_termios = tcgetattr(&stdin)?;
    let _stdout_termios = tcgetattr(&stdout)?;
    let stdin_flags = fcntl_getfl(&stdin)?;
    let stdout_flags = fcntl_getfl(&stdout)?;
    if !descriptor_roles_allow(stdin_flags, stdout_flags) {
        return Err(NativeTtyError::InvalidDescriptorRoles);
    }
    let stdin_stat = fstat(&stdin)?;
    let stdout_stat = fstat(&stdout)?;
    let fingerprint = TerminalFingerprint {
        st_dev: stdin_stat.st_dev as u64,
        st_ino: stdin_stat.st_ino as u64,
        st_rdev: stdin_stat.st_rdev as u64,
    };
    if fingerprint
        != (TerminalFingerprint {
            st_dev: stdout_stat.st_dev as u64,
            st_ino: stdout_stat.st_ino as u64,
            st_rdev: stdout_stat.st_rdev as u64,
        })
    {
        return Err(NativeTtyError::DifferentTerminals);
    }

    let session_id = getsid(None)?;
    if tcgetsid(&stdin)? != session_id || tcgetsid(&stdout)? != session_id {
        return Err(NativeTtyError::NotControllingTerminal);
    }
    let foreground_pgid = getpgrp();
    if tcgetpgrp(&stdin)? != foreground_pgid || tcgetpgrp(&stdout)? != foreground_pgid {
        return Err(NativeTtyError::BackgroundProcessGroup);
    }

    let winsize = tcgetwinsize(&stdin)?;
    if winsize.ws_col == 0 || winsize.ws_row == 0 {
        return Err(NativeTtyError::InvalidGeometry);
    }

    Ok(ClientTtyWitness {
        stdin,
        stdout,
        fingerprint,
        baseline: TerminalBaseline {
            termios: stdin_termios,
            stdin_flags,
            stdout_flags,
        },
        session_id,
        foreground_pgid,
        observed_geometry: TerminalGeometryV1 {
            cols: winsize.ws_col,
            rows: winsize.ws_row,
            xpixel: winsize.ws_xpixel,
            ypixel: winsize.ws_ypixel,
        },
    })
}

pub(crate) fn verify_bootstrap_tty(
    peer: PeerIdentity,
    descriptors: [OwnedFd; 2],
) -> Result<ControllingTtyWitness, NativeTtyError> {
    let [stdin, stdout] = descriptors;
    if !isatty(&stdin) || !isatty(&stdout) {
        return Err(NativeTtyError::NotATerminal);
    }

    let stdin_termios = tcgetattr(&stdin)?;
    let _stdout_termios = tcgetattr(&stdout)?;
    let stdin_flags = fcntl_getfl(&stdin)?;
    let stdout_flags = fcntl_getfl(&stdout)?;
    if !descriptor_roles_allow(stdin_flags, stdout_flags) {
        return Err(NativeTtyError::InvalidDescriptorRoles);
    }
    let stdin_stat = fstat(&stdin)?;
    let stdout_stat = fstat(&stdout)?;
    let fingerprint = TerminalFingerprint {
        st_dev: stdin_stat.st_dev as u64,
        st_ino: stdin_stat.st_ino as u64,
        st_rdev: stdin_stat.st_rdev as u64,
    };
    if fingerprint
        != (TerminalFingerprint {
            st_dev: stdout_stat.st_dev as u64,
            st_ino: stdout_stat.st_ino as u64,
            st_rdev: stdout_stat.st_rdev as u64,
        })
    {
        return Err(NativeTtyError::DifferentTerminals);
    }

    let peer_pid = Pid::from_raw(peer.pid() as i32).ok_or(NativeTtyError::InvalidPeer)?;
    let peer_session_id = getsid(Some(peer_pid))?;
    if tcgetsid(&stdin)? != peer_session_id || tcgetsid(&stdout)? != peer_session_id {
        return Err(NativeTtyError::NotControllingTerminal);
    }
    let foreground_pgid = getpgid(Some(peer_pid))?;
    if tcgetpgrp(&stdin)? != foreground_pgid || tcgetpgrp(&stdout)? != foreground_pgid {
        return Err(NativeTtyError::BackgroundProcessGroup);
    }

    let winsize = tcgetwinsize(&stdin)?;
    if winsize.ws_col == 0 || winsize.ws_row == 0 {
        return Err(NativeTtyError::InvalidGeometry);
    }

    Ok(ControllingTtyWitness {
        stdin,
        stdout,
        fingerprint,
        baseline: TerminalBaseline {
            termios: stdin_termios,
            stdin_flags,
            stdout_flags,
        },
        peer_session_id,
        foreground_pgid,
        initial_geometry: TerminalGeometryV1 {
            cols: winsize.ws_col,
            rows: winsize.ws_row,
            xpixel: winsize.ws_xpixel,
            ypixel: winsize.ws_ypixel,
        },
    })
}

impl ControllingTtyWitness {
    pub(crate) fn stdin(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stdin.as_fd()
    }

    pub(crate) fn stdout(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stdout.as_fd()
    }

    pub(crate) fn revalidate_peer(&self, peer: PeerIdentity) -> Result<(), NativeTtyError> {
        let stdin_stat = fstat(&self.stdin)?;
        let stdout_stat = fstat(&self.stdout)?;
        let stdin_fingerprint = TerminalFingerprint {
            st_dev: stdin_stat.st_dev as u64,
            st_ino: stdin_stat.st_ino as u64,
            st_rdev: stdin_stat.st_rdev as u64,
        };
        let stdout_fingerprint = TerminalFingerprint {
            st_dev: stdout_stat.st_dev as u64,
            st_ino: stdout_stat.st_ino as u64,
            st_rdev: stdout_stat.st_rdev as u64,
        };
        if stdin_fingerprint != self.fingerprint || stdout_fingerprint != self.fingerprint {
            return Err(NativeTtyError::DifferentTerminals);
        }

        let peer_pid = Pid::from_raw(peer.pid() as i32).ok_or(NativeTtyError::InvalidPeer)?;
        if getsid(Some(peer_pid))? != self.peer_session_id
            || tcgetsid(&self.stdin)? != self.peer_session_id
            || tcgetsid(&self.stdout)? != self.peer_session_id
        {
            return Err(NativeTtyError::NotControllingTerminal);
        }
        if getpgid(Some(peer_pid))? != self.foreground_pgid
            || tcgetpgrp(&self.stdin)? != self.foreground_pgid
            || tcgetpgrp(&self.stdout)? != self.foreground_pgid
        {
            return Err(NativeTtyError::BackgroundProcessGroup);
        }
        Ok(())
    }

    pub(crate) const fn fingerprint(&self) -> TerminalFingerprint {
        self.fingerprint
    }

    pub(crate) fn initial_geometry(&self) -> TerminalGeometryV1 {
        self.initial_geometry.clone()
    }
}

impl ClientTtyWitness {
    pub(crate) fn stdin(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stdin.as_fd()
    }

    pub(crate) fn stdout(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stdout.as_fd()
    }

    pub(crate) fn enter_native_relay(self) -> Result<NativeRelayTerminal, NativeTtyError> {
        NativeRelayTerminal::enter(self)
    }

    /// The witness's only descriptor-bearing operation: hand the owned duplicates to Task 2.
    pub(crate) fn bootstrap(
        self,
        connection: NativeBootstrapClient,
        context: DirectNativeRequestContext,
    ) -> Result<NativeBootstrapClientSession, BootstrapError> {
        connection.request(self, context)
    }
}

impl NativeRelayTerminal {
    fn enter(witness: ClientTtyWitness) -> Result<Self, NativeTtyError> {
        let ClientTtyWitness {
            stdin,
            stdout,
            baseline,
            ..
        } = witness;
        // Arm restoration before the first mutation: even a syscall which partially changes the
        // terminal and then reports failure must unwind through the retained baseline.
        let mut terminal = Self {
            stdin,
            stdout,
            baseline,
            armed: true,
        };
        let mut raw = terminal.baseline.termios.clone();
        raw.make_raw();
        let entry =
            retry_interrupted(|| set_terminal_attr(terminal.stdin.as_fd(), &raw)).and_then(|()| {
                retry_interrupted(|| {
                    set_status_flags(
                        terminal.stdin.as_fd(),
                        terminal.baseline.stdin_flags | OFlags::NONBLOCK,
                    )
                })
            });
        if let Err(entry) = entry {
            return match terminal.restore_result() {
                Ok(()) => Err(entry.into()),
                Err(cleanup) => Err(NativeTtyError::EntryCleanup {
                    entry,
                    cleanup: cleanup.0,
                }),
            };
        }
        Ok(terminal)
    }

    pub(crate) fn stdin(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stdin.as_fd()
    }

    pub(crate) fn stdout(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stdout.as_fd()
    }

    pub(crate) fn restore_result(&mut self) -> Result<(), NativeTtyRestoreError> {
        if !self.armed {
            return Ok(());
        }
        // Restore both descriptor roles even though native relay changes only stdin today. The
        // baseline is an ownership contract, and exact write-back makes future flag use safe.
        let mut first_error = None;
        for result in [
            retry_interrupted(|| set_status_flags(self.stdin.as_fd(), self.baseline.stdin_flags)),
            retry_interrupted(|| set_status_flags(self.stdout.as_fd(), self.baseline.stdout_flags)),
            retry_interrupted(|| set_terminal_attr(self.stdin.as_fd(), &self.baseline.termios)),
        ] {
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(NativeTtyRestoreError(error));
        }
        self.armed = false;
        Ok(())
    }
}

impl Drop for NativeRelayTerminal {
    fn drop(&mut self) {
        let _ = self.restore_result();
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::{BufRead, Write};

    use super::*;
    use crate::pty::{PtyMaster, WinSize};

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
                    if error == rustix::io::Errno::AGAIN
                        && std::time::Instant::now() < deadline =>
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
                    if error == rustix::io::Errno::AGAIN
                        && std::time::Instant::now() < deadline =>
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

    #[test]
    fn relay_exit_preserves_the_primary_error_and_names_cleanup_failure() {
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
        let observed_stdin = fcntl_dupfd_cloexec(witness.stdin(), 3).unwrap();
        let mut terminal = witness.enter_native_relay().expect("enter raw mode");
        F_SETFL_RESULTS.with(|results| {
            results
                .borrow_mut()
                .extend([Some(rustix::io::Errno::BADF), None])
        });

        let error = crate::native_relay::finish_terminal_relay(
            &mut terminal,
            Err("reading the native pane socket: reset".into()),
        )
        .unwrap_err();
        assert!(
            error.contains("reading the native pane socket: reset"),
            "{error}"
        );
        assert!(error.contains("restoring native terminal state"), "{error}");

        // A failed explicit restore leaves Drop armed for the last-resort retry.
        drop(terminal);
        assert_eq!(fcntl_getfl(&observed_stdin).unwrap(), baseline_stdin_flags);
    }

    #[test]
    fn production_relay_open_failure_reports_cleanup_and_drop_retries_restoration() {
        const PROBE: &str = "MARION_NATIVE_RELAY_OPEN_FAILURE_PROBE";
        if std::env::var_os(PROBE).is_none() {
            let name = format!(
                "{}::production_relay_open_failure_reports_cleanup_and_drop_retries_restoration",
                module_path!().split_once("::").expect("crate::module").1
            );
            let probe = std::process::Command::new(
                std::env::current_exe().expect("the unit-test binary has a path"),
            )
            .args(["--exact", "--nocapture", "--test-threads", "1", &name])
            .env(PROBE, "1")
            .output()
            .expect("the isolated production relay probe runs");
            assert!(
                probe.status.success(),
                "the isolated production relay probe failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&probe.stdout),
                String::from_utf8_lossy(&probe.stderr)
            );
            return;
        }

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
        let observed_stdin = fcntl_dupfd_cloexec(witness.stdin(), 3).unwrap();
        let mut terminal = witness.enter_native_relay().expect("enter raw mode");
        let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut request = String::new();
            std::io::BufReader::new(server.try_clone().unwrap())
                .read_line(&mut request)
                .expect("read native attach request");
            server.write_all(b"not-json\n").unwrap();
            server.flush().unwrap();
        });
        F_SETFL_RESULTS.with(|results| {
            results
                .borrow_mut()
                .extend([Some(rustix::io::Errno::BADF), None])
        });

        let error = crate::native_relay::relay_claimed(
            marion_core::contract::AgentId("native".into()),
            &mut terminal,
            client,
        )
        .unwrap_err();
        server.join().unwrap();
        assert!(error.contains("unreadable native pane frame"), "{error}");
        assert!(error.contains("terminal restoration"), "{error}");

        drop(terminal);
        assert_eq!(fcntl_getfl(&observed_stdin).unwrap(), baseline_stdin_flags);
    }
}
