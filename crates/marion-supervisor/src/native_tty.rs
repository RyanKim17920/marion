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

/// A terminal device number as `(major, minor)`, so the process table's spelling and `fstat`'s
/// spelling of the same device compare equal on every platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalDevice {
    major: u32,
    minor: u32,
}

impl TerminalDevice {
    fn from_rdev(rdev: u64) -> Self {
        let dev = rdev as rustix::fs::Dev;
        Self {
            major: rustix::fs::major(dev),
            minor: rustix::fs::minor(dev),
        }
    }
}

/// What the kernel's process table says about a peer's controlling terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerTerminal {
    /// `None` when the peer has no controlling terminal at all.
    device: Option<TerminalDevice>,
    /// The terminal's foreground process group, as the process table records it for the peer.
    foreground_pgid: Option<Pid>,
}

/// **macOS: `sysctl(KERN_PROC_PID)` → `kinfo_proc.kp_eproc.{e_tdev, e_tpgid}`.**
///
/// Offsets measured on this platform with `offsetof` rather than mirrored from a header:
/// `e_tdev` at **572** (a 4-byte `dev_t`, `-1` for no terminal) and `e_tpgid` at **576** (a
/// 4-byte `pid_t`), inside the measured **648**-byte struct. A shorter answer is refused rather
/// than read past.
#[cfg(target_os = "macos")]
fn peer_controlling_terminal(peer_pid: Pid) -> Result<PeerTerminal, NativeTtyError> {
    const CTL_KERN: i32 = 1;
    const KERN_PROC: i32 = 14;
    const KERN_PROC_PID: i32 = 1;
    const CAP: usize = 4096;
    const E_TDEV: usize = 572;
    const E_TPGID: usize = 576;
    const NEEDED: usize = E_TPGID + 4;

    unsafe extern "C" {
        fn sysctl(
            name: *mut i32,
            namelen: u32,
            oldp: *mut core::ffi::c_void,
            oldlenp: *mut usize,
            newp: *mut core::ffi::c_void,
            newlen: usize,
        ) -> i32;
    }

    let mut mib = [
        CTL_KERN,
        KERN_PROC,
        KERN_PROC_PID,
        peer_pid.as_raw_nonzero().get(),
    ];
    let mut buf = [0u8; CAP];
    let mut len = CAP;
    // SAFETY: `mib` is four `i32`s and `namelen` says four; `buf` is `CAP` bytes and `len` says
    // `CAP`; `newp`/`newlen` are the documented "reading, not writing" pair.
    let rc = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(NativeTtyError::PeerTerminal(format!(
            "the kernel refused to describe process {}: {}",
            peer_pid.as_raw_nonzero(),
            std::io::Error::last_os_error()
        )));
    }
    if len < NEEDED {
        return Err(NativeTtyError::PeerTerminal(format!(
            "the kernel described process {} in {len} bytes, too few to hold its terminal",
            peer_pid.as_raw_nonzero()
        )));
    }
    let field = |at: usize| i32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
    let tdev = field(E_TDEV);
    let tpgid = field(E_TPGID);
    Ok(PeerTerminal {
        device: (tdev != -1).then(|| TerminalDevice::from_rdev(i64::from(tdev) as u64)),
        foreground_pgid: Pid::from_raw(tpgid),
    })
}

/// **Linux: `/proc/<pid>/stat` fields `tty_nr` (7) and `tpgid` (8).**
///
/// Fields are counted after the last `)` so a `comm` containing spaces or parentheses cannot
/// shift them. `tty_nr` uses the kernel's `new_encode_dev` layout, decoded here into the same
/// `(major, minor)` pair `fstat` yields.
#[cfg(target_os = "linux")]
fn peer_controlling_terminal(peer_pid: Pid) -> Result<PeerTerminal, NativeTtyError> {
    let pid = peer_pid.as_raw_nonzero().get();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|error| {
        NativeTtyError::PeerTerminal(format!("/proc/{pid}/stat could not be read: {error}"))
    })?;
    let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest).ok_or_else(|| {
        NativeTtyError::PeerTerminal(format!("/proc/{pid}/stat is not in the documented shape"))
    })?;
    let mut fields = after_comm.split_ascii_whitespace();
    // state ppid pgrp session tty_nr tpgid
    let tty_nr = fields.nth(4);
    let tpgid = fields.next();
    let (Some(tty_nr), Some(tpgid)) = (tty_nr, tpgid) else {
        return Err(NativeTtyError::PeerTerminal(format!(
            "/proc/{pid}/stat has too few fields"
        )));
    };
    let tty_nr: u64 = tty_nr.parse().map_err(|_| {
        NativeTtyError::PeerTerminal(format!("/proc/{pid}/stat tty_nr is not a number"))
    })?;
    let tpgid: i32 = tpgid.parse().map_err(|_| {
        NativeTtyError::PeerTerminal(format!("/proc/{pid}/stat tpgid is not a number"))
    })?;
    let device = (tty_nr != 0).then(|| TerminalDevice {
        major: ((tty_nr >> 8) & 0xfff) as u32,
        minor: ((tty_nr & 0xff) | ((tty_nr >> 12) & 0xfff00)) as u32,
    });
    Ok(PeerTerminal {
        device,
        foreground_pgid: Pid::from_raw(tpgid),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_controlling_terminal(_peer_pid: Pid) -> Result<PeerTerminal, NativeTtyError> {
    Err(NativeTtyError::PeerTerminal(
        "this platform has no supported process-table terminal read".into(),
    ))
}

/// Prove, from the kernel's process table rather than a terminal ioctl, that the terminal behind
/// `fingerprint` is the peer's controlling terminal and that the peer sits in its foreground
/// process group.
///
/// `tcgetsid`/`tcgetpgrp` cannot do this from a detached supervisor: on Linux and macOS alike they
/// answer `ENOTTY` unless the terminal is the **caller's** controlling terminal, and the supervisor
/// lives in its own session. The process table records the same two facts about the peer.
fn verify_peer_terminal(
    peer_pid: Pid,
    fingerprint: TerminalFingerprint,
) -> Result<(Pid, Pid), NativeTtyError> {
    let terminal = peer_controlling_terminal(peer_pid)?;
    if terminal.device != Some(TerminalDevice::from_rdev(fingerprint.st_rdev)) {
        return Err(NativeTtyError::NotControllingTerminal);
    }
    let peer_pgid = getpgid(Some(peer_pid))?;
    if terminal.foreground_pgid != Some(peer_pgid) {
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

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "reviewed suspend transition stays dark until the relay signal pump consumes it"
        )
    )]
    pub(crate) fn restore_for_suspend(&mut self) -> Result<(), NativeTtyRestoreError> {
        self.restore_result()
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "reviewed resume transition stays dark until the relay signal pump consumes it"
        )
    )]
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

/// Test-only fixtures for callers that drive a [`NativeRelayTerminal`] on a fresh PTY.
#[cfg(test)]
pub(crate) mod test_support {
    use rustix::fs::{OFlags, fcntl_getfl};
    use rustix::io::fcntl_dupfd_cloexec;
    use rustix::process::getpgrp;
    use rustix::termios::tcgetattr;

    use super::{
        ClientTtyWitness, F_SETFL_RESULTS, NativeRelayTerminal, TerminalBaseline,
        TerminalFingerprint,
    };
    use crate::pty::{PtyMaster, WinSize};
    use marion_proto::TerminalGeometryV1;

    /// A relay terminal already in raw mode on a test PTY, plus what a test needs to prove it
    /// was restored: a retained duplicate of stdin and the cooked flags it started with.
    pub(crate) struct RawRelayTerminal {
        pub(crate) master: PtyMaster,
        pub(crate) terminal: NativeRelayTerminal,
        pub(crate) observed_stdin: std::os::fd::OwnedFd,
        pub(crate) baseline_stdin_flags: OFlags,
    }

    pub(crate) fn raw_relay_terminal_on_test_pty() -> RawRelayTerminal {
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
        let terminal = witness.enter_native_relay().expect("enter raw mode");
        RawRelayTerminal {
            master,
            terminal,
            observed_stdin,
            baseline_stdin_flags,
        }
    }

    /// Make the next stdin status-flag restoration fail with `EBADF` on this thread; the retry
    /// after it succeeds.
    pub(crate) fn fail_next_stdin_flag_restore() {
        F_SETFL_RESULTS.with(|results| {
            results
                .borrow_mut()
                .extend([Some(rustix::io::Errno::BADF), None])
        });
    }
}

#[cfg(test)]
mod tests {
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
}
