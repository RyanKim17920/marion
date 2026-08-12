//! Foreground controlling-terminal authority for direct native CLI requests.

use std::io::{stdin, stdout};
use std::os::fd::{AsFd, OwnedFd};

use marion_proto::TerminalGeometryV1;
use rustix::fs::{OFlags, fcntl_getfl, fstat};
use rustix::io::fcntl_dupfd_cloexec;
use rustix::process::{Pid, getpgid, getpgrp, getsid};
use rustix::termios::{Termios, isatty, tcgetattr, tcgetpgrp, tcgetsid, tcgetwinsize};

use crate::native_bootstrap::{
    BootstrapError, DirectNativeRequestContext, NativeBootstrapClient,
    NativeBootstrapClientSession, PeerIdentity,
};

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

    /// The witness's only descriptor-bearing operation: hand the owned duplicates to Task 2.
    pub(crate) fn bootstrap(
        self,
        connection: NativeBootstrapClient,
        context: DirectNativeRequestContext,
    ) -> Result<NativeBootstrapClientSession, BootstrapError> {
        connection.request(self, context)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

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
}
