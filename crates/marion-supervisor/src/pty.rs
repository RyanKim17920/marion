//! **The pty host — the master lives here, in the supervisor** (design §5.3, §5.2, §6.4).
//!
//! # Why the supervisor and not the client
//!
//! A pty master is not a view onto a terminal, it is the *terminal itself*. Close it and the slave
//! hangs up; the kernel then SIGHUPs the session's foreground process group. So if a client held
//! the master and was SIGKILLed, every agent in a pane would die with it — and §7.3.1's invariant,
//! *"a crashed client MUST leave every node exactly as it was"*, would regress precisely at the
//! surface M3 adds. The master is therefore owned by [`PtyHost`], which lives for the supervisor's
//! lifetime, and a client is a **listener**: it receives bytes and can be dropped without the fd
//! noticing. `the_supervisor_and_not_the_client_holds_the_master` is the guard.
//!
//! # Bytes on the wire, not grid cells
//!
//! What crosses to a client is [`marion_proto::Event::NodePty`] — the raw byte stream, as text. The
//! grid is a **derived per-viewer object**: two clients on one node may have different window
//! sizes, different scrollback positions and different ideas of what is on screen, and a supervisor
//! that shipped cells would have to pick one. §5.3's emulator (`marion-term`) runs on the viewer's
//! side of that seam.
//!
//! # Hand-rolled, zero new dependencies
//!
//! §5.3 picks `pty-process` with `features = ["async"]` for tokio `AsyncRead`/`AsyncWrite`. **That
//! reason is stale**: this workspace has no tokio and no `async fn` anywhere — every I/O path in
//! `marion-supervisor` is a blocking read on a dedicated thread — so the feature that decided the
//! crate buys nothing here. What is left is `posix_openpt`/`grantpt`/`unlockpt`/`ptsname`, two
//! `ioctl`s and a `setsid`, which is the same hand-declared `unsafe extern "C"` device
//! `serve.rs`'s `getuid`/`getpeereid` and `procid.rs`'s `sysctl` already use. §5.3 has been updated
//! to say so.
//!
//! **`ptsname` is serialized under a mutex** rather than replaced by `ioctl(TIOCPTYGNAME)`. It is
//! not reentrant and macOS has no `ptsname_r`, so *something* must serialize it; the choice is
//! between one process-wide lock held for the microseconds between the call and the copy, or a
//! macOS-only `TIOCPTYGNAME` arm beside a Linux-only `ptsname_r` arm — two platform paths where
//! there is currently one. The lock is the smaller thing and it covers the only reentrancy this
//! workspace can actually control: another marion thread opening a pty at the same instant.
//! (Nothing here can stop a *foreign* library calling `ptsname`; neither could `ptsname_r`, since
//! that hazard is about the other caller's buffer, not ours.)
//!
//! # The fd topology, and which parts are load-bearing
//!
//! 1. `posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC)` — **not `openpty(3)`**, which lives in
//!    `libutil` and Rust's std does not link it. `O_NOCTTY` matters because the supervisor is a
//!    session leader after S15's double-fork, and a session leader that opens a tty without it
//!    acquires a controlling terminal. **`O_CLOEXEC` is load-bearing**: if the child, or any
//!    grandchild it spawns, inherits the master, the supervisor's `read()` never returns EOF after
//!    the child dies and [`PtyHost`]'s reader thread hangs forever.
//! 2. `grantpt`, `unlockpt`, `ptsname` — in that order, before the slave is opened.
//! 3. `ioctl(master, TIOCSWINSZ)` for the initial size, **before the child exists**, so no harness
//!    ever paints at a size marion did not choose. *(§5.3 and this increment's brief both say
//!    "before the child exists", which is achievable, and imply "immediately after `unlockpt`",
//!    which on macOS is not: **measured on Darwin 25.5.0, `TIOCSWINSZ` on a master no slave has
//!    ever been opened on fails with `ENOTTY`** — before `grantpt`, and equally after `unlockpt`.
//!    It succeeds from the first slave open onwards, including after that slave is closed again.
//!    So the size is **held** by [`PtyMaster`] and applied at every [`PtyMaster::open_slave`], which
//!    still puts it strictly before `exec`. The alternative — open and close a throwaway slave
//!    inside `PtyMaster::open` — would leave a window in which the master has no slave at all, and
//!    a read in that window is EOF, which would end the reader thread before the child ever
//!    started.)*
//! 4. stdin, stdout and stderr each get an **independent `dup`** of the slave, so each closes
//!    independently and a harness that closes stderr does not take stdout with it.
//! 5. `pre_exec`: `setsid()` then `ioctl(<the slave fd>, TIOCSCTTY, 0)`. **Not** also
//!    `Command::process_group(0)` — `setsid` already makes the child a session *and* process-group
//!    leader, and the redundant `setpgid` std would insert runs *before* the closure, which is
//!    merely wasteful today and one ordering change away from being wrong.
//!
//!    *(§5.3 and this increment's brief both spell it `ioctl(0, TIOCSCTTY, 0)`, and **fd 0 is the
//!    wrong descriptor for two of the three stdin plans**. A `shared` node — the preset the brief
//!    itself calls out — has a *pipe* on stdin and the pty on stdout/stderr, so `ioctl(0, …)` would
//!    answer `ENOTTY`, `pre_exec` would return an error, and the spawn would fail outright. The
//!    ioctl is therefore issued on whichever descriptor actually is the slave: fd 0 under
//!    [`StdinPlan::TerminalSlave`], fd 1 otherwise.)*
//!
//!    **`TIOCSCTTY` is load-bearing on both topologies, and this doc used to say otherwise.** It
//!    claimed that with the slave on fd 0 macOS makes the pty the controlling terminal on `setsid()`
//!    alone, so the ioctl was redundant there and its deletion unobservable. Measured false — S19,
//!    `tests/fixtures/s19/README.md`, nine cells, Darwin 25.5.0: `setsid()` alone leaves
//!    `tcgetsid(master)` at `ENOTTY` in **every** cell, and only the explicit ioctl claims the
//!    terminal. The probe behind the old claim ran its child through a shell, and macOS's `/bin/sh`
//!    claims the terminal *itself* when it starts as a session leader without one and its stdin is
//!    a tty — so what it measured was `sh`. Every test in `pty/tests.rs` inherits that confound for
//!    the same reason, which is why
//!    [`tests::tiocsctty_and_not_setsid_is_what_claims_the_terminal`] execs `/bin/sleep` directly:
//!    it is the only shell-free topology in the file and the only place the deletion goes red.
//! 6. **The parent closes its own slave copies immediately after `spawn()`.** Otherwise the master
//!    never sees EOF, because the parent is itself a writer.
//! 7. `on_started(pid)` fires between `spawn()` and the first byte, exactly as `duplex.rs` does.
//! 8. **`EIO` on the master is EOF, not an error.** Linux reports the last slave closing that way;
//!    macOS returns 0. A reader treating `EIO` as a fault reports a read failure for every normal
//!    exit.

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use marion_core::contract::AgentId;
use marion_harness::{ControlTransport, PtyWitness};
use marion_proto::{
    Event, Frame, Notification, OpaquePaneBytesV1, PaneFrameKindV1, PaneFrameV1, PaneReadyTokenV1,
};

use crate::serve::{ConnId, Outbound};

mod splice;
/// Production-dark durable binary PTY recording and recovery.
pub mod stream;

// ---------------------------------------------------------------------------------------------
// libc, hand-declared
// ---------------------------------------------------------------------------------------------

unsafe extern "C" {
    fn posix_openpt(flags: c_int) -> c_int;
    fn grantpt(fd: c_int) -> c_int;
    fn unlockpt(fd: c_int) -> c_int;
    fn ptsname(fd: c_int) -> *mut c_char;
    fn setsid() -> c_int;
    fn killpg(pgid: c_int, sig: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// Platform numbers, taken from this machine's headers rather than from memory. The two `ioctl`
/// requests are round-tripped by test (`TIOCGWINSZ` reads back what `TIOCSWINSZ` wrote, and
/// `TIOCSCTTY` is proven by `the_child_is_a_session_leader_with_the_master_as_its_terminal`), so a
/// wrong constant is a red suite rather than a silent no-op — which is what a wrong `ioctl` number
/// usually is.
#[cfg(target_os = "macos")]
mod sys {
    pub const O_NOCTTY: super::c_int = 0x0002_0000;
    pub const O_CLOEXEC: super::c_int = 0x0100_0000;
    pub const TIOCSWINSZ: super::c_ulong = 0x8008_7467;
    pub const TIOCGWINSZ: super::c_ulong = 0x4008_7468;
    pub const TIOCSCTTY: super::c_ulong = 0x2000_7461;
    pub const O_NONBLOCK: super::c_int = 0x0004;
}

#[cfg(target_os = "linux")]
mod sys {
    pub const O_NOCTTY: super::c_int = 0o400;
    pub const O_CLOEXEC: super::c_int = 0o2_000_000;
    pub const TIOCSWINSZ: super::c_ulong = 0x5414;
    pub const TIOCGWINSZ: super::c_ulong = 0x5413;
    pub const TIOCSCTTY: super::c_ulong = 0x540E;
    pub const O_NONBLOCK: super::c_int = 0o4000;
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!(
    "marion's pty host declares its own ioctl numbers and has them only for macOS and Linux. \
     §5.3 defers Windows; a third unix needs its constants measured on that machine and a test \
     run, not copied from a header by eye."
);

const O_RDWR: c_int = 0x0002;
const F_GETFD: c_int = 1;
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const FD_CLOEXEC: c_int = 1;
const SIGWINCH: c_int = 28;
const EIO: i32 = 5;
/// "Inappropriate ioctl for device". See topology point 3: macOS answers this for `TIOCSWINSZ` on a
/// master that has never had a slave, and it is not a failure — it is "not yet".
const ENOTTY: i32 = 25;

/// `struct winsize`. Field order is `ws_row, ws_col, ws_xpixel, ws_ypixel` on both platforms.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RawWinSize {
    row: u16,
    col: u16,
    xpixel: u16,
    ypixel: u16,
}

/// A terminal size. **Columns first**, matching `pty.cast`'s `"COLSxROWS"` and *not* the kernel
/// struct's row-first order — the two disagree, and the only defence against writing one where the
/// other belongs is a type that says which it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
}

impl WinSize {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    /// The `r` record's payload, and the shape the five committed captures use:
    /// `tests/fixtures/s2/*.cast` carry `"100x24"` for a 100-column, 24-row terminal.
    pub fn as_cast(&self) -> String {
        format!("{}x{}", self.cols, self.rows)
    }

    fn raw(&self) -> RawWinSize {
        RawWinSize {
            row: self.rows,
            col: self.cols,
            xpixel: 0,
            ypixel: 0,
        }
    }
}

/// `ptsname(3)` is not reentrant and macOS has no `ptsname_r`. See the module doc for why this is a
/// lock rather than a second platform arm.
static PTSNAME: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------------------------
// The master
// ---------------------------------------------------------------------------------------------

/// An open pty master, and the slave path that goes with it.
///
/// Dropping this closes the master, which hangs up the slave and SIGHUPs the child's foreground
/// process group. That is a real effect, not a resource release, which is why
/// [`PtyHost::shutdown`] is explicit about doing it **last**.
#[derive(Debug)]
pub struct PtyMaster {
    fd: OwnedFd,
    slave: PathBuf,
    /// The size marion has decided on, whether or not the kernel has accepted it yet. See topology
    /// point 3: until a slave exists there is nothing to size, so the value is held here and
    /// re-applied at every [`Self::open_slave`].
    size: Mutex<WinSize>,
}

impl PtyMaster {
    /// Steps 1–3 of the module doc's topology: open, unlock, name, size.
    pub fn open(size: WinSize) -> io::Result<Self> {
        // SAFETY: a plain flags argument; the call allocates a new descriptor or returns -1.
        let fd = unsafe { posix_openpt(O_RDWR | sys::O_NOCTTY | sys::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor this call owns and hands to `OwnedFd` immediately, so
        // every early return below closes it exactly once.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let raw = fd.as_raw_fd();
        // SAFETY: `raw` is open for the whole of both calls; neither takes a pointer.
        if unsafe { grantpt(raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { unlockpt(raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let slave = {
            let _guard = PTSNAME.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: `raw` is open. `ptsname` returns a pointer into a libc-owned static buffer,
            // valid until the next `ptsname` on any thread — which the guard above excludes for
            // marion's own threads — and it is copied out before the guard is released.
            let p = unsafe { ptsname(raw) };
            if p.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: non-null and NUL-terminated by contract; copied, never retained.
            let bytes = unsafe { CStr::from_ptr(p) }.to_bytes().to_vec();
            PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
        };
        let master = Self {
            fd,
            slave,
            size: Mutex::new(size),
        };
        master.set_nonblocking()?;
        master.apply_size(size)?;
        Ok(master)
    }

    /// **The master is non-blocking, and the reader polls it.**
    ///
    /// A blocking `read` cannot be interrupted by a flag, so a host whose child never produces EOF
    /// — a grandchild inherited the slave, or there is no child at all — would hang its reader
    /// thread forever, and `shutdown` would hang joining it. `serve.rs`'s accept loop makes the
    /// same choice for the same reason and states it: a sleep cannot fail in the ways a signal or a
    /// self-connection can. The poll interval is `POLL`, and the cost is one `read` returning
    /// `EAGAIN` every 5 ms on an idle node.
    fn set_nonblocking(&self) -> io::Result<()> {
        let raw = self.fd.as_raw_fd();
        // SAFETY: `fd` is open; `F_GETFL` reads and `F_SETFL` writes only the descriptor's flags.
        let flags = unsafe { fcntl(raw, F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above.
        if unsafe { fcntl(raw, F_SETFL, flags | sys::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn slave_path(&self) -> &Path {
        &self.slave
    }

    pub fn as_raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Is the master close-on-exec? Load-bearing rather than curious — see the module doc, point 1.
    /// Read back from the kernel because `posix_openpt`'s treatment of `O_CLOEXEC` is
    /// implementation-defined (POSIX names only `O_RDWR` and `O_NOCTTY`).
    pub fn is_cloexec(&self) -> bool {
        // SAFETY: `fd` is open; `F_GETFD` takes no further argument and only reads.
        let flags = unsafe { fcntl(self.fd.as_raw_fd(), F_GETFD) };
        flags >= 0 && (flags & FD_CLOEXEC) != 0
    }

    /// Open a fresh slave descriptor, **and apply the held size**.
    ///
    /// `O_NOCTTY` because acquiring the controlling terminal is the child's job, done deliberately
    /// in `pre_exec` after `setsid`, and never the parent's. The size is re-applied here because
    /// this is the first moment the kernel will accept it (topology point 3).
    pub fn open_slave(&self) -> io::Result<OwnedFd> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(sys::O_NOCTTY)
            .open(&self.slave)?;
        let fd = OwnedFd::from(f);
        let size = *self.size.lock().unwrap_or_else(|e| e.into_inner());
        self.apply_size(size)?;
        Ok(fd)
    }

    /// The size marion has decided on. Not a kernel read — see [`Self::size`] for that.
    pub fn intended_size(&self) -> WinSize {
        *self.size.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_size(&self, size: WinSize) -> io::Result<()> {
        *self.size.lock().unwrap_or_else(|e| e.into_inner()) = size;
        self.apply_size(size)
    }

    fn apply_size(&self, size: WinSize) -> io::Result<()> {
        let ws = size.raw();
        // SAFETY: `fd` is open and `&ws` is a live, correctly-shaped `struct winsize` the kernel
        // only reads for `TIOCSWINSZ`.
        let rc = unsafe { ioctl(self.fd.as_raw_fd(), sys::TIOCSWINSZ, &ws) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            // **Not yet, rather than no** — topology point 3. Reporting this as a failure would
            // make `PtyMaster::open` fall over on macOS every time.
            if e.raw_os_error() == Some(ENOTTY) {
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }

    /// Read the size back from the kernel. Only the tests need this; it is what turns
    /// `TIOCSWINSZ`'s number from an assumption into a round trip.
    pub fn size(&self) -> io::Result<WinSize> {
        let mut ws = RawWinSize::default();
        // SAFETY: `fd` is open and `&mut ws` is a live, correctly-shaped out-parameter.
        let rc = unsafe { ioctl(self.fd.as_raw_fd(), sys::TIOCGWINSZ, &mut ws) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(WinSize::new(ws.col, ws.row))
    }

    /// One `read`, with **`EIO` reported as end of stream**.
    ///
    /// See the module doc, point 8: Linux signals "the last slave closed" with `EIO` and macOS with
    /// a zero-length read. Treating `EIO` as a fault means every ordinary child exit is logged as a
    /// read failure, and a host that stops on the first "error" stops on every clean exit.
    /// `EINTR` is retried, because a signal arriving mid-read is not information. `EAGAIN` is
    /// surfaced as [`io::ErrorKind::WouldBlock`] — the master is non-blocking, see
    /// [`Self::set_nonblocking`] — and is the reader loop's cue to sleep, never to stop.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: `fd` is open and `buf` is a live slice whose length is passed exactly.
            let n = unsafe { read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(EIO) => return Ok(0),
                _ if e.kind() == io::ErrorKind::Interrupted => continue,
                _ => return Err(e),
            }
        }
    }

    /// Write keystrokes into the master. Partial writes are looped; `EINTR` is retried; `EAGAIN`
    /// waits, because the master is non-blocking and a full tty input buffer is backpressure from a
    /// harness that has not read yet, not a failure.
    pub fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            // SAFETY: `fd` is open and `buf` is a live slice whose length is passed exactly.
            let n = unsafe { write(self.fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
            if n > 0 {
                buf = &buf[n as usize..];
                continue;
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL);
                    continue;
                }
                _ => return Err(e),
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// S11 MUST #2, in the type system: which fd is stdin
// ---------------------------------------------------------------------------------------------

/// What a node's **stdin** is, decided from the control axis alone.
///
/// See [`stdin_plan`] for why this is a separate decision from [`PtyWitness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinPlan {
    /// A pipe marion writes typed frames into. §6.4: `claude -p` **refuses** a pty stdin, exiting 1
    /// with *"Input must be provided either through stdin or as a prompt argument"* — and S11
    /// isolated the trigger to `isatty(0)`, so the refusal follows the fd, not the harness.
    Piped,
    /// The pty slave. The node is driven by keystrokes and there is nothing else to type into.
    TerminalSlave,
    /// `/dev/null`. The prompt rode argv; a node given a readable stdin it was never told about is
    /// a node that can block on it.
    Null,
}

/// **A total match, with no `_` arm — and that is the point.**
///
/// [`PtyWitness`] alone is not enough to keep a headless node off the pty, and the `shared` preset
/// is the counterexample: it is `Typed(StreamJson)` **and** `NativePty`, so it yields a witness
/// *and* must still speak stream-json over pipes. A witness-gated `spawn_pty` would happily hand it
/// the slave. This decides stdin on the other axis, and it is exhaustive so that a fourth
/// `ControlTransport` **fails to compile** until somebody decides which of the three it is. A `_`
/// arm here would pick one silently, and the wrong pick is a node that exits 1 on boot with an
/// error message about a prompt.
pub fn stdin_plan(control: ControlTransport) -> StdinPlan {
    match control {
        ControlTransport::Typed(_) => StdinPlan::Piped,
        ControlTransport::TerminalInput => StdinPlan::TerminalSlave,
        ControlTransport::LaunchOnly => StdinPlan::Null,
    }
}

// ---------------------------------------------------------------------------------------------
// Spawning under the pty
// ---------------------------------------------------------------------------------------------

/// A child running under a pty, with the handles the host needs to end it truthfully.
#[derive(Debug)]
pub struct PtyChild {
    child: Child,
    pid: i32,
    /// `setsid` makes the child its own session and process-group leader, so `pgid == pid`. Kept as
    /// its own field because that is a fact about how it was spawned, not an arithmetic identity a
    /// later reader should have to re-derive.
    pgid: i32,
    /// The three descriptors the parent handed over and then closed. Kept only so
    /// `the_parent_closes_its_own_slave_copies` can check the close happened, which is otherwise
    /// observable only as a `read` that never returns.
    handed: [RawFd; 3],
    /// The write half of a [`StdinPlan::Piped`] stdin. `None` for the other two plans — a node
    /// typed into through the slave, and one with nothing to say to it.
    stdin: Option<std::process::ChildStdin>,
    /// **Set the moment this process is reaped, and never cleared.**
    ///
    /// It is what makes [`Self::kill_and_reap`] and [`Drop`] idempotent, and idempotence here is
    /// not tidiness: after a `wait` the pid is free for the kernel to reissue, so a second
    /// `kill_process_tree(self.pid)` would signal whatever is wearing the number now. A `bool`
    /// would do for that, but keeping the status means a caller that killed through `Drop`'s path
    /// and one that killed through `shutdown` read the same answer.
    reaped: Option<std::process::ExitStatus>,
}

impl PtyChild {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// The three descriptors the parent handed the child and then closed. `-1` where the child was
    /// not given the slave for that stream. See [`PtyChild::handed`].
    pub fn handed_fds(&self) -> [RawFd; 3] {
        self.handed
    }

    /// The typed control plane's pipe, for a `shared` node. Taken once; the second call is `None`.
    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.stdin.take()
    }

    pub fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        if let Some(status) = self.reaped {
            return Ok(status);
        }
        let status = self.child.wait()?;
        self.reaped = Some(status);
        Ok(status)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        if let Some(status) = self.reaped {
            return Ok(Some(status));
        }
        let status = self.child.try_wait()?;
        if let Some(s) = status {
            self.reaped = Some(s);
        }
        Ok(status)
    }

    /// Observe that this child exited without consuming its wait status.
    ///
    /// The waitable zombie pins the pid and process-group identity until [`Self::kill_and_reap`]
    /// has swept descendants. A `Child::try_wait` here would reap first and make the later group
    /// signal vulnerable to pid reuse.
    pub fn poll_exited_unreaped(&self) -> io::Result<bool> {
        if self.reaped.is_some() {
            return Ok(true);
        }
        let pid = rustix::process::Pid::from_raw(self.pid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero child pid"))?;
        rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
        .map_err(Into::into)
    }

    /// **Kill this node's whole tree and reap it. Idempotent, and callable from an unwind.**
    ///
    /// The one place the two owners of a pty child agree: [`PtyHost::shutdown`] calls it to get the
    /// status it records, [`Drop`] calls it to make sure somebody did. A second call after the
    /// process has been reaped does nothing at all — see [`Self::reaped`], because the pid is the
    /// kernel's to reissue the instant the first `wait` returns.
    pub fn kill_and_reap(&mut self) -> io::Result<std::process::ExitStatus> {
        if let Some(status) = self.reaped {
            return Ok(status);
        }
        // The group, not the pid: `setsid` made this child a group leader and a harness starts
        // tool-call descendants of its own, which reparent to pid 1 the moment the leader dies
        // (S7, §11 item 18) and can then never be found by an ancestry walk.
        crate::run::kill_process_tree(self.pid);
        self.wait()
    }
}

impl Drop for PtyChild {
    /// **The window between `command.spawn()` returning and somebody taking responsibility.**
    ///
    /// [`PtyHost`] has had a `Drop` net of its own for a while, but it only covers a child that has
    /// already been [`PtyHost::adopt`]ed, and adoption is at best several statements after the
    /// spawn: `on_started` runs first, then [`spawn_pty`] returns, then the caller does whatever it
    /// does before calling `adopt`. An unwind anywhere in there — a panicking `on_started` hook, a
    /// panicking or early-returning caller — used to drop a bare `std::process::Child`, and
    /// `std::process::Child`'s own `Drop` *neither kills nor reaps*. The handle is gone, `PtyHost`
    /// never got it, and what is left is a live process on a pty nothing is reading: §11 item 30's
    /// untracked live process, arrived at by a panic instead of by a crash.
    ///
    /// So the responsibility is the value's, from the instant the value exists. `spawn_pty` builds
    /// this struct **before** it calls `on_started` for exactly that reason, and every path out of
    /// the window — return, `?`, panic — goes through here.
    ///
    /// Errors are dropped because a `Drop` has nowhere to put them and panicking during an unwind
    /// aborts the process. The kill is what matters and it is unconditional.
    fn drop(&mut self) {
        let _ = self.kill_and_reap();
    }
}

/// Start `command` with the pty slave as all three of its standard streams.
///
/// **`witness` is the whole signature.** It is unforgeable outside `marion-harness` and comes only
/// from [`marion_harness::ExecutionSurfaces::display_plane`], so a node whose surfaces declare no
/// display plane cannot reach this function — §5.1's hole, where an `Invocation` alone was enough
/// to call the pty launcher, is closed by the type rather than by a check somebody must remember to
/// write. It is deliberately unused in the body: a capability's job is to have been required.
///
/// `on_started` fires between `spawn()` and the first byte, for `duplex.rs`'s reason — that is the
/// first instant a durable record can name a process truthfully, and this session's `procid`
/// start-identity read is only sound while marion still holds the `Child`.
pub fn spawn_pty(
    witness: PtyWitness,
    command: &mut Command,
    master: &PtyMaster,
    stdin: StdinPlan,
    on_started: Option<&dyn Fn(i32)>,
) -> io::Result<PtyChild> {
    let _ = witness;

    // Three **independent** dups, so each stream closes independently (topology point 4). `dup` and
    // not three `open`s: the brief's topology, and it keeps all three on one open file description
    // so the termios state a harness sets through one is the state it reads through another.
    let out = master.open_slave()?;
    let err = out.try_clone()?;
    let inp = match stdin {
        StdinPlan::TerminalSlave => Some(out.try_clone()?),
        StdinPlan::Piped | StdinPlan::Null => None,
    };
    let handed = [
        inp.as_ref().map_or(-1, AsRawFd::as_raw_fd),
        out.as_raw_fd(),
        err.as_raw_fd(),
    ];
    // Which descriptor the child will find the slave on. See the module doc, topology point 5:
    // `ioctl(0, …)` is right only when stdin *is* the slave, and answers `ENOTTY` for a `shared`
    // node, whose stdin is a pipe.
    let ctty_fd: c_int = match stdin {
        StdinPlan::TerminalSlave => 0,
        StdinPlan::Piped | StdinPlan::Null => 1,
    };

    command.stdout(Stdio::from(out)).stderr(Stdio::from(err));
    match (stdin, inp) {
        (StdinPlan::TerminalSlave, Some(fd)) => {
            command.stdin(Stdio::from(fd));
        }
        (StdinPlan::Piped, _) => {
            command.stdin(Stdio::piped());
        }
        _ => {
            command.stdin(Stdio::null());
        }
    }

    // SAFETY: async-signal-safe body only — two syscalls and no allocation, no locking and no
    // Rust-side global state. std has already dup2'd the stdio and has not yet `exec`d, so fd 0 is
    // the slave. `setsid` makes the child a session leader with no controlling terminal, which is
    // the precondition `TIOCSCTTY` needs; it also subsumes `Command::process_group(0)`, so that is
    // deliberately not set as well.
    unsafe {
        command.pre_exec(move || {
            if setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if ioctl(ctty_fd, sys::TIOCSCTTY, 0 as c_int) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = crate::spawn_receive_gate::SPAWN_RECEIVE_GATE.spawn(command)?;
    let pid = child.id() as i32;
    // The write half of a `Piped` stdin, before the reset below can touch anything. `None` for the
    // other two plans, which is the honest answer: there is nothing to write into.
    let stdin_pipe = child.stdin.take();

    // **Topology point 6, and it must be here.** `Stdio::from(OwnedFd)` parks the descriptor on the
    // `Command`, which outlives `spawn`, so without this the parent stays a writer on the slave and
    // the master never reaches EOF — the reader thread would then block forever after the child
    // died. Replacing the three slots drops the `Stdio`s, which closes them.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // **Owned before it is announced.** The `Child` goes into a `PtyChild` first, so that from here
    // on every exit from this function — including an unwind out of the hook below — runs
    // `PtyChild::drop` and kills and reaps the process. Announcing first and wrapping afterwards is
    // the ordering this had, and a panicking `on_started` dropped a bare `std::process::Child`,
    // which neither kills nor reaps: the pid was already announced, the handle was gone, and
    // nothing could ever wait on it again. See `PtyChild::drop`.
    let child = PtyChild {
        child,
        pid,
        pgid: pid,
        handed,
        stdin: stdin_pipe,
        reaped: None,
    };

    // Topology point 7 — before one byte, exactly as `duplex::run_duplex` does it.
    if let Some(started) = on_started {
        started(pid);
    }

    Ok(child)
}

// ---------------------------------------------------------------------------------------------
// pty.cast
// ---------------------------------------------------------------------------------------------

/// asciicast v3, at `AgentDir::pty_cast()`.
///
/// The shape is read off the five committed captures (`tests/fixtures/s2/*.cast`) rather than from
/// memory: a header object, then one JSON array per line carrying the **relative** interval since
/// the previous record, a one-letter code, and a string. `o` output, `i` input, `r` resize as
/// `"COLSxROWS"`, `x` exit.
///
/// **`i` and `r` are not optional.** C1's mouse-through is invisible in the output stream — a
/// harness that received a mouse report and did nothing leaves no `o` trace at all — and C2's
/// `CSI 3J` finding cannot be re-checked after the fact without knowing what size the terminal was
/// when each repaint happened.
///
/// **The origin is borrowed, never minted.** See [`CastWriter::create`].
pub struct CastWriter {
    file: File,
    origin: Instant,
    prev_ns: u128,
}

impl CastWriter {
    /// Create the file and write its header.
    ///
    /// `origin` **must** be [`crate::events::EventWriter::origin`] for the same node.
    /// `EventWriter.start` is set per node at `open`, and §4.2 gives `mono_ns` its job — *"to align
    /// the two"*. A cast that called its own `Instant::now()` would be offset from `events.jsonl`
    /// by the node's whole setup (directory creation, the journal write, the ready gate), so a
    /// reader lining a frame up against a tool call would be silently wrong by that much, with
    /// nothing on either file to say by how much. `mono_ns_and_the_cast_share_one_epoch` is the
    /// guard.
    pub fn create(path: &Path, size: WinSize, term: &str, origin: Instant) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = File::create(path)?;
        let header = serde_json::json!({
            "version": 3,
            "term": {"cols": size.cols, "rows": size.rows, "type": term},
            "timestamp": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "env": {"TERM": term},
        });
        writeln!(file, "{header}")?;
        file.sync_data()?;
        Ok(Self {
            file,
            origin,
            prev_ns: 0,
        })
    }

    fn record(&mut self, code: &str, data: &str) -> io::Result<()> {
        let now_ns = self.origin.elapsed().as_nanos();
        // Saturating rather than wrapping: two records written from two threads can observe
        // `elapsed()` out of order by a few nanoseconds, and a negative interval is unreplayable
        // where a zero one is merely uninformative.
        let delta_ns = now_ns.saturating_sub(self.prev_ns);
        self.prev_ns = now_ns;
        // Six decimal places, matching `tests/fixtures/s2/extract.py`'s `round(ts - prev, 6)`.
        let interval = (delta_ns as f64 / 1e9 * 1e6).round() / 1e6;
        let line = serde_json::to_string(&(interval, code, data))?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        Ok(())
    }

    pub fn output(&mut self, text: &str) -> io::Result<()> {
        self.record("o", text)
    }

    pub fn input(&mut self, text: &str) -> io::Result<()> {
        self.record("i", text)
    }

    pub fn resize(&mut self, size: WinSize) -> io::Result<()> {
        self.record("r", &size.as_cast())?;
        // **Flushed before the ioctl can run.** `resize_with_hook` writes this record first
        // precisely so that no output produced by the new size can precede it, and a record sitting
        // in a `File`'s buffer would give that ordering back. (`File` is unbuffered in std today;
        // the `sync_data` is the durable half, which a crash between the record and the ioctl needs
        // — see `PtyHost::resize`.)
        self.file.sync_data()
    }

    pub fn exit(&mut self, status: &str) -> io::Result<()> {
        self.record("x", status)?;
        self.file.sync_data()
    }
}

// ---------------------------------------------------------------------------------------------
// A read() is not a character
// ---------------------------------------------------------------------------------------------

/// Decodes a pty byte stream to text, **carrying an incomplete trailing UTF-8 sequence across
/// reads**.
///
/// This is S11's *"a `read()` is not a frame"* MUST in a third guise, and the corpus proves the
/// failure rather than predicting it: all five committed captures are valid UTF-8 end to end, yet
/// the `.cast` files contain **23 U+FFFD in 9 regions**, because `tests/fixtures/s2/extract.py`
/// called `decode(..., "replace")` on each pty read chunk independently and the box-drawing glyphs
/// that straddle a chunk boundary were split. `NOTES.txt` records it as a capture-host defect. A
/// `from_utf8_lossy` per read here would reproduce it exactly, in production, forever.
///
/// The carry is bounded by three bytes — the longest incomplete prefix of a UTF-8 sequence — so
/// this cannot grow. Genuinely invalid bytes (which a real terminal stream can carry, since a
/// harness may emit latin-1 from a subprocess) are replaced once, in place, and do not stall the
/// carry.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    carry: Vec<u8>,
}

impl Utf8Stream {
    /// Longest decodable prefix of `carry ++ chunk`; the rest is kept for the next call.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        self.carry.extend_from_slice(chunk);
        let mut out = String::with_capacity(self.carry.len());
        loop {
            match std::str::from_utf8(&self.carry) {
                Ok(s) => {
                    out.push_str(s);
                    self.carry.clear();
                    return out;
                }
                Err(e) => {
                    let good = e.valid_up_to();
                    // Valid by construction: `valid_up_to` is exactly the length that parses.
                    out.push_str(std::str::from_utf8(&self.carry[..good]).unwrap_or_default());
                    match e.error_len() {
                        // Truncated, not invalid: keep it and wait for the rest of the sequence.
                        None => {
                            self.carry.drain(..good);
                            debug_assert!(self.carry.len() <= 3, "a UTF-8 prefix is at most 3 B");
                            return out;
                        }
                        Some(bad) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            self.carry.drain(..good + bad);
                        }
                    }
                }
            }
        }
    }

    /// At end of stream a truncated sequence will never complete, so it becomes one U+FFFD.
    /// Reporting it rather than dropping it is the difference between *"the child died mid-glyph"*
    /// and *"nothing was there"*.
    pub fn finish(&mut self) -> String {
        if self.carry.is_empty() {
            return String::new();
        }
        self.carry.clear();
        char::REPLACEMENT_CHARACTER.to_string()
    }

    pub fn pending(&self) -> usize {
        self.carry.len()
    }
}

// ---------------------------------------------------------------------------------------------
// Probes: counted, never answered
// ---------------------------------------------------------------------------------------------

/// The four probe families §5.3's table measured, as literals.
const PROBES: [&[u8]; 5] = [
    b"\x1b[c",    // DA1
    b"\x1b[>0q",  // XTVERSION
    b"\x1b[6n",   // CPR/DSR
    b"\x1b]10;?", // OSC 10, foreground colour
    b"\x1b]11;?", // OSC 11, background colour
];

/// The longest probe literal, and therefore how much of the previous chunk must be kept so a probe
/// split across two `read`s is still seen. One more instance of the same MUST.
const PROBE_TAIL: usize = 5;

/// **Counts probes. Does not answer them.** §9's M3 criterion de-scopes answering explicitly —
/// *"since our fixtures show both harnesses proceeding unanswered, this is not the pass
/// criterion"* — and `tests/fixtures/s2/ptyhost.py`, which answers nothing, drove complete sessions
/// on both harnesses. What is kept is the diagnostic: if §11 item 9's `ESC[6n` stall ever bites, an
/// operator sees *"3 unanswered probes"* beside a hung node instead of an unexplained hang.
#[derive(Debug, Default)]
struct ProbeScan {
    tail: Vec<u8>,
}

impl ProbeScan {
    fn count(&mut self, chunk: &[u8]) -> u64 {
        let mut window = std::mem::take(&mut self.tail);
        let seam = window.len();
        window.extend_from_slice(chunk);
        let mut n = 0u64;
        for probe in PROBES {
            let mut from = 0;
            while let Some(at) = find(&window[from..], probe) {
                let start = from + at;
                // A hit entirely inside the carried tail was counted last time.
                if start + probe.len() > seam {
                    n += 1;
                }
                from = start + 1;
            }
        }
        let keep = window.len().min(PROBE_TAIL - 1);
        self.tail = window[window.len() - keep..].to_vec();
        n
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

// ---------------------------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------------------------

/// How many bytes one `read` from the master may take. `tests/fixtures/s2/ptyhost.py` used 65536
/// and the captures were made with it; S11 measured a pty capping a read at 1024 anyway, so this is
/// a ceiling rather than a size.
const READ_CHUNK: usize = 65536;

/// How long the reader sleeps when the master has nothing. `tests/fixtures/s2/ptyhost.py` used a
/// 50 ms `select` timeout; `serve.rs`'s accept loop uses 5 ms. This takes the smaller: a pty is
/// interactive and a viewer feels 50 ms of added latency on every keystroke echo.
const POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// Maximum time shutdown waits for a cast-recorded input's master-write outcome.
///
/// A write to a terminal whose peer stopped reading can remain blocked independently of process
/// teardown. Completion waits for the ordinary post-kill result, then fails replay closed rather
/// than letting one client hold shutdown forever.
const CONTROL_DELIVERY_GRACE: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------------------------
// One writer, and the type that says so
// ---------------------------------------------------------------------------------------------

/// Who currently holds a node's write half, if anyone.
///
/// `Mutex<Option<ConnId>>` rather than an `AtomicBool` because the interesting answer is not
/// *whether* the half is taken but **by whom**: a second client must be refused with a sentence
/// naming the connection that has it, not with a bare failure.
type WriterSlot = Mutex<Option<ConnId>>;

/// **Proof that its holder is this node's one writer.** Required by [`PtyHost::write_input`].
///
/// # Why a token and not a check
///
/// Reading a node is fan-out: any number of clients may hold a [`Outbound`] listener, because a
/// byte delivered twice costs nothing. **Writing is not.** Two clients typing into one pty
/// interleave at whatever granularity their reads happen to have, so `ls -la` and `git status`
/// become `lgits -lta`tus — and neither operator sees anything wrong, because the pty echoes the
/// mess back to both of them identically. There is no error, no log line and no way to tell it
/// from a harness misbehaving.
///
/// So the write half is **leased**, and the lease is an unforgeable value rather than a flag
/// somebody must remember to check. This is the same device `PtyWitness` uses one layer down: a
/// `bool` proves nothing at a call site, because a caller who forgot to test it holds the same
/// `true` as one who did. A caller who has a `WriteLease` has necessarily been given one.
///
/// The lease is released on [`Drop`], so a client that disconnects, panics or is torn down hands
/// the write half back without anybody writing cleanup for it — which matters because §7.3.1's
/// invariant is precisely about what a *crashed* client leaves behind. A node whose one writer
/// died and never released the lease would be permanently read-only.
#[derive(Debug)]
pub struct WriteLease {
    conn: ConnId,
    /// The slot to clear on drop. Shared with the host, and used by [`PtyHost::write_input`] to
    /// tell a lease for *this* node from a lease for some other one.
    slot: Arc<WriterSlot>,
}

impl WriteLease {
    /// The connection this lease belongs to.
    pub fn conn(&self) -> ConnId {
        self.conn
    }
}

impl Drop for WriteLease {
    fn drop(&mut self) {
        let mut held = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        // Only clear the slot if it is still *ours*. It cannot be anyone else's while this lease
        // exists, but taking the defensive branch costs one comparison and makes the invariant
        // local rather than a thing a reader has to prove from the other methods.
        if *held == Some(self.conn) {
            *held = None;
        }
    }
}

/// Why a client did not get the write half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterBusy {
    /// Another connection holds it. **Named**, so the refusal can be a sentence: a client told only
    /// "busy" cannot tell a stale lease from a colleague in the same node.
    HeldBy(ConnId),
}

impl std::fmt::Display for WriterBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HeldBy(c) => write!(
                f,
                "this node's input is held by connection {}; attach is read-only until it detaches",
                c.0
            ),
        }
    }
}

impl std::error::Error for WriterBusy {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlPhase {
    Live,
    Closing,
    Ended,
}

#[derive(Debug)]
struct ControlState {
    phase: ControlPhase,
    admitted: usize,
    next_input_delivery: u64,
    input_deliveries: BTreeMap<u64, Option<InputFailureTarget>>,
    input_delivery_failure: Option<String>,
}

#[derive(Debug, Clone)]
struct InputFailureTarget {
    out: Outbound,
    agent_id: String,
}

impl InputFailureTarget {
    fn fail(&self, error: String) {
        self.out.fail(crate::serve::Departure::PaneInputFailed {
            agent_id: self.agent_id.clone(),
            error,
        });
    }
}

#[derive(Debug)]
struct ControlGate {
    state: Mutex<ControlState>,
    drained: Condvar,
}

struct LegacyListeners {
    state: Mutex<LegacyListenerState>,
    drained: Condvar,
    #[cfg(test)]
    delivery_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

struct LegacyListenerState {
    open: bool,
    delivering: usize,
    listeners: Vec<Outbound>,
}

impl LegacyListeners {
    fn new() -> Self {
        Self {
            state: Mutex::new(LegacyListenerState {
                open: true,
                delivering: 0,
                listeners: Vec::new(),
            }),
            drained: Condvar::new(),
            #[cfg(test)]
            delivery_hook: Mutex::new(None),
        }
    }

    fn add(&self, out: Outbound) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.open {
            state.listeners.push(out);
        }
    }

    fn remove(&self, conn: ConnId) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .listeners
            .retain(|out| out.conn() != conn);
    }

    fn begin_delivery(&self) -> Option<Vec<Outbound>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open {
            return None;
        }
        state.delivering += 1;
        Some(state.listeners.clone())
    }

    fn finish_delivery(&self, alive: &[ConnId]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.listeners.retain(|out| alive.contains(&out.conn()));
        state.delivering -= 1;
        if state.delivering == 0 {
            self.drained.notify_all();
        }
    }

    fn close_and_clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.open = false;
        while state.delivering != 0 {
            state = self.drained.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.listeners.clear();
    }

    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .listeners
            .len()
    }

    #[cfg(test)]
    fn set_delivery_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self.delivery_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    fn observe_delivery(&self) {
        #[cfg(test)]
        if let Some(hook) = self
            .delivery_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            hook();
        }
    }
}

impl ControlGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ControlState {
                phase: ControlPhase::Live,
                admitted: 0,
                next_input_delivery: 1,
                input_deliveries: BTreeMap::new(),
                input_delivery_failure: None,
            }),
            drained: Condvar::new(),
        })
    }

    fn admit_write(
        self: &Arc<Self>,
        target: Option<InputFailureTarget>,
    ) -> io::Result<(ControlPermit, InputDelivery)> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.phase {
            ControlPhase::Live => {
                state.admitted += 1;
                let delivery_id = state.next_input_delivery;
                state.next_input_delivery = state
                    .next_input_delivery
                    .checked_add(1)
                    .expect("pty input delivery ids do not wrap");
                state.input_deliveries.insert(delivery_id, target.clone());
                Ok((
                    ControlPermit {
                        gate: Arc::clone(self),
                    },
                    InputDelivery {
                        gate: Arc::clone(self),
                        delivery_id,
                        target,
                        finished: false,
                    },
                ))
            }
            ControlPhase::Closing => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty control arrived after closing began",
            )),
            ControlPhase::Ended => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty input arrived after shutdown",
            )),
        }
    }

    fn admit_resize(self: &Arc<Self>) -> io::Result<Option<ControlPermit>> {
        self.admit(true)
    }

    fn admit(self: &Arc<Self>, ended_is_noop: bool) -> io::Result<Option<ControlPermit>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.phase {
            ControlPhase::Live => {
                state.admitted += 1;
                Ok(Some(ControlPermit {
                    gate: Arc::clone(self),
                }))
            }
            ControlPhase::Closing => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty control arrived after closing began",
            )),
            ControlPhase::Ended if ended_is_noop => Ok(None),
            ControlPhase::Ended => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty input arrived after shutdown",
            )),
        }
    }

    fn seal(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.phase == ControlPhase::Live {
            state.phase = ControlPhase::Closing;
        }
    }

    fn wait_drained(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.admitted != 0 {
            state = self.drained.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn wait_input_deliveries(&self, deadline: Instant) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while !state.input_deliveries.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, timed_out) = self
                .drained
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
            if timed_out.timed_out() && !state.input_deliveries.is_empty() {
                break;
            }
        }
        if state.input_deliveries.is_empty() {
            return state.input_delivery_failure.clone();
        }
        let failure = format!(
            "{} pty input delivery attempt(s) remained unresolved at shutdown",
            state.input_deliveries.len()
        );
        if state.input_delivery_failure.is_none() {
            state.input_delivery_failure = Some(failure.clone());
        }
        let targets = state
            .input_deliveries
            .values()
            .filter_map(Clone::clone)
            .collect::<Vec<_>>();
        let outcome = state.input_delivery_failure.clone();
        drop(state);
        // Socket shutdown is external I/O. It runs after the control lock is released and before
        // shutdown is allowed to retain terminal End.
        for target in targets {
            target.fail(failure.clone());
        }
        outcome
    }

    fn finish_input_delivery(&self, delivery_id: u64, failure: Option<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.input_delivery_failure.is_none() {
            state.input_delivery_failure = failure;
        }
        state
            .input_deliveries
            .remove(&delivery_id)
            .expect("an input delivery is resolved exactly once");
        self.drained.notify_all();
    }

    fn mark_ended(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).phase = ControlPhase::Ended;
        self.drained.notify_all();
    }
}

struct ControlPermit {
    gate: Arc<ControlGate>,
}

impl Drop for ControlPermit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        state.admitted = state
            .admitted
            .checked_sub(1)
            .expect("a control permit is released exactly once");
        if state.admitted == 0 {
            self.gate.drained.notify_all();
        }
    }
}

struct InputDelivery {
    gate: Arc<ControlGate>,
    delivery_id: u64,
    target: Option<InputFailureTarget>,
    finished: bool,
}

#[derive(Debug)]
struct ReaderCompletion {
    finished: Mutex<bool>,
    ready: Condvar,
}

impl ReaderCompletion {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            finished: Mutex::new(false),
            ready: Condvar::new(),
        })
    }

    fn finish(&self) {
        *self.finished.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_all();
    }

    fn wait_until(&self, deadline: Instant) -> bool {
        let mut finished = self.finished.lock().unwrap_or_else(|e| e.into_inner());
        while !*finished {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, timeout) = self
                .ready
                .wait_timeout(finished, deadline.saturating_duration_since(now))
                .unwrap_or_else(|e| e.into_inner());
            finished = next;
            if timeout.timed_out() && !*finished {
                return false;
            }
        }
        true
    }
}

struct ReaderFinished(Arc<ReaderCompletion>);

impl Drop for ReaderFinished {
    fn drop(&mut self) {
        self.0.finish();
    }
}

impl InputDelivery {
    fn finish(mut self, failure: Option<String>) {
        if let (Some(target), Some(error)) = (&self.target, &failure) {
            target.fail(error.clone());
        }
        self.gate.finish_input_delivery(self.delivery_id, failure);
        self.finished = true;
    }
}

impl Drop for InputDelivery {
    fn drop(&mut self) {
        if !self.finished {
            let failure = "pty input delivery ended without a master-write outcome".to_string();
            if let Some(target) = &self.target {
                target.fail(failure.clone());
            }
            self.gate
                .finish_input_delivery(self.delivery_id, Some(failure));
        }
    }
}

pub(crate) struct OpaqueInputAdmission {
    permit: ControlPermit,
    delivery: InputDelivery,
}

/// Shared between the host and its reader thread.
struct Shared {
    self_weak: Weak<Shared>,
    agent_id: AgentId,
    /// The compatibility cast and authoritative byte stream share one per-host lock. This is the
    /// linearization point for every durable PTY fact; no global registry or control-gate lock is
    /// ever held while its `sync_data` runs.
    recorders: Mutex<Recorders>,
    /// Clients receiving [`Event::NodePty`]. **Listeners, not owners** — see the module doc.
    listeners: LegacyListeners,
    /// The one connection allowed to type. See [`WriteLease`].
    writer: Arc<WriterSlot>,
    seq: AtomicU64,
    probes: AtomicU64,
    bytes: Arc<AtomicU64>,
    splice: splice::PtySplice,
    splice_error: Mutex<Option<String>>,
    splice_disabled: AtomicBool,
    pane_streams: Mutex<PaneStreams>,
    pane_clock: Mutex<Arc<dyn Fn() -> Instant + Send + Sync>>,
    #[cfg(test)]
    pane_transition_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pane_delivery_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pane_splice_outcome_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pane_pre_pull_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    pane_guarded_pull_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    reader_would_block_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Resizes handed to the reader thread, and the answer coming back. See [`PtyHost::resize`].
    resize: Mutex<ResizeQueue>,
    resize_done: Condvar,
    /// See [`PtyHost::set_resize_hook`]. On `Shared` rather than on `PtyHost` because the point it
    /// has to run at is inside the reader thread.
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    resize_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    resize_ioctl_failure: Mutex<Option<String>>,
    #[cfg(test)]
    resize_cast_failure: Mutex<Option<String>>,
}

struct Recorders {
    cast: CastWriter,
    durable: DurableRecorder,
    cast_failure: Option<String>,
    #[cfg(test)]
    next_durable_failure: Option<String>,
}

struct DurableRecorder {
    writer: Option<stream::SessionWriter>,
    failure: Option<String>,
    sealed: Option<stream::TerminalOutcome>,
    output_complete: bool,
}

impl DurableRecorder {
    fn available(writer: stream::SessionWriter) -> Self {
        Self {
            writer: Some(writer),
            failure: None,
            sealed: None,
            output_complete: true,
        }
    }

    fn unavailable(error: stream::StreamError) -> Self {
        Self {
            writer: None,
            failure: Some(format!("authoritative PTY stream is unavailable: {error}")),
            sealed: None,
            output_complete: false,
        }
    }

    fn append_output(&mut self, bytes: &[u8]) -> Result<(), String> {
        if !self.output_complete {
            return Ok(());
        }
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.sealed.is_some() {
            return Err("authoritative PTY stream is already sealed".to_string());
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| "authoritative PTY stream has no active writer".to_string())?;
        match writer.append_output(bytes) {
            Ok(()) => Ok(()),
            Err(stream::StreamError::OutputBudgetExhausted { .. }) => {
                // Output truncation is a typed, non-fatal boundary. Keep the writer for ordered
                // input evidence, resize facts, and End, but never claim the display is complete.
                self.output_complete = false;
                Ok(())
            }
            Err(error) => Err(self.fail(error.to_string())),
        }
    }

    fn append_resize(&mut self, size: WinSize) -> Result<(), String> {
        self.append_with(|writer| writer.append_resize(size))
    }

    fn append_input_evidence(&mut self, byte_len: usize) -> Result<(), String> {
        self.append_with(|writer| writer.append_input_evidence(byte_len))
    }

    fn append_with(
        &mut self,
        append: impl FnOnce(&mut stream::SessionWriter) -> Result<(), stream::StreamError>,
    ) -> Result<(), String> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.sealed.is_some() {
            return Err("authoritative PTY stream is already sealed".to_string());
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| "authoritative PTY stream has no active writer".to_string())?;
        if let Err(error) = append(writer) {
            let error = format!("authoritative PTY stream durability failed: {error}");
            self.failure = Some(error.clone());
            self.writer.take();
            return Err(error);
        }
        Ok(())
    }

    fn fail(&mut self, error: String) -> String {
        let error = format!("authoritative PTY stream durability failed: {error}");
        if self.failure.is_none() {
            self.failure = Some(error);
            self.writer.take();
        }
        self.failure
            .clone()
            .expect("the failure was just installed")
    }

    fn seal(&mut self, mut outcome: stream::TerminalOutcome) -> Result<(), String> {
        outcome = self.qualify_outcome(outcome);
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if let Some(sealed) = self.sealed {
            return if sealed == outcome {
                Ok(())
            } else {
                Err("authoritative PTY stream was already sealed with another outcome".into())
            };
        }
        let writer = self
            .writer
            .take()
            .ok_or_else(|| "authoritative PTY stream has no active writer to seal".to_string())?;
        match writer.seal(outcome) {
            Ok(()) => {
                self.sealed = Some(outcome);
                Ok(())
            }
            Err(error) => {
                let error = format!("authoritative PTY stream seal failed: {error}");
                self.failure = Some(error.clone());
                Err(error)
            }
        }
    }

    fn qualify_outcome(&self, mut outcome: stream::TerminalOutcome) -> stream::TerminalOutcome {
        outcome.stream_complete &= self.output_complete;
        outcome
    }

    #[cfg(test)]
    fn limit_output_for_test(&mut self, limit: usize) {
        if let Some(writer) = self.writer.as_mut() {
            writer.limit_output_for_test(limit);
        }
    }
}

impl Recorders {
    fn note_cast_failure(&mut self, context: &str, error: &io::Error) {
        if self.cast_failure.is_none() {
            self.cast_failure = Some(format!("pty.cast {context} failed: {error}"));
        }
    }

    #[cfg(test)]
    fn inject_durable_failure(&mut self) -> Result<(), String> {
        match self.next_durable_failure.take() {
            Some(error) => Err(self.durable.fail(error)),
            None => Ok(()),
        }
    }

    #[cfg(not(test))]
    fn inject_durable_failure(&mut self) -> Result<(), String> {
        Ok(())
    }
}

struct PaneStreams {
    generation: u64,
    valid: bool,
    slots: BTreeMap<ConnId, PaneSlot>,
}

struct PaneSlot {
    generation: u64,
    token: [u8; 32],
    cut: u64,
    out: Outbound,
    cancellation: Arc<PaneCancellation>,
    pending_since: Instant,
    phase: PanePhase,
}

const PANE_STREAM_BTREE_SLOT_OVERHEAD_BYTES: usize = 3 * std::mem::size_of::<usize>();
const COMPLETED_PANE_HASHMAP_SLOT_OVERHEAD_BYTES: usize = 4 * std::mem::size_of::<usize>();
const ARC_ALLOCATION_HEADER_BYTES: usize = 2 * std::mem::size_of::<usize>();
const PANE_CANCELLATION_ARC_RESERVE_BYTES: usize =
    ARC_ALLOCATION_HEADER_BYTES + std::mem::size_of::<PaneCancellation>();
const PANE_STREAM_SLOT_RESERVE_BYTES: usize = std::mem::size_of::<ConnId>()
    + std::mem::size_of::<PaneSlot>()
    + PANE_STREAM_BTREE_SLOT_OVERHEAD_BYTES;
/// Conservative fixed charge for the two cache-owned Arc allocations and one completed-map slot.
/// `Shared` includes the inline `PaneStreams` registry header; its future BTree storage is reserved
/// per configured slot below. This is a stable ownership charge, not a claim about allocator RSS.
const COMPLETED_HOST_FIXED_RESERVE_BYTES: usize = ARC_ALLOCATION_HEADER_BYTES
    + std::mem::size_of::<PtyHost>()
    + ARC_ALLOCATION_HEADER_BYTES
    + std::mem::size_of::<Shared>()
    + std::mem::size_of::<AgentId>()
    + std::mem::size_of::<Arc<PtyHost>>()
    + COMPLETED_PANE_HASHMAP_SLOT_OVERHEAD_BYTES;

fn completed_host_charge_from(splice_charge: usize, subscriber_limit: usize) -> Option<usize> {
    subscriber_limit
        .checked_mul(
            PANE_STREAM_SLOT_RESERVE_BYTES.checked_add(PANE_CANCELLATION_ARC_RESERVE_BYTES)?,
        )
        .and_then(|pane_slots| {
            splice_charge
                .checked_add(COMPLETED_HOST_FIXED_RESERVE_BYTES)?
                .checked_add(pane_slots)
        })
}

#[cfg(test)]
fn completed_host_charge_from_for_test(
    splice_charge: usize,
    subscriber_limit: usize,
) -> Option<usize> {
    completed_host_charge_from(splice_charge, subscriber_limit)
}

const PANE_PENDING_TTL: Duration = Duration::from_secs(10);

enum PanePhase {
    Pending(splice::ReplaySubscription),
    Transitioning {
        subscriber: splice::SubscriberId,
        wake_pending: bool,
    },
    Ready(splice::ReadySubscription),
}

enum PaneFlowCursor {
    Replay(splice::ReplaySubscription),
    Ready(splice::ReadySubscription),
}

struct PaneFlow {
    shared: Weak<Shared>,
    out: Outbound,
    conn: ConnId,
    generation: u64,
    subscriber: splice::SubscriberId,
    cancellation: Arc<PaneCancellation>,
    cursor: Option<PaneFlowCursor>,
    transition_hook_pending: bool,
    terminal_cleanup_pending: bool,
    installed: bool,
}

enum PaneFlowCompletion {
    Installed,
    Continue(splice::ReadySubscription),
    Cancelled,
}

impl PaneFlow {
    fn replay(
        shared: &Shared,
        out: Outbound,
        conn: ConnId,
        generation: u64,
        subscriber: splice::SubscriberId,
        cancellation: Arc<PaneCancellation>,
        replay: splice::ReplaySubscription,
    ) -> Self {
        Self {
            shared: shared.self_weak.clone(),
            out,
            conn,
            generation,
            subscriber,
            cancellation,
            cursor: Some(PaneFlowCursor::Replay(replay)),
            transition_hook_pending: true,
            terminal_cleanup_pending: false,
            installed: false,
        }
    }

    fn ready(
        shared: &Shared,
        out: Outbound,
        conn: ConnId,
        generation: u64,
        subscriber: splice::SubscriberId,
        cancellation: Arc<PaneCancellation>,
        ready: splice::ReadySubscription,
    ) -> Self {
        Self {
            shared: shared.self_weak.clone(),
            out,
            conn,
            generation,
            subscriber,
            cancellation,
            cursor: Some(PaneFlowCursor::Ready(ready)),
            transition_hook_pending: false,
            terminal_cleanup_pending: false,
            installed: false,
        }
    }

    fn next_frame(&mut self) -> Option<Frame> {
        if self.cancellation.is_cancelled() {
            return None;
        }
        let shared = self.shared.upgrade()?;
        if self.terminal_cleanup_pending {
            self.cursor.take();
            shared.remove_transition(self.conn, self.generation, self.subscriber);
            self.installed = true;
            return None;
        }
        loop {
            if self.cancellation.is_cancelled() {
                return None;
            }
            shared.observe_pane_pre_pull();
            let cancelled = self
                .cancellation
                .cancelled
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *cancelled {
                return None;
            }
            match self.cursor.take()? {
                PaneFlowCursor::Replay(mut replay) => {
                    shared.observe_pane_guarded_pull();
                    let pulled = replay.pull_one();
                    drop(cancelled);
                    match pulled {
                        Ok(splice::ReplayPull::Record(record)) => {
                            return Some(self.record_frame(
                                &shared,
                                PaneFlowCursor::Replay(replay),
                                record,
                            ));
                        }
                        Ok(splice::ReplayPull::Ready(ready)) => {
                            self.cursor = Some(PaneFlowCursor::Ready(ready));
                        }
                        Err(error) => {
                            self.fail_if_overflowed(&shared, &error);
                            return None;
                        }
                    }
                }
                PaneFlowCursor::Ready(mut ready) => {
                    shared.observe_pane_guarded_pull();
                    let pulled = ready.pull_one();
                    drop(cancelled);
                    match pulled {
                        Ok(Some(record)) => {
                            return Some(self.record_frame(
                                &shared,
                                PaneFlowCursor::Ready(ready),
                                record,
                            ));
                        }
                        Ok(None) => match shared.finish_pane_flow(
                            self.conn,
                            self.generation,
                            self.subscriber,
                            ready,
                            std::mem::take(&mut self.transition_hook_pending),
                        ) {
                            PaneFlowCompletion::Installed => {
                                self.installed = true;
                                return None;
                            }
                            PaneFlowCompletion::Continue(ready) => {
                                self.cursor = Some(PaneFlowCursor::Ready(ready));
                            }
                            PaneFlowCompletion::Cancelled => return None,
                        },
                        Err(error) => {
                            self.fail_if_overflowed(&shared, &error);
                            return None;
                        }
                    }
                }
            }
        }
    }

    fn record_frame(
        &mut self,
        shared: &Shared,
        cursor: PaneFlowCursor,
        record: splice::DisplayRecord,
    ) -> Frame {
        self.terminal_cleanup_pending = matches!(record.kind, splice::DisplayKind::End);
        self.cursor = Some(cursor);
        shared.observe_pane_delivery();
        Frame::Notification(Notification::new(Event::NodePaneFrame(pane_frame(
            &shared.agent_id,
            record,
        ))))
    }

    fn fail_if_overflowed(&self, shared: &Shared, error: &splice::SpliceError) {
        if matches!(error, splice::SpliceError::SubscriberOverflowed(id) if *id == self.subscriber)
        {
            self.out.fail(crate::serve::Departure::PaneOverflow {
                agent_id: shared.agent_id.0.clone(),
            });
        }
    }
}

impl Drop for PaneFlow {
    fn drop(&mut self) {
        if self.installed {
            return;
        }
        self.cancellation.cancel();
        if let Some(shared) = self.shared.upgrade() {
            shared.remove_transition(self.conn, self.generation, self.subscriber);
        }
    }
}

#[derive(Default)]
struct PaneCancellation {
    cancelled: Mutex<bool>,
}

impl PaneCancellation {
    fn cancel(&self) {
        *self.cancelled.lock().unwrap_or_else(|e| e.into_inner()) = true;
    }

    fn is_cancelled(&self) -> bool {
        *self.cancelled.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl PanePhase {
    fn subscriber(&self) -> Result<splice::SubscriberId, splice::SpliceError> {
        match self {
            Self::Pending(subscription) => subscription.id(),
            Self::Transitioning { subscriber, .. } => Ok(*subscriber),
            Self::Ready(subscription) => subscription.id(),
        }
    }
}

/// **A resize in flight between the caller who asked for it and the thread that performs it.**
///
/// See [`PtyHost::resize`] for why the reader performs it rather than the caller. Coalescing is
/// deliberate: two resizes queued before either is applied are one `TIOCSWINSZ` and one `r` record,
/// because an intermediate geometry nothing was ever painted at is not a fact about the session.
struct ResizeQueue {
    /// The size the reader should apply next, if any.
    pending: Option<WinSize>,
    /// Monotonic ticket, bumped per request, so a waiter can tell *its* resize from a later one's.
    requested: u64,
    /// The highest ticket the reader has applied.
    applied: u64,
    /// What went wrong applying it, pre-formatted — the reader has no other way to answer.
    failure: Option<String>,
    /// **Whether a reader thread is still there to perform one.** Without it, a `resize` after the
    /// reader has ended (EOF, or after `shutdown`) would wait for a thread that will never wake it.
    /// Set false by `read_loop` on its way out, and by then the caller falls back to doing the work
    /// itself — the same two steps, in the same order, with nothing left to interleave with.
    reader: bool,
}

impl Shared {
    fn set_size(&self, master: &PtyMaster, size: WinSize) -> io::Result<()> {
        #[cfg(test)]
        if let Some(error) = self
            .resize_ioctl_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Err(io::Error::other(error));
        }
        master.set_size(size)
    }

    fn resize_cast(&self, cast: &mut CastWriter, size: WinSize) -> io::Result<()> {
        #[cfg(test)]
        if let Some(error) = self
            .resize_cast_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Err(io::Error::other(error));
        }
        cast.resize(size)
    }

    fn retain_output(&self, chunk: &[u8]) {
        if self.splice_disabled.load(Ordering::SeqCst) {
            return;
        }
        match self.splice.retain_output(Arc::from(chunk)) {
            Ok(outcome) => self.handle_splice_outcome(outcome),
            Err(error) => self.latch_splice_error(error),
        }
    }

    fn retain_resize(
        &self,
        size: WinSize,
    ) -> Result<Option<splice::EmitOutcome>, splice::SpliceError> {
        if self.splice_disabled.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.splice.retain_resize(size.rows, size.cols).map(Some)
    }

    fn finish_resize_retention(
        &self,
        outcome: Result<Option<splice::EmitOutcome>, splice::SpliceError>,
    ) {
        match outcome {
            Ok(Some(outcome)) => self.handle_splice_outcome(outcome),
            Ok(None) => {}
            Err(error) => self.latch_splice_error(error),
        }
    }

    fn retain_end(&self) {
        if self.splice_disabled.load(Ordering::SeqCst) {
            return;
        }
        match self.splice.retain_end() {
            Ok(outcome) => {
                self.handle_splice_outcome(outcome);
            }
            Err(error) => self.latch_splice_error(error),
        }
    }

    fn handle_splice_outcome(&self, outcome: splice::EmitOutcome) {
        self.prune_expired_pane_replays();
        #[cfg(test)]
        if let Some(hook) = self
            .pane_splice_outcome_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            hook();
        }
        for subscriber in outcome.overflowed {
            self.cancel_subscriber(subscriber);
        }
        for subscriber in outcome.wake_ready {
            self.wake_subscriber(subscriber);
        }
    }

    fn pane_now(&self) -> Instant {
        let clock = self
            .pane_clock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        clock()
    }

    fn prune_expired_pane_replays(&self) {
        let now = self.pane_now();
        let expired = {
            let mut streams = self.pane_streams.lock().unwrap_or_else(|e| e.into_inner());
            let conns = streams
                .slots
                .iter()
                .filter_map(|(conn, slot)| {
                    (matches!(slot.phase, PanePhase::Pending(_))
                        && now.saturating_duration_since(slot.pending_since) >= PANE_PENDING_TTL)
                        .then_some(*conn)
                })
                .collect::<Vec<_>>();
            conns
                .into_iter()
                .filter_map(|conn| streams.slots.remove(&conn))
                .map(|slot| {
                    slot.cancellation.cancel();
                    slot.out
                })
                .collect::<Vec<_>>()
        };
        for out in expired {
            out.fail(crate::serve::Departure::PaneReplayExpired {
                agent_id: self.agent_id.0.clone(),
            });
        }
    }

    fn cancel_subscriber(&self, subscriber: splice::SubscriberId) {
        let out = {
            let mut streams = self.pane_streams.lock().unwrap_or_else(|e| e.into_inner());
            let Some(conn) = streams.slots.iter().find_map(|(conn, slot)| {
                (slot.phase.subscriber().ok() == Some(subscriber)).then_some(*conn)
            }) else {
                return;
            };
            let transitioning = matches!(
                streams.slots.get(&conn).map(|slot| &slot.phase),
                Some(PanePhase::Transitioning { .. })
            );
            if transitioning {
                let slot = streams
                    .slots
                    .get(&conn)
                    .expect("the matching transition is still registered");
                slot.cancellation.cancel();
                slot.out.clone()
            } else {
                let slot = streams
                    .slots
                    .remove(&conn)
                    .expect("the matching subscriber is still registered");
                slot.cancellation.cancel();
                slot.out
            }
        };
        out.fail(crate::serve::Departure::PaneOverflow {
            agent_id: self.agent_id.0.clone(),
        });
    }

    fn wake_subscriber(&self, subscriber: splice::SubscriberId) {
        let Some((conn, generation, out, cancellation, subscription)) =
            self.take_ready_subscription(subscriber)
        else {
            return;
        };
        let mut flow = PaneFlow::ready(
            self,
            out.clone(),
            conn,
            generation,
            subscriber,
            cancellation,
            subscription,
        );
        out.start_flow(move || flow.next_frame());
    }

    fn take_ready_subscription(
        &self,
        subscriber: splice::SubscriberId,
    ) -> Option<(
        ConnId,
        u64,
        Outbound,
        Arc<PaneCancellation>,
        splice::ReadySubscription,
    )> {
        let mut streams = self.pane_streams.lock().unwrap_or_else(|e| e.into_inner());
        let conn = *streams.slots.iter().find_map(|(conn, slot)| {
            (slot.phase.subscriber().ok() == Some(subscriber)).then_some(conn)
        })?;
        let slot = streams.slots.get_mut(&conn)?;
        match &mut slot.phase {
            PanePhase::Transitioning { wake_pending, .. } => {
                *wake_pending = true;
                None
            }
            PanePhase::Pending(_) => None,
            PanePhase::Ready(_) => {
                let phase = std::mem::replace(
                    &mut slot.phase,
                    PanePhase::Transitioning {
                        subscriber,
                        wake_pending: false,
                    },
                );
                let PanePhase::Ready(subscription) = phase else {
                    unreachable!("the phase was matched above")
                };
                Some((
                    conn,
                    slot.generation,
                    slot.out.clone(),
                    Arc::clone(&slot.cancellation),
                    subscription,
                ))
            }
        }
    }

    fn remove_transition(&self, conn: ConnId, generation: u64, subscriber: splice::SubscriberId) {
        let mut streams = self.pane_streams.lock().unwrap_or_else(|e| e.into_inner());
        let remove = streams.slots.get(&conn).is_some_and(|slot| {
            slot.generation == generation
                && matches!(
                    slot.phase,
                    PanePhase::Transitioning {
                        subscriber: active,
                        ..
                    } if active == subscriber
                )
        });
        if remove {
            streams.slots.remove(&conn);
        }
    }

    fn observe_pane_delivery(&self) {
        #[cfg(test)]
        if let Some(hook) = self
            .pane_delivery_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            hook();
        }
    }

    #[cfg(test)]
    fn observe_pane_pre_pull(&self) {
        let hook = self
            .pane_pre_pull_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(not(test))]
    fn observe_pane_pre_pull(&self) {}

    #[cfg(test)]
    fn observe_pane_guarded_pull(&self) {
        let hook = self
            .pane_guarded_pull_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(not(test))]
    fn observe_pane_guarded_pull(&self) {}

    fn finish_pane_flow(
        &self,
        conn: ConnId,
        generation: u64,
        subscriber: splice::SubscriberId,
        ready: splice::ReadySubscription,
        _invoke_transition_hook: bool,
    ) -> PaneFlowCompletion {
        #[cfg(test)]
        if _invoke_transition_hook
            && let Some(hook) = self
                .pane_transition_hook
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
        {
            hook();
        }
        let mut streams = self.pane_streams.lock().unwrap_or_else(|e| e.into_inner());
        let valid = streams.valid && streams.generation == generation;
        let Some(slot) = streams.slots.get_mut(&conn) else {
            return PaneFlowCompletion::Cancelled;
        };
        let matching = slot.generation == generation
            && matches!(
                slot.phase,
                PanePhase::Transitioning {
                    subscriber: active,
                    ..
                } if active == subscriber
            );
        if !valid || !matching || slot.cancellation.is_cancelled() {
            streams.slots.remove(&conn);
            return PaneFlowCompletion::Cancelled;
        }
        let wake_pending = match &mut slot.phase {
            PanePhase::Transitioning { wake_pending, .. } => std::mem::take(wake_pending),
            _ => unreachable!("the transition was matched above"),
        };
        if wake_pending {
            PaneFlowCompletion::Continue(ready)
        } else {
            slot.phase = PanePhase::Ready(ready);
            PaneFlowCompletion::Installed
        }
    }

    fn latch_splice_error(&self, error: splice::SpliceError) {
        if self.splice_disabled.swap(true, Ordering::SeqCst) {
            return;
        }
        let error = error.to_string();
        *self.splice_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
        let outbounds = {
            let mut streams = self.pane_streams.lock().unwrap_or_else(|e| e.into_inner());
            streams.generation = streams.generation.wrapping_add(1);
            streams.valid = false;
            let outbounds = streams
                .slots
                .values()
                .map(|slot| {
                    slot.cancellation.cancel();
                    slot.out.clone()
                })
                .collect::<Vec<_>>();
            streams
                .slots
                .retain(|_, slot| matches!(slot.phase, PanePhase::Transitioning { .. }));
            outbounds
        };
        for out in outbounds {
            out.fail(crate::serve::Departure::PaneRetentionFailed {
                agent_id: self.agent_id.0.clone(),
                error: error.clone(),
            });
        }
    }

    fn invalidate_pane_streams(&self) {
        let outbounds = {
            let mut streams = self.pane_streams.lock().unwrap_or_else(|e| e.into_inner());
            streams.generation = streams.generation.wrapping_add(1);
            streams.valid = false;
            let outbounds = streams
                .slots
                .values()
                .map(|slot| {
                    slot.cancellation.cancel();
                    slot.out.clone()
                })
                .collect::<Vec<_>>();
            streams
                .slots
                .retain(|_, slot| matches!(slot.phase, PanePhase::Transitioning { .. }));
            outbounds
        };
        for out in outbounds {
            out.fail(crate::serve::Departure::PaneReplayEvicted {
                agent_id: self.agent_id.0.clone(),
            });
        }
    }

    #[cfg(test)]
    fn fail_pane_connection(&self, conn: ConnId, error: String) {
        let out = {
            let mut streams = self.pane_streams.lock().unwrap_or_else(|e| e.into_inner());
            let Some(slot) = streams.slots.remove(&conn) else {
                return;
            };
            slot.cancellation.cancel();
            slot.out
        };
        out.fail(crate::serve::Departure::PaneInputFailed {
            agent_id: self.agent_id.0.clone(),
            error,
        });
    }

    fn emit(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let mono_ns = {
            let recorders = self.recorders.lock().unwrap_or_else(|e| e.into_inner());
            recorders.cast.origin.elapsed().as_nanos() as u64
        };
        let Some(listeners) = self.listeners.begin_delivery() else {
            return;
        };
        self.listeners.observe_delivery();
        let alive = listeners
            .iter()
            .filter_map(|listener| {
                listener
                    .notify(Event::NodePty {
                        agent_id: self.agent_id.clone(),
                        seq,
                        mono_ns,
                        bytes: text.to_string(),
                    })
                    .then_some(listener.conn())
            })
            .collect::<Vec<_>>();
        self.listeners.finish_delivery(&alive);
    }
}

fn pane_frame(agent_id: &AgentId, record: splice::DisplayRecord) -> PaneFrameV1 {
    let frame = match record.kind {
        splice::DisplayKind::Output(bytes) => PaneFrameKindV1::Output {
            bytes: OpaquePaneBytesV1::new(bytes.as_ref()),
        },
        splice::DisplayKind::Resize { rows, cols } => PaneFrameKindV1::Resize { cols, rows },
        splice::DisplayKind::End => PaneFrameKindV1::End {},
    };
    // Host splices synthesize initial geometry at zero; retained records remain one-origin, making
    // the combined versioned wire stream dense without consuming a configured retained slot.
    PaneFrameV1::new(agent_id.clone(), record.seq, frame)
}

/// One node's pty, its recording, and the thread that reads it.
///
/// **Every method takes `&self`, including [`Self::adopt`] and [`Self::shutdown`], and the two
/// interior mutexes below are what buys that.** It is not a style choice. A live pane has two
/// owners at once: the launcher that spawned the child and must reap it, and
/// `RegistryHandle::register_pane`, which holds an `Arc<PtyHost>` so `node/attach` can lease the
/// keyboard and fan the bytes out. `Arc` gives no `&mut`, so a `shutdown(&mut self)` made the host
/// un-registerable and a registered host un-reapable — which is exactly the shape that kept
/// `root::launch_terminal` unwritten. Neither lock is ever held across a call that can block on the
/// other, and `shutdown` **takes** the child out before waiting on it, so a concurrent
/// [`Self::resize`] from an attached client cannot queue behind a `wait`.
pub struct PtyHost {
    master: Arc<PtyMaster>,
    shared: Arc<Shared>,
    child: Mutex<Option<PtyChild>>,
    reader: Mutex<Option<std::thread::JoinHandle<io::Result<()>>>>,
    reader_completion: Arc<ReaderCompletion>,
    stopped: Arc<AtomicBool>,
    replay_completion: Mutex<Option<Result<usize, String>>>,
    control: Arc<ControlGate>,
    #[cfg(test)]
    control_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    post_cast_input_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    next_master_input_failure: Mutex<Option<String>>,
    #[cfg(test)]
    before_child_sweep_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    before_reader_stop_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    reader_drain_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    invalidation_hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PaneReplayReservationError {
    #[error("the pane replay stream is unavailable: {0}")]
    Unavailable(String),
    #[error("this connection already has a pane replay transition in flight")]
    Busy,
    #[error("the pane replay could not mint a readiness token")]
    Entropy,
}

struct PtyHostStart {
    splice_limits: splice::SpliceLimits,
    start_reader: bool,
}

impl PtyHost {
    /// Take ownership of a master and start recording. The child may be attached later with
    /// [`Self::adopt`], or never — a host with no child is exactly what a test needs to drive the
    /// slave by hand.
    pub fn start(
        agent_id: AgentId,
        master: PtyMaster,
        cast_path: &Path,
        size: WinSize,
        term: &str,
        origin: Instant,
    ) -> io::Result<Self> {
        Self::start_inner(
            agent_id,
            master,
            cast_path,
            size,
            term,
            origin,
            PtyHostStart {
                splice_limits: splice::SpliceLimits::production(),
                start_reader: true,
            },
        )
    }

    #[cfg(test)]
    fn start_with_splice_limits(
        agent_id: AgentId,
        master: PtyMaster,
        cast_path: &Path,
        size: WinSize,
        term: &str,
        origin: Instant,
        splice_limits: splice::SpliceLimits,
    ) -> io::Result<Self> {
        Self::start_inner(
            agent_id,
            master,
            cast_path,
            size,
            term,
            origin,
            PtyHostStart {
                splice_limits,
                start_reader: true,
            },
        )
    }

    #[cfg(test)]
    fn start_without_reader_for_test(
        agent_id: AgentId,
        master: PtyMaster,
        cast_path: &Path,
        size: WinSize,
        term: &str,
        origin: Instant,
    ) -> io::Result<Self> {
        Self::start_inner(
            agent_id,
            master,
            cast_path,
            size,
            term,
            origin,
            PtyHostStart {
                splice_limits: splice::SpliceLimits::production(),
                start_reader: false,
            },
        )
    }

    fn start_inner(
        agent_id: AgentId,
        master: PtyMaster,
        cast_path: &Path,
        size: WinSize,
        term: &str,
        origin: Instant,
        options: PtyHostStart,
    ) -> io::Result<Self> {
        let cast = CastWriter::create(cast_path, size, term, origin)?;
        // The binary stream is an authoritative enhancement, not the compatibility launch gate.
        // A secure-path or entropy failure leaves the legacy host usable, but opaque input and
        // completed replay fail closed through the retained `Unavailable` state.
        let durable = match stream::SessionWriter::create(cast_path, &agent_id, size) {
            Ok(writer) => DurableRecorder::available(writer),
            Err(error) => DurableRecorder::unavailable(error),
        };
        let shared = Arc::new_cyclic(|self_weak| Shared {
            self_weak: self_weak.clone(),
            agent_id,
            recorders: Mutex::new(Recorders {
                cast,
                durable,
                cast_failure: None,
                #[cfg(test)]
                next_durable_failure: None,
            }),
            listeners: LegacyListeners::new(),
            writer: Arc::new(Mutex::new(None)),
            seq: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            bytes: Arc::new(AtomicU64::new(0)),
            splice: splice::PtySplice::new_with_initial_resize(
                options.splice_limits,
                size.rows,
                size.cols,
            ),
            splice_error: Mutex::new(None),
            splice_disabled: AtomicBool::new(false),
            pane_streams: Mutex::new(PaneStreams {
                generation: 1,
                valid: true,
                slots: BTreeMap::new(),
            }),
            pane_clock: Mutex::new(Arc::new(Instant::now)),
            #[cfg(test)]
            pane_transition_hook: Mutex::new(None),
            #[cfg(test)]
            pane_delivery_hook: Mutex::new(None),
            #[cfg(test)]
            pane_splice_outcome_hook: Mutex::new(None),
            #[cfg(test)]
            pane_pre_pull_hook: Mutex::new(None),
            #[cfg(test)]
            pane_guarded_pull_hook: Mutex::new(None),
            #[cfg(test)]
            reader_would_block_hook: Mutex::new(None),
            resize: Mutex::new(ResizeQueue {
                pending: None,
                requested: 0,
                applied: 0,
                failure: None,
                reader: options.start_reader,
            }),
            resize_done: Condvar::new(),
            #[cfg(test)]
            resize_hook: Mutex::new(None),
            #[cfg(test)]
            resize_ioctl_failure: Mutex::new(None),
            #[cfg(test)]
            resize_cast_failure: Mutex::new(None),
        });
        let master = Arc::new(master);
        let stopped = Arc::new(AtomicBool::new(false));
        let reader_completion = ReaderCompletion::new();
        let reader = if options.start_reader {
            let master = Arc::clone(&master);
            let shared = Arc::clone(&shared);
            let stopped = Arc::clone(&stopped);
            let completion = Arc::clone(&reader_completion);
            Some(
                std::thread::Builder::new()
                    .name("marion-pty".into())
                    .spawn(move || {
                        let _finished = ReaderFinished(completion);
                        read_loop(&master, &shared, &stopped)
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            master,
            shared,
            child: Mutex::new(None),
            reader: Mutex::new(reader),
            reader_completion,
            stopped,
            replay_completion: Mutex::new(None),
            control: ControlGate::new(),
            #[cfg(test)]
            control_hook: Mutex::new(None),
            #[cfg(test)]
            post_cast_input_hook: Mutex::new(None),
            #[cfg(test)]
            next_master_input_failure: Mutex::new(None),
            #[cfg(test)]
            before_child_sweep_hook: Mutex::new(None),
            #[cfg(test)]
            before_reader_stop_hook: Mutex::new(None),
            #[cfg(test)]
            reader_drain_hook: Mutex::new(None),
            #[cfg(test)]
            invalidation_hook: Mutex::new(None),
        })
    }

    /// Run `hook` on the reader thread, inside [`apply_pending_resize`]'s drain and **before** the
    /// `TIOCSWINSZ`, so a test can force the interleaving the labelling claim is about instead of
    /// racing for it. See `every_output_record_replays_at_the_geometry_it_was_emitted_at`.
    ///
    /// That position is the only one that is deterministic. A hook on the *caller's* thread races
    /// the reader — it can write a marker at the old geometry and lose to an ioctl that has already
    /// happened — and a test whose red state depends on who won is a test that passes by luck.
    /// Running inside the drain means the bytes it writes are read and recorded by the very loop
    /// that is about to perform the resize, in that order, every time.
    #[cfg(test)]
    pub(crate) fn set_resize_hook(&mut self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .shared
            .resize_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    #[cfg(test)]
    pub(crate) fn clear_resize_hook(&mut self) {
        *self
            .shared
            .resize_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[cfg(test)]
    fn fail_next_resize_ioctl(&self, error: &str) {
        *self
            .shared
            .resize_ioctl_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
    }

    #[cfg(test)]
    fn fail_next_cast_resize(&self, error: &str) {
        *self
            .shared
            .resize_cast_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
    }

    #[cfg(test)]
    pub(crate) fn fail_next_durable_append(&self, error: &str) {
        self.shared
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_durable_failure = Some(error.to_string());
    }

    #[cfg(test)]
    fn limit_durable_output_for_test(&self, limit: usize) {
        self.shared
            .recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .durable
            .limit_output_for_test(limit);
    }

    /// The running byte count, shareable into a resize hook. See
    /// `a_resize_record_precedes_the_output_it_explains`: the hook must be able to *wait* for the
    /// recorder, or the interleaving it exists to force is still a race.
    #[cfg(test)]
    pub(crate) fn bytes_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.shared.bytes)
    }

    pub fn adopt(&self, child: PtyChild) {
        *self.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    }

    pub fn master(&self) -> &PtyMaster {
        &self.master
    }

    /// Has the adopted child exited, without reaping it?
    ///
    /// The launcher polls this after adoption, so `false` also covers the structurally impossible
    /// no-child case. Leaving an exited leader waitable pins its pid/process group until shutdown
    /// has swept same-group descendants; [`Self::shutdown`] then reaps and reports the exact status.
    pub fn poll_exited_unreaped(&self) -> io::Result<bool> {
        match self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            None => Ok(false),
            Some(child) => child.poll_exited_unreaped(),
        }
    }

    /// Reap an exited child if one is ready.
    ///
    /// Production pane waiting uses [`Self::poll_exited_unreaped`]; this consuming probe remains
    /// for tests that own teardown immediately and need the status at the observation point.
    #[cfg(test)]
    pub(crate) fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
        match self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            None => Ok(None),
            Some(child) => child.try_wait(),
        }
    }

    pub fn child_pid(&self) -> Option<i32> {
        self.child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(PtyChild::pid)
    }

    /// How many terminal probes this node has emitted and marion has not answered. See
    /// [`ProbeScan`]: a counter, deliberately, and not a responder.
    pub fn unanswered_probes(&self) -> u64 {
        self.shared.probes.load(Ordering::SeqCst)
    }

    pub fn bytes_read(&self) -> u64 {
        self.shared.bytes.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn retention_snapshot(&self) -> Vec<splice::DisplayRecord> {
        self.shared.splice.snapshot()
    }

    #[cfg(test)]
    fn retention_error(&self) -> Option<String> {
        self.shared
            .splice_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The memory charge for a completed replay, available only after the reader has drained.
    /// Eligibility is separate from legacy shutdown success: production-dark retention failure
    /// evicts the replay cache entry without changing the node's outcome or its cast.
    pub(crate) fn completed_replay_charge(&self) -> io::Result<usize> {
        match self
            .replay_completion
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(Ok(charge)) => Ok(charge),
            Some(Err(error)) => Err(io::Error::other(error)),
            None => Err(io::Error::other(
                "pty replay completion was requested before shutdown",
            )),
        }
    }

    fn mark_replay_ineligible(&self, reason: String) {
        let mut completion = self
            .replay_completion
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !matches!(&*completion, Some(Err(_))) {
            *completion = Some(Err(reason));
        }
    }

    #[cfg(test)]
    pub(crate) fn set_control_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self.control_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    #[cfg(test)]
    fn observe_control_hook(&self) {
        if let Some(hook) = self
            .control_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_post_cast_input_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .post_cast_input_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    #[cfg(test)]
    fn observe_post_cast_input_hook(&self) {
        if let Some(hook) = self
            .post_cast_input_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_next_master_input(&self, error: impl Into<String>) {
        *self
            .next_master_input_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(error.into());
    }

    #[cfg(test)]
    fn take_master_input_failure(&self) -> Option<String> {
        self.next_master_input_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    #[cfg(test)]
    pub(crate) fn set_before_child_sweep_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .before_child_sweep_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    #[cfg(test)]
    fn observe_before_child_sweep_hook(&self) {
        if let Some(hook) = self
            .before_child_sweep_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_reader_would_block_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .shared
            .reader_would_block_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    #[cfg(test)]
    pub(crate) fn set_before_reader_stop_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .before_reader_stop_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    #[cfg(test)]
    fn observe_before_reader_stop_hook(&self) {
        if let Some(hook) = self
            .before_reader_stop_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_reader_drain_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .reader_drain_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    #[cfg(test)]
    fn observe_reader_drain_hook(&self) {
        if let Some(hook) = self
            .reader_drain_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            hook();
        }
    }

    #[cfg(test)]
    pub(crate) fn stopped_for_test(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn set_invalidation_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .invalidation_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// The ordinal the next [`Event::NodePty`] will carry.
    pub fn next_seq(&self) -> u64 {
        self.shared.seq.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn emit_for_test(&self, text: &str) {
        self.shared.emit(text);
    }

    #[cfg(test)]
    pub(crate) fn set_legacy_delivery_hook(&self, hook: Box<dyn Fn() + Send + Sync>) {
        self.shared.listeners.set_delivery_hook(hook);
    }

    /// Subscribe a client. It receives bytes from **now**; replay is I4's problem.
    ///
    /// (A note for I4, since it is the thing a reader will get wrong: a bare tail of `pty.cast` is
    /// always wrong for Claude Code, which enters the alternate screen at byte 67 of the session
    /// and never leaves it. A replay that starts anywhere after that byte hands the emulator a
    /// stream whose buffer state it cannot know.)
    pub fn listen(&self, out: Outbound) {
        self.shared.listeners.add(out);
    }

    /// Reserve the exact retained prefix now, but deliberately send nothing until the caller has
    /// received the attach response and returns the advertised token.
    pub fn begin_pane_replay(
        &self,
        conn: ConnId,
        out: Outbound,
    ) -> Option<marion_proto::result::PaneReadyDescriptorV1> {
        self.reserve_pane_replay(conn, out).ok()
    }

    pub(crate) fn reserve_pane_replay(
        &self,
        conn: ConnId,
        out: Outbound,
    ) -> Result<marion_proto::result::PaneReadyDescriptorV1, PaneReplayReservationError> {
        self.shared.prune_expired_pane_replays();
        if self.shared.splice_disabled.load(Ordering::SeqCst) {
            let error = self
                .shared
                .splice_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| "retention is disabled".into());
            return Err(PaneReplayReservationError::Unavailable(error));
        }
        let pending_since = self.shared.pane_now();
        // Entropy is the only external fallible operation. Do it before taking any state lock so
        // a failure leaves the connection's existing legacy or pane subscription byte-for-byte
        // unchanged.
        let mut token = [0u8; 32];
        getrandom::fill(&mut token).map_err(|_| PaneReplayReservationError::Entropy)?;
        // Commit order: legacy listeners, pane registry, then splice. Emitters release the splice
        // lock before consulting the pane registry, so no reverse nested acquisition exists.
        let mut listeners = self
            .shared
            .listeners
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut streams = self
            .shared
            .pane_streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !streams.valid {
            return Err(PaneReplayReservationError::Unavailable(
                "this pane generation was invalidated".into(),
            ));
        }
        if let Some(existing) = streams.slots.get(&conn)
            && matches!(existing.phase, PanePhase::Transitioning { .. })
        {
            return Err(PaneReplayReservationError::Busy);
        }
        // Register the replacement before touching the old slot. If the splice bound or terminal
        // state refuses, dropping this temporary is a no-op and the old stream stays authoritative.
        let replay = self
            .shared
            .splice
            .begin_replay()
            .map_err(|error| match error {
                splice::SpliceError::SubscriberLimitExceeded { .. }
                | splice::SpliceError::WrongSubscriberPhase => PaneReplayReservationError::Busy,
                error => PaneReplayReservationError::Unavailable(error.to_string()),
            })?;
        let cut = replay.cut();
        let generation = streams.generation;
        if let Some(existing) = streams.slots.remove(&conn) {
            existing.cancellation.cancel();
        }
        listeners
            .listeners
            .retain(|listener| listener.conn() != conn);
        streams.slots.insert(
            conn,
            PaneSlot {
                generation,
                token,
                cut,
                out,
                cancellation: Arc::new(PaneCancellation::default()),
                pending_since,
                phase: PanePhase::Pending(replay),
            },
        );
        Ok(marion_proto::result::PaneReadyDescriptorV1 {
            token: PaneReadyTokenV1::new(token),
            cut,
        })
    }

    pub fn pane_ready(&self, conn: ConnId, token: &PaneReadyTokenV1, cut: u64) {
        self.shared.prune_expired_pane_replays();
        let Some((generation, subscriber, out, cancellation, replay)) = ({
            let mut streams = self
                .shared
                .pane_streams
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let valid = streams.valid;
            let generation = streams.generation;
            let Some(slot) = streams.slots.get_mut(&conn) else {
                return;
            };
            if !valid
                || slot.generation != generation
                || slot.token != *token.as_bytes()
                || slot.cut != cut
            {
                return;
            }
            let Ok(subscriber) = slot.phase.subscriber() else {
                return;
            };
            let phase = std::mem::replace(
                &mut slot.phase,
                PanePhase::Transitioning {
                    subscriber,
                    wake_pending: false,
                },
            );
            let PanePhase::Pending(replay) = phase else {
                slot.phase = phase;
                return;
            };
            Some((
                generation,
                subscriber,
                slot.out.clone(),
                Arc::clone(&slot.cancellation),
                replay,
            ))
        }) else {
            return;
        };
        let mut flow = PaneFlow::replay(
            &self.shared,
            out.clone(),
            conn,
            generation,
            subscriber,
            cancellation,
            replay,
        );
        out.start_flow(move || flow.next_frame());
    }

    pub(crate) fn pane_replay_reserved(
        &self,
        conn: ConnId,
        token: &PaneReadyTokenV1,
        cut: u64,
    ) -> bool {
        let streams = self
            .shared
            .pane_streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        streams.valid
            && streams.slots.get(&conn).is_some_and(|slot| {
                slot.generation == streams.generation
                    && slot.token == *token.as_bytes()
                    && slot.cut == cut
                    && matches!(slot.phase, PanePhase::Pending(_))
                    && !slot.cancellation.is_cancelled()
            })
    }

    /// Permanently disable this host's pane stream registrations and cancel every delivery that
    /// was already in flight. The pty, cast, legacy listeners, and write lease are untouched.
    pub fn invalidate_pane_streams(&self) {
        #[cfg(test)]
        if let Some(hook) = self
            .invalidation_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            hook();
        }
        self.shared.invalidate_pane_streams();
    }

    /// Drop a connection's listener.
    ///
    /// The fan-out already self-prunes — [`Shared::emit`] retains only listeners whose `notify`
    /// succeeded — so this is not what keeps a *dead* client off the list. It is what keeps a
    /// **live** one off it: a client that detached from this node and is still connected for
    /// another would otherwise go on being sent bytes for a pane it is no longer drawing, and the
    /// self-pruning cannot see the difference because those sends succeed.
    pub fn unlisten(&self, conn: ConnId) {
        self.shared.listeners.remove(conn);
        let mut streams = self
            .shared
            .pane_streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let transitioning = streams.slots.get(&conn).is_some_and(|slot| {
            slot.cancellation.cancel();
            matches!(slot.phase, PanePhase::Transitioning { .. })
        });
        if !transitioning {
            streams.slots.remove(&conn);
        }
    }

    /// End exactly one negotiated pane connection after an opaque input notification failed.
    /// The slot lock is released before socket shutdown so the serve loop's `gone` cleanup may
    /// immediately re-enter [`Self::unlisten`] without a lock cycle.
    #[cfg(test)]
    pub(crate) fn fail_pane_connection(&self, conn: ConnId, error: String) {
        self.shared.fail_pane_connection(conn, error);
    }

    pub fn listeners(&self) -> usize {
        self.shared.listeners.len()
    }

    pub(crate) fn clear_legacy_listeners(&self) {
        self.shared.listeners.close_and_clear();
    }

    /// Claim the write half for `conn`.
    ///
    /// The **second** caller is refused, by name, and gets a read-only attach. A supervisor that
    /// handed both clients the write half would produce interleaved keystrokes that look to each
    /// operator like the harness misbehaving — see [`WriteLease`].
    ///
    /// Re-claiming from the connection that already holds it is still a refusal rather than a
    /// second lease. Two live leases for one connection would each clear the slot on drop, so the
    /// first drop would silently open the node to a third client while the second lease was still
    /// being used.
    pub fn lease_writer(&self, conn: ConnId) -> Result<WriteLease, WriterBusy> {
        let mut held = self.shared.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(owner) = *held {
            return Err(WriterBusy::HeldBy(owner));
        }
        *held = Some(conn);
        Ok(WriteLease {
            conn,
            slot: Arc::clone(&self.shared.writer),
        })
    }

    /// Which connection holds the write half, if any.
    pub fn writer(&self) -> Option<ConnId> {
        *self.shared.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keystrokes in. Recorded as `i` **before** they are written, for the same asymmetry the
    /// resize order is chosen on: an `i` for a keystroke that did not land replays as a keystroke
    /// the harness ignored, while a keystroke with no record is a session whose input is missing.
    ///
    /// **`lease` is the whole signature**, exactly as `witness` is [`spawn_pty`]'s: it is
    /// deliberately unused in the body, because a capability's job is to have been required. A node
    /// cannot be typed into by a client that was not given the write half, and that is enforced by
    /// the type rather than by a check at each call site.
    ///
    /// A lease issued by a *different* [`PtyHost`] is refused. Without that check the token would
    /// prove only "somebody, somewhere, holds a write half", which in a fleet with several
    /// terminal nodes is not the claim being made.
    ///
    /// # Bytes the cast cannot carry are refused, not silently rewritten
    ///
    /// This used to record `String::from_utf8_lossy(bytes)` and write the original slice, so for
    /// `b"\xff"` the cast said U+FFFD and the node received `0xff`. That is the one thing a
    /// recording may not do: the `i` records exist because C1's mouse-through leaves no `o` trace
    /// at all, which makes them the *only* evidence of what was typed — and evidence that differs
    /// from what happened is worse than none, because nothing downstream can tell.
    ///
    /// asciicast has no byte-exact escape: `data` is a JSON string, so a byte that is not part of
    /// valid UTF-8 has no faithful spelling in the format. Given that, the two honest options are
    /// to invent a private encoding the corpus and `asciinema` would not read, or to refuse. It
    /// refuses — and refuses **before** the write, so the node never receives something the cast
    /// does not say it received. The failure is loud in the direction that matters: the record and
    /// the node cannot disagree, because on this path neither of them gets anything.
    ///
    /// **Nothing legitimate is lost today**, and that is checked rather than hoped: the only
    /// production caller is `RegistryHandle::deliver_input`, and `marion_proto::Input::NodePtyWrite`
    /// carries `bytes` as a **`String`** — the protocol's own doc says so and says the bytes reach
    /// the master unaltered — so every byte that can arrive over the wire is valid UTF-8 by
    /// construction and this refusal is unreachable from a client. It guards the `pub` `&[u8]`
    /// surface, which is what the in-crate callers and the tests use. If marion ever needs to carry
    /// 8-bit input (a raw C1 `0x9b` CSI, a latin-1 paste), the change is a byte-exact cast encoding
    /// **and** a wire type that can express it — not a quieter version of this.
    pub fn write_input(&self, lease: &WriteLease, bytes: &[u8]) -> io::Result<()> {
        self.validate_writer_lease(lease)?;
        let text = std::str::from_utf8(bytes).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "refusing to type {} byte(s) into this node: they are not valid UTF-8 (at \
                     offset {}), and `pty.cast`'s `i` record is a JSON string with no byte-exact \
                     spelling for them. Recording a substitute for input the node really received \
                     would make the only evidence of what was typed disagree with what was typed, \
                     so nothing is written and nothing is recorded.",
                    bytes.len(),
                    e.valid_up_to()
                ),
            )
        })?;
        let (permit, delivery) = self.control.admit_write(None)?;
        #[cfg(test)]
        self.observe_control_hook();
        // Both records are linearized before delivery. The binary evidence is best-effort on this
        // compatibility path: a dark-stream failure must not break an existing UTF-8 client, but it
        // does permanently disqualify completed replay.
        let (durable_failure, cast_result) = {
            let mut recorders = self
                .shared
                .recorders
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let durable_failure = recorders
                .inject_durable_failure()
                .and_then(|()| recorders.durable.append_input_evidence(bytes.len()))
                .err();
            let cast_result = recorders.cast.input(text);
            if let Err(error) = &cast_result {
                recorders.note_cast_failure("input", error);
            }
            (durable_failure, cast_result)
        };
        if let Some(error) = durable_failure {
            self.mark_replay_ineligible(error);
        }
        if let Err(error) = cast_result {
            let failure = format!("pty input recording failed before delivery: {error}");
            drop(permit);
            delivery.finish(Some(failure.clone()));
            self.mark_replay_ineligible(failure);
            return Err(error);
        }
        // Closing needs to order cast mutation before terminal `x`, not wait on a kernel write to
        // a harness that may never read again. The gate therefore ends at the durable record.
        drop(permit);
        #[cfg(test)]
        self.observe_post_cast_input_hook();
        #[cfg(test)]
        let outcome = match self.take_master_input_failure() {
            Some(error) => Err(io::Error::other(error)),
            None => self.master.write_all(bytes),
        };
        #[cfg(not(test))]
        let outcome = self.master.write_all(bytes);
        let delivery_failure = outcome.as_ref().err().map(|error| {
            format!("pty input was recorded but delivery failed before completion: {error}")
        });
        delivery.finish(delivery_failure.clone());
        if let Some(error) = delivery_failure {
            self.mark_replay_ineligible(error);
        }
        outcome
    }

    /// Test-only low-level delivery of opaque bytes without pane-v1 negotiation.
    ///
    /// Unlike [`Self::write_input`], this path never invents a legacy asciicast `i` string. It
    /// commits content-independent length and ordering evidence under the host's recorder lock,
    /// releases every durability lock, and only then writes the original slice to the master.
    /// Failure of that evidence step therefore means failure before delivery, not an unaudited
    /// write. Production callers must use [`Self::admit_opaque_input`] so the exact negotiated
    /// pane slot and failure channel are captured before delivery.
    #[cfg(test)]
    fn write_opaque_input(&self, lease: &WriteLease, bytes: &[u8]) -> io::Result<()> {
        self.validate_writer_lease(lease)?;
        let (permit, delivery) = self.control.admit_write(None)?;
        self.write_opaque_input_admitted(OpaqueInputAdmission { permit, delivery }, bytes)
    }

    /// Select a negotiated pane input while the caller still holds the global pane registry lock.
    /// The returned admission owns the control barrier and the slot's exact outbound channel; all
    /// recorder and master I/O happens later through [`Self::write_opaque_input_admitted`].
    pub(crate) fn admit_opaque_input(
        &self,
        lease: &WriteLease,
        conn: ConnId,
    ) -> io::Result<OpaqueInputAdmission> {
        self.validate_writer_lease(lease)?;
        let target = {
            let streams = self
                .shared
                .pane_streams
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let slot = streams.slots.get(&conn).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "opaque pane input requires a negotiated pane-v1 stream",
                )
            })?;
            let phase_is_admissible = matches!(
                slot.phase,
                PanePhase::Pending(_) | PanePhase::Transitioning { .. } | PanePhase::Ready(_)
            );
            if !streams.valid
                || slot.generation != streams.generation
                || slot.cancellation.is_cancelled()
                || !phase_is_admissible
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "opaque pane input belongs to an invalidated pane-v1 stream",
                ));
            }
            InputFailureTarget {
                out: slot.out.clone(),
                agent_id: self.shared.agent_id.0.clone(),
            }
        };
        let (permit, delivery) = self.control.admit_write(Some(target))?;
        Ok(OpaqueInputAdmission { permit, delivery })
    }

    /// Complete an opaque write selected by [`Self::admit_opaque_input`].
    pub(crate) fn write_opaque_input_admitted(
        &self,
        admission: OpaqueInputAdmission,
        bytes: &[u8],
    ) -> io::Result<()> {
        let OpaqueInputAdmission { permit, delivery } = admission;
        #[cfg(test)]
        self.observe_control_hook();
        let evidence = {
            let mut recorders = self
                .shared
                .recorders
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            recorders
                .inject_durable_failure()
                .and_then(|()| recorders.durable.append_input_evidence(bytes.len()))
        };
        if let Err(error) = evidence {
            drop(permit);
            delivery.finish(Some(error.clone()));
            self.mark_replay_ineligible(error.clone());
            return Err(io::Error::other(error));
        }
        drop(permit);
        #[cfg(test)]
        self.observe_post_cast_input_hook();
        #[cfg(test)]
        let outcome = match self.take_master_input_failure() {
            Some(error) => Err(io::Error::other(error)),
            None => self.master.write_all(bytes),
        };
        #[cfg(not(test))]
        let outcome = self.master.write_all(bytes);
        let delivery_failure = outcome.as_ref().err().map(|error| {
            format!("pty opaque input was evidenced but delivery failed before completion: {error}")
        });
        delivery.finish(delivery_failure.clone());
        if let Some(error) = delivery_failure {
            self.mark_replay_ineligible(error);
        }
        outcome
    }

    fn validate_writer_lease(&self, lease: &WriteLease) -> io::Result<()> {
        if Arc::ptr_eq(&lease.slot, &self.shared.writer) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "write lease belongs to a different node",
            ))
        }
    }

    /// **The reader thread performs the resize, and that is what makes the `r` record true.**
    ///
    /// # What the old order got wrong, and what it got right
    ///
    /// This used to be `record` → `TIOCSWINSZ` → `killpg(SIGWINCH)`, on the caller's thread, and
    /// the *reason* for that order still stands: the two crash windows are not symmetric. An `r`
    /// for a resize that never happened replays as a harmless reflow to a size nothing was painted
    /// at, while **output painted at a size no record mentions is unreplayable** — the emulator has
    /// no way to learn the geometry and every absolute cursor address after it lands in the wrong
    /// place. So the record must never come after output produced at the new size.
    ///
    /// The defect was not the order; it was **what the record claimed**. The caller wrote the `r`,
    /// released the cast lock, and only then issued the ioctl — with an `fsync` in between. For the
    /// whole of that window the terminal was still the *old* size, and anything the child wrote in
    /// it was recorded after the `r`. Replay then applies `100x24` to bytes the node emitted at
    /// `120x40`. `a_resize_record_precedes_the_output_it_explains` forced exactly that interleaving
    /// and asserted only `r < o`, so it passed while the labelling was wrong.
    ///
    /// # The fix, and why it is not simply "swap the two lines"
    ///
    /// Swapping them narrows the window but does not close it, because the mislabelled bytes are
    /// the ones the recorder has *read but not yet written* — a queue no ordering of two statements
    /// on another thread can see. The only place where "everything emitted at the old size has been
    /// recorded" is knowable is the reader loop itself, so that is where the resize now happens:
    /// this method queues the size and waits, and `apply_pending_resize` performs the `TIOCSWINSZ`
    /// and writes the `r` **under the cast lock**, at the top of the loop, immediately after the
    /// previous chunk was recorded and — in the idle case that reaches the `WouldBlock` arm — after
    /// the pty has been drained. Nothing can be recorded between the ioctl and the record, so the
    /// `r` sits exactly at the boundary it claims.
    ///
    /// The crash argument survives intact and gets stronger: the ioctl and the record are now
    /// atomic with respect to the cast, so there is no interval in which output at the new size can
    /// be written ahead of the record that explains it — which is the property the old order was
    /// reaching for by paying an fsync for it.
    ///
    /// **The fallback is not a fast path.** If no reader thread is running — after `shutdown`, or
    /// after the child hung up — there is nothing to interleave with and nothing to wait for, so
    /// the caller does the same two steps itself under the same lock.
    ///
    /// The explicit `SIGWINCH` is redundant with the kernel's own delivery on a size change. It is
    /// kept because **all five committed captures were made with it** (`ptyhost.py` sends it), so
    /// removing it would make marion's traffic differ from the corpus every claim in §5.3 rests on
    /// — an optimisation paid for in evidence. It is sent after the record for the same reason
    /// everything else is: the repaint it provokes must not be recorded ahead of the new geometry.
    pub fn resize(&self, size: WinSize) -> io::Result<()> {
        let Some(_permit) = self.control.admit_resize()? else {
            return Ok(());
        };
        let ticket = {
            let mut q = self.shared.resize.lock().unwrap_or_else(|e| e.into_inner());
            q.requested += 1;
            q.pending = Some(size);
            q.requested
        };
        self.shared.resize_done.notify_all();

        let mut q = self.shared.resize.lock().unwrap_or_else(|e| e.into_inner());
        while q.applied < ticket && q.reader {
            q = match self.shared.resize_done.wait(q) {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
        }
        if q.applied < ticket {
            // No reader to do it. Take the pending size back and do the work here, in the same
            // order and under the same lock.
            let size = q.pending.take().unwrap_or(size);
            q.applied = q.requested;
            let mut recorders = self
                .shared
                .recorders
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.shared.set_size(&self.master, size)?;
            let durable_failure = recorders
                .inject_durable_failure()
                .and_then(|()| recorders.durable.append_resize(size))
                .err();
            let retention = self.shared.retain_resize(size);
            let cast_result = self.shared.resize_cast(&mut recorders.cast, size);
            if let Err(error) = &cast_result {
                recorders.note_cast_failure("resize", error);
            }
            drop(recorders);
            if let Some(error) = durable_failure {
                eprintln!("marion: {error}");
            }
            self.shared.finish_resize_retention(retention);
            cast_result?;
        } else if let Some(failure) = q.failure.take() {
            return Err(io::Error::other(failure));
        }
        drop(q);

        // The pgid is read out and the lock released before the signal, so this never waits on
        // `shutdown`'s reap — and `shutdown` takes the child out first, so after it there is no
        // group left to notify and none is invented.
        let pgid = self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(PtyChild::pgid);
        if let Some(pgid) = pgid {
            // SAFETY: a signal to a process group id; no pointers. A group that has already exited
            // answers `ESRCH`, which is not an error here.
            unsafe { killpg(pgid, SIGWINCH) };
        }
        Ok(())
    }

    /// **Kill and reap the child, then close the master. In that order, and it is a truthfulness
    /// requirement rather than a tidiness one.**
    ///
    /// Closing the master first hangs up the slave, the kernel SIGHUPs the child's foreground
    /// process group, and every live pty node's record then reads *"the terminal hung up"* — when
    /// what actually happened is that marion decided to stop. The recorded cause of death would be
    /// a fabrication produced by marion's own teardown, on every node, every time.
    ///
    /// Joining the reader thread between the two is what makes "then" mean anything: the thread
    /// holds an `Arc<PtyMaster>`, so the fd cannot close until it has finished, and it finishes at
    /// the EOF the reaped child produces.
    /// **Idempotent.** A second call finds no child and no reader and writes a second `x` record —
    /// which is why the caller is `launch_terminal`'s single exit path rather than every owner of
    /// the `Arc`.
    pub fn shutdown(&self) -> io::Result<Option<std::process::ExitStatus>> {
        self.shutdown_with_timeout_outcome(false)
    }

    /// Shut down after the bounded root wait and preserve whether that wait expired in the typed
    /// terminal End. All other shutdown callers use [`Self::shutdown`] and therefore cannot be
    /// mislabeled as timeouts.
    pub(crate) fn shutdown_with_timeout_outcome(
        &self,
        timed_out: bool,
    ) -> io::Result<Option<std::process::ExitStatus>> {
        self.control.seal();
        // **Taken out, not borrowed.** Holding the lock across `wait` would park a concurrent
        // `resize` from an attached client behind however long the child takes to die, and the
        // client cannot know that is what it is waiting for.
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
        let status = match child.as_mut() {
            None => None,
            // Through `PtyChild`'s own idempotent call, so this and the `Drop` net below cannot
            // drift — and so the `Drop` that runs when the local below goes out of scope finds the
            // process already reaped instead of signalling a pid the kernel may have reissued.
            Some(child) => {
                #[cfg(test)]
                self.observe_before_child_sweep_hook();
                Some(child.kill_and_reap()?)
            }
        };
        // Killing/reaping closes the slave first, which unblocks an already-admitted master write.
        // The wait is host-local and never runs under the global pane registry lock. Once it
        // returns, no admitted input or resize can append cast/splice state ahead of End and `x`.
        self.control.wait_drained();
        let input_delivery_failure = self
            .control
            .wait_input_deliveries(Instant::now() + CONTROL_DELIVERY_GRACE);
        #[cfg(test)]
        self.observe_before_reader_stop_hook();
        let has_reader = self
            .reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        let reader_finished = if has_reader {
            #[cfg(test)]
            self.observe_reader_drain_hook();
            self.reader_completion
                .wait_until(Instant::now() + crate::run::DRAIN_GRACE)
        } else {
            false
        };
        if !reader_finished {
            self.stopped.store(true, Ordering::SeqCst);
        }
        let reader = self.reader.lock().unwrap_or_else(|e| e.into_inner()).take();
        let (reader_disposition, reader_failure) = match reader {
            Some(t) => match t.join() {
                Ok(Ok(())) => (stream::ReaderDisposition::CleanEof, None),
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => (
                    stream::ReaderDisposition::ForcedStop,
                    Some(format!(
                        "pty reader was forced before terminal End: {error}"
                    )),
                ),
                Ok(Err(error)) => (
                    stream::ReaderDisposition::ReadError,
                    Some(format!("pty reader failed before terminal End: {error}")),
                ),
                Err(_) => (
                    stream::ReaderDisposition::ReadError,
                    Some("pty reader thread panicked before terminal End".to_string()),
                ),
            },
            None => (
                stream::ReaderDisposition::ForcedStop,
                Some("pty host had no reader completion proof".to_string()),
            ),
        };
        // Kernel EOF is necessary but no longer sufficient to publish pane End: an opaque input
        // selected before closing may still be resolving outside the recorder lock. The delivery
        // barrier above either observed its outcome or visibly failed its exact socket on grace
        // expiry, so End can only become visible after both facts are settled.
        if reader_disposition == stream::ReaderDisposition::CleanEof {
            self.shared.retain_end();
        }
        // `x` is synced first. The typed End then records whether that compatibility write, the
        // reader, and the child status collectively proved a replayable terminal outcome. Even an
        // ineligible forced/read/cast outcome is sealed when storage remains healthy, so recovery
        // can distinguish a known bad ending from a torn file.
        let input_complete = input_delivery_failure.is_none();
        let (cast_result, stream_completion) = {
            let mut recorders = self
                .shared
                .recorders
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let cast_result = recorders.cast.exit(&exit_word(status));
            if let Err(error) = &cast_result {
                recorders.note_cast_failure("exit", error);
            }
            let cast_complete = recorders.cast_failure.is_none();
            let stream_completion = status.as_ref().map(|status| {
                let mut outcome = recorders.durable.qualify_outcome(terminal_outcome(
                    status,
                    timed_out,
                    reader_disposition,
                    cast_complete,
                ));
                outcome.stream_complete &= input_complete;
                recorders.durable.seal(outcome).and_then(|()| {
                    if outcome.replay_eligible() {
                        Ok(())
                    } else {
                        Err(format!(
                            "authoritative PTY stream sealed with an ineligible terminal outcome: {outcome:?}"
                        ))
                    }
                })
            });
            (cast_result, stream_completion)
        };

        let completion_failure = reader_failure
            .or(input_delivery_failure)
            .or_else(|| stream_completion.and_then(Result::err));
        let eligibility = completion_failure.map_or_else(
            || {
                if let Some(error) = self
                    .shared
                    .splice_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                {
                    Err(format!("pty retention failed before completion: {error}"))
                } else {
                    match self.shared.splice.completion_charge() {
                        Err(error) => Err(format!(
                            "pty retention completion charge failed: {error}"
                        )),
                        Ok(None) => Err(
                            "pty retention ended without a terminal End record".to_string(),
                        ),
                        Ok(Some(splice_charge)) => completed_host_charge_from(
                            splice_charge,
                            self.shared.splice.subscriber_limit(),
                        )
                        .ok_or_else(|| {
                            "pty retention completion charge overflowed conservative host, registry, or PaneStreams reserve"
                                .to_string()
                        }),
                    }
                }
            },
            Err,
        );
        let mut completion = self
            .replay_completion
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if completion.is_none() {
            *completion = Some(eligibility);
        }
        self.control.mark_ended();
        cast_result?;
        Ok(status)
    }

    pub(crate) fn seal_controls(&self) {
        self.control.seal();
    }
}

/// asciicast v3's `x` payload: the exit status, as a string a human can read back.
fn exit_word(status: Option<std::process::ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status {
        None => "detached".to_string(),
        Some(s) => match (s.code(), s.signal()) {
            (Some(c), _) => c.to_string(),
            (None, Some(sig)) => format!("signal {sig}"),
            (None, None) => "unknown".to_string(),
        },
    }
}

fn terminal_outcome(
    status: &std::process::ExitStatus,
    timed_out: bool,
    reader: stream::ReaderDisposition,
    cast_complete: bool,
) -> stream::TerminalOutcome {
    use std::os::unix::process::ExitStatusExt;
    stream::TerminalOutcome {
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
        reader,
        cast_complete,
        stream_complete: true,
    }
}

impl Drop for PtyHost {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(child) = self
            .child
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            // A host dropped without `shutdown` still must not leave a live child behind — that is
            // §9's M2 criterion, and a leaked pty child is exactly the untracked live process it
            // forbids. This is the safety net, not the path: `shutdown` is what records why.
            //
            // **Here rather than left to `PtyChild::drop`, and the ordering is why.** That `Drop`
            // would run after this body, i.e. after the reader thread is joined below — and the
            // reader blocks in `read` until the master sees EOF, which it does not until the child
            // is dead. Relying on the field's own `Drop` would deadlock the join.
            let _ = child.kill_and_reap();
        }
        if let Some(t) = self
            .reader
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = t.join();
        }
    }
}

fn read_loop(master: &PtyMaster, shared: &Shared, stopped: &AtomicBool) -> io::Result<()> {
    let _gone = ReaderGone(shared);
    let mut buf = vec![0u8; READ_CHUNK];
    let mut utf8 = Utf8Stream::default();
    let mut probes = ProbeScan::default();
    let reached_eof = loop {
        // **At the top, and it drains before it resizes.** See [`apply_pending_resize`]: the whole
        // of the labelling fix is that everything the node emitted at the old geometry is in the
        // file before the `r` that ends it.
        apply_pending_resize(master, shared, &mut buf, &mut utf8, &mut probes);
        let n = match master.read(&mut buf) {
            // EOF: the last slave closed. `PtyMaster::read` reports Linux's `EIO` this way too.
            Ok(0) => break true,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                #[cfg(test)]
                if let Some(hook) = shared
                    .reader_would_block_hook
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    hook();
                }
                // A stop request bounds teardown, but it is not terminal-stream evidence. A
                // descendant may still hold the slave open and write later, so leave without End
                // and report the replay as incomplete. Only the `Ok(0)` arm above proves EOF (and
                // includes the terminal `EIO` normalized by `PtyMaster::read`).
                if stopped.load(Ordering::SeqCst) {
                    break false;
                }
                std::thread::sleep(POLL);
                continue;
            }
            Err(e) => {
                eprintln!("marion: pty read failed: {e}");
                return Err(e);
            }
        };
        record_chunk(shared, &mut utf8, &mut probes, &buf[..n]);
    };
    let tail = utf8.finish();
    if !tail.is_empty() {
        let mut recorders = shared.recorders.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = recorders.cast.output(&tail) {
            recorders.note_cast_failure("output tail", &error);
            eprintln!("marion: pty.cast write failed: {error}");
        }
        drop(recorders);
        shared.emit(&tail);
    }
    if reached_eof {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "pty reader stopped before kernel EOF",
        ))
    }
}

/// **Hands the resize job back when the reader leaves, however it leaves.**
///
/// A `resize` arriving after the reader has gone would otherwise wait on a `Condvar` nothing will
/// ever notify. A plain pair of statements at the end of [`read_loop`] covers the return paths and
/// not an unwind — and a panic on the reader thread is precisely when a caller blocked on it is
/// least able to work out what happened. See [`ResizeQueue::reader`].
struct ReaderGone<'a>(&'a Shared);

impl Drop for ReaderGone<'_> {
    fn drop(&mut self) {
        self.0
            .resize
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reader = false;
        self.0.resize_done.notify_all();
    }
}

/// One chunk of pty output, counted, scanned for probes, decoded and recorded.
///
/// Extracted from [`read_loop`] because [`apply_pending_resize`] has to do exactly the same thing
/// while it drains, and a second copy of it is how the drain and the loop would come to disagree
/// about the UTF-8 carry — which is S11's *"a `read()` is not a frame"* defect reintroduced by
/// duplication rather than by ignorance.
fn record_chunk(shared: &Shared, utf8: &mut Utf8Stream, probes: &mut ProbeScan, chunk: &[u8]) {
    let seen = probes.count(chunk);
    if seen > 0 {
        shared.probes.fetch_add(seen, Ordering::SeqCst);
    }
    let text = utf8.push(chunk);
    // Raw output is authoritative and is committed before either compatibility projection. The
    // same per-host lock orders it against every resize and input-evidence record, but is released
    // before listener delivery or any other subsystem lock is acquired.
    let (durable_failure, cast_failure) = {
        let mut recorders = shared.recorders.lock().unwrap_or_else(|e| e.into_inner());
        let durable_failure = recorders
            .inject_durable_failure()
            .and_then(|()| recorders.durable.append_output(chunk))
            .err();
        let cast_failure = if text.is_empty() {
            None
        } else {
            recorders.cast.output(&text).err()
        };
        if let Some(error) = &cast_failure {
            recorders.note_cast_failure("output", error);
        }
        (durable_failure, cast_failure)
    };
    if let Some(error) = durable_failure {
        eprintln!("marion: {error}");
    }
    shared.retain_output(chunk);
    // Published last: tests and diagnostics use this counter as the completion witness for the
    // whole recording step, not merely for the kernel read returning.
    shared.bytes.fetch_add(chunk.len() as u64, Ordering::SeqCst);
    if text.is_empty() {
        return;
    }
    // **`pty.cast` only. Never `events.jsonl`.** See the module doc's sibling argument in
    // `no_pty_byte_reaches_events_jsonl`: §3.4 said terminal bytes land in `events.jsonl` as
    // `Payload::Raw`, and three things say otherwise — `Payload::Raw`'s own doc defines it as
    // *a line of stdout*; `EventReader::open_path` reads the whole file on every attach, so
    // every attach to any node would pay for a megabyte of escape sequences; and `event.rs:40`
    // says `mono_ns` exists to **align** the two files rather than to merge them. §3.4 has been
    // updated.
    if let Some(error) = cast_failure {
        eprintln!("marion: pty.cast write failed: {error}");
    }
    shared.emit(&text);
}

/// How many reads the drain below will make before resizing anyway.
///
/// A bound and not a policy: a node writing faster than marion reads must not be able to postpone a
/// resize indefinitely, and a client whose window is the wrong size is a worse failure than a few
/// mislabelled bytes. 64 reads of [`READ_CHUNK`] is 4 MiB, which no interactive repaint approaches.
const RESIZE_DRAIN_READS: usize = 64;

/// **Drain, then `TIOCSWINSZ`, then write the `r` — all on the reader thread.**
///
/// Called from the top of [`read_loop`], which is the only thread that may read the master.
///
/// # Why the drain is the fix
///
/// `PtyHost::resize` used to do the two steps itself: write the `r`, `fsync` it, release the cast
/// lock, then issue the ioctl. The order was chosen for a real reason — an `r` for a resize that
/// did not happen replays as a harmless reflow, whereas output painted at a size no record mentions
/// is unreplayable — and that reasoning is kept. What was wrong was the *claim*: for the whole of
/// that window the terminal was still the old size, so everything the node wrote in it was recorded
/// after a record saying it was the new one.
///
/// Swapping the two statements does not fix it, and this is the part that is easy to miss: the
/// mislabelled bytes are the ones the recorder has not read yet. They may be sitting in the pty
/// buffer, and no ordering of two statements on the *caller's* thread can see them. Only the reader
/// can, so the resize happens here, and it happens after this function has read the master empty.
/// Everything the node emitted at the old geometry is therefore in the file **before** the `r`, by
/// construction rather than by timing.
///
/// The ioctl and the record are then taken under one hold of the cast lock. The ioctl provokes a
/// `SIGWINCH` and the node can begin repainting at the new size immediately; without the lock that
/// repaint could be written ahead of the record that explains it, which is the same defect
/// mirrored. With it, nothing can be recorded between them.
fn apply_pending_resize(
    master: &PtyMaster,
    shared: &Shared,
    buf: &mut [u8],
    utf8: &mut Utf8Stream,
    probes: &mut ProbeScan,
) {
    let size = {
        let mut q = shared.resize.lock().unwrap_or_else(|e| e.into_inner());
        match q.pending.take() {
            Some(size) => size,
            None => return,
        }
    };

    // The hook writes at the *old* geometry from inside the drain, so its bytes are read and
    // recorded by this very loop, before the ioctl below. See `PtyHost::set_resize_hook`.
    #[cfg(test)]
    {
        let hook = shared.resize_hook.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(hook) = hook.as_ref() {
            hook();
        }
    }

    for _ in 0..RESIZE_DRAIN_READS {
        match master.read(buf) {
            Ok(0) => break,
            Ok(n) => record_chunk(shared, utf8, probes, &buf[..n]),
            // Empty: everything the node emitted at the old size is now in the file.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }

    let (outcome, retention, durable_failure) = {
        let mut recorders = shared.recorders.lock().unwrap_or_else(|e| e.into_inner());
        match shared.set_size(master, size) {
            Ok(()) => {
                let durable_failure = recorders
                    .inject_durable_failure()
                    .and_then(|()| recorders.durable.append_resize(size))
                    .err();
                let retention = shared.retain_resize(size);
                let outcome = shared.resize_cast(&mut recorders.cast, size);
                if let Err(error) = &outcome {
                    recorders.note_cast_failure("resize", error);
                }
                (outcome, Some(retention), durable_failure)
            }
            Err(error) => (Err(error), None, None),
        }
    };
    if let Some(error) = durable_failure {
        eprintln!("marion: {error}");
    }
    if let Some(retention) = retention {
        shared.finish_resize_retention(retention);
    }

    let mut q = shared.resize.lock().unwrap_or_else(|e| e.into_inner());
    q.applied = q.requested;
    // Kept rather than printed: the caller is blocked on this and is the one that can report it.
    q.failure = outcome.err().map(|e| e.to_string());
    drop(q);
    shared.resize_done.notify_all();
}

#[cfg(test)]
mod tests;
