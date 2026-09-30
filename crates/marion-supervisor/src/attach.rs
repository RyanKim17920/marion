//! `marion attach <agent-id>` — one node's terminal, in this one (§5.3, §9's M3).
//!
//! # What this is: the grid, in the loop
//!
//! `marion-tui` builds the pieces of a client: the screen guard, the sticky-mode preamble, the
//! keystroke filter with `^]` reserved, the asciicast reader, the replay plan, the redraw edge, the
//! pane and — since the increment that made a node own a pty — the `ratatui` backend. This module
//! is the **process** that holds them together.
//!
//! The node's bytes are fed to a [`marion_term::Term`] built with [`marion_tui::grid_options`],
//! [`marion_tui::Redraw`] decides when a frame is complete, and [`marion_tui::Pane`] paints it
//! through [`marion_tui::ScreenBackend`]. That is what makes the two decisions marion has already
//! taken about a pane actually hold:
//!
//! * [`marion_tui::MAX_SCROLLBACK`] bounds what one pane costs, whatever the node emits.
//! * **`CSI 3J` is intercepted** by `marion_term`'s `Suppressor` instead of reaching the operator's
//!   real scrollback — §9's M3 criterion C2, and unreachable without a parser in the path.
//!
//! # What this used to be, and what routing through the grid actually cost
//!
//! This module rendered **pass-through**: the node's bytes straight to the operator's terminal,
//! which was then the emulator. Its stated blocker — *"`marion-tui` depends on `ratatui` with
//! `default-features = false`, so there is no such backend in the tree"* — was **half wrong**, and
//! the wrong half was the one that mattered: the four *implementations* are behind cargo features,
//! but the `Backend` **trait** is in `ratatui-core` and is unconditional. No dependency changed.
//!
//! Pass-through was not a stub and the ledger is not one-sided:
//!
//! * **Kept — but this bullet was wrong until [`View::mirror_modes`] existed, and the correction
//!   is the interesting part.** It read *"the mouse still works with no encoder … the operator's
//!   terminal is put into SGR-1006 by `Sticky`'s mirrored preamble"*, and the first half is true:
//!   a mouse report is input, [`Keys`] forwards it verbatim, and nothing on the render side
//!   touches it. The second half was not. The preamble is written from `Sticky::initial(…)`, whose
//!   every mouse mode is **off**, and nothing afterwards updated it — the node's `?1000h` reaches
//!   the emulator in this process, which is not the operator's screen. So no terminal was ever put
//!   into any tracking mode, no report was ever produced, and a click in a pane did nothing.
//!   `mirror_modes` is the missing line, and it is the delta this bullet always described.
//! * **Lost.** Anything `alacritty_terminal` does not model no longer reaches the operator at all,
//!   where pass-through delivered it verbatim: OSC 8 hyperlinks, OSC 52 clipboard writes, and inline
//!   image protocols (sixel, kitty). A pane is now exactly as expressive as marion's VT, which is
//!   the price of marion having an opinion about what crosses it.
//! * **Paid.** A full parse and a viewport repaint per frame instead of one `write(2)`.
//!   `marion-tui`'s `a_frame_is_one_write_and_not_one_per_cell` holds the repaint to a single
//!   `write` regardless of cell count, and `Redraw` holds it to one repaint per DECSET 2026 frame
//!   rather than one per `read`, which is the bound that matters for a harness that paints in
//!   bursts.
//!
//! A pane is not a transparent pipe any more. It is a terminal marion owns, which is what §5.3
//! says it is.
//!
//! # Attaching must never start a supervisor
//!
//! `marion run` calls `detach::ensure_supervisor`, which starts one if none answers. This dials and
//! refuses. The difference is not caution: a node id only means something to a supervisor that has
//! it, so a freshly started one would answer `node/attach` with `NotFound` for a node the operator
//! can see in another window — a confusing report of a real absence caused by marion itself.
//!
//! # Leaving
//!
//! `^] d` closes the socket and restores the terminal. **The node keeps running**, which is the
//! whole point (§7.3.1, and M2's property at the surface M3 adds): the supervisor owns the pty
//! master, the client was only a listener, and `WriteLease` releases the keyboard on the departure
//! the closed socket produces. Nothing here has to ask for that, and this module deliberately sends
//! no `node/kill` and no `session/quit` on the way out.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use marion_core::contract::AgentId;
use marion_core::proto::{
    Call, ClientNotification, Event, Frame, Input, MethodResult, NodePaneReadyV1, NodePaneWriteV1,
    RequestId,
};
use marion_term::Term;
use marion_tui::{Action, Keys, Pane, Redraw, Screen, ScreenBackend, Sticky};
use ratatui::Terminal;

/// How long one write to the supervisor may block. A bound on a stalled peer, never a cadence: a
/// write that completes costs nothing, and one that cannot is a supervisor that stopped reading.
const WRITE_BOUND: std::time::Duration = std::time::Duration::from_millis(50);

/// How long the stream must stay quiet after output before an unbracketed straggler is painted —
/// a **deadline set by the last frame**, not a tick. See [`Session::pump`]: with nothing dirty the
/// loop waits with no timeout at all.
const PAINT_QUIET: std::time::Duration = std::time::Duration::from_millis(50);

/// How long the supervisor may stay silent before answering `node/attach`. No keyboard runs before the answer — it says whether
/// this client holds the write half — so an unbounded wait here is a terminal nothing can leave.
const ATTACH_ANSWER_BOUND: std::time::Duration = std::time::Duration::from_secs(10);

/// The id of the one request this client sends.
const ATTACH_REQUEST: RequestId = RequestId::Number(1);

#[cfg(test)]
thread_local! {
    static ATTACH_ANSWER_BOUND_FOR_TEST: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

fn attach_answer_bound() -> std::time::Duration {
    #[cfg(test)]
    if let Some(bound) = ATTACH_ANSWER_BOUND_FOR_TEST.with(std::cell::Cell::get) {
        return bound;
    }
    ATTACH_ANSWER_BOUND
}

/// Set by the `SIGWINCH` handler and read by the loop.
///
/// A `static` and an `AtomicBool` because a signal handler may do essentially nothing: a store to a
/// lock-free atomic is async-signal-safe, and anything that allocates, locks or writes a socket is
/// not. The size is read *in the loop*, where a syscall is allowed.
static RESIZED: AtomicBool = AtomicBool::new(false);

/// The self-pipe the `SIGWINCH` handler rings so the loop's `poll(2)` returns: the signal may be
/// delivered to the keyboard thread, which would leave the main thread's wait uninterrupted.
/// [`crate::wake::Pipe::wake`] is an atomic swap and one `write(2)`, both async-signal-safe, and
/// the pipe is created before the handler is installed. `None` inside only if no descriptor could
/// be had, in which case a resize is noticed at the next frame.
static RESIZE_WAKE: std::sync::OnceLock<Option<crate::wake::Pipe>> = std::sync::OnceLock::new();

fn resize_wake() -> Option<&'static crate::wake::Pipe> {
    RESIZE_WAKE
        .get_or_init(|| crate::wake::Pipe::new().ok())
        .as_ref()
}

unsafe extern "C" {
    fn signal(sig: std::ffi::c_int, handler: usize) -> usize;
}

/// `SIGWINCH`. 28 on both Darwin and Linux.
const SIGWINCH: std::ffi::c_int = 28;

extern "C" fn on_winch(_sig: std::ffi::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
    if let Some(Some(wake)) = RESIZE_WAKE.get() {
        wake.wake();
    }
}

fn arm_resize_tracking(resized: &AtomicBool, install: impl FnOnce()) {
    resized.store(false, Ordering::SeqCst);
    install();
}

/// Say why this pane will not take keys, onto the screen rather than a stderr the next frame
/// erases. §5.3's refusal is a sentence, so the holder is named whenever the supervisor named one.
fn announce_read_only(
    screen: &Screen,
    pane: &marion_core::proto::result::PaneAttach,
) -> Result<(), Refusal> {
    if pane.writable {
        return Ok(());
    }
    let banner = pane.held_by.map_or_else(
        || "\r\nmarion: read-only — this retained pane has no live keyboard.\r\n".into(),
        |holder| {
            format!("\r\nmarion: read-only — connection {holder} is typing into this node.\r\n")
        },
    );
    screen
        .write(banner.as_bytes())
        .map_err(|e| format!("showing the pane's read-only status: {e}"))
}

/// Everything that can stop an attach before it starts, each as a sentence naming what to do.
///
/// A single `String` rather than a typed error: every one of these ends the process with the same
/// exit code and the same shape of message, and the caller is a `main` that prints it. A type here
/// would be a vocabulary with exactly one consumer.
type Refusal = String;

/// How an attach that did not fail ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leave {
    /// The operator typed `^] d`; the node keeps running.
    Detached,
    /// The node's pane ended.
    Ended,
}

/// The one line printed after a detach, on the restored terminal: the node is still running and
/// this is the way back. The full id, because `marion attach` takes nothing shorter.
pub fn reattach_hint(agent_id: &str) -> String {
    format!(
        "marion: detached; node {agent_id} keeps running. Back in: `marion attach {agent_id}`, \
         or `marion ls` to see every node."
    )
}

/// Attach to `agent`, render its pane, and return when the operator detaches or the node ends.
///
/// `repo` is the directory whose supervisor to ask — §2 keys a supervisor on the git common dir, so
/// this is the same resolution `marion run` performs and not a second one.
pub fn run(agent: &str, repo: &Path, state_dir: &Path) -> Result<Leave, Refusal> {
    let key = crate::socket::project_root(repo);
    let paths = crate::socket::socket_paths(state_dir, &key, crate::socket::own_uid());
    if crate::socket::nobody_is_serving(&paths) {
        return Err(format!(
            "no supervisor is serving `{}`, so there is no node `{agent}` to attach to. Attaching \
             deliberately does not start one: a supervisor started now would have no record of \
             this node and would answer with a `not found` that is about marion rather than about \
             the node. Run `marion run <agent-type>` in this project first.",
            key.display()
        ));
    }
    let stream = crate::client_auth::dial(&paths).map_err(|e| {
        format!(
            "the supervisor for `{}` holds its lock but did not answer on {}: {e}",
            key.display(),
            paths.socket().display()
        )
    })?;

    let id = AgentId(agent.to_string());
    let mut session = Session::open(stream, id)?;
    session.pump()
}

/// One attach, from the `node/attach` response to the operator leaving.
struct Session {
    stream: UnixStream,
    writer: Arc<std::sync::Mutex<UnixStream>>,
    inbound: Vec<u8>,
    id: AgentId,
    input_fd: std::os::fd::RawFd,
    /// The grid, the backend and the guard, in one value. `None` until `node/attach` has answered
    /// with a pane — the refusals above it must leave the operator's shell exactly as it was.
    view: Option<View>,
    /// Whether this client holds the node's write half. A read-only attach still renders; it just
    /// sends nothing, and the operator was told which connection has the keyboard.
    writable: bool,
    pane_stream: PaneStream,
    /// Set by the stdin thread when the operator types `^] d`, and by the session on its way out;
    /// raising it wakes whichever of the two threads is waiting.
    leaving: Arc<crate::wake::Flag>,
    keyboard_failure: Arc<std::sync::Mutex<Option<String>>>,
    keyboard: Option<std::thread::JoinHandle<()>>,
}

/// Where the pane-v1 stream stands: before the attach response, or at the next expected frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneStream {
    Negotiating,
    V1 { next_seq: u64, cut: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneProgress {
    Continue,
    End,
}

fn write_serialized<W: Write>(
    writer: &Arc<std::sync::Mutex<W>>,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
    writer.write_all(bytes)?;
    writer.flush()
}

/// What one read onto the inbound buffer produced.
enum Fill {
    /// Bytes arrived, or the read was interrupted: either way, look for a frame again.
    Again,
    /// Nothing arrived before the deadline, or a resize or a detach woke the wait: the loop's
    /// chance to notice a `SIGWINCH`, a detach, or a stream gone quiet.
    Idle,
}

/// What one keyboard wait produced.
enum KeyRead {
    /// Woken with nothing typed (a detach raised `leaving`, or an interrupted wait): the loop's
    /// chance to notice `leaving`.
    Idle,
    Bytes(usize),
    /// The worker is done, and has already recorded why and set `leaving`.
    Stop,
}

/// The keyboard worker's whole life, on its own thread.
///
/// It ends by setting `leaving` rather than by exiting the process, so the terminal is restored by
/// the `Screen` guard on the main thread's normal return — a `std::process::exit` here would skip
/// every destructor and leave the operator's terminal in raw mode.
fn watch_keyboard(
    id: AgentId,
    writable: bool,
    input_fd: std::os::fd::RawFd,
    writer: Arc<std::sync::Mutex<UnixStream>>,
    leaving: Arc<crate::wake::Flag>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
) {
    let Some(mut stdin) = open_keyboard(input_fd, &failure, &leaving) else {
        return;
    };
    pump_keyboard(&mut stdin, id, writable, writer, leaving, failure);
}

/// The operator's keyboard as [`pump_keyboard`] reads it: a wait for input or a detach, then the
/// read, as two steps so the worker can look at `leaving` between them. A trait only so a test can hold
/// the wait open at the one instant that matters.
trait KeyboardInput {
    /// Block until input is ready (`true`) or `leaving` is raised (`false`), with no timeout.
    fn wait(&mut self, leaving: &crate::wake::Flag) -> std::io::Result<bool>;
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>>;
}

impl KeyboardInput for marion_tui::guard::Keyboard {
    fn wait(&mut self, leaving: &crate::wake::Flag) -> std::io::Result<bool> {
        // SAFETY: the keyboard descriptor is open for this reader's lifetime (`Keyboard::open`
        // checked it); the borrow ends with the wait.
        let keyboard = unsafe { std::os::fd::BorrowedFd::borrow_raw(self.fd()) };
        Ok(crate::wake::wait_until(&[Some(keyboard), leaving.fd()], None)[0])
    }
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        self.read_ready(buf)
    }
}

/// [`watch_keyboard`]'s loop, over an already-open keyboard.
fn pump_keyboard(
    stdin: &mut impl KeyboardInput,
    id: AgentId,
    writable: bool,
    writer: Arc<std::sync::Mutex<UnixStream>>,
    leaving: Arc<crate::wake::Flag>,
    failure: Arc<std::sync::Mutex<Option<String>>>,
) {
    let mut keys = Keys::new();
    let mut buf = [0u8; 4096];
    while !leaving.load(Ordering::SeqCst) {
        let n = match read_keyboard(stdin, &mut buf, &failure, &leaving) {
            KeyRead::Idle => continue,
            KeyRead::Bytes(n) => n,
            KeyRead::Stop => return,
        };
        for action in keys.feed(&buf[..n]) {
            // A read-only attach has no write half: the supervisor answers an unleased keystroke
            // by closing the connection, so only the operator's chords are acted on.
            if !writable && matches!(action, Action::Forward(_)) {
                continue;
            }
            if forward_key(action, &id, &writer, &failure, &leaving).is_break() {
                return;
            }
        }
    }
}

/// The operator's terminal, opened for the worker. A refusal here is the session's, not a detach.
fn open_keyboard(
    input_fd: std::os::fd::RawFd,
    failure: &std::sync::Mutex<Option<String>>,
    leaving: &crate::wake::Flag,
) -> Option<marion_tui::guard::Keyboard> {
    match marion_tui::guard::Keyboard::open(input_fd) {
        Ok(stdin) => Some(stdin),
        Err(error) => {
            *failure.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(format!("watching pane keyboard input: {error}"));
            leaving.store(true, Ordering::SeqCst);
            None
        }
    }
}

/// One wait for a key or a detach, then a read of what is there. A closed stdin is reported rather
/// than treated as a detach.
///
/// The wait is `poll(2)` on the keyboard and on `leaving`'s descriptor with no timeout, so an idle
/// attach's keyboard thread never wakes, and the session's drop — which raises `leaving` before
/// joining this thread — ends it at once.
///
/// **`leaving` is looked at again between the wait and the read.** The session can end while the
/// wait is open — a detach, the supervisor closing the connection — and a keystroke that woke the
/// wait after that belongs to the operator's shell, not to a node this client no longer holds. It
/// is left unread in the terminal rather than consumed and forwarded.
fn read_keyboard(
    stdin: &mut impl KeyboardInput,
    buf: &mut [u8; 4096],
    failure: &std::sync::Mutex<Option<String>>,
    leaving: &crate::wake::Flag,
) -> KeyRead {
    let read = match stdin.wait(leaving) {
        Ok(false) => Ok(None),
        Ok(true) if leaving.load(Ordering::SeqCst) => return KeyRead::Stop,
        Ok(true) => stdin.read(buf),
        Err(error) => Err(error),
    };
    match read {
        Ok(None) => KeyRead::Idle,
        Ok(Some(0)) => {
            *failure.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("pane keyboard input closed while this attach held the write lease".into());
            leaving.store(true, Ordering::SeqCst);
            KeyRead::Stop
        }
        Ok(Some(n)) => KeyRead::Bytes(n),
        Err(error) => {
            *failure.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(format!("reading pane keyboard input: {error}"));
            leaving.store(true, Ordering::SeqCst);
            KeyRead::Stop
        }
    }
}

/// One filtered keystroke, sent byte for byte. `Break` ends the worker: an operator detach, or a
/// failure it has just recorded.
fn forward_key(
    action: Action,
    id: &AgentId,
    writer: &Arc<std::sync::Mutex<UnixStream>>,
    failure: &std::sync::Mutex<Option<String>>,
    leaving: &crate::wake::Flag,
) -> std::ops::ControlFlow<()> {
    let bytes = match action {
        Action::Detach => {
            leaving.store(true, Ordering::SeqCst);
            return std::ops::ControlFlow::Break(());
        }
        // The rendered attach paints its own status row already, so the toggle has nothing to
        // show here; the keys are consumed rather than forwarded so the two clients agree on what
        // `^] s` is.
        Action::ToggleStatus => return std::ops::ControlFlow::Continue(()),
        Action::Forward(bytes) => bytes,
    };
    let input = pane_write(id, bytes);
    let method = input.method();
    let f = Frame::Input(ClientNotification::new(input));
    if let Err(error) = write_serialized(writer, f.to_line().as_bytes()) {
        *failure.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(format!("sending {method} from the keyboard: {error}"));
        leaving.store(true, Ordering::SeqCst);
        return std::ops::ControlFlow::Break(());
    }
    std::ops::ControlFlow::Continue(())
}

/// One forwarded keystroke as the byte-exact `node/pane-write` it is sent as.
fn pane_write(id: &AgentId, bytes: Vec<u8>) -> Input {
    Input::NodePaneWrite(NodePaneWriteV1 {
        agent_id: id.clone(),
        bytes: marion_core::proto::OpaquePaneBytesV1::new(bytes),
    })
}

/// One pane's emulator and its painter.
///
/// The three travel together and always will: a byte fed to `term` is a frame `redraw` may declare
/// complete, which is a repaint `terminal` performs. Keeping them in one struct is what lets
/// [`Session`] hold a single `Option` for *"the terminal has been entered"* rather than three that
/// could disagree.
struct View {
    /// **The grid marion owns.** Built with [`marion_tui::grid_options`], which is where
    /// `suppress_erase_saved` and [`marion_tui::MAX_SCROLLBACK`] come from — the two decisions that
    /// were inert while this client rendered pass-through.
    term: Term,
    redraw: Redraw,
    terminal: Terminal<ScreenBackend>,
    /// The node's private modes as this client has seen them, and **the whole of how the mouse
    /// gets through** (§9's M3 criterion C1).
    ///
    /// A mouse report is *input*: the operator's own terminal produces it, [`Keys`] forwards it to
    /// `node/pane-write` untouched, and no encoder is involved anywhere on marion's side. But a
    /// terminal only produces one if something enabled tracking on it, and the only thing that
    /// could is this client — the node's `?1000h` goes into [`Self::term`], which is an emulator in
    /// this process and not the operator's screen.
    ///
    /// So the node's modes are tracked here and mirrored out as a delta
    /// ([`marion_tui::guard::mirror_delta`]). **Mirrored, never invented**: a pane showing a codex
    /// session, which enables no tracking at all, leaves the operator's own text selection working.
    ///
    /// Before this field existed the preamble was `Sticky::initial(…)` — every mouse mode off —
    /// and nothing updated it afterwards, so the operator's terminal was never put into any
    /// tracking mode and a click in a pane did nothing at all. This module's own header claimed
    /// otherwise.
    modes: Sticky,
}

impl View {
    /// Build the node's grid and the operator's viewport independently. Pane-v1 replays ordered
    /// node resizes into the first pair; a local `SIGWINCH` changes only the second pair until the
    /// resulting `node/resize` comes back through that ordered stream.
    fn enter_split(
        screen: Screen,
        grid_cols: u16,
        grid_rows: u16,
        viewport_cols: u16,
        viewport_rows: u16,
    ) -> Result<Self, Refusal> {
        let term = Term::with_options(
            marion_term::Size::new(grid_cols.max(1) as usize, grid_rows.max(1) as usize),
            marion_tui::grid_options(),
        );
        let terminal = Terminal::new(ScreenBackend::new(screen, viewport_cols, viewport_rows))
            .map_err(|e| format!("starting the pane's renderer: {e}"))?;
        Ok(Self {
            term,
            redraw: Redraw::new(),
            terminal,
            // The same value the preamble was just written from, so the first delta is measured
            // against what the operator's terminal was actually put into rather than against a
            // second guess at it.
            modes: Sticky::initial(grid_cols, grid_rows),
        })
    }

    /// Feed the node's bytes to the grid, and paint iff a frame closed.
    ///
    /// [`Redraw`] is what makes that "iff" real: a harness that brackets its output with DECSET
    /// 2026 paints once per frame however many `read`s it took, and one that does not is caught by
    /// [`Self::idle`] instead. Painting per `read` would repaint a half-drawn screen, which is the
    /// tearing pass-through did not have and a naive grid would introduce.
    ///
    /// **The mirror runs before the grid**, and the order is not arbitrary: a node that enables
    /// tracking and immediately paints something the operator would click on should have its
    /// terminal already reporting when that paint lands. Nothing downstream depends on it, so this
    /// is a preference rather than a correctness argument — stated so a later reader does not
    /// reorder it wondering whether it mattered.
    fn feed(&mut self, bytes: impl AsRef<[u8]>) -> Result<(), Refusal> {
        let bytes = bytes.as_ref();
        self.mirror_modes(&String::from_utf8_lossy(bytes))?;
        self.term.advance(bytes);
        if self.redraw.on_feed(&self.term, bytes.len()) {
            self.paint()?;
        }
        Ok(())
    }

    /// Put the operator's terminal into whatever tracking modes the node has asked for.
    ///
    /// A failed mirror write is terminal for the attach. Continuing would hold a write lease while
    /// the operator can neither see the node's mode nor produce the matching mouse reports.
    ///
    /// **The chunk-boundary caveat is [`Sticky::absorb`]'s and is inherited rather than repaired.**
    /// A `?1000h` split across two `node/pane-frame` outputs is not seen. `marion-tui`'s
    /// `no_tracked_sequence_straddles_a_record_boundary_in_any_capture` measures that no committed
    /// capture splits one; the failure if a future one does is a mouse that does not work, which is
    /// the same failure this whole method exists to fix and not a new class of it.
    fn mirror_modes(&mut self, text: &str) -> Result<(), Refusal> {
        let was = self.modes;
        self.modes.absorb(text);
        if was != self.modes {
            self.terminal
                .backend()
                .screen()
                .mirror(&was, &self.modes)
                .map_err(|e| format!("mirroring the node's terminal modes: {e}"))?;
        }
        Ok(())
    }

    /// The unsynchronized straggler: a node that wrote something and opened no frame bracket.
    fn idle(&mut self) -> Result<(), Refusal> {
        if self.redraw.on_idle(&self.term) {
            self.paint()?;
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), Refusal> {
        if self.redraw.on_end() {
            self.paint()?;
        }
        Ok(())
    }

    fn paint(&mut self) -> Result<(), Refusal> {
        let term = &self.term;
        self.terminal
            .draw(|f| f.render_widget(Pane(term), f.area()))
            .map(|_| ())
            .map_err(|e| format!("painting the pane on the operator's terminal: {e}"))
    }

    /// Apply one ordered node-grid resize. Pane-v1 keeps this independent from the local viewport:
    /// a SIGWINCH resizes the backend immediately, but the emulator changes only when this record
    /// arrives in the node's dense stream.
    fn resize_grid(&mut self, cols: u16, rows: u16) -> Result<(), Refusal> {
        self.term.resize(marion_term::Size::new(
            cols.max(1) as usize,
            rows.max(1) as usize,
        ));
        self.paint()
    }

    fn resize_viewport(&mut self, cols: u16, rows: u16) -> Result<(), Refusal> {
        self.terminal.backend_mut().set_size(cols, rows);
        self.terminal
            .autoresize()
            .map_err(|e| format!("resizing the pane's local viewport: {e}"))?;
        self.paint()
    }
}

impl Session {
    fn open(stream: UnixStream, id: AgentId) -> Result<Session, Refusal> {
        Self::open_with_terminal(stream, id, std::io::stdout(), 0, None)
    }

    fn open_with_terminal<S: Write + Send + 'static>(
        stream: UnixStream,
        id: AgentId,
        output: S,
        input_fd: std::os::fd::RawFd,
        geometry: Option<(u16, u16)>,
    ) -> Result<Session, Refusal> {
        // Reads block, and are only issued after `poll(2)` said the socket is readable — see
        // [`Self::next_frame`] — so no read timeout is needed to keep the loop responsive.
        let writer_stream = stream
            .try_clone()
            .map_err(|e| format!("cloning the supervisor socket writer: {e}"))?;
        writer_stream
            .set_write_timeout(Some(WRITE_BOUND))
            .map_err(|e| format!("bounding supervisor socket writes: {e}"))?;
        let writer = Arc::new(std::sync::Mutex::new(writer_stream));
        let mut s = Session {
            stream,
            writer,
            inbound: Vec::new(),
            id,
            input_fd,
            view: None,
            writable: false,
            pane_stream: PaneStream::Negotiating,
            leaving: Arc::new(crate::wake::Flag::new()),
            keyboard_failure: Arc::new(std::sync::Mutex::new(None)),
            keyboard: None,
        };
        s.attach(output, geometry)?;
        Ok(s)
    }

    #[cfg(test)]
    fn open_for_test<S: Write + Send + 'static>(
        stream: UnixStream,
        id: AgentId,
        output: S,
        input_fd: std::os::fd::RawFd,
        geometry: (u16, u16),
    ) -> Result<Session, Refusal> {
        Self::open_with_terminal(stream, id, output, input_fd, Some(geometry))
    }

    /// `node/attach`, and everything that has to be true before a byte is painted.
    ///
    /// Durable `node/event` replay may precede the response and is intentionally ignored by this
    /// terminal client. Pane-v1 may not: its Ready token has not been advertised yet, and accepting
    /// an early pane frame would silently create a hole the server cannot replay afterwards.
    fn attach<S: Write + Send + 'static>(
        &mut self,
        output: S,
        geometry: Option<(u16, u16)>,
    ) -> Result<(), Refusal> {
        self.send_attach()?;
        let response = self.await_attach_response()?;
        let pane = self.decode_attached_pane(response)?;
        self.writable = pane.writable;
        let descriptor = self.ready_descriptor(&pane)?;
        self.pane_stream = PaneStream::V1 {
            next_seq: 0,
            cut: descriptor.cut,
        };

        // Clear, install, then take the authoritative size. A signal before installation is
        // reflected by the size read; one after installation remains set for the pump. Reversing
        // the first two steps would erase an edge arriving between them.
        resize_wake();
        arm_resize_tracking(&RESIZED, || unsafe {
            signal(SIGWINCH, on_winch as *const () as usize);
        });
        let (viewport_cols, viewport_rows) = geometry
            .or_else(|| marion_tui::guard::window_size(self.input_fd))
            .unwrap_or((pane.cols.max(1), pane.rows.max(1)));
        let screen = Screen::enter(
            output,
            self.input_fd,
            &Sticky::initial(viewport_cols, viewport_rows),
        )
        .map_err(|e| format!("entering the terminal: {e}"))?;
        announce_read_only(&screen, &pane)?;
        self.view = Some(View::enter_split(
            screen,
            pane.cols,
            pane.rows,
            viewport_cols,
            viewport_rows,
        )?);

        let frame = Frame::Input(ClientNotification::new(Input::NodePaneReady(
            NodePaneReadyV1 {
                agent_id: self.id.clone(),
                token: descriptor.token,
                cut: descriptor.cut,
            },
        )));
        self.write_frame(&frame, "sending node/pane-ready")?;
        if self.writable {
            // Per-session, not process-global: every attach announces its initial local geometry
            // even if a prior attach consumed the last SIGWINCH edge.
            self.send_size(viewport_cols, viewport_rows)?;
        }
        // Every attach, read-only included: the keyboard reader is the only thing that hears
        // `^] d`, and a read-only view the operator cannot leave is a wedged terminal.
        self.start_keyboard()
    }

    /// The display plane the answer attached to, or why this node has none.
    fn decode_attached_pane(
        &self,
        response: marion_core::proto::Response,
    ) -> Result<marion_core::proto::result::PaneAttach, Refusal> {
        let body = match response.outcome {
            marion_core::proto::Outcome::Result(body) => body,
            marion_core::proto::Outcome::Error(error) => {
                return Err(format!(
                    "the supervisor refused the attach: {}",
                    error.message
                ));
            }
        };
        let MethodResult::NodeAttach(attached) = marion_core::proto::Method::NodeAttach
            .decode_result(&body)
            .map_err(|e| format!("the supervisor's node/attach answer did not decode: {e}"))?
        else {
            return Err("the supervisor answered node/attach with another method's result".into());
        };
        attached.pane.ok_or_else(|| {
            format!(
                "node `{}` has no display plane, so there is no pane to attach to: it runs headless \
                 and renders as structured events. `marion run` shows those live, and the node's transcript is \
                 replayable with `node/attach` from a client that draws them.",
                self.id.0
            )
        })
    }

    /// The pane-v1 Ready boundary the answer advertised. A pane without one is refused rather
    /// than rendered: every frame after it would be a stream the client cannot place.
    fn ready_descriptor(
        &self,
        pane: &marion_core::proto::result::PaneAttach,
    ) -> Result<marion_core::proto::result::PaneReadyDescriptorV1, Refusal> {
        pane.pane_ready.clone().ok_or_else(|| {
            format!(
                "the supervisor accepted pane-stream v1 for node `{}` but omitted its Ready \
                 descriptor; refusing an ambiguous display stream",
                self.id.0
            )
        })
    }

    fn send_attach(&mut self) -> Result<(), Refusal> {
        let frame = Frame::Request(marion_core::proto::Request::new(
            ATTACH_REQUEST,
            Call::NodeAttach(marion_core::proto::params::NodeAttachParams {
                agent_id: self.id.clone(),
                pane_stream: Some(marion_core::proto::params::PaneStreamCapabilityV1::new()),
            }),
        ));
        self.write_frame(&frame, "sending node/attach")
    }

    fn await_attach_response(&mut self) -> Result<marion_core::proto::Response, Refusal> {
        // A silence bound, not a total one: durable replay may precede the answer at any length,
        // and each notification of it is the supervisor still answering.
        let bound = attach_answer_bound();
        let mut deadline = std::time::Instant::now() + bound;
        loop {
            match self.next_frame(Some(deadline))? {
                Some(Frame::Response(response)) if response.id == ATTACH_REQUEST => {
                    return Ok(response);
                }
                Some(Frame::Response(response)) => {
                    return Err(format!(
                        "the supervisor answered node/attach with unrelated response id {:?}",
                        response.id
                    ));
                }
                Some(Frame::Notification(note)) => {
                    self.absorb_pre_response(note)?;
                    deadline = std::time::Instant::now() + bound;
                }
                Some(other) => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame while answering node/attach: \
                         {other:?}"
                    ));
                }
                None if std::time::Instant::now() >= deadline => {
                    return Err(format!(
                        "the supervisor went {}s without answering node/attach for `{}`; the \
                         node was left as it was",
                        bound.as_secs_f32(),
                        self.id.0
                    ));
                }
                None => continue,
            }
        }
    }

    /// One notification that arrived before the attach response.
    ///
    /// Durable transcript replay and unrelated notifications may precede an attach response, and
    /// are ignored: this command renders only the display plane. A display frame for this node is
    /// the exception — it crosses a Ready boundary the response has not advertised yet.
    fn absorb_pre_response(
        &mut self,
        note: marion_core::proto::Notification,
    ) -> Result<(), Refusal> {
        if crate::pane_client::pane_event_targets(&self.id, &note.event) {
            return Err(format!(
                "the supervisor sent node `{}` a pane frame before its node/attach \
                 response advertised the Ready boundary",
                self.id.0
            ));
        }
        Ok(())
    }

    fn write_frame(&mut self, frame: &Frame, action: &str) -> Result<(), Refusal> {
        write_serialized(&self.writer, frame.to_line().as_bytes())
            .map_err(|e| format!("{action}: {e}"))
    }

    /// The stdin reader. A thread, because there is no portable way to select on a tty and a socket
    /// together without an event loop this client does not need.
    ///
    /// It ends by setting `leaving` rather than by exiting the process, so the terminal is restored
    /// by the `Screen` guard on the main thread's normal return — a `std::process::exit` here would
    /// skip every destructor and leave the operator's terminal in raw mode.
    fn start_keyboard(&mut self) -> Result<(), Refusal> {
        let leaving = Arc::clone(&self.leaving);
        let failure = Arc::clone(&self.keyboard_failure);
        let id = self.id.clone();
        let writable = self.writable;
        let input_fd = self.input_fd;
        let writer = Arc::clone(&self.writer);
        // Only a writable reader writes; a read-only one must start even on a socket the
        // supervisor has already closed, where this call fails.
        if writable {
            writer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_write_timeout(Some(WRITE_BOUND))
                .map_err(|e| format!("bounding pane keyboard writes: {e}"))?;
        }
        self.keyboard = Some(
            std::thread::Builder::new()
                .name("marion-attach-keys".into())
                .spawn(move || {
                    watch_keyboard(id, writable, input_fd, writer, leaving, failure);
                })
                .map_err(|e| format!("starting the pane keyboard reader: {e}"))?,
        );
        Ok(())
    }

    /// One frame, or `None` if none arrived by `deadline` (`None`: no deadline) or a resize or a
    /// detach woke the wait. `None` is not an error: it is the loop's chance to notice a
    /// `SIGWINCH`, a detach, or a stream gone quiet.
    fn next_frame(
        &mut self,
        deadline: Option<std::time::Instant>,
    ) -> Result<Option<Frame>, Refusal> {
        loop {
            if let Some(end) = self.inbound.iter().position(|byte| *byte == b'\n') {
                return self.take_frame(end).map(Some);
            }
            if self.inbound.len() > crate::serve::MAX_FRAME_BYTES {
                return Err(format!(
                    "the supervisor sent an unterminated frame larger than {} bytes",
                    crate::serve::MAX_FRAME_BYTES
                ));
            }
            if !self.wait_inbound(deadline) {
                return Ok(None);
            }
            match self.fill_inbound()? {
                Fill::Again => continue,
                Fill::Idle => return Ok(None),
            }
        }
    }

    /// Block in `poll(2)` until the supervisor's socket is readable (`true`), or until `deadline`,
    /// a `SIGWINCH` or a detach (`false`). The resize pipe is drained here, before the loop reads
    /// [`RESIZED`], so an edge after the drain leaves it readable for the next wait.
    fn wait_inbound(&self, deadline: Option<std::time::Instant>) -> bool {
        use std::os::fd::AsFd;
        let resize = resize_wake();
        let ready = crate::wake::wait_until(
            &[
                Some(self.stream.as_fd()),
                resize.map(|w| w.fd()),
                self.leaving.fd(),
            ],
            deadline,
        );
        if ready[1]
            && let Some(wake) = resize
        {
            wake.drain();
        }
        ready[0]
    }

    /// The completed line ending at `end`, taken out of the inbound buffer and decoded.
    fn take_frame(&mut self, end: usize) -> Result<Frame, Refusal> {
        if end > crate::serve::MAX_FRAME_BYTES {
            return Err(format!(
                "the supervisor sent a frame larger than {} bytes",
                crate::serve::MAX_FRAME_BYTES
            ));
        }
        let mut line = self.inbound.drain(..=end).collect::<Vec<_>>();
        line.pop();
        let line = std::str::from_utf8(&line)
            .map_err(|_| "the supervisor sent a non-UTF-8 protocol frame".to_string())?;
        Frame::from_line(line)
            .map_err(|e| format!("the supervisor sent a frame marion cannot read: {e}"))
    }

    /// One read onto the end of the inbound buffer. A close is a refusal, and it says whether it
    /// interrupted a frame, because a half-frame is evidence and an idle close is not.
    fn fill_inbound(&mut self) -> Result<Fill, Refusal> {
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            Ok(0) if self.inbound.is_empty() => Err("the supervisor closed the connection".into()),
            Ok(0) => Err(format!(
                "the supervisor closed the connection with {} bytes of an unfinished \
                 frame",
                self.inbound.len()
            )),
            Ok(n) => {
                self.inbound.extend_from_slice(&chunk[..n]);
                Ok(Fill::Again)
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(Fill::Again),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Fill::Idle),
            Err(e) => Err(format!("reading from the supervisor: {e}")),
        }
    }

    /// The render loop: bytes out, geometry in, until the operator leaves or the node ends.
    ///
    /// **Event-driven**: the wait is `poll(2)` on the socket, the resize pipe and the detach flag.
    /// Its only deadline is [`PAINT_QUIET`] after the last frame, while something fed is still
    /// unpainted; with nothing pending it has none, so an idle pane makes no wakeups.
    fn pump(&mut self) -> Result<Leave, Refusal> {
        let mut quiet_at: Option<std::time::Instant> = None;
        loop {
            // The worker records a failure for every stop but the operator's `^] d`.
            if self.leaving.load(Ordering::SeqCst) {
                if let Some(error) = self
                    .keyboard_failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    return Err(error);
                }
                return Ok(Leave::Detached);
            }
            self.forward_size()?;
            match self.next_frame(quiet_at) {
                Ok(Some(Frame::Notification(note))) => {
                    if self.consume_pane_event(note.event)? == PaneProgress::End {
                        return Ok(Leave::Ended);
                    }
                    quiet_at = Some(std::time::Instant::now() + PAINT_QUIET);
                }
                // The quiet deadline passing is the one moment the loop knows the stream is quiet.
                // That is exactly when an unbracketed straggler must be painted — a node that
                // wrote and opened no DECSET 2026 frame would otherwise sit unpainted until its
                // next byte. A resize or a detach waking the wait early is not quiet.
                Ok(None) => {
                    if quiet_at.is_some_and(|at| std::time::Instant::now() >= at) {
                        quiet_at = None;
                        if let Some(v) = &mut self.view {
                            v.idle()?;
                        }
                    }
                }
                Ok(Some(other)) => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame during pane display: {other:?}"
                    ));
                }
                Err(error) => match self.pane_stream {
                    PaneStream::V1 { .. } => {
                        return Err(format!(
                            "the pane stream ended before its terminal End frame: {error}"
                        ));
                    }
                    PaneStream::Negotiating => {
                        return Err(format!("the attach ended before negotiation: {error}"));
                    }
                },
            }
        }
    }

    fn consume_pane_event(&mut self, event: Event) -> Result<PaneProgress, Refusal> {
        match self.pane_stream {
            PaneStream::V1 { next_seq, cut } => self.consume_v1_pane_event(next_seq, cut, event),
            PaneStream::Negotiating => {
                Err("a pane event arrived before attach negotiation completed".into())
            }
        }
    }

    /// One event from the negotiated pane-v1 stream, painted and then acknowledged by advancing
    /// the expected sequence. The cut never moves; only the sequence does.
    fn consume_v1_pane_event(
        &mut self,
        next_seq: u64,
        cut: u64,
        event: Event,
    ) -> Result<PaneProgress, Refusal> {
        let decoded = crate::pane_client::decode_pane_v1_event(&self.id, next_seq, cut, event)?;
        let progress = match decoded.action {
            crate::pane_client::PaneV1Action::Output(bytes) => {
                self.view_mut()?.feed(bytes.as_bytes())?;
                PaneProgress::Continue
            }
            crate::pane_client::PaneV1Action::Resize { cols, rows } => {
                self.view_mut()?.resize_grid(cols, rows)?;
                PaneProgress::Continue
            }
            crate::pane_client::PaneV1Action::End => {
                // End is the quiet edge too: paint a final unbracketed tail before the
                // screen guard is restored.
                self.view_mut()?.end()?;
                PaneProgress::End
            }
            crate::pane_client::PaneV1Action::Ignore => PaneProgress::Continue,
        };
        self.pane_stream = PaneStream::V1 {
            next_seq: decoded.next_seq,
            cut,
        };
        Ok(progress)
    }

    fn view_mut(&mut self) -> Result<&mut View, Refusal> {
        self.view
            .as_mut()
            .ok_or_else(|| "the pane stream arrived before its grid was initialized".into())
    }

    /// Tell the supervisor this terminal's size, if it has changed.
    ///
    /// Initial geometry is sent explicitly by [`Self::attach`]; this consumes only later signal
    /// edges. That makes a second in-process attach independent of what the first consumed.
    fn forward_size(&mut self) -> Result<(), Refusal> {
        if !RESIZED.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let Some((cols, rows)) = marion_tui::guard::window_size(self.input_fd) else {
            return Ok(());
        };
        match self.pane_stream {
            PaneStream::V1 { .. } => self.view_mut()?.resize_viewport(cols, rows)?,
            PaneStream::Negotiating => {
                return Err("the terminal resized before attach negotiation completed".into());
            }
        }
        if self.writable {
            self.send_size(cols, rows)
        } else {
            Ok(())
        }
    }

    fn send_size(&mut self, cols: u16, rows: u16) -> Result<(), Refusal> {
        let f = Frame::Input(ClientNotification::new(Input::NodeResize {
            agent_id: self.id.clone(),
            cols,
            rows,
        }));
        self.write_frame(&f, "sending node/resize")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.leaving.store(true, Ordering::SeqCst);
        // Raising the flag wakes the reader's `poll`, so this join returns at once. It must
        // finish before the screen leaves raw mode or the detached thread could steal the first
        // key from the tree/shell that resumes afterwards.
        if let Some(keyboard) = self.keyboard.take() {
            let _ = keyboard.join();
        }
        // Explicit, though `Screen`'s own `Drop` would do it: the order matters on the way out.
        // The terminal must be restored before this process's last words are printed, or an error
        // message lands on the alternate screen and disappears with it.
        if let Some(v) = &self.view {
            v.terminal.backend().screen().leave();
        }
    }
}

#[cfg(test)]
mod tests;
