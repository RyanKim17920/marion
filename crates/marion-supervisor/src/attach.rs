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

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use marion_core::contract::AgentId;
use marion_proto::{Call, ClientNotification, Event, Frame, Input, MethodResult, RequestId};
use marion_term::Term;
use marion_tui::{Action, Keys, Pane, Redraw, Screen, ScreenBackend, Sticky};
use ratatui::Terminal;

/// How long a socket read may block before the loop looks at the flags again.
///
/// The loop has two other things to notice — a `SIGWINCH` and the reader thread's detach — and
/// neither of them writes to the socket. A blocking read would make an idle node's pane unable to
/// resize and unable to be left, which is the failure a reader would report as "attach hangs".
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Set by the `SIGWINCH` handler and read by the loop.
///
/// A `static` and an `AtomicBool` because a signal handler may do essentially nothing: a store to a
/// lock-free atomic is async-signal-safe, and anything that allocates, locks or writes a socket is
/// not. The size is read *in the loop*, where a syscall is allowed.
static RESIZED: AtomicBool = AtomicBool::new(true);

unsafe extern "C" {
    fn signal(sig: std::ffi::c_int, handler: usize) -> usize;
}

/// `SIGWINCH`. 28 on both Darwin and Linux.
const SIGWINCH: std::ffi::c_int = 28;

extern "C" fn on_winch(_sig: std::ffi::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

/// This user's real uid, which §2's `/tmp` fallback keys on. The same three lines `marion run`
/// uses, and deliberately not shared with it: the binary's copy is in a `main` this module must
/// not depend on, and moving it into `socket.rs` would put a `getuid` in a module whose whole
/// subject is paths.
fn uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: reads the calling process's real uid and cannot fail.
    unsafe { getuid() }
}

/// Everything that can stop an attach before it starts, each as a sentence naming what to do.
///
/// A single `String` rather than a typed error: every one of these ends the process with the same
/// exit code and the same shape of message, and the caller is a `main` that prints it. A type here
/// would be a vocabulary with exactly one consumer.
type Refusal = String;

/// Attach to `agent`, render its pane, and return when the operator detaches or the node ends.
///
/// `repo` is the directory whose supervisor to ask — §2 keys a supervisor on the git common dir, so
/// this is the same resolution `marion run` performs and not a second one.
pub fn run(agent: &str, repo: &Path, state_dir: &Path) -> Result<(), Refusal> {
    let key = crate::socket::project_root(repo);
    let paths = crate::socket::socket_paths(state_dir, &key, uid());
    if crate::socket::nobody_is_serving(&paths) {
        return Err(format!(
            "no supervisor is serving `{}`, so there is no node `{agent}` to attach to. Attaching \
             deliberately does not start one: a supervisor started now would have no record of \
             this node and would answer with a `not found` that is about marion rather than about \
             the node. Run `marion run <agent-type>` in this project first.",
            key.display()
        ));
    }
    let stream = UnixStream::connect(paths.socket()).map_err(|e| {
        format!(
            "the supervisor for `{}` holds its lock but did not answer on {}: {e}",
            key.display(),
            paths.socket().display()
        )
    })?;
    stream
        .set_read_timeout(Some(POLL))
        .map_err(|e| format!("setting a read bound on the supervisor socket: {e}"))?;

    let id = AgentId(agent.to_string());
    let mut session = Session::open(stream, id)?;
    session.pump()
}

/// One attach, from the `node/attach` response to the operator leaving.
struct Session {
    stream: UnixStream,
    lines: BufReader<UnixStream>,
    id: AgentId,
    /// The grid, the backend and the guard, in one value. `None` until `node/attach` has answered
    /// with a pane — the refusals above it must leave the operator's shell exactly as it was.
    view: Option<View>,
    /// Whether this client holds the node's write half. A read-only attach still renders; it just
    /// sends nothing, and the operator was told which connection has the keyboard.
    writable: bool,
    /// Set by the stdin thread when the operator types `^] d`.
    leaving: Arc<AtomicBool>,
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
    /// `node/pty-write` untouched, and no encoder is involved anywhere on marion's side. But a
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
    fn enter(screen: Screen, cols: u16, rows: u16) -> Result<Self, Refusal> {
        let term = Term::with_options(
            marion_term::Size::new(cols.max(1) as usize, rows.max(1) as usize),
            marion_tui::grid_options(),
        );
        let terminal = Terminal::new(ScreenBackend::new(screen, cols, rows))
            .map_err(|e| format!("starting the pane's renderer: {e}"))?;
        Ok(Self {
            term,
            redraw: Redraw::new(),
            terminal,
            // The same value the preamble was just written from, so the first delta is measured
            // against what the operator's terminal was actually put into rather than against a
            // second guess at it.
            modes: Sticky::initial(cols, rows),
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
    fn feed(&mut self, text: &str) {
        self.mirror_modes(text);
        self.term.advance(text.as_bytes());
        if self.redraw.on_feed(&self.term, text.len()) {
            self.paint();
        }
    }

    /// Put the operator's terminal into whatever tracking modes the node has asked for.
    ///
    /// A failed write is dropped for [`Self::paint`]'s reason: the operator's terminal going away
    /// is reported by the loop noticing the socket or the node, not by an error raised from inside
    /// a byte feed with a half-painted screen.
    ///
    /// **The chunk-boundary caveat is [`Sticky::absorb`]'s and is inherited rather than repaired.**
    /// A `?1000h` split across two `node/pty` notifications is not seen. `marion-tui`'s
    /// `no_tracked_sequence_straddles_a_record_boundary_in_any_capture` measures that no committed
    /// capture splits one; the failure if a future one does is a mouse that does not work, which is
    /// the same failure this whole method exists to fix and not a new class of it.
    fn mirror_modes(&mut self, text: &str) {
        let was = self.modes;
        self.modes.absorb(text);
        if was != self.modes {
            let _ = self.terminal.backend().screen().mirror(&was, &self.modes);
        }
    }

    /// The unsynchronized straggler: a node that wrote something and opened no frame bracket.
    fn idle(&mut self) {
        if self.redraw.on_idle(&self.term) {
            self.paint();
        }
    }

    fn paint(&mut self) {
        // A paint that fails is a terminal that has gone — reported by the loop noticing the socket
        // or the node, never by this returning an error nobody can act on with the screen already
        // half-written.
        let term = &self.term;
        let _ = self
            .terminal
            .draw(|f| f.render_widget(Pane(term), f.area()));
    }

    /// The operator's window changed. **The grid is resized as well as the backend**, and both
    /// before the repaint: a `Terminal` whose backend reports a size its buffer does not have
    /// draws the old geometry into the new one, and `Pane` clips rather than scaling, so the
    /// mismatch shows as a pane that has stopped filling its window.
    fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(marion_term::Size::new(
            cols.max(1) as usize,
            rows.max(1) as usize,
        ));
        self.terminal.backend_mut().set_size(cols, rows);
        let _ = self.terminal.autoresize();
        self.paint();
    }
}

impl Session {
    fn open(stream: UnixStream, id: AgentId) -> Result<Session, Refusal> {
        let lines = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("cloning the supervisor socket: {e}"))?,
        );
        let mut s = Session {
            stream,
            lines,
            id,
            view: None,
            writable: false,
            leaving: Arc::new(AtomicBool::new(false)),
        };
        s.attach()?;
        Ok(s)
    }

    /// `node/attach`, and everything that has to be true before a byte is painted.
    ///
    /// **The replay notifications arrive before the response**, by the handler's construction, so
    /// this reads frames until the response rather than expecting it first. They are discarded
    /// here: they are `node/event` records of what the node *said*, which is the transcript
    /// surface, and this client draws a terminal.
    fn attach(&mut self) -> Result<(), Refusal> {
        let frame = Frame::Request(marion_proto::Request::new(
            RequestId::Number(1),
            Call::NodeAttach(marion_proto::params::NodeAttachParams {
                agent_id: self.id.clone(),
            }),
        ));
        self.stream
            .write_all(frame.to_line().as_bytes())
            .and_then(|()| self.stream.flush())
            .map_err(|e| format!("sending node/attach: {e}"))?;

        let result = loop {
            match self.next_frame()? {
                Some(Frame::Response(r)) => break r,
                // Replay, and anything else the supervisor says while answering.
                Some(_) => continue,
                None => continue,
            }
        };
        let body = match result.outcome {
            marion_proto::Outcome::Result(b) => b,
            marion_proto::Outcome::Error(e) => {
                return Err(format!("the supervisor refused the attach: {}", e.message));
            }
        };
        let MethodResult::NodeAttach(attached) = marion_proto::Method::NodeAttach
            .decode_result(&body)
            .map_err(|e| format!("the supervisor's node/attach answer did not decode: {e}"))?
        else {
            return Err("the supervisor answered node/attach with another method's result".into());
        };
        let Some(pane) = attached.pane else {
            return Err(format!(
                "node `{}` has no display plane, so there is no pane to attach to. §3.4 gives a \
                 node a pty only where its surfaces declare `NativePty`; this one renders as \
                 structured events. `marion run` shows those live, and the node's transcript is \
                 replayable with `node/attach` from a client that draws them.",
                self.id.0
            ));
        };
        self.writable = pane.writable;

        // The terminal is entered **after** the refusals above, so an attach that cannot happen
        // leaves the operator's shell exactly as it was rather than flashing an alternate screen.
        //
        // `Sticky::default()` is the honest starting assumption: this client has replayed nothing,
        // so it knows none of the node's modes yet, and the preamble it writes is the neutral one.
        // The node's own sequences arrive in the byte stream and set the rest.
        // The client's own geometry, or the pty's if this terminal has none to report — a pipe
        // has no size, and inventing 80x24 for it would put a wrong number in the preamble rather
        // than the node's real one.
        let (cols, rows) =
            marion_tui::guard::window_size(0).unwrap_or((pane.cols.max(1), pane.rows.max(1)));
        let screen = Screen::enter(std::io::stdout(), 0, &Sticky::initial(cols, rows))
            .map_err(|e| format!("entering the terminal: {e}"))?;
        if !pane.writable {
            let _ = screen.write(
                format!(
                    "\r\nmarion: read-only — connection {} is typing into this node.\r\n",
                    pane.held_by.unwrap_or_default()
                )
                .as_bytes(),
            );
        }
        self.view = Some(View::enter(screen, cols, rows)?);

        if self.writable {
            // SAFETY: installing a handler whose whole body is one atomic store. Done after the
            // screen guard so a failure above cannot leave a handler pointing into a torn-down
            // process.
            unsafe { signal(SIGWINCH, on_winch as *const () as usize) };
            self.start_keyboard();
        }
        Ok(())
    }

    /// The stdin reader. A thread, because there is no portable way to select on a tty and a socket
    /// together without an event loop this client does not need.
    ///
    /// It ends by setting `leaving` rather than by exiting the process, so the terminal is restored
    /// by the `Screen` guard on the main thread's normal return — a `std::process::exit` here would
    /// skip every destructor and leave the operator's terminal in raw mode.
    fn start_keyboard(&self) {
        let leaving = Arc::clone(&self.leaving);
        let id = self.id.clone();
        let Ok(mut sock) = self.stream.try_clone() else {
            return;
        };
        std::thread::Builder::new()
            .name("marion-attach-keys".into())
            .spawn(move || {
                let mut keys = Keys::new();
                let mut buf = [0u8; 4096];
                let mut stdin = std::io::stdin();
                while !leaving.load(Ordering::SeqCst) {
                    let n = match stdin.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    for action in keys.feed(&buf[..n]) {
                        match action {
                            Action::Detach => {
                                leaving.store(true, Ordering::SeqCst);
                                return;
                            }
                            Action::Forward(bytes) => {
                                // **Lossy, and that is the right conversion here.** A keystroke
                                // that is not valid UTF-8 is a byte the operator's terminal
                                // produced under some encoding marion does not speak; passing it
                                // through as a replacement character is wrong in the same small
                                // way for every terminal, whereas dropping the whole chunk would
                                // lose the ordinary keys around it.
                                let text = String::from_utf8_lossy(&bytes).into_owned();
                                let f =
                                    Frame::Input(ClientNotification::new(Input::NodePtyWrite {
                                        agent_id: id.clone(),
                                        bytes: text,
                                    }));
                                if sock.write_all(f.to_line().as_bytes()).is_err() {
                                    return;
                                }
                                let _ = sock.flush();
                            }
                        }
                    }
                }
            })
            .ok();
    }

    /// One frame, or `None` if the read timed out. A timeout is not an error: it is the loop's
    /// chance to notice a `SIGWINCH` or a detach.
    fn next_frame(&mut self) -> Result<Option<Frame>, Refusal> {
        let mut line = String::new();
        match self.lines.read_line(&mut line) {
            Ok(0) => Err("the supervisor closed the connection".into()),
            Ok(_) => Frame::from_line(&line)
                .map(Some)
                .map_err(|e| format!("the supervisor sent a frame marion cannot read: {e}")),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(format!("reading from the supervisor: {e}")),
        }
    }

    /// The render loop: bytes out, geometry in, until the operator leaves or the node ends.
    fn pump(&mut self) -> Result<(), Refusal> {
        loop {
            if self.leaving.load(Ordering::SeqCst) {
                return Ok(());
            }
            self.forward_size();
            match self.next_frame() {
                Ok(Some(Frame::Notification(n))) => match n.event {
                    Event::NodePty {
                        agent_id, bytes, ..
                    } if agent_id == self.id => {
                        if let Some(v) = &mut self.view {
                            v.feed(&bytes);
                        }
                    }
                    // The node reached a terminal state. Leaving is the honest response: there is
                    // nothing more to paint, and a client that stayed would look like a hung pane.
                    Event::NodeState {
                        agent_id, state, ..
                    } if agent_id == self.id && state.is_exited() => {
                        return Ok(());
                    }
                    _ => {}
                },
                // A read timeout, or a frame for some other node. Either way it is the one moment
                // the loop knows the stream is quiet, which is exactly when an unbracketed
                // straggler must be painted — a node that wrote and opened no DECSET 2026 frame
                // would otherwise sit unpainted until its next byte.
                Ok(_) => {
                    if let Some(v) = &mut self.view {
                        v.idle();
                    }
                }
                // The supervisor going away ends the attach and is not a failure of it: the node
                // was never this client's to keep.
                Err(_) => return Ok(()),
            }
        }
    }

    /// Tell the supervisor this terminal's size, if it has changed.
    ///
    /// Runs once at startup too — `RESIZED` starts `true` — because the master's size was fixed
    /// before the child existed, by a supervisor that could not know what terminal would attach.
    /// A client that only spoke on `SIGWINCH` would render a node painted at somebody else's
    /// geometry until the operator happened to drag a window edge.
    fn forward_size(&mut self) {
        if !self.writable || !RESIZED.swap(false, Ordering::SeqCst) {
            return;
        }
        let Some((cols, rows)) = marion_tui::guard::window_size(0) else {
            return;
        };
        // **This client's own grid first, and the node second.** The two are independent: the
        // supervisor's `TIOCSWINSZ` decides what the *node* paints at, and the grid here decides
        // what this operator sees. Resizing only the node would leave the pane rendering the new
        // output into the old geometry until something else happened to repaint.
        if let Some(v) = &mut self.view {
            v.resize(cols, rows);
        }
        let f = Frame::Input(ClientNotification::new(Input::NodeResize {
            agent_id: self.id.clone(),
            cols,
            rows,
        }));
        let _ = self
            .stream
            .write_all(f.to_line().as_bytes())
            .and_then(|()| self.stream.flush());
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.leaving.store(true, Ordering::SeqCst);
        // Explicit, though `Screen`'s own `Drop` would do it: the order matters on the way out.
        // The terminal must be restored before this process's last words are printed, or an error
        // message lands on the alternate screen and disappears with it.
        if let Some(v) = &self.view {
            v.terminal.backend().screen().leave();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A sink that records what marion wrote to the operator's terminal, so the assertions are
    /// about bytes rather than about a mock's expectations.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A `View` over a sink, with the guard's terminal side inert: `Screen::enter` on a non-tty fd
    /// installs no raw mode, which is exactly the state a test wants. The preamble is discarded so
    /// each assertion is about this view's own later writes.
    fn view(cols: u16, rows: u16) -> (View, Sink) {
        let sink = Sink::default();
        let screen = Screen::enter(sink.clone(), -1, &Sticky::initial(cols, rows))
            .expect("a sink is always enterable");
        let v = View::enter(screen, cols, rows).expect("a view over a sink");
        sink.0.lock().unwrap().clear();
        (v, sink)
    }

    fn written(sink: &Sink) -> String {
        String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned()
    }

    /// **§9's M3 criterion C1, the mouse clause's first leg: a terminal that reports at all.**
    ///
    /// The sequences below are the ones `tests/fixtures/s2/claude-2.1.220-boot-exit.raw.bin`
    /// carries at bytes 98–122, in that order. They are pinned here rather than against a live
    /// harness because **no harness marion can pane sends them today**: 2.1.225 enables no mouse
    /// mode at all (probed 2026-08-08) and codex never has (§5.3). The mirror is not thereby
    /// speculative — it is the whole reason a click worked on 2.1.220 and the reason one will work
    /// on the next harness that asks — but a live assertion would be pinning a version.
    ///
    /// Mutation: drop the `screen().mirror(...)` call in `View::mirror_modes`, or return early
    /// from it. This fails, and so does nothing else in the workspace.
    #[test]
    fn the_nodes_mouse_modes_are_mirrored_onto_the_operators_terminal() {
        let (mut v, sink) = view(80, 24);
        v.feed("\u{1b}[?1049h\u{1b}[?1000h\u{1b}[?1002h\u{1b}[?1003h\u{1b}[?1006h");
        let out = written(&sink);
        for m in ["?1000h", "?1002h", "?1003h", "?1006h"] {
            assert!(
                out.contains(m),
                "the node asked for {m} and the operator's terminal was never told, so it would \
                 emit no report for `Keys` to forward: {out:?}"
            );
        }
        // **`?1049` is marion's own and is deliberately not mirrored.** The client entered the
        // alternate screen in its preamble and leaves it in `leave_bytes`; echoing the node's
        // switch would double-enter one buffer and, on the node's restore, drop marion out of a
        // screen it is still painting into.
        assert!(
            !out.contains("?1049"),
            "the node's alternate-screen switch was forwarded to the operator's terminal, which \
             is marion's own to hold: {out:?}"
        );
    }

    /// A delta, not a re-assertion: a mode already mirrored is not sent again, and one turned off
    /// is turned off.
    #[test]
    fn the_mirror_sends_only_what_changed() {
        let (mut v, sink) = view(80, 24);
        v.feed("\u{1b}[?1006h");
        sink.0.lock().unwrap().clear();

        v.feed("some ordinary output with no private modes in it");
        assert_eq!(
            written(&sink),
            "",
            "a plain paint put mode sequences on the operator's terminal"
        );

        v.feed("\u{1b}[?1006h");
        assert_eq!(
            written(&sink),
            "",
            "a mode the operator's terminal is already in was re-asserted"
        );

        v.feed("\u{1b}[?1006l");
        assert_eq!(
            written(&sink),
            "\u{1b}[?1006l",
            "the node turned tracking off and the operator's terminal kept reporting"
        );
    }
}
