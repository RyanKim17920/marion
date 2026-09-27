//! **What wakes the home screen**: one `poll(2)` over every descriptor that can change what it
//! shows, so an idle screen sleeps in the kernel rather than spinning a timer.
//!
//! The things that change the screen are the keyboard, the supervisor's `tree/subscribe` stream,
//! a side thread finishing (the doctor probe, a `marion run`), and the window being resized. The
//! first two are descriptors already. The side threads report over a channel, which cannot be
//! polled, so each report is followed by a byte on a [`Wake`] pair. A resize is a `SIGWINCH`,
//! whose handler writes a byte to the same pair — a `write(2)` is async-signal-safe, and a flag
//! alone would not end a `poll` that the signal did not interrupt.
//!
//! The only timeouts left are the ones a screen genuinely needs, and the caller states each:
//! a clock that ticks while something on screen is running, and looking again for a supervisor
//! that is not there yet.

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

/// A self-pipe: [`Waker`]s write a byte, the loop polls [`Wake::fd`] and [`Wake::drain`]s it.
pub struct Wake {
    rx: UnixStream,
    tx: Arc<UnixStream>,
}

/// The writing end, for a side thread. Cheap to clone.
#[derive(Clone)]
pub struct Waker(Arc<UnixStream>);

impl Waker {
    /// Wake the loop. A full buffer means a wake is already pending, which is all a wake says.
    pub fn wake(&self) {
        let _ = rustix::io::write(self.0.as_fd(), &[1]);
    }
}

impl Wake {
    pub fn new() -> std::io::Result<Wake> {
        let (rx, tx) = UnixStream::pair()?;
        rx.set_nonblocking(true)?;
        tx.set_nonblocking(true)?;
        Ok(Wake {
            rx,
            tx: Arc::new(tx),
        })
    }

    pub fn waker(&self) -> Waker {
        Waker(self.tx.clone())
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.rx.as_fd()
    }

    /// Empty the pair, and say whether a `SIGWINCH` arrived since the last drain.
    pub fn drain(&mut self) -> bool {
        let mut buf = [0u8; 64];
        while matches!(self.rx.read(&mut buf), Ok(n) if n > 0) {}
        RESIZED.swap(false, Ordering::SeqCst)
    }

    /// Route `SIGWINCH` to this pair. Installed on every entry to the screen: an attach started
    /// from it installs its own handler while it runs.
    pub fn watch_resizes(&self) {
        WINCH_FD.store(self.tx.as_raw_fd(), Ordering::SeqCst);
        // SAFETY: `on_winch` only stores to atomics and calls `write(2)`, both async-signal-safe.
        unsafe {
            signal(SIGWINCH, on_winch as *const () as usize);
        }
    }
}

/// `SIGWINCH`: 28 on Darwin and Linux alike.
const SIGWINCH: std::ffi::c_int = 28;
static WINCH_FD: AtomicI32 = AtomicI32::new(-1);
static RESIZED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn signal(sig: std::ffi::c_int, handler: usize) -> usize;
}

extern "C" fn on_winch(_sig: std::ffi::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
    let fd: RawFd = WINCH_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: the descriptor is the live writing end the loop owns for the process's life.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let _ = rustix::io::write(fd, &[1]);
    }
}

/// Wait until one of `fds` is readable (or hung up), or `timeout` passes; `None` waits for as
/// long as it takes. Says which were ready, in order. An interrupted wait is a wait that saw
/// nothing: the caller's loop looks again.
pub fn wait(fds: &[BorrowedFd<'_>], timeout: Option<Duration>) -> std::io::Result<Vec<bool>> {
    let mut pfds: Vec<PollFd> = fds
        .iter()
        .map(|fd| PollFd::from_borrowed_fd(*fd, PollFlags::IN))
        .collect();
    let ts = timeout.map(|t| Timespec {
        tv_sec: t.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: t.subsec_nanos().into(),
    });
    match poll(&mut pfds, ts.as_ref()) {
        Ok(_) => {}
        Err(rustix::io::Errno::INTR) => return Ok(vec![false; fds.len()]),
        Err(e) => return Err(e.into()),
    }
    let woke = PollFlags::IN | PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL;
    Ok(pfds.iter().map(|p| p.revents().intersects(woke)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_wait_with_nothing_ready_sleeps_its_whole_timeout_and_sees_nothing() {
        let wake = Wake::new().unwrap();
        let t = Instant::now();
        let ready = wait(&[wake.fd()], Some(Duration::from_millis(30))).unwrap();
        assert_eq!(ready, [false]);
        assert!(
            t.elapsed() >= Duration::from_millis(25),
            "{:?}",
            t.elapsed()
        );
    }

    /// The resize flag is process-wide, as a signal handler's state must be: the two tests that
    /// read it take turns.
    static RESIZE_FLAG: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_side_threads_wake_ends_an_unbounded_wait() {
        let _flag = RESIZE_FLAG.lock().unwrap_or_else(|e| e.into_inner());
        let mut wake = Wake::new().unwrap();
        let waker = wake.waker();
        let t = std::thread::spawn(move || waker.wake());
        // Only the wake can end this early; a broken wake fails at the bound rather than hanging.
        let ready = wait(&[wake.fd()], Some(Duration::from_secs(10))).unwrap();
        t.join().unwrap();
        assert_eq!(ready, [true]);
        assert!(!wake.drain(), "a plain wake is not a resize");
        let again = wait(&[wake.fd()], Some(Duration::ZERO)).unwrap();
        assert_eq!(again, [false], "drained");
    }

    #[test]
    fn a_resize_signal_wakes_the_wait_and_says_it_was_a_resize() {
        unsafe extern "C" {
            fn getpid() -> i32;
            fn kill(pid: i32, sig: std::ffi::c_int) -> std::ffi::c_int;
        }
        let _flag = RESIZE_FLAG.lock().unwrap_or_else(|e| e.into_inner());
        let mut wake = Wake::new().unwrap();
        wake.watch_resizes();
        // SAFETY: a signal to this process whose handler was just installed.
        assert_eq!(unsafe { kill(getpid(), SIGWINCH) }, 0);
        let ready = wait(&[wake.fd()], Some(Duration::from_secs(10))).unwrap();
        assert_eq!(ready, [true]);
        assert!(wake.drain(), "the handler marked it a resize");
    }

    #[test]
    fn several_descriptors_report_which_one_woke() {
        let a = Wake::new().unwrap();
        let b = Wake::new().unwrap();
        b.waker().wake();
        let ready = wait(&[a.fd(), b.fd()], Some(Duration::from_secs(10))).unwrap();
        assert_eq!(ready, [false, true]);
    }
}
