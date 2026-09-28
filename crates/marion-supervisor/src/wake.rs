//! **Event-driven waiting**: the pieces a long-lived loop blocks on instead of a sleep.
//!
//! A supervisor runs all day, and a loop that wakes every few milliseconds to ask "anything new?"
//! costs a context switch per wake whether or not the answer is yes. The loops that used to do that
//! — the registry follower, the accept loop, a connection writer — now block on a descriptor or a
//! condition variable and are woken by the thing that changed:
//!
//! - [`Pipe`] is a self-pipe: a pollable descriptor another thread makes readable. It is what a
//!   `poll(2)` loop waits on beside its sockets.
//! - [`Signal`] is a broadcast "this changed" with a generation counter. A thread can block on it
//!   directly ([`Signal::wait_past`]) or attach a [`Pipe`] so that the change also wakes a `poll`.
//! - [`Watch`] makes a **file** pollable: kqueue `EVFILT_VNODE` on macOS and the BSDs, inotify on
//!   Linux. It is how a process learns that *another* process appended to a journal.
//!
//! **None of these is ever the only path to correctness.** Every loop that waits on one keeps a
//! bounded safety timeout, so a wake that is lost — a platform without a watch, a file replaced
//! under the watch, a descriptor limit — costs latency up to that bound and never a hang. What the
//! wakes buy is that the bound can be seconds rather than milliseconds.

use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// **The one bounded poll**, for a wait nothing can wake: a child's exit where the caller holds no
/// descriptor for it, a file another process creates without a watch. `cond` is asked every `step`
/// until `budget` runs out, and once more after it, so a transition that lands exactly at the
/// deadline is still seen; the answer is the condition's, never the clock's.
///
/// Every sleep-until-deadline loop in the one-shot paths goes through here, so the efficiency
/// check has one site to hold to a clear deadline instead of one per copy.
pub fn poll_until(budget: Duration, step: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(step);
    }
    cond()
}

/// `Child::wait` with a ceiling, blocking on the child's exit ([`ProcExit`]) rather than re-asking on
/// a timer. `None` is "still running at the deadline", which is a finding and not an error — or a
/// child that can no longer be waited on at all.
pub fn wait_bounded(
    child: &mut std::process::Child,
    budget: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + budget;
    let exit = ProcExit::new(child.id() as i32);
    loop {
        match step_child(child, &exit, &[], Some(deadline)) {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
}

/// A self-pipe: [`Self::wake`] from any thread makes [`Self::fd`] readable until [`Self::drain`].
///
/// A socket pair rather than `pipe(2)`: it is created close-on-exec by the standard library on every
/// platform marion builds for, where `pipe2` is Linux-only. Both ends are non-blocking, so a wake
/// never blocks its caller and a drain never blocks the waiter.
///
/// **Wakes coalesce.** `pending` is set by the first wake after a drain and only that wake writes a
/// byte, so a burst of appends costs one byte in the buffer rather than filling it. The waiter's
/// rule is *drain, then look*: whatever changed before a wake is visible to the look that follows
/// the drain, and a wake that lands after the drain leaves the descriptor readable for the next
/// wait.
#[derive(Debug)]
pub struct Pipe {
    read: UnixStream,
    write: UnixStream,
    pending: AtomicBool,
}

impl Pipe {
    pub fn new() -> std::io::Result<Pipe> {
        let (read, write) = UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        Ok(Pipe {
            read,
            write,
            pending: AtomicBool::new(false),
        })
    }

    /// Make the descriptor readable. Never blocks and never fails visibly: a full buffer already
    /// means "readable", which is all a wake has to achieve.
    pub fn wake(&self) {
        use std::io::Write;
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = (&self.write).write(&[1]);
        }
    }

    /// Consume every pending wake. Call **before** examining the state the wakes are about.
    ///
    /// **Read, then clear `pending`** — never the other way round. Cleared first, a wake landing
    /// between the clear and the read would write its byte, have it consumed by this read, and
    /// leave `pending` set with the descriptor empty: every later wake would see `pending` and
    /// write nothing, and the waiter would never be woken again. In this order a wake during the
    /// read sees `pending` still set and writes nothing, which is safe because what it announces
    /// happened before the caller's look; a wake after the clear writes a byte.
    pub fn drain(&self) {
        use std::io::Read;
        let mut buf = [0u8; 64];
        while let Ok(n) = (&self.read).read(&mut buf) {
            if n < buf.len() {
                break;
            }
        }
        self.pending.store(false, Ordering::Release);
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }
}

/// A broadcast "something changed", with a generation so a waiter cannot miss a change that
/// happened between its look and its wait.
#[derive(Debug, Default)]
pub struct Signal {
    state: Mutex<SignalState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct SignalState {
    generation: u64,
    pipes: Vec<Weak<Pipe>>,
}

impl Signal {
    pub fn new() -> Arc<Signal> {
        Arc::new(Signal::default())
    }

    /// Advance the generation, wake every thread in [`Self::wait_past`], and wake every attached
    /// pipe. Attached pipes whose owner has gone are forgotten here.
    pub fn notify(&self) {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        state.pipes.retain(|p| match p.upgrade() {
            Some(p) => {
                p.wake();
                true
            }
            None => false,
        });
        drop(state);
        self.changed.notify_all();
    }

    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Block until the generation is no longer `seen`, or `timeout` passes. Returns the generation
    /// at return, so a caller loops on `seen = wait_past(seen, …)` and re-examines its state after
    /// every return — timeout or not, since a timeout is the safety path.
    pub fn wait_past(&self, seen: u64, timeout: Duration) -> u64 {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.lock();
        while state.generation == seen {
            let left = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if left.is_zero() {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        state.generation
    }

    /// Wake `pipe` on every future [`Self::notify`], for as long as its owner holds it.
    pub fn attach(&self, pipe: &Arc<Pipe>) {
        self.lock().pipes.push(Arc::downgrade(pipe));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SignalState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A stop flag whose raising also wakes whoever is waiting on it: [`Self::store`] of `true` rings a
/// [`Pipe`] a `poll` loop can include through [`Self::fd`]. `load`/`store` mirror `AtomicBool`'s so
/// it drops in where one was.
#[derive(Debug)]
pub struct Flag {
    set: AtomicBool,
    wake: Option<Pipe>,
}

impl Default for Flag {
    fn default() -> Self {
        Flag::new()
    }
}

impl Flag {
    pub fn new() -> Flag {
        Flag {
            set: AtomicBool::new(false),
            wake: Pipe::new().ok(),
        }
    }

    pub fn load(&self, order: Ordering) -> bool {
        self.set.load(order)
    }

    /// Raising rings the descriptor; lowering drains it, so a flag that is reused (a worker
    /// parked and restarted) does not leave its next waiter woken by the last raise. Lower it only
    /// while nobody is waiting on it.
    pub fn store(&self, value: bool, order: Ordering) {
        self.set.store(value, order);
        if let Some(wake) = &self.wake {
            if value {
                wake.wake();
            } else {
                wake.drain();
            }
        }
    }

    /// [`Self::store`], returning the previous value.
    pub fn swap(&self, value: bool, order: Ordering) -> bool {
        let was = self.set.swap(value, order);
        if let Some(wake) = &self.wake {
            if value {
                wake.wake();
            } else {
                wake.drain();
            }
        }
        was
    }

    /// Readable once the flag has been raised. `None` if no descriptor could be had, in which case
    /// a waiter's timeout is the whole of its latency.
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.wake.as_ref().map(|w| w.fd())
    }
}

/// Block until one of `fds` is readable or `timeout` passes (`None` waits indefinitely), and say
/// which were. An `EINTR` is an early return with nothing ready — every caller re-examines its
/// state after a wait anyway.
pub fn wait_readable(fds: &[BorrowedFd<'_>], timeout: Option<Duration>) -> Vec<bool> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut polls: Vec<PollFd<'_>> = fds
        .iter()
        .map(|fd| PollFd::new(fd, PollFlags::IN))
        .collect();
    let timeout = timeout.map(|t| Timespec {
        tv_sec: t.as_secs().min(i64::MAX as u64) as i64,
        tv_nsec: t.subsec_nanos() as _,
    });
    match poll(&mut polls, timeout.as_ref()) {
        Ok(_) => polls
            .iter()
            .map(|p| {
                p.revents()
                    .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
            })
            .collect(),
        Err(_) => vec![false; fds.len()],
    }
}

/// A **file** made pollable: readable when the file is written, extended, created, removed or
/// renamed — or, for a directory, when its entries change (one is created, removed or renamed).
///
/// The target need not exist, nor its directory. Until it does the watch is on its nearest
/// existing ancestor, whose entries changing is how a creation shows; [`Self::rearm`] moves the
/// watch down onto the file once it exists, and back off it when it is removed or replaced. Where no mechanism is available (another
/// platform, or a descriptor limit) [`Self::fd`] is `None` and the caller's safety timeout is the
/// whole of its wait — degraded latency, never a missed record.
pub struct Watch {
    target: PathBuf,
    inner: Option<imp::Inner>,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch")
            .field("target", &self.target)
            .field("armed", &self.inner.is_some())
            .finish()
    }
}

impl Watch {
    pub fn new(target: &Path) -> Watch {
        let mut watch = Watch {
            target: target.to_path_buf(),
            inner: imp::Inner::new(),
        };
        watch.rearm();
        watch
    }

    /// The descriptor to include in a `poll`. `None` means "nothing will wake you; use your timeout".
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.inner.as_ref().map(|i| i.fd())
    }

    /// Consume the pending events and re-arm onto whatever the target now is. Call after the
    /// descriptor was readable, **before** reading the file.
    pub fn rearm(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            inner.drain_and_rearm(&self.target);
        }
    }
}

/// Where a watch for `target` must sit: the target itself once it exists, else its **nearest
/// existing ancestor** — a node's `events.jsonl` can be followed before its agent directory is
/// made, and the creation of each directory on the way down is an entry change in the one above,
/// which moves the watch a level closer at the next re-arm.
fn watch_point(target: &Path) -> Option<PathBuf> {
    let mut at = target;
    loop {
        if at.exists() {
            return Some(at.to_path_buf());
        }
        at = at.parent().filter(|p| !p.as_os_str().is_empty())?;
    }
}

/// How many times one re-arm re-chooses its watch point before settling for the last choice. Each
/// round is a directory created (or removed) under it mid-registration; past the limit the next
/// event re-arms again.
const REARM_LIMIT: usize = 8;

/// A **child's exit** made pollable, without reaping it: readable once the process has exited.
///
/// kqueue `EVFILT_PROC` `NOTE_EXIT` on macOS and the BSDs, a pidfd on Linux. Neither consumes the
/// wait status, so a caller that must sweep a still-pinned process group before reaping (a pty
/// node) can wait on it exactly as a caller that reaps at once can.
///
/// **Register first, then look.** A process that is already gone when the watch is made may leave
/// it unarmed (kqueue refuses a pid it cannot find) and never readable, so every waiter checks the
/// exit itself after creating the watch and after every wake; the watch only decides how long the
/// wait between checks is. [`Self::fd`] is `None` where no mechanism exists, and [`wait_until`]
/// then re-checks at [`DEGRADED_RECHECK`].
///
/// The pid must be the caller's own unreaped child, so it cannot be reissued while watched.
pub struct ProcExit {
    inner: Option<proc_imp::Inner>,
}

impl std::fmt::Debug for ProcExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcExit")
            .field("armed", &self.inner.is_some())
            .finish()
    }
}

impl ProcExit {
    pub fn new(pid: i32) -> ProcExit {
        ProcExit {
            inner: proc_imp::Inner::new(pid),
        }
    }

    /// The descriptor to include in a `poll`. `None` means "nothing will wake you".
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.inner.as_ref().map(|i| i.fd())
    }
}

/// A **signal's arrival** made pollable, for a signal the caller keeps blocked and inspects with
/// `sigpending` (so a handler never runs and nothing else would wake a `poll`).
///
/// kqueue `EVFILT_SIGNAL` on macOS and the BSDs, which records every attempt to deliver the
/// signal — blocked or not — from its registration on; a signalfd on Linux, readable while the
/// signal is pending for the calling thread and **never read**, since a read would consume it.
/// Create it on the thread that blocks the signal.
///
/// Register, then look: a signal already pending when the watch was made may not show on it, so a
/// waiter checks `sigpending` after creating the watch and after every wake, as with
/// [`ProcExit`]. `None` from [`Self::fd`] means no mechanism; [`wait_until`] then re-checks at
/// [`DEGRADED_RECHECK`].
pub struct SignalWatch {
    inner: Option<signal_imp::Inner>,
}

impl std::fmt::Debug for SignalWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalWatch")
            .field("armed", &self.inner.is_some())
            .finish()
    }
}

impl SignalWatch {
    pub fn new(signal: i32) -> SignalWatch {
        SignalWatch {
            inner: signal_imp::Inner::new(signal),
        }
    }

    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.inner.as_ref().map(|i| i.fd())
    }

    /// Consume the postings seen so far, so the descriptor is quiet until the next one. Call after
    /// it woke a wait and **before** looking at `sigpending`. A no-op on a signalfd, whose
    /// readiness is the pending state itself: consuming that would consume the signal.
    pub fn drain(&self) {
        if let Some(inner) = &self.inner {
            inner.drain();
        }
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod signal_imp {
    //! kqueue `EVFILT_SIGNAL`: readable once the signal has been posted since registration or
    //! the last [`Inner::drain`].
    use rustix::event::kqueue::{Event, EventFilter, EventFlags, kevent, kqueue};
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

    pub(super) struct Inner {
        kq: OwnedFd,
    }

    impl Inner {
        pub(super) fn new(signal: i32) -> Option<Inner> {
            let kq = kqueue().ok()?;
            let signal = rustix::process::Signal::from_named_raw(signal)?;
            let change = Event::new(
                EventFilter::Signal { signal, times: 0 },
                EventFlags::ADD,
                std::ptr::null_mut(),
            );
            let mut none: Vec<Event> = Vec::new();
            // SAFETY: a signal filter names a signal, not a descriptor.
            unsafe { kevent(&kq, &[change], &mut none, Some(std::time::Duration::ZERO)) }.ok()?;
            Some(Inner { kq })
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            self.kq.as_fd()
        }

        pub(super) fn drain(&self) {
            let mut buf: Vec<Event> = Vec::with_capacity(4);
            loop {
                buf.clear();
                // SAFETY: the only registration is a signal filter, which names no descriptor.
                let got = unsafe {
                    kevent(
                        &self.kq,
                        &[],
                        rustix::buffer::spare_capacity(&mut buf),
                        Some(std::time::Duration::ZERO),
                    )
                };
                if !matches!(got, Ok(n) if n > 0) {
                    return;
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod signal_imp {
    //! A signalfd over one signal, polled and never read.
    use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};

    /// glibc's and musl's `sigset_t`: 1024 bits. The kernel reads the first 64.
    type SigSet = [u64; 16];

    unsafe extern "C" {
        fn signalfd(
            fd: std::ffi::c_int,
            mask: *const SigSet,
            flags: std::ffi::c_int,
        ) -> std::ffi::c_int;
    }

    /// `SFD_CLOEXEC | SFD_NONBLOCK`: `O_CLOEXEC` and `O_NONBLOCK` on every Linux marion builds for.
    const SFD_FLAGS: std::ffi::c_int = 0o2_000_000 | 0o4_000;

    pub(super) struct Inner {
        fd: OwnedFd,
    }

    impl Inner {
        pub(super) fn new(signal: i32) -> Option<Inner> {
            let bit = u32::try_from(signal).ok()?.checked_sub(1)?;
            let mut mask: SigSet = [0; 16];
            *mask.get_mut((bit / 64) as usize)? |= 1u64 << (bit % 64);
            // SAFETY: `mask` is a live, correctly sized `sigset_t`; -1 asks for a new descriptor.
            let fd = unsafe { signalfd(-1, &mask, SFD_FLAGS) };
            if fd < 0 {
                return None;
            }
            // SAFETY: `signalfd` just returned this descriptor and nothing else owns it.
            Some(Inner {
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
            })
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            self.fd.as_fd()
        }

        pub(super) fn drain(&self) {}
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "linux"
)))]
mod signal_imp {
    //! No signal watch: waiters re-check at [`super::DEGRADED_RECHECK`].
    use std::os::fd::BorrowedFd;

    pub(super) struct Inner(std::convert::Infallible);

    impl Inner {
        pub(super) fn new(_signal: i32) -> Option<Inner> {
            None
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            match self.0 {}
        }

        pub(super) fn drain(&self) {
            match self.0 {}
        }
    }
}

/// **One step of waiting for a child**: its status if it has exited; otherwise block until it
/// exits (`exit`), one of `also` is readable, or `deadline` passes, and look once more. Reaps, as
/// `Child::try_wait` does. A caller loops on it and re-examines its own state between steps — the
/// step never sleeps on a timer, so the loop is exactly as busy as the child and `also` are.
pub fn step_child(
    child: &mut std::process::Child,
    exit: &ProcExit,
    also: &[Option<BorrowedFd<'_>>],
    deadline: Option<Instant>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    if let Some(status) = child.try_wait()? {
        return Ok(Some(status));
    }
    let mut fds = vec![exit.fd()];
    fds.extend_from_slice(also);
    wait_until(&fds, deadline);
    child.try_wait()
}

/// How often a wait re-checks when one of its sources has no descriptor: a platform with neither
/// kqueue nor inotify/pidfd, a Linux older than pidfd (5.3), or a descriptor limit. **Degraded
/// latency, never the normal path** — on macOS and on any current Linux every source marion waits
/// on has a descriptor and a wait sleeps until its event or its deadline.
pub const DEGRADED_RECHECK: Duration = Duration::from_millis(50);

/// Block until one of `fds` is readable or `deadline` passes (`None`: no deadline), and say which
/// were. A `None` entry is a source that could not be made pollable, and caps the wait at
/// [`DEGRADED_RECHECK`] so that source is still looked at. Callers re-examine their state after
/// every return, as with [`wait_readable`].
pub fn wait_until(fds: &[Option<BorrowedFd<'_>>], deadline: Option<Instant>) -> Vec<bool> {
    let mut timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
    if fds.iter().any(Option::is_none) {
        timeout = Some(timeout.map_or(DEGRADED_RECHECK, |t| t.min(DEGRADED_RECHECK)));
    }
    let present: Vec<BorrowedFd<'_>> = fds.iter().flatten().copied().collect();
    let mut ready = wait_readable(&present, timeout).into_iter();
    fds.iter()
        .map(|fd| fd.is_some() && ready.next().unwrap_or(false))
        .collect()
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod proc_imp {
    //! kqueue `EVFILT_PROC` `NOTE_EXIT`, one-shot: the kqueue is readable once the exit is pending
    //! and stays readable, since nothing drains it.
    use rustix::event::kqueue::{Event, EventFilter, EventFlags, ProcessEvents, kevent, kqueue};
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

    pub(super) struct Inner {
        kq: OwnedFd,
    }

    impl Inner {
        pub(super) fn new(pid: i32) -> Option<Inner> {
            let pid = rustix::process::Pid::from_raw(pid)?;
            let kq = kqueue().ok()?;
            let change = Event::new(
                EventFilter::Proc {
                    pid,
                    flags: ProcessEvents::EXIT,
                },
                EventFlags::ADD | EventFlags::ONESHOT,
                std::ptr::null_mut(),
            );
            let mut none: Vec<Event> = Vec::new();
            // SAFETY: a process filter names a pid, not a descriptor, so there is no descriptor
            // lifetime for the registration to outlive.
            unsafe { kevent(&kq, &[change], &mut none, Some(std::time::Duration::ZERO)) }.ok()?;
            Some(Inner { kq })
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            self.kq.as_fd()
        }
    }
}

#[cfg(target_os = "linux")]
mod proc_imp {
    //! A pidfd: readable once the process has exited, reaped or not.
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

    pub(super) struct Inner {
        fd: OwnedFd,
    }

    impl Inner {
        pub(super) fn new(pid: i32) -> Option<Inner> {
            let pid = rustix::process::Pid::from_raw(pid)?;
            let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok()?;
            Some(Inner { fd })
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            self.fd.as_fd()
        }
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "linux"
)))]
mod proc_imp {
    //! No process-exit mechanism: waiters re-check at [`super::DEGRADED_RECHECK`].
    use std::os::fd::BorrowedFd;

    pub(super) struct Inner(std::convert::Infallible);

    impl Inner {
        pub(super) fn new(_pid: i32) -> Option<Inner> {
            None
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            match self.0 {}
        }
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
mod imp {
    //! kqueue `EVFILT_VNODE`. The kqueue descriptor itself is what callers poll: it is readable
    //! while events are pending.
    use rustix::event::kqueue::{Event, EventFilter, EventFlags, VnodeEvents, kevent, kqueue};
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
    use std::path::{Path, PathBuf};

    pub(super) struct Inner {
        kq: OwnedFd,
        /// The open vnode being watched and the path it was opened at: the file itself, or its
        /// nearest existing ancestor while it does not exist. Closing it removes its registration,
        /// so replacing it is the whole of a re-arm.
        watched: Option<(File, PathBuf)>,
    }

    impl Inner {
        pub(super) fn new() -> Option<Inner> {
            Some(Inner {
                kq: kqueue().ok()?,
                watched: None,
            })
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            self.kq.as_fd()
        }

        pub(super) fn drain_and_rearm(&mut self, target: &Path) {
            let mut gone = false;
            let mut buf: Vec<Event> = Vec::with_capacity(8);
            loop {
                buf.clear();
                // SAFETY: the only registered descriptor is `watched`'s file, which stays open for
                // as long as its registration exists (closing it deregisters).
                let got = unsafe {
                    kevent(
                        &self.kq,
                        &[],
                        rustix::buffer::spare_capacity(&mut buf),
                        Some(std::time::Duration::ZERO),
                    )
                };
                match got {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        for ev in &buf {
                            if let EventFilter::Vnode { flags, .. } = ev.filter() {
                                gone |= flags.intersects(
                                    VnodeEvents::DELETE | VnodeEvents::RENAME | VnodeEvents::REVOKE,
                                );
                            }
                        }
                    }
                }
            }
            // Register, then look again: a directory created on the way down between choosing
            // the watch point and registering on it raised its event before anyone listened, so
            // the choice is re-made after every registration until it holds.
            for _ in 0..super::REARM_LIMIT {
                let want = super::watch_point(target);
                if !gone && matches!((&self.watched, &want), (Some((_, at)), Some(w)) if at == w) {
                    return;
                }
                gone = false;
                self.watched = None;
                let Some(path) = want else {
                    return;
                };
                let Ok(file) = File::open(&path) else {
                    return;
                };
                let flags = if path == target {
                    VnodeEvents::WRITE
                        | VnodeEvents::EXTEND
                        | VnodeEvents::DELETE
                        | VnodeEvents::RENAME
                        | VnodeEvents::REVOKE
                } else {
                    VnodeEvents::WRITE | VnodeEvents::DELETE | VnodeEvents::RENAME
                };
                let change = Event::new(
                    EventFilter::Vnode {
                        vnode: file.as_raw_fd(),
                        flags,
                    },
                    EventFlags::ADD | EventFlags::CLEAR,
                    std::ptr::null_mut(),
                );
                let mut none: Vec<Event> = Vec::new();
                // SAFETY: `file` is kept in `watched` for as long as the registration exists.
                if unsafe {
                    kevent(
                        &self.kq,
                        &[change],
                        &mut none,
                        Some(std::time::Duration::ZERO),
                    )
                }
                .is_err()
                {
                    return;
                }
                self.watched = Some((file, path));
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    //! inotify. One watch at a time: the file, or its nearest existing ancestor while it does not
    //! exist.
    use rustix::fs::inotify;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::path::{Path, PathBuf};

    pub(super) struct Inner {
        fd: OwnedFd,
        watched: Option<(i32, PathBuf)>,
    }

    impl Inner {
        pub(super) fn new() -> Option<Inner> {
            let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
                .ok()?;
            Some(Inner { fd, watched: None })
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            self.fd.as_fd()
        }

        pub(super) fn drain_and_rearm(&mut self, target: &Path) {
            let mut gone = false;
            let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
            let mut reader = inotify::Reader::new(&self.fd, &mut buf);
            while let Ok(ev) = reader.next() {
                let flags = ev.events();
                gone |= flags.intersects(
                    inotify::ReadFlags::DELETE_SELF
                        | inotify::ReadFlags::MOVE_SELF
                        | inotify::ReadFlags::IGNORED,
                );
            }
            // Register, then look again — see the kqueue implementation.
            for _ in 0..super::REARM_LIMIT {
                let want = super::watch_point(target);
                if !gone && matches!((&self.watched, &want), (Some((_, at)), Some(w)) if at == w) {
                    return;
                }
                gone = false;
                if let Some((wd, _)) = self.watched.take() {
                    let _ = inotify::remove_watch(&self.fd, wd);
                }
                let Some(path) = want else {
                    return;
                };
                let added = if path == target {
                    // `ATTRIB` for an unlink: a file someone still holds open (a lock) loses a
                    // link without being deleted, so `DELETE_SELF` would not fire.
                    let mut flags = inotify::WatchFlags::MODIFY
                        | inotify::WatchFlags::ATTRIB
                        | inotify::WatchFlags::DELETE_SELF
                        | inotify::WatchFlags::MOVE_SELF;
                    // A directory's own `MODIFY` is not raised by its entries changing, as
                    // kqueue's `NOTE_WRITE` on a directory is: ask for the entry events too, so a
                    // watched directory means the same thing on both platforms.
                    if target.is_dir() {
                        flags |= inotify::WatchFlags::CREATE
                            | inotify::WatchFlags::DELETE
                            | inotify::WatchFlags::MOVED_FROM
                            | inotify::WatchFlags::MOVED_TO;
                    }
                    inotify::add_watch(&self.fd, target, flags)
                } else {
                    inotify::add_watch(
                        &self.fd,
                        &path,
                        inotify::WatchFlags::CREATE
                            | inotify::WatchFlags::MOVED_TO
                            | inotify::WatchFlags::DELETE_SELF
                            | inotify::WatchFlags::MOVE_SELF,
                    )
                };
                let Ok(wd) = added else {
                    return;
                };
                self.watched = Some((wd, path));
            }
        }
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    target_os = "linux"
)))]
mod imp {
    //! No file-change mechanism: callers rely on their safety timeout.
    use std::os::fd::BorrowedFd;
    use std::path::Path;

    pub(super) struct Inner(std::convert::Infallible);

    impl Inner {
        pub(super) fn new() -> Option<Inner> {
            None
        }

        pub(super) fn fd(&self) -> BorrowedFd<'_> {
            match self.0 {}
        }

        pub(super) fn drain_and_rearm(&mut self, _target: &Path) {
            match self.0 {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marion_testsupport::scratch;

    const BOUND: Duration = Duration::from_secs(5);

    fn readable(fd: Option<BorrowedFd<'_>>, timeout: Duration) -> bool {
        let fd = fd.expect("a watch is available on this platform");
        wait_readable(&[fd], Some(timeout))[0]
    }

    #[test]
    fn a_pipe_wake_makes_a_blocked_poll_return_and_a_drain_clears_it() {
        let pipe = Arc::new(Pipe::new().unwrap());
        assert!(!wait_readable(&[pipe.fd()], Some(Duration::ZERO))[0]);
        let waker = Arc::clone(&pipe);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.wake();
            waker.wake();
        });
        let start = Instant::now();
        assert!(wait_readable(&[pipe.fd()], Some(BOUND))[0]);
        assert!(start.elapsed() < BOUND);
        t.join().unwrap();
        pipe.drain();
        assert!(
            !wait_readable(&[pipe.fd()], Some(Duration::ZERO))[0],
            "coalesced wakes drain to not-readable"
        );
    }

    /// **No wake is ever lost to a drain.** A waker announcing a change races a waiter that drains
    /// and looks; whatever the interleaving, a change the waiter has not seen leaves the pipe
    /// readable. Clearing `pending` before the read lost wakes here within a few thousand rounds
    /// and then stayed silent for good.
    #[test]
    fn a_wake_racing_a_drain_is_never_lost() {
        use std::sync::atomic::AtomicU64;
        let pipe = Arc::new(Pipe::new().unwrap());
        let changed = Arc::new(AtomicU64::new(0));
        const ROUNDS: u64 = 50_000;
        let waker = {
            let (pipe, changed) = (Arc::clone(&pipe), Arc::clone(&changed));
            std::thread::spawn(move || {
                for i in 1..=ROUNDS {
                    changed.store(i, Ordering::SeqCst);
                    pipe.wake();
                    // Spread the wakes out so they land all over the waiter's drain.
                    for _ in 0..(i % 64) {
                        std::hint::spin_loop();
                    }
                }
            })
        };
        let mut seen = 0;
        while seen < ROUNDS {
            let readable = wait_readable(&[pipe.fd()], Some(BOUND))[0];
            pipe.drain();
            let now = changed.load(Ordering::SeqCst);
            assert!(
                readable || now == seen,
                "a change ({now} after {seen}) left the pipe silent"
            );
            seen = now;
        }
        waker.join().unwrap();
    }

    #[test]
    fn a_signal_wakes_a_waiter_and_an_attached_pipe_within_a_bound() {
        let signal = Signal::new();
        let pipe = Arc::new(Pipe::new().unwrap());
        signal.attach(&pipe);
        let seen = signal.generation();
        let notifier = Arc::clone(&signal);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            notifier.notify();
        });
        let start = Instant::now();
        let now = signal.wait_past(seen, BOUND);
        assert!(now != seen, "the wait ended on the notify, not the timeout");
        assert!(start.elapsed() < BOUND);
        assert!(wait_readable(&[pipe.fd()], Some(BOUND))[0]);
        t.join().unwrap();
    }

    #[test]
    fn raising_a_flag_wakes_a_poll_on_it() {
        let flag = Flag::new();
        assert!(!wait_readable(&[flag.fd().unwrap()], Some(Duration::ZERO))[0]);
        flag.store(true, Ordering::SeqCst);
        assert!(flag.load(Ordering::SeqCst));
        assert!(wait_readable(&[flag.fd().unwrap()], Some(BOUND))[0]);
    }

    #[test]
    fn lowering_a_flag_drains_it_so_a_reused_flag_does_not_wake_its_next_waiter() {
        let flag = Flag::new();
        assert!(!flag.swap(true, Ordering::SeqCst));
        assert!(wait_readable(&[flag.fd().unwrap()], Some(Duration::ZERO))[0]);
        assert!(flag.swap(false, Ordering::SeqCst));
        assert!(
            !wait_readable(&[flag.fd().unwrap()], Some(Duration::ZERO))[0],
            "a lowered flag is quiet"
        );
        flag.store(true, Ordering::SeqCst);
        flag.store(false, Ordering::SeqCst);
        assert!(!wait_readable(&[flag.fd().unwrap()], Some(Duration::ZERO))[0]);
    }

    /// A blocked signal runs no handler and interrupts no `poll`; the watch is what wakes a waiter
    /// for it and stays readable until drained — on Linux, until the signal is no longer pending.
    /// Run in a child process that starts with the signal
    /// blocked — the mask survives `exec`, so every thread libtest makes inherits it and a
    /// process-directed signal (what a terminal's `^Z` is) can only go pending.
    #[test]
    fn a_signal_watch_fires_for_a_blocked_signal_and_leaves_it_pending() {
        use std::os::unix::process::CommandExt;
        const PROBE: &str = "MARION_WAKE_SIGNAL_WATCH_PROBE";
        // SIGUSR2: 31 on Darwin, 12 on Linux.
        #[cfg(target_os = "linux")]
        const SIGUSR2: i32 = 12;
        #[cfg(not(target_os = "linux"))]
        const SIGUSR2: i32 = 31;
        type SigSet = [u64; 16];
        unsafe extern "C" {
            fn sigprocmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
        }
        #[cfg(target_os = "linux")]
        const SIG_BLOCK: i32 = 0;
        #[cfg(not(target_os = "linux"))]
        const SIG_BLOCK: i32 = 1;
        let bit: u64 = 1 << (SIGUSR2 - 1);
        if std::env::var_os(PROBE).is_none() {
            let mut probe = std::process::Command::new(std::env::current_exe().unwrap());
            probe
                .args([
                    "--exact",
                    "wake::tests::a_signal_watch_fires_for_a_blocked_signal_and_leaves_it_pending",
                    "--test-threads=1",
                ])
                .env(PROBE, "1");
            // SAFETY: `sigprocmask` is async-signal-safe, and the forked child is single-threaded
            // until `exec`; `set` is a live buffer at least as large as the platform `sigset_t`.
            unsafe {
                probe.pre_exec(move || {
                    let mut set: SigSet = [0; 16];
                    set[0] = bit;
                    if sigprocmask(SIG_BLOCK, &set, std::ptr::null_mut()) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
            let status = probe.status().unwrap();
            assert!(status.success(), "the probe failed: {status}");
            return;
        }
        let watch = SignalWatch::new(SIGUSR2);
        assert!(!readable(watch.fd(), Duration::ZERO), "nothing posted yet");
        let signal = rustix::process::Signal::from_named_raw(SIGUSR2).unwrap();
        rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
        assert!(readable(watch.fd(), BOUND), "the posting makes it readable");
        assert!(
            readable(watch.fd(), Duration::ZERO),
            "and it stays readable"
        );
        watch.drain();
        #[cfg(not(target_os = "linux"))]
        assert!(
            !readable(watch.fd(), Duration::ZERO),
            "a drained kqueue is quiet until the next posting"
        );
        #[cfg(target_os = "linux")]
        assert!(
            readable(watch.fd(), Duration::ZERO),
            "a signalfd stays readable while the signal is pending: a drain must not consume it"
        );
    }

    #[test]
    fn a_signal_wait_times_out_without_a_notify() {
        let signal = Signal::new();
        let seen = signal.generation();
        assert_eq!(signal.wait_past(seen, Duration::from_millis(10)), seen);
    }

    #[test]
    fn a_watch_fires_on_an_append_from_any_writer() {
        let dir = scratch("wake-watch-append");
        let path = dir.join("journal.jsonl");
        std::fs::write(&path, b"a\n").unwrap();
        let mut watch = Watch::new(&path);
        assert!(!readable(watch.fd(), Duration::ZERO));
        marion_testsupport::append(&path, b"b\n");
        assert!(
            readable(watch.fd(), BOUND),
            "an append makes the watch readable"
        );
        watch.rearm();
        assert!(
            !readable(watch.fd(), Duration::ZERO),
            "rearm consumed the event"
        );
    }

    #[test]
    fn a_watch_on_a_file_that_does_not_exist_yet_fires_when_it_is_created_and_then_follows_it() {
        let dir = scratch("wake-watch-create");
        let path = dir.join("journal.jsonl");
        let mut watch = Watch::new(&path);
        assert!(!readable(watch.fd(), Duration::ZERO));
        marion_testsupport::append(&path, b"a\n");
        assert!(readable(watch.fd(), BOUND), "creation shows on the parent");
        watch.rearm();
        marion_testsupport::append(&path, b"b\n");
        assert!(
            readable(watch.fd(), BOUND),
            "after rearm the file itself is watched"
        );
    }

    #[test]
    fn a_proc_exit_fires_when_the_child_exits_and_leaves_it_unreaped() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read _"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let exit = ProcExit::new(child.id() as i32);
        assert!(!readable(exit.fd(), Duration::ZERO), "still running");
        drop(child.stdin.take());
        assert!(readable(exit.fd(), BOUND), "the exit makes it readable");
        // Not reaped by the watch: the wait status is still there for the owner to take.
        let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
        let status = rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        )
        .unwrap();
        assert!(status.is_some(), "the zombie is still waitable");
        child.wait().unwrap();
    }

    #[test]
    fn a_proc_exit_made_after_the_child_exited_never_hides_the_exit() {
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id() as i32;
        // A zombie by now; the exit must either show through the watch or leave it unarmed, which
        // a waiter answers by checking the status itself. What it must never be is armed and mute.
        std::thread::sleep(Duration::from_millis(50));
        let exit = ProcExit::new(pid);
        if exit.fd().is_some() {
            assert!(readable(exit.fd(), BOUND));
        }
        child.wait().unwrap();
    }

    #[test]
    fn wait_until_caps_an_unpollable_source_at_the_degraded_recheck() {
        let start = Instant::now();
        let ready = wait_until(&[None], Some(Instant::now() + BOUND));
        assert_eq!(ready, vec![false]);
        assert!(start.elapsed() < BOUND, "capped, not the whole deadline");
        let pipe = Pipe::new().unwrap();
        pipe.wake();
        assert_eq!(
            wait_until(&[None, Some(pipe.fd())], Some(Instant::now() + BOUND)),
            vec![false, true]
        );
    }

    /// A node's stream can be followed before its agent directory exists: the watch starts on the
    /// nearest ancestor there is and follows each creation down to the file.
    #[test]
    fn a_watch_whose_directories_do_not_exist_yet_follows_their_creation_down_to_the_file() {
        let dir = scratch("wake-watch-deep");
        let path = dir.join("agents").join("root").join("events.jsonl");
        let mut watch = Watch::new(&path);
        assert!(!readable(watch.fd(), Duration::ZERO));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(
            readable(watch.fd(), BOUND),
            "the first directory's creation shows"
        );
        watch.rearm();
        marion_testsupport::append(&path, b"a\n");
        assert!(
            readable(watch.fd(), BOUND),
            "and so, a level down, does the file's"
        );
        watch.rearm();
        marion_testsupport::append(&path, b"b\n");
        assert!(
            readable(watch.fd(), BOUND),
            "after which the file itself is watched"
        );
    }

    #[test]
    fn a_watch_on_a_directory_fires_when_an_entry_is_removed_or_replaced() {
        let dir = scratch("wake-watch-dir");
        let entry = dir.join("supervisor.sock");
        std::fs::write(&entry, b"").unwrap();
        let mut watch = Watch::new(&dir);
        assert!(!readable(watch.fd(), Duration::ZERO));
        std::fs::remove_file(&entry).unwrap();
        assert!(
            readable(watch.fd(), BOUND),
            "an unlinked entry shows on the directory"
        );
        watch.rearm();
        assert!(!readable(watch.fd(), Duration::ZERO));
        std::fs::write(dir.join("supervisor.sock.new"), b"").unwrap();
        std::fs::rename(dir.join("supervisor.sock.new"), &entry).unwrap();
        assert!(
            readable(watch.fd(), BOUND),
            "a replaced entry shows on the directory"
        );
    }

    #[test]
    fn a_watch_fires_when_its_file_is_removed() {
        let dir = scratch("wake-watch-remove");
        let path = dir.join("supervisor.lock");
        std::fs::write(&path, b"").unwrap();
        let watch = Watch::new(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(readable(watch.fd(), BOUND));
    }
}
