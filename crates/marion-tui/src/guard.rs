//! marion's **own** terminal: raw mode, the alternate screen, and getting out of both.
//!
//! # The failure this module exists to prevent
//!
//! A TUI that panics with the terminal still in raw mode leaves the operator at a shell that does
//! not echo, does not line-edit and does not respond to `^C` — the classic way a terminal program
//! wrecks the session it was invited into. Restoring in `Drop` is not enough on its own: a panic
//! unwinds, and a panic that aborts (`panic = "abort"`, a double panic, a stack overflow) does not
//! unwind at all. So the restore lives in **two** places that cannot both be forgotten: [`Screen`]'s
//! `Drop`, and a `std::panic` hook installed at the same instant the screen is entered.
//!
//! Both call the same idempotent [`Screen::leave`]; the second call is a no-op, so unwinding
//! through the hook and then dropping the guard restores once, not twice.
//!
//! # Why the termios struct is never inspected
//!
//! `struct termios` has a different field layout, a different `tcflag_t` width and a different
//! `NCCS` on Darwin and Linux — 72 bytes here (measured on Darwin 25.5.0), 60 on glibc x86-64 —
//! and hand-declaring it is two platform-specific definitions that are wrong in a way nothing
//! catches until a terminal is left broken on the platform nobody tested.
//!
//! So it is never declared. [`Termios`] is an **opaque, over-sized, 8-byte-aligned blob** that only
//! ever travels between `tcgetattr`, `cfmakeraw` and `tcsetattr`. marion reads no field and sets no
//! flag: the raw-mode *policy* is `cfmakeraw`'s, which is libc's own and correct on both platforms,
//! and the restore is a byte-for-byte write-back of exactly what `tcgetattr` returned. That is also
//! the strongest restore available — stronger than re-deriving "the flags we think we cleared",
//! which would silently drop any flag the operator's shell had set that marion does not know about.
//!
//! # The order, and why it is the exact inverse
//!
//! Entering: raw mode, then the alternate screen, then the mouse modes the node asked for.
//! Leaving: mouse modes off, then the alternate screen, then cooked mode. Reversed, because the
//! alternate screen must be left while the terminal can still be written to sensibly, and cooked
//! mode is restored last so that a failure anywhere earlier still ends with a usable shell.

use std::io::Write;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::sticky::Sticky;

// ---------------------------------------------------------------------------------------------
// libc, hand-declared — the same device `marion-supervisor::pty` uses, for the same reason.
// ---------------------------------------------------------------------------------------------

unsafe extern "C" {
    fn tcgetattr(fd: std::ffi::c_int, termios_p: *mut Termios) -> std::ffi::c_int;
    fn tcsetattr(
        fd: std::ffi::c_int,
        optional_actions: std::ffi::c_int,
        termios_p: *const Termios,
    ) -> std::ffi::c_int;
    fn cfmakeraw(termios_p: *mut Termios);
    fn isatty(fd: std::ffi::c_int) -> std::ffi::c_int;
}

/// `TCSAFLUSH` — apply once the output queue has drained and discard pending input.
///
/// `TCSANOW` (0) would apply mid-drain; `TCSAFLUSH` is what every terminal library uses on the way
/// *in*, because keystrokes typed before raw mode took effect were typed at a different terminal.
/// Measured on Darwin 25.5.0: `TCSANOW = 0`, `TCSAFLUSH = 2`. The two constants agree on Linux.
const TCSAFLUSH: std::ffi::c_int = 2;

/// An opaque `struct termios`, never interpreted.
///
/// 128 bytes against a measured 72 on Darwin and 60 on glibc x86-64, and `align(8)` because
/// `tcflag_t`/`speed_t` are `unsigned long` on Darwin. See the module doc: the slack is deliberate,
/// and the alignment is the part that would actually be unsound to get wrong.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct Termios([u8; 128]);

impl Termios {
    fn zeroed() -> Self {
        Self([0; 128])
    }
}

impl std::fmt::Debug for Termios {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The bytes are libc's business, not a reader's. Printing them would invite comparison.
        f.write_str("Termios(<opaque>)")
    }
}

impl PartialEq for Termios {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Termios {}

/// Read a descriptor's terminal settings, if it is a terminal at all.
pub fn get_termios(fd: RawFd) -> Option<Termios> {
    let mut t = Termios::zeroed();
    // SAFETY: `t` is a live, correctly aligned buffer at least as large as the platform's
    // `struct termios`; `tcgetattr` writes no more than that. A non-tty `fd` answers `ENOTTY`.
    let rc = unsafe { tcgetattr(fd, &mut t) };
    (rc == 0).then_some(t)
}

/// Write terminal settings back verbatim.
fn set_termios(fd: RawFd, t: &Termios) -> bool {
    // SAFETY: `t` is a blob this process obtained from `tcgetattr` on some descriptor, so it is a
    // valid `struct termios` prefix; `tcsetattr` reads and does not write through the pointer.
    unsafe { tcsetattr(fd, TCSAFLUSH, t) == 0 }
}

/// Is this descriptor a terminal?
pub fn is_tty(fd: RawFd) -> bool {
    // SAFETY: a pure query on an integer descriptor.
    unsafe { isatty(fd) == 1 }
}

/// Raw mode on one descriptor, and the settings to put back.
///
/// `Copy`, so the panic hook can hold its own copy rather than sharing ownership with the guard —
/// a hook that borrowed the guard would have to outlive it, which is exactly backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMode {
    fd: RawFd,
    original: Termios,
}

impl RawMode {
    /// Put `fd` into raw mode, remembering what it was.
    ///
    /// `None` when `fd` is not a terminal — which is not an error. `marion attach` run with its
    /// input redirected has nothing to put into raw mode and should still render.
    pub fn enable(fd: RawFd) -> Option<Self> {
        let original = get_termios(fd)?;
        let mut raw = original;
        // SAFETY: `raw` is a blob `tcgetattr` just filled, so it is a valid `struct termios`.
        unsafe { cfmakeraw(&mut raw) };
        set_termios(fd, &raw).then_some(Self { fd, original })
    }

    /// Put back **exactly** what was found. See the module doc on why this is a write-back and not
    /// a re-derivation.
    pub fn restore(&self) -> bool {
        set_termios(self.fd, &self.original)
    }

    /// The settings this will restore to. Exposed so a test can prove the round trip rather than
    /// trusting it.
    pub fn original(&self) -> Termios {
        self.original
    }
}

// ---------------------------------------------------------------------------------------------
// The escape sequences, as pure functions
// ---------------------------------------------------------------------------------------------

/// `?1049h`, and the mouse modes `modes` says the node wants reported to it.
///
/// **marion mirrors the node's modes onto its own terminal and invents none.** If the node never
/// asked for a mouse, marion does not enable one: an operator who selects text with the mouse in a
/// pane showing a Codex session — which enables no tracking at all, measured in [`crate::sticky`] —
/// should get their terminal's own selection, not a stream of reports nobody will read.
pub fn enter_bytes(modes: &Sticky) -> Vec<u8> {
    let mut out = b"\x1b[?1049h".to_vec();
    out.extend_from_slice(&mouse_bytes(modes, true));
    out
}

/// The exact inverse of [`enter_bytes`], in reverse order, plus an unconditional cursor show.
///
/// **The mouse resets are unconditional, and that is deliberate.** A leave must undo whatever the
/// session reached, not whatever it started with: the node may have enabled `?1003` at minute nine
/// and the guard's copy of the modes may be stale or, in the panic path, unreachable. Sending
/// `?1000l` to a terminal that never had `?1000h` is a documented no-op; failing to send it to one
/// that did leaves the operator's shell emitting mouse reports as text forever.
pub fn leave_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[?1006l");
    out.extend_from_slice(b"\x1b[?1003l");
    out.extend_from_slice(b"\x1b[?1002l");
    out.extend_from_slice(b"\x1b[?1000l");
    // Cursor visible before the buffer switch: a terminal that hid the cursor on the alternate
    // screen and switched back without showing it leaves a shell with no cursor.
    out.extend_from_slice(b"\x1b[?25h");
    out.extend_from_slice(b"\x1b[?1049l");
    out
}

/// The `h`/`l` sequence for the four tracked mouse modes.
fn mouse_bytes(modes: &Sticky, on: bool) -> Vec<u8> {
    let f = if on { b'h' } else { b'l' };
    let mut out = Vec::new();
    for (wanted, num) in [
        (modes.mouse_button, "1000"),
        (modes.mouse_drag, "1002"),
        (modes.mouse_motion, "1003"),
        (modes.mouse_sgr, "1006"),
    ] {
        if wanted {
            out.extend_from_slice(b"\x1b[?");
            out.extend_from_slice(num.as_bytes());
            out.push(f);
        }
    }
    out
}

/// The bytes that move marion's own terminal from `was` to `now`.
///
/// A node turns mouse tracking on and off during a session — Claude Code enables all four modes a
/// few dozen bytes after boot, and a harness may drop them again — so the mirror is a **delta**,
/// not a re-assertion. Re-sending `?1000h` every frame would work and would also put four escape
/// sequences on the wire per paint; emitting only what changed keeps the guard's output
/// proportional to the events that justified it.
pub fn mirror_delta(was: &Sticky, now: &Sticky) -> Vec<u8> {
    let mut out = Vec::new();
    for (before, after, num) in [
        (was.mouse_button, now.mouse_button, "1000"),
        (was.mouse_drag, now.mouse_drag, "1002"),
        (was.mouse_motion, now.mouse_motion, "1003"),
        (was.mouse_sgr, now.mouse_sgr, "1006"),
    ] {
        if before != after {
            out.extend_from_slice(b"\x1b[?");
            out.extend_from_slice(num.as_bytes());
            out.push(if after { b'h' } else { b'l' });
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------------------------

/// What a restore needs, in a form a panic hook can own outright.
#[derive(Clone)]
struct Restore {
    sink: Arc<Mutex<dyn Write + Send>>,
    raw: Option<RawMode>,
    done: Arc<AtomicBool>,
}

impl Restore {
    /// Undo everything, once. Later calls are no-ops.
    ///
    /// Returns whether this call was the one that did it, which is what makes "once" testable.
    fn run(&self) -> bool {
        if self.done.swap(true, Ordering::SeqCst) {
            return false;
        }
        if let Ok(mut w) = self.sink.lock() {
            let _ = w.write_all(&leave_bytes());
            let _ = w.flush();
        }
        // Cooked mode **last**: see the module doc. A poisoned sink mutex or a failed write must
        // not be able to skip this, which is why it is outside the `if let`.
        if let Some(raw) = &self.raw {
            raw.restore();
        }
        true
    }
}

/// marion's terminal, entered.
///
/// Construct with [`Screen::enter`]; the terminal is restored when this is dropped **and** if the
/// process panics first, whichever happens.
pub struct Screen {
    restore: Restore,
    /// The hook installed at `enter`, so `Drop` can tell whether it is still the live one.
    installed: Arc<AtomicBool>,
}

impl Screen {
    /// Enter raw mode and the alternate screen on `sink`, putting `input_fd` into raw mode, and
    /// install the panic hook that undoes both.
    ///
    /// `input_fd` is separate from `sink` because they are: marion reads keys from stdin and writes
    /// frames to stdout, and a `marion attach` whose stdout is a pipe still wants raw keys.
    ///
    /// **The previous panic hook is chained, not replaced.** Rust's default hook is what prints the
    /// message and the backtrace; a TUI that swallowed it would restore a clean terminal and tell
    /// the operator nothing about why their session ended. The restore runs *first*, so the message
    /// lands on a terminal that can display it.
    pub fn enter<S: Write + Send + 'static>(
        sink: S,
        input_fd: RawFd,
        modes: &Sticky,
    ) -> std::io::Result<Self> {
        let sink: Arc<Mutex<dyn Write + Send>> = Arc::new(Mutex::new(sink));
        // Raw mode first, so a failure to write the alternate-screen switch still leaves a
        // terminal this guard knows how to put back.
        let raw = RawMode::enable(input_fd);
        {
            let mut w = sink.lock().expect("a fresh mutex is never poisoned");
            w.write_all(&enter_bytes(modes))?;
            w.flush()?;
        }
        let restore = Restore {
            sink,
            raw,
            done: Arc::new(AtomicBool::new(false)),
        };
        let installed = Arc::new(AtomicBool::new(true));

        let hooked = restore.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            hooked.run();
            previous(info);
        }));

        Ok(Self { restore, installed })
    }

    /// Restore now. Idempotent; `Drop` calls it too.
    pub fn leave(&self) {
        self.restore.run();
    }

    /// Whether the terminal has already been restored. Diagnostic, and what the panic test asserts.
    pub fn restored(&self) -> bool {
        self.restore.done.load(Ordering::SeqCst)
    }

    /// Mirror a change in the node's mouse modes onto marion's own terminal.
    pub fn mirror(&self, was: &Sticky, now: &Sticky) -> std::io::Result<()> {
        let delta = mirror_delta(was, now);
        if delta.is_empty() {
            return Ok(());
        }
        let mut w = self.restore.sink.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(&delta)?;
        w.flush()
    }

    /// Write frame bytes to the terminal. The backend's one way out.
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self.restore.sink.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(bytes)?;
        w.flush()
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.restore.run();
        self.installed.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex as StdMutex, MutexGuard};

    /// `std::panic::set_hook` is process-global, so the tests that install one must not overlap.
    static HOOK: StdMutex<()> = StdMutex::new(());

    fn serialized() -> MutexGuard<'static, ()> {
        HOOK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A sink whose bytes a test can read back.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn all_modes() -> Sticky {
        Sticky {
            mouse_button: true,
            mouse_drag: true,
            mouse_motion: true,
            mouse_sgr: true,
            ..Sticky::initial(80, 24)
        }
    }

    #[test]
    fn entering_switches_the_buffer_before_it_enables_a_mouse() {
        let s = String::from_utf8(enter_bytes(&all_modes())).unwrap();
        assert!(
            s.starts_with("\x1b[?1049h"),
            "the buffer switch comes first: {s:?}"
        );
        assert_eq!(s, "\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
    }

    #[test]
    fn a_node_that_wants_no_mouse_gets_none_enabled_on_the_operators_terminal() {
        // Codex enables no tracking at all; marion must not invent it and steal the operator's
        // own click-to-select.
        assert_eq!(
            enter_bytes(&Sticky::initial(80, 24)),
            b"\x1b[?1049h".to_vec()
        );
    }

    #[test]
    fn leaving_undoes_every_mouse_mode_unconditionally_and_leaves_the_buffer_last() {
        let s = String::from_utf8(leave_bytes()).unwrap();
        for m in ["1000", "1002", "1003", "1006"] {
            assert!(
                s.contains(&format!("\x1b[?{m}l")),
                "no reset for ?{m}: {s:?}"
            );
        }
        assert!(s.contains("\x1b[?25h"), "the cursor must come back");
        assert!(
            s.ends_with("\x1b[?1049l"),
            "the buffer switch is last: {s:?}"
        );
    }

    #[test]
    fn the_leave_sequence_is_the_enter_sequence_reversed() {
        // Not a byte-level inverse — the resets are unconditional — but every mode the enter can
        // set must have a reset, or a session ends with a mode still on.
        let enter = String::from_utf8(enter_bytes(&all_modes())).unwrap();
        let leave = String::from_utf8(leave_bytes()).unwrap();
        for m in ["1049", "1000", "1002", "1003", "1006"] {
            assert!(enter.contains(&format!("\x1b[?{m}h")));
            assert!(
                leave.contains(&format!("\x1b[?{m}l")),
                "enter sets ?{m} and leave never clears it"
            );
        }
    }

    #[test]
    fn the_mirror_emits_only_what_changed() {
        let off = Sticky::initial(80, 24);
        assert_eq!(
            mirror_delta(&off, &off),
            Vec::<u8>::new(),
            "no change, no bytes"
        );
        assert_eq!(mirror_delta(&off, &all_modes()).len(), 4 * 8);

        let mut one = off;
        one.mouse_sgr = true;
        assert_eq!(mirror_delta(&off, &one), b"\x1b[?1006h".to_vec());
        assert_eq!(
            mirror_delta(&one, &off),
            b"\x1b[?1006l".to_vec(),
            "and back off"
        );
    }

    /// **Mutation: delete the `set_hook` call in [`Screen::enter`].**
    ///
    /// Without the hook a panic leaves the alternate screen on and the terminal in raw mode, and
    /// the sink holds no leave sequence at the moment the panic is observed. With it, the restore
    /// has already run by the time `catch_unwind` returns.
    #[test]
    fn a_panic_restores_the_terminal_before_the_guard_is_dropped() {
        let _lock = serialized();
        let sink = Sink::default();
        let screen = Screen::enter(sink.clone(), -1, &all_modes()).expect("enter");
        assert!(!screen.restored(), "nothing has panicked yet");
        assert!(
            !sink.text().contains("\x1b[?1049l"),
            "the leave sequence must not be written on the way in"
        );

        // The guard is deliberately *not* moved into the closure: the hook, and only the hook, is
        // what can restore here. A `Drop`-based restore would happen after `catch_unwind` returns.
        let panicked = std::panic::catch_unwind(|| panic!("a TUI thread died"));
        assert!(panicked.is_err(), "the panic must actually have happened");

        assert!(
            screen.restored(),
            "the panic hook did not restore the terminal — an operator is now at a raw-mode shell"
        );
        let out = sink.text();
        assert!(
            out.contains("\x1b[?1049l"),
            "no buffer switch back: {out:?}"
        );
        assert!(
            out.contains("\x1b[?1000l"),
            "mouse tracking left on: {out:?}"
        );
        drop(screen);
    }

    #[test]
    fn the_restore_runs_exactly_once_however_many_times_it_is_asked() {
        let _lock = serialized();
        let sink = Sink::default();
        {
            let screen = Screen::enter(sink.clone(), -1, &all_modes()).expect("enter");
            screen.leave();
            screen.leave();
        } // and Drop
        let leaves = sink.text().matches("\x1b[?1049l").count();
        assert_eq!(
            leaves, 1,
            "the terminal was restored {leaves} times, not once"
        );
    }

    /// The chained hook: Rust's own panic message must still be printed. A TUI that swallowed it
    /// would leave a clean terminal and no explanation.
    #[test]
    fn the_previous_panic_hook_still_runs() {
        let _lock = serialized();
        let seen = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&seen);
        let outer = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| flag.store(true, Ordering::SeqCst)));

        let sink = Sink::default();
        let screen = Screen::enter(sink, -1, &all_modes()).expect("enter");
        let _ = std::panic::catch_unwind(|| panic!("boom"));
        assert!(screen.restored());
        assert!(
            seen.load(Ordering::SeqCst),
            "the chained hook was dropped on the floor"
        );

        drop(screen);
        std::panic::set_hook(outer);
    }

    // -----------------------------------------------------------------------------------------
    // A real terminal, so the termios round trip is measured and not merely asserted
    // -----------------------------------------------------------------------------------------

    unsafe extern "C" {
        fn posix_openpt(flags: std::ffi::c_int) -> std::ffi::c_int;
        fn grantpt(fd: std::ffi::c_int) -> std::ffi::c_int;
        fn unlockpt(fd: std::ffi::c_int) -> std::ffi::c_int;
        fn ptsname(fd: std::ffi::c_int) -> *mut std::ffi::c_char;
        fn close(fd: std::ffi::c_int) -> std::ffi::c_int;
    }

    /// A pty slave, kept open, purely so these tests have a descriptor `tcgetattr` accepts.
    ///
    /// The alternative — testing against the test runner's own stdin — passes locally and fails
    /// under `cargo test` in CI, where stdin is not a terminal. A pty is a terminal everywhere.
    struct Tty {
        master: std::ffi::c_int,
        slave: std::fs::File,
    }

    impl Tty {
        fn open() -> Self {
            // O_RDWR | O_NOCTTY, the numbers `marion-supervisor::pty` measured on this platform.
            let master = unsafe { posix_openpt(0x0002 | 0x20000) };
            assert!(master >= 0, "posix_openpt failed");
            assert_eq!(unsafe { grantpt(master) }, 0);
            assert_eq!(unsafe { unlockpt(master) }, 0);
            let name = unsafe { std::ffi::CStr::from_ptr(ptsname(master)) }
                .to_str()
                .expect("a pts path is ascii")
                .to_owned();
            let slave = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
                .expect("open the slave");
            Self { master, slave }
        }

        fn fd(&self) -> RawFd {
            std::os::fd::AsRawFd::as_raw_fd(&self.slave)
        }
    }

    impl Drop for Tty {
        fn drop(&mut self) {
            unsafe { close(self.master) };
        }
    }

    #[test]
    fn raw_mode_changes_a_real_terminal_and_puts_back_exactly_what_it_found() {
        let tty = Tty::open();
        assert!(is_tty(tty.fd()), "a pty slave is a terminal");
        let before = get_termios(tty.fd()).expect("tcgetattr on a pty slave");

        let raw = RawMode::enable(tty.fd()).expect("raw mode on a pty slave");
        let during = get_termios(tty.fd()).expect("tcgetattr");
        assert_ne!(
            before, during,
            "cfmakeraw changed nothing — raw mode is not actually being applied"
        );
        assert_eq!(
            raw.original(),
            before,
            "the guard remembered the wrong settings"
        );

        assert!(raw.restore());
        assert_eq!(
            get_termios(tty.fd()).expect("tcgetattr"),
            before,
            "the terminal was not put back byte-for-byte"
        );
    }

    /// **Mutation: delete the `set_hook` call in [`Screen::enter`], on a real terminal.**
    ///
    /// The sink-based test above proves the leave *sequence* is written. This proves the half that
    /// actually wrecks an operator's shell: the terminal is out of raw mode again after a panic,
    /// measured with `tcgetattr` rather than inferred from bytes.
    #[test]
    fn a_panic_takes_a_real_terminal_out_of_raw_mode() {
        let _lock = serialized();
        let tty = Tty::open();
        let cooked = get_termios(tty.fd()).expect("tcgetattr");

        let screen = Screen::enter(Sink::default(), tty.fd(), &all_modes()).expect("enter");
        assert_ne!(
            get_termios(tty.fd()).expect("tcgetattr"),
            cooked,
            "the screen did not put the terminal into raw mode, so this test proves nothing"
        );

        let _ = std::panic::catch_unwind(|| panic!("a TUI thread died"));

        assert_eq!(
            get_termios(tty.fd()).expect("tcgetattr"),
            cooked,
            "a panic left the terminal in raw mode — the operator's shell no longer echoes"
        );
        drop(screen);
    }

    #[test]
    fn a_descriptor_that_is_not_a_terminal_is_not_an_error() {
        // `marion attach < /dev/null` has nothing to put into raw mode and must still render.
        assert!(RawMode::enable(-1).is_none());
        assert!(!is_tty(-1));
        assert!(get_termios(-1).is_none());
    }
}
