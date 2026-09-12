//! Foreground controlling-terminal authority for direct native CLI requests.

use std::io::{stdin, stdout};
use std::os::fd::{AsFd, OwnedFd};

use marion_core::proto::TerminalGeometryV1;
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
use crate::procid::TerminalDevice;

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
    state: NativeRelayTerminalState,
    armed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeRelayTerminalState {
    Raw,
    Restored,
    RestorePending,
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
    #[error("terminal restoration is pending before raw-mode re-entry")]
    RestorationPending,
    #[error(
        "terminal relay entry failed: {entry}; cleanup stage `terminal restoration` also failed: {cleanup}"
    )]
    EntryCleanup {
        entry: rustix::io::Errno,
        cleanup: rustix::io::Errno,
    },
    #[error("terminal inspection failed: {0}")]
    Inspection(#[from] rustix::io::Errno),
    #[error("the peer's controlling terminal could not be read from the kernel: {0}")]
    PeerTerminal(String),
}

/// Prove, from the kernel's process table rather than a terminal ioctl, that the terminal behind
/// `fingerprint` is the peer's controlling terminal and that the peer sits in its foreground
/// process group.
///
/// `tcgetsid`/`tcgetpgrp` cannot do this from a detached supervisor: on Linux and macOS alike they
/// answer `ENOTTY` unless the terminal is the **caller's** controlling terminal, and the supervisor
/// lives in its own session. The process table records the same two facts about the peer
/// (`procid::controlling_terminal`, the one process-table read this crate has). Both
/// `verify_bootstrap_tty` and `revalidate_peer` come through here, so the two cannot drift.
fn verify_peer_terminal(
    peer_pid: Pid,
    fingerprint: TerminalFingerprint,
) -> Result<(Pid, Pid), NativeTtyError> {
    let terminal = crate::procid::controlling_terminal(peer_pid.as_raw_nonzero().get())
        .map_err(NativeTtyError::PeerTerminal)?;
    if terminal.device != Some(TerminalDevice::from_rdev(fingerprint.st_rdev)) {
        return Err(NativeTtyError::NotControllingTerminal);
    }
    let peer_pgid = getpgid(Some(peer_pid))?;
    if terminal.foreground_pgid != Some(peer_pgid.as_raw_nonzero().get()) {
        return Err(NativeTtyError::BackgroundProcessGroup);
    }
    let peer_session_id = getsid(Some(peer_pid))?;
    Ok((peer_session_id, peer_pgid))
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
    let (peer_session_id, foreground_pgid) = verify_peer_terminal(peer_pid, fingerprint)?;

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
        let (peer_session_id, foreground_pgid) = verify_peer_terminal(peer_pid, self.fingerprint)?;
        if peer_session_id != self.peer_session_id {
            return Err(NativeTtyError::NotControllingTerminal);
        }
        if foreground_pgid != self.foreground_pgid {
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
            state: NativeRelayTerminalState::Restored,
            armed: false,
        };
        terminal.enter_raw()?;
        Ok(terminal)
    }

    fn enter_raw(&mut self) -> Result<(), NativeTtyError> {
        match self.state {
            NativeRelayTerminalState::Raw => return Ok(()),
            NativeRelayTerminalState::Restored => {}
            NativeRelayTerminalState::RestorePending => {
                return Err(NativeTtyError::RestorationPending);
            }
        }
        // Arm restoration before the first mutation: even a syscall which partially changes the
        // terminal and then reports failure must unwind through the retained baseline.
        self.armed = true;
        let mut raw = self.baseline.termios.clone();
        raw.make_raw();
        let entry =
            retry_interrupted(|| set_terminal_attr(self.stdin.as_fd(), &raw)).and_then(|()| {
                retry_interrupted(|| {
                    set_status_flags(
                        self.stdin.as_fd(),
                        self.baseline.stdin_flags | OFlags::NONBLOCK,
                    )
                })
            });
        if let Err(entry) = entry {
            return match self.restore_result() {
                Ok(()) => Err(entry.into()),
                Err(cleanup) => Err(NativeTtyError::EntryCleanup {
                    entry,
                    cleanup: cleanup.0,
                }),
            };
        }
        self.state = NativeRelayTerminalState::Raw;
        Ok(())
    }

    pub(crate) fn restore_for_suspend(&mut self) -> Result<(), NativeTtyRestoreError> {
        self.restore_result()
    }

    pub(crate) fn reenter_after_continue(&mut self) -> Result<(), NativeTtyError> {
        self.enter_raw()
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
            self.state = NativeRelayTerminalState::RestorePending;
            return Err(NativeTtyRestoreError(error));
        }
        self.armed = false;
        self.state = NativeRelayTerminalState::Restored;
        Ok(())
    }
}

impl Drop for NativeRelayTerminal {
    fn drop(&mut self) {
        let _ = self.restore_result();
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
