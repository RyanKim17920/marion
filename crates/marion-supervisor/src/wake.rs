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

/// `Child::wait` with a ceiling, over [`poll_until`]. `None` is "still running at the deadline",
/// which is a finding and not an error — or a child that can no longer be waited on at all.
pub fn wait_bounded(
    child: &mut std::process::Child,
    budget: Duration,
) -> Option<std::process::ExitStatus> {
    let mut status = None;
    poll_until(budget, Duration::from_millis(20), || {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                true
            }
            Ok(None) => false,
            Err(_) => true,
        }
    });
    status
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
    pub fn drain(&self) {
        use std::io::Read;
        self.pending.store(false, Ordering::Release);
        let mut buf = [0u8; 64];
        while let Ok(n) = (&self.read).read(&mut buf) {
            if n < buf.len() {
                break;
            }
        }
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

    pub fn store(&self, value: bool, order: Ordering) {
        self.set.store(value, order);
        if value && let Some(wake) = &self.wake {
            wake.wake();
        }
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
/// renamed.
///
/// The target need not exist. Until it does the watch is on its parent directory, whose entries
/// changing is how a creation shows; [`Self::rearm`] moves the watch onto the file once it exists,
/// and back off it when it is removed or replaced. Where no mechanism is available (another
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
    use std::path::Path;

    pub(super) struct Inner {
        kq: OwnedFd,
        /// The open vnode being watched: the file itself, or its parent while it does not exist.
        /// Closing it removes its registration, so replacing it is the whole of a re-arm.
        watched: Option<(File, bool)>,
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
            let is_file = target.exists();
            if !gone && matches!(self.watched, Some((_, watching_file)) if watching_file == is_file)
            {
                return;
            }
            self.watched = None;
            let path = if is_file {
                target
            } else {
                match target.parent() {
                    Some(parent) => parent,
                    None => return,
                }
            };
            let Ok(file) = File::open(path) else {
                return;
            };
            let flags = if is_file {
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
            .is_ok()
            {
                self.watched = Some((file, is_file));
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    //! inotify. One watch at a time: the file, or its parent directory while it does not exist.
    use rustix::fs::inotify;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::path::Path;

    pub(super) struct Inner {
        fd: OwnedFd,
        watched: Option<(i32, bool)>,
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
            let exists = target.exists();
            if !gone && matches!(self.watched, Some((_, watching_file)) if watching_file == exists)
            {
                return;
            }
            if let Some((wd, _)) = self.watched.take() {
                let _ = inotify::remove_watch(&self.fd, wd);
            }
            let added = if exists {
                inotify::add_watch(
                    &self.fd,
                    target,
                    // `ATTRIB` for an unlink: a file someone still holds open (a lock) loses a link
                    // without being deleted, so `DELETE_SELF` would not fire.
                    inotify::WatchFlags::MODIFY
                        | inotify::WatchFlags::ATTRIB
                        | inotify::WatchFlags::DELETE_SELF
                        | inotify::WatchFlags::MOVE_SELF,
                )
                .map(|wd| (wd, true))
            } else if let Some(parent) = target.parent() {
                inotify::add_watch(
                    &self.fd,
                    parent,
                    inotify::WatchFlags::CREATE
                        | inotify::WatchFlags::MOVED_TO
                        | inotify::WatchFlags::DELETE_SELF
                        | inotify::WatchFlags::MOVE_SELF,
                )
                .map(|wd| (wd, false))
            } else {
                return;
            };
            if let Ok(w) = added {
                self.watched = Some(w);
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
