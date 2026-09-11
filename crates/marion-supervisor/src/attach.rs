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
use marion_proto::{
    Call, ClientNotification, Event, Frame, Input, MethodResult, NodePaneReadyV1, NodePaneWriteV1,
    RequestId,
};
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
static RESIZED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn signal(sig: std::ffi::c_int, handler: usize) -> usize;
}

/// `SIGWINCH`. 28 on both Darwin and Linux.
const SIGWINCH: std::ffi::c_int = 28;

extern "C" fn on_winch(_sig: std::ffi::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

fn arm_resize_tracking(resized: &AtomicBool, install: impl FnOnce()) {
    resized.store(false, Ordering::SeqCst);
    install();
}

/// Say why this pane will not take keys, onto the screen rather than a stderr the next frame
/// erases. §5.3's refusal is a sentence, so the holder is named whenever the supervisor named one.
fn announce_read_only(
    screen: &Screen,
    pane: &marion_proto::result::PaneAttach,
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

/// This user's real uid, which §2's `/tmp` fallback keys on. The same three lines `marion run`
/// uses, and deliberately not shared with it: the binary's copy is in a `main` this module must
/// not depend on, and moving it into `socket.rs` would put a `getuid` in a module whose whole
/// subject is paths.
pub(crate) fn uid() -> u32 {
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
    writer: Arc<std::sync::Mutex<UnixStream>>,
    inbound: Vec<u8>,
    legacy_prefix: String,
    id: AgentId,
    input_fd: std::os::fd::RawFd,
    /// The grid, the backend and the guard, in one value. `None` until `node/attach` has answered
    /// with a pane — the refusals above it must leave the operator's shell exactly as it was.
    view: Option<View>,
    /// Whether this client holds the node's write half. A read-only attach still renders; it just
    /// sends nothing, and the operator was told which connection has the keyboard.
    writable: bool,
    pane_stream: PaneStream,
    /// Set by the stdin thread when the operator types `^] d`.
    leaving: Arc<AtomicBool>,
    keyboard_failure: Arc<std::sync::Mutex<Option<String>>>,
    keyboard: Option<std::thread::JoinHandle<()>>,
}

/// The display protocol selected by the attach response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneStream {
    Negotiating,
    Legacy,
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

/// Incremental UTF-8 for legacy `node/pty-write`. Raw tty reads are not character boundaries;
/// keeping the incomplete suffix is what prevents a split paste from turning one scalar into
/// U+FFFD.
#[derive(Default)]
struct KeyboardUtf8 {
    pending: Vec<u8>,
}

impl KeyboardUtf8 {
    fn push(&mut self, bytes: &[u8]) -> Result<Option<String>, Refusal> {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_owned();
                self.pending.clear();
                Ok((!text.is_empty()).then_some(text))
            }
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                let suffix = self.pending.split_off(valid);
                if suffix.len() > 3 {
                    return Err(
                        "pane keyboard input contains an overlong incomplete UTF-8 scalar".into(),
                    );
                }
                let prefix = String::from_utf8(std::mem::replace(&mut self.pending, suffix))
                    .expect("from_utf8 identified this exact prefix as valid");
                Ok((!prefix.is_empty()).then_some(prefix))
            }
            Err(error) => Err(format!(
                "pane keyboard input is not valid UTF-8 at byte {}",
                error.valid_up_to()
            )),
        }
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// The attach response selects how the keyboard is represented on the wire. Pane-v1 is byte
/// native; the legacy notification can carry only complete UTF-8 prefixes.
enum KeyboardEncoder {
    Legacy(KeyboardUtf8),
    V1,
}

impl KeyboardEncoder {
    fn for_stream(stream: PaneStream) -> Result<Self, Refusal> {
        match stream {
            PaneStream::Legacy => Ok(Self::Legacy(KeyboardUtf8::default())),
            PaneStream::V1 { .. } => Ok(Self::V1),
            PaneStream::Negotiating => {
                Err("cannot start the keyboard before the pane protocol is selected".into())
            }
        }
    }

    fn encode(&mut self, id: &AgentId, bytes: Vec<u8>) -> Result<Option<Input>, Refusal> {
        match self {
            Self::Legacy(utf8) => Ok(utf8.push(&bytes)?.map(|bytes| Input::NodePtyWrite {
                agent_id: id.clone(),
                bytes,
            })),
            Self::V1 => Ok(Some(Input::NodePaneWrite(NodePaneWriteV1 {
                agent_id: id.clone(),
                bytes: marion_proto::OpaquePaneBytesV1::new(bytes),
            }))),
        }
    }

    fn has_pending_utf8(&self) -> bool {
        matches!(self, Self::Legacy(utf8) if utf8.has_pending())
    }
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
    /// A `?1000h` split across two `node/pty` notifications is not seen. `marion-tui`'s
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

    fn resize_legacy(&mut self, cols: u16, rows: u16) -> Result<(), Refusal> {
        self.term.resize(marion_term::Size::new(
            cols.max(1) as usize,
            rows.max(1) as usize,
        ));
        self.resize_viewport(cols, rows)
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
        stream
            .set_read_timeout(Some(POLL))
            .map_err(|e| format!("bounding supervisor socket reads: {e}"))?;
        let writer_stream = stream
            .try_clone()
            .map_err(|e| format!("cloning the supervisor socket writer: {e}"))?;
        writer_stream
            .set_write_timeout(Some(POLL))
            .map_err(|e| format!("bounding supervisor socket writes: {e}"))?;
        let writer = Arc::new(std::sync::Mutex::new(writer_stream));
        let mut s = Session {
            stream,
            writer,
            inbound: Vec::new(),
            legacy_prefix: String::new(),
            id,
            input_fd,
            view: None,
            writable: false,
            pane_stream: PaneStream::Negotiating,
            leaving: Arc::new(AtomicBool::new(false)),
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
        let (response, requested_v1) = self.negotiated_response()?;
        let pane = self.decode_attached_pane(response)?;
        self.writable = pane.writable;
        let ready = pane.pane_ready.clone();
        self.pane_stream = self.select_pane_stream(requested_v1, ready.as_ref())?;

        // Clear, install, then take the authoritative size. A signal before installation is
        // reflected by the size read; one after installation remains set for the pump. Reversing
        // the first two steps would erase an edge arriving between them.
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
        self.enter_view(screen, &pane, viewport_cols, viewport_rows)?;

        if let Some(descriptor) = ready {
            let frame = Frame::Input(ClientNotification::new(Input::NodePaneReady(
                NodePaneReadyV1 {
                    agent_id: self.id.clone(),
                    token: descriptor.token,
                    cut: descriptor.cut,
                },
            )));
            self.write_frame(&frame, "sending node/pane-ready")?;
        }
        if self.writable {
            // Per-session, not process-global: every attach announces its initial local geometry
            // even if a prior attach consumed the last SIGWINCH edge.
            self.send_size(viewport_cols, viewport_rows)?;
            if matches!(self.pane_stream, PaneStream::Legacy) {
                self.view_mut()?.resize_grid(viewport_cols, viewport_rows)?;
            }
            self.start_keyboard()?;
        }
        Ok(())
    }

    /// The attach response, and whether pane-v1 is the capability it answers.
    ///
    /// An older supervisor rejects the additive capability at parameter decoding. That is the one
    /// safe downgrade: the rejected request had no side effects. Every classified refusal and every
    /// internal failure remains visible instead of being retried through a weaker protocol.
    fn negotiated_response(&mut self) -> Result<(marion_proto::Response, bool), Refusal> {
        self.send_attach(RequestId::Number(1), true)?;
        let response = self.await_attach_response(RequestId::Number(1), true)?;
        if !matches!(&response.outcome, marion_proto::Outcome::Error(e)
            if e.code == marion_proto::error::INVALID_PARAMS)
        {
            return Ok((response, true));
        }
        self.send_attach(RequestId::Number(2), false)?;
        let retried = self.await_attach_response(RequestId::Number(2), false)?;
        Ok((retried, false))
    }

    /// The display plane the answer attached to, or why this node has none.
    fn decode_attached_pane(
        &self,
        response: marion_proto::Response,
    ) -> Result<marion_proto::result::PaneAttach, Refusal> {
        let body = match response.outcome {
            marion_proto::Outcome::Result(body) => body,
            marion_proto::Outcome::Error(error) => {
                return Err(format!(
                    "the supervisor refused the attach: {}",
                    error.message
                ));
            }
        };
        let MethodResult::NodeAttach(attached) = marion_proto::Method::NodeAttach
            .decode_result(&body)
            .map_err(|e| format!("the supervisor's node/attach answer did not decode: {e}"))?
        else {
            return Err("the supervisor answered node/attach with another method's result".into());
        };
        attached.pane.ok_or_else(|| {
            format!(
                "node `{}` has no display plane, so there is no pane to attach to. §3.4 gives a \
                 node a pty only where its surfaces declare `NativePty`; this one renders as \
                 structured events. `marion run` shows those live, and the node's transcript is \
                 replayable with `node/attach` from a client that draws them.",
                self.id.0
            )
        })
    }

    /// Which pane protocol the exchange actually settled on. The two mismatched combinations are
    /// refused rather than rendered: each would leave the display stream ambiguous in a direction
    /// the client cannot recover from afterwards.
    fn select_pane_stream(
        &self,
        requested_v1: bool,
        ready: Option<&marion_proto::result::PaneReadyDescriptorV1>,
    ) -> Result<PaneStream, Refusal> {
        match (requested_v1, ready) {
            (true, Some(descriptor)) => Ok(PaneStream::V1 {
                next_seq: 0,
                cut: descriptor.cut,
            }),
            (false, None) => Ok(PaneStream::Legacy),
            (true, None) => Err(format!(
                "the supervisor accepted pane-stream v1 for node `{}` but omitted its Ready \
                 descriptor; refusing an ambiguous display stream",
                self.id.0
            )),
            (false, Some(_)) => Err(format!(
                "the supervisor answered node `{}`'s explicit legacy retry with an unsolicited \
                 pane-v1 Ready descriptor",
                self.id.0
            )),
        }
    }

    /// The grid this attach paints into. Both protocols enter the same split view; an explicit
    /// legacy retry additionally replays the prefix that preceded its response.
    fn enter_view(
        &mut self,
        screen: Screen,
        pane: &marion_proto::result::PaneAttach,
        viewport_cols: u16,
        viewport_rows: u16,
    ) -> Result<(), Refusal> {
        self.view = Some(match self.pane_stream {
            PaneStream::V1 { .. } => {
                View::enter_split(screen, pane.cols, pane.rows, viewport_cols, viewport_rows)?
            }
            PaneStream::Legacy => {
                View::enter_split(screen, pane.cols, pane.rows, viewport_cols, viewport_rows)?
            }
            PaneStream::Negotiating => unreachable!("the response selected a pane protocol"),
        });
        if matches!(self.pane_stream, PaneStream::Legacy) {
            let prefix = std::mem::take(&mut self.legacy_prefix);
            if !prefix.is_empty() {
                self.view_mut()?.feed(prefix.as_bytes())?;
            }
        }
        Ok(())
    }

    fn send_attach(&mut self, id: RequestId, pane_v1: bool) -> Result<(), Refusal> {
        let frame = Frame::Request(marion_proto::Request::new(
            id,
            Call::NodeAttach(marion_proto::params::NodeAttachParams {
                agent_id: self.id.clone(),
                pane_stream: pane_v1.then(marion_proto::params::PaneStreamCapabilityV1::new),
            }),
        ));
        self.write_frame(&frame, "sending node/attach")
    }

    fn await_attach_response(
        &mut self,
        expected: RequestId,
        pane_v1: bool,
    ) -> Result<marion_proto::Response, Refusal> {
        loop {
            match self.next_frame()? {
                Some(Frame::Response(response)) if response.id == expected => return Ok(response),
                Some(Frame::Response(response)) => {
                    return Err(format!(
                        "the supervisor answered node/attach with unrelated response id {:?}",
                        response.id
                    ));
                }
                Some(Frame::Notification(note)) => self.absorb_pre_response(note, pane_v1)?,
                Some(other) => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame while answering node/attach: \
                         {other:?}"
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
    /// the exception — under pane-v1 it crosses a Ready boundary the response has not advertised
    /// yet, and under an explicit legacy retry it is either prefix bytes or a protocol the client
    /// did not ask for.
    fn absorb_pre_response(
        &mut self,
        note: marion_proto::Notification,
        pane_v1: bool,
    ) -> Result<(), Refusal> {
        if pane_v1 && crate::pane_client::pane_event_targets(&self.id, &note.event) {
            return Err(format!(
                "the supervisor sent node `{}` a pane frame before its node/attach \
                 response advertised the Ready boundary",
                self.id.0
            ));
        }
        if !pane_v1
            && matches!(&note.event, Event::NodePty { agent_id, .. }
                if agent_id == &self.id)
        {
            let Event::NodePty { bytes, .. } = note.event else {
                unreachable!("the guard selected node/pty")
            };
            return self.absorb_legacy_prefix(&bytes);
        }
        if !pane_v1
            && matches!(&note.event, Event::NodePaneFrame(frame)
                if frame.agent_id == self.id)
        {
            return Err(format!(
                "the supervisor sent node `{}` a pane-v1 frame while answering its \
                 explicit legacy attach",
                self.id.0
            ));
        }
        Ok(())
    }

    /// Hold legacy pane output that preceded the attach response, under the frame bound.
    fn absorb_legacy_prefix(&mut self, bytes: &str) -> Result<(), Refusal> {
        let next = self
            .legacy_prefix
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| "legacy pane prefix byte count overflowed".to_string())?;
        if next > crate::serve::MAX_FRAME_BYTES {
            return Err(format!(
                "legacy pane output exceeded the {}-byte attach prefix bound before \
                 its response",
                crate::serve::MAX_FRAME_BYTES
            ));
        }
        self.legacy_prefix.push_str(bytes);
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
        let input_fd = self.input_fd;
        let writer = Arc::clone(&self.writer);
        let mut encoder = KeyboardEncoder::for_stream(self.pane_stream)?;
        writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_write_timeout(Some(POLL))
            .map_err(|e| format!("bounding pane keyboard writes: {e}"))?;
        self.keyboard = Some(
            std::thread::Builder::new()
            .name("marion-attach-keys".into())
            .spawn(move || {
                let mut keys = Keys::new();
                let mut buf = [0u8; 4096];
                let mut stdin = match marion_tui::guard::Keyboard::open(input_fd) {
                    Ok(stdin) => stdin,
                    Err(error) => {
                        *failure.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("watching pane keyboard input: {error}"));
                        leaving.store(true, Ordering::SeqCst);
                        return;
                    }
                };
                while !leaving.load(Ordering::SeqCst) {
                    let n = match stdin.read_within(&mut buf, POLL) {
                        Ok(None) => continue,
                        Ok(Some(0)) => {
                            *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                                if encoder.has_pending_utf8() {
                                    "pane keyboard input closed in the middle of a UTF-8 scalar"
                                        .into()
                                } else {
                                    "pane keyboard input closed while this attach held the write \
                                     lease"
                                        .into()
                                },
                            );
                            leaving.store(true, Ordering::SeqCst);
                            return;
                        }
                        Ok(Some(n)) => n,
                        Err(error) => {
                            *failure.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(format!("reading pane keyboard input: {error}"));
                            leaving.store(true, Ordering::SeqCst);
                            return;
                        }
                    };
                    for action in keys.feed(&buf[..n]) {
                        match action {
                            Action::Detach => {
                                if encoder.has_pending_utf8() {
                                    *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                                        "pane detach arrived in the middle of a UTF-8 scalar".into(),
                                    );
                                }
                                leaving.store(true, Ordering::SeqCst);
                                return;
                            }
                            Action::Forward(bytes) => {
                                let input = match encoder.encode(&id, bytes) {
                                    Ok(Some(input)) => input,
                                    Ok(None) => continue,
                                    Err(error) => {
                                        *failure.lock().unwrap_or_else(|e| e.into_inner()) =
                                            Some(error);
                                        leaving.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                };
                                let method = input.method();
                                let f = Frame::Input(ClientNotification::new(input));
                                if let Err(error) =
                                    write_serialized(&writer, f.to_line().as_bytes())
                                {
                                    *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(
                                        format!("sending {method} from the keyboard: {error}"),
                                    );
                                    leaving.store(true, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }
                    }
                }
            })
            .map_err(|e| format!("starting the pane keyboard reader: {e}"))?,
        );
        Ok(())
    }

    /// One frame, or `None` if the read timed out. A timeout is not an error: it is the loop's
    /// chance to notice a `SIGWINCH` or a detach.
    fn next_frame(&mut self) -> Result<Option<Frame>, Refusal> {
        loop {
            if let Some(end) = self.inbound.iter().position(|byte| *byte == b'\n') {
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
                return Frame::from_line(line)
                    .map(Some)
                    .map_err(|e| format!("the supervisor sent a frame marion cannot read: {e}"));
            }
            if self.inbound.len() > crate::serve::MAX_FRAME_BYTES {
                return Err(format!(
                    "the supervisor sent an unterminated frame larger than {} bytes",
                    crate::serve::MAX_FRAME_BYTES
                ));
            }
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) if self.inbound.is_empty() => {
                    return Err("the supervisor closed the connection".into());
                }
                Ok(0) => {
                    return Err(format!(
                        "the supervisor closed the connection with {} bytes of an unfinished \
                         frame",
                        self.inbound.len()
                    ));
                }
                Ok(n) => self.inbound.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(format!("reading from the supervisor: {e}")),
            }
        }
    }

    /// The render loop: bytes out, geometry in, until the operator leaves or the node ends.
    fn pump(&mut self) -> Result<(), Refusal> {
        loop {
            if self.leaving.load(Ordering::SeqCst) {
                if let Some(error) = self
                    .keyboard_failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    return Err(error);
                }
                return Ok(());
            }
            self.forward_size()?;
            match self.next_frame() {
                Ok(Some(Frame::Notification(note))) => {
                    if self.consume_pane_event(note.event)? == PaneProgress::End {
                        return Ok(());
                    }
                }
                // A read timeout is the one moment the loop knows the stream is quiet. That is
                // exactly when an unbracketed straggler must be painted — a node that wrote and
                // opened no DECSET 2026 frame
                // would otherwise sit unpainted until its next byte.
                Ok(None) => {
                    if let Some(v) = &mut self.view {
                        v.idle()?;
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
                    // Legacy has no display-plane terminal marker. Preserve its historical
                    // treatment of connection loss as the end of a view.
                    PaneStream::Legacy => return Ok(()),
                    PaneStream::Negotiating => {
                        return Err(format!("the attach ended before negotiation: {error}"));
                    }
                },
            }
        }
    }

    fn consume_pane_event(&mut self, event: Event) -> Result<PaneProgress, Refusal> {
        match self.pane_stream {
            PaneStream::Legacy => match event {
                Event::NodePty {
                    agent_id, bytes, ..
                } if agent_id == self.id => {
                    self.view_mut()?.feed(bytes.as_bytes())?;
                    Ok(PaneProgress::Continue)
                }
                Event::NodeState {
                    agent_id, state, ..
                } if agent_id == self.id && state.is_exited() => {
                    self.view_mut()?.end()?;
                    Ok(PaneProgress::End)
                }
                Event::NodePaneFrame(frame) if frame.agent_id == self.id => Err(format!(
                    "the supervisor mixed node/pane-frame into node `{}`'s explicit legacy pane \
                     stream",
                    self.id.0
                )),
                _ => Ok(PaneProgress::Continue),
            },
            PaneStream::V1 { next_seq, cut } => {
                let decoded =
                    crate::pane_client::decode_pane_v1_event(&self.id, next_seq, cut, event)?;
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
            PaneStream::Negotiating => {
                Err("a pane event arrived before attach negotiation completed".into())
            }
        }
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
            PaneStream::Legacy if self.writable => self.view_mut()?.resize_legacy(cols, rows)?,
            PaneStream::Legacy => self.view_mut()?.resize_viewport(cols, rows)?,
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
        // The reader polls stdin with a `POLL` bound, so this join is bounded by `POLL`. It must
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
mod tests {
    use super::*;
    use marion_proto::PaneFrameKindV1;
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsRawFd;
    use std::sync::Mutex;

    /// Mutation: install first and clear second. An edge delivered by the newly installed handler
    /// would then be erased before the authoritative size read reaches the pump.
    #[test]
    fn a_resize_edge_after_handler_install_is_not_erased() {
        let resized = AtomicBool::new(true);
        arm_resize_tracking(&resized, || {
            assert!(
                !resized.load(Ordering::SeqCst),
                "the stale resize bit was not cleared before handler installation"
            );
            resized.store(true, Ordering::SeqCst);
        });
        assert!(
            resized.load(Ordering::SeqCst),
            "a resize edge after handler installation was erased"
        );
    }

    /// Mutation: replace the interactive attach's pane capability with `None`. The paired server
    /// sees the exact request the shipping client wrote, rather than a separately constructed
    /// value that could drift from it.
    #[test]
    fn an_interactive_attach_explicitly_requests_pane_stream_v1() {
        let (client, server) = UnixStream::pair().expect("an attach socket pair");
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let mut lines = BufReader::new(server);
            let mut line = String::new();
            lines.read_line(&mut line).expect("the attach request");
            let Frame::Request(request) = Frame::from_line(&line).expect("a protocol frame") else {
                panic!("interactive attach sent something other than a request")
            };
            let Call::NodeAttach(params) = request.call else {
                panic!("interactive attach sent another method")
            };
            seen_tx
                .send(params.pane_stream.is_some())
                .expect("the observation is delivered");
        });

        let opened = Session::open(client, AgentId("root".into()));
        assert!(opened.is_err(), "the fixture deliberately sends no answer");
        server.join().expect("the paired server did not panic");
        assert!(
            seen_rx.recv().expect("the request was observed"),
            "the interactive client silently selected the legacy lossy pane stream"
        );
    }

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

    #[derive(Clone)]
    struct FailAfter {
        writes_left: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Write for FailAfter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self
                .writes_left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_err()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "test terminal is gone",
                ));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn rendering_failure_is_visible_to_the_pane_loop() {
        let sink = FailAfter {
            // Screen::enter consumes one write for its preamble. The first paint then fails.
            writes_left: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        };
        let screen = Screen::enter(sink, -1, &Sticky::initial(80, 24)).unwrap();
        let v = View::enter_split(screen, 80, 24, 80, 24).unwrap();
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut session = bare_session(
            stream,
            PaneStream::V1 {
                next_seq: 0,
                cut: 0,
            },
            Some(v),
        );
        session
            .consume_pane_event(pane_event(
                0,
                PaneFrameKindV1::Output {
                    bytes: marion_proto::OpaquePaneBytesV1::new(b"unbracketed tail"),
                },
            ))
            .unwrap();
        let error = session
            .consume_pane_event(pane_event(1, PaneFrameKindV1::End {}))
            .unwrap_err();
        assert!(error.contains("painting the pane"), "{error}");
    }

    /// **A writable attach's first big paint must reach the terminal, however slowly it drains.**
    ///
    /// The operator's stdin and stdout are two descriptors on one tty description, exactly as this
    /// test arranges them: the keyboard reader watches the slave, the screen paints through a dup
    /// of it. A reader that made "its" descriptor nonblocking made the paint nonblocking too, and a
    /// paint bigger than the pty's output buffer — codex's TUI opens with 2.3 MB — came back
    /// `EAGAIN`, which `paint` reports as fatal: `marion attach` exited with "painting the pane on
    /// the operator's terminal: Resource temporarily unavailable (os error 35)". Nothing reads the
    /// master for a moment here, so the paint has to wait rather than fail.
    #[test]
    fn a_paint_larger_than_the_pty_buffer_completes_while_the_keyboard_is_watched() {
        use std::os::fd::AsRawFd;
        const COLS: u16 = 200;
        const ROWS: u16 = 200;

        let master = crate::pty::PtyMaster::open(crate::pty::WinSize::new(COLS, ROWS))
            .expect("an operator pty");
        let slave = master.open_slave().expect("the operator's tty");
        let stdout = std::fs::File::from(slave.try_clone().expect("dup the tty, as a shell does"));

        // Every cell in a different colour: each is an SGR plus a glyph, so the frame is far past
        // a pty output buffer (64 KiB here) before a quarter of the grid is painted.
        let mut screenful = Vec::new();
        for row in 0..ROWS as usize {
            for col in 0..COLS as usize {
                let colour = ((row * COLS as usize + col) % 255) + 1;
                screenful.extend_from_slice(format!("\x1b[38;5;{colour}mX").as_bytes());
            }
            if row + 1 < ROWS as usize {
                screenful.extend_from_slice(b"\r\n");
            }
        }

        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            server
                .write_all(
                    pane_attach_response(marion_proto::result::PaneAttach {
                        cols: COLS,
                        rows: ROWS,
                        writable: true,
                        held_by: None,
                        pane_ready: Some(marion_proto::result::PaneReadyDescriptorV1 {
                            token: pane_token(),
                            cut: 0,
                        }),
                    })
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
            // Ready and the initial NodeResize, after which the keyboard reader is running.
            for _ in 0..2 {
                line.clear();
                lines.read_line(&mut line).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            for (seq, kind) in [
                (
                    0,
                    PaneFrameKindV1::Output {
                        bytes: marion_proto::OpaquePaneBytesV1::new(screenful),
                    },
                ),
                (1, PaneFrameKindV1::End {}),
            ] {
                server
                    .write_all(
                        Frame::Notification(marion_proto::Notification::new(pane_event(seq, kind)))
                            .to_line()
                            .as_bytes(),
                    )
                    .unwrap();
            }
            server.flush().unwrap();
        });

        let painted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let drainer = {
            let painted = Arc::clone(&painted);
            let finished = Arc::clone(&finished);
            let master_fd = master.as_raw();
            std::thread::spawn(move || {
                // Long enough for the paint to have filled the pty and blocked.
                std::thread::sleep(std::time::Duration::from_millis(600));
                // The master has its own file description; this flag reaches neither slave fd.
                let master = unsafe { std::os::fd::BorrowedFd::borrow_raw(master_fd) };
                rustix::fs::fcntl_setfl(master, rustix::fs::OFlags::NONBLOCK).unwrap();
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match rustix::io::read(master, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            painted.fetch_add(n, Ordering::SeqCst);
                        }
                        Err(rustix::io::Errno::AGAIN) => {
                            if finished.load(Ordering::SeqCst) {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        let mut session = Session::open_for_test(
            client,
            AgentId("root".into()),
            stdout,
            slave.as_raw_fd(),
            (COLS, ROWS),
        )
        .unwrap();
        let pumped = session.pump();
        drop(session);
        finished.store(true, Ordering::SeqCst);
        drainer.join().unwrap();
        server_thread.join().unwrap();
        pumped.unwrap_or_else(|e| panic!("the pane loop failed on a slow terminal: {e}"));
        assert!(
            painted.load(Ordering::SeqCst) > 64 * 1024,
            "the frame was only {} bytes, which fits a pty buffer and proves nothing",
            painted.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn writable_attach_reports_keyboard_setup_failure_instead_of_hanging() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            server
                .write_all(
                    pane_attach_response(marion_proto::result::PaneAttach {
                        cols: 80,
                        rows: 24,
                        writable: true,
                        held_by: None,
                        pane_ready: Some(marion_proto::result::PaneReadyDescriptorV1 {
                            token: pane_token(),
                            cut: 0,
                        }),
                    })
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
            // Ready and the explicit initial NodeResize are both flushed before keyboard starts.
            for _ in 0..2 {
                line.clear();
                lines.read_line(&mut line).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        let mut session = Session::open_for_test(
            client,
            AgentId("root".into()),
            Sink::default(),
            -1,
            (80, 24),
        )
        .unwrap();
        let error = session.pump().unwrap_err();
        assert!(error.contains("keyboard"), "{error}");
        server_thread.join().unwrap();
    }

    fn pane_token() -> marion_proto::PaneReadyTokenV1 {
        marion_proto::PaneReadyTokenV1::new([0x5a; 32])
    }

    fn pane_attach_response(pane: marion_proto::result::PaneAttach) -> Frame {
        pane_attach_response_with_id(RequestId::Number(1), pane)
    }

    fn pane_attach_response_with_id(
        id: RequestId,
        pane: marion_proto::result::PaneAttach,
    ) -> Frame {
        use marion_core::encoding::Duration;
        use marion_core::harness::Harness;
        use marion_core::node::{NodeState, ReapState};
        use marion_proto::model::{AttachMode, NodeSummary, ReplayPoint};

        Frame::Response(marion_proto::Response::ok(
            id,
            &MethodResult::NodeAttach(marion_proto::result::NodeAttachResult {
                node: NodeSummary {
                    agent_id: AgentId("root".into()),
                    parent_id: None,
                    name: None,
                    agent_type: "codex-impl".into(),
                    harness: Harness::Codex,
                    harness_version: None,
                    depth: 0,
                    state: NodeState::Running,
                    reap_state: ReapState::Live,
                    timeout: Duration::from_secs(900),
                    pane: true,
                },
                mode: AttachMode::ResubscribeFrom(ReplayPoint {
                    records: 0,
                    src_seq: None,
                }),
                pane: Some(pane),
            }),
        ))
    }

    fn bare_session(stream: UnixStream, pane_stream: PaneStream, view: Option<View>) -> Session {
        stream.set_read_timeout(Some(POLL)).unwrap();
        let writer = stream.try_clone().unwrap();
        writer.set_write_timeout(Some(POLL)).unwrap();
        Session {
            stream,
            writer: Arc::new(std::sync::Mutex::new(writer)),
            inbound: Vec::new(),
            legacy_prefix: String::new(),
            id: AgentId("root".into()),
            input_fd: -1,
            view,
            writable: false,
            pane_stream,
            leaving: Arc::new(AtomicBool::new(false)),
            keyboard_failure: Arc::new(std::sync::Mutex::new(None)),
            keyboard: None,
        }
    }

    fn pane_event(seq: u64, frame: PaneFrameKindV1) -> Event {
        Event::NodePaneFrame(marion_proto::PaneFrameV1::new(
            AgentId("root".into()),
            seq,
            frame,
        ))
    }

    #[test]
    fn pane_v1_consumes_binary_output_dense_resize_and_end_in_order() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let (v, _sink) = view(80, 24);
        let mut session = bare_session(
            stream,
            PaneStream::V1 {
                next_seq: 0,
                cut: 0,
            },
            Some(v),
        );
        // C1 CSI is a raw terminal control byte. Lossy UTF-8 turns it into a printable replacement
        // glyph, so this fixture catches the exact corruption the byte-native path prevents.
        let raw = [0x9b, b'3', b'1', b'm', b'A'];
        assert_eq!(
            session
                .consume_pane_event(pane_event(
                    0,
                    PaneFrameKindV1::Output {
                        bytes: marion_proto::OpaquePaneBytesV1::new(raw),
                    },
                ))
                .unwrap(),
            PaneProgress::Continue
        );
        session
            .consume_pane_event(pane_event(
                1,
                PaneFrameKindV1::Resize {
                    cols: 100,
                    rows: 40,
                },
            ))
            .unwrap();
        let mut reference =
            Term::with_options(marion_term::Size::new(80, 24), marion_tui::grid_options());
        reference.advance(&raw);
        reference.resize(marion_term::Size::new(100, 40));
        let mut lossy =
            Term::with_options(marion_term::Size::new(80, 24), marion_tui::grid_options());
        lossy.advance(String::from_utf8_lossy(&raw).as_bytes());
        lossy.resize(marion_term::Size::new(100, 40));
        assert_ne!(
            reference.viewport_lines(),
            lossy.viewport_lines(),
            "the fixture would not catch a lossy UTF-8 conversion"
        );
        assert_eq!(
            session.view.as_ref().unwrap().term.viewport_lines(),
            reference.viewport_lines(),
            "pane output took a lossy text conversion before the grid"
        );
        assert_eq!(session.view.as_ref().unwrap().term.size().cols, 100);
        assert_eq!(session.view.as_ref().unwrap().term.size().rows, 40);
        assert_eq!(
            session
                .consume_pane_event(pane_event(2, PaneFrameKindV1::End {}))
                .unwrap(),
            PaneProgress::End
        );
        assert!(matches!(
            session.pane_stream,
            PaneStream::V1 {
                next_seq: 3,
                cut: 0
            }
        ));
    }

    #[test]
    fn pane_v1_rejects_sequence_gaps_duplicates_and_legacy_pty() {
        for seq in [0, 2] {
            let (stream, _peer) = UnixStream::pair().unwrap();
            let (v, _sink) = view(80, 24);
            let mut session = bare_session(
                stream,
                PaneStream::V1 {
                    next_seq: 1,
                    cut: 0,
                },
                Some(v),
            );
            let error = session
                .consume_pane_event(pane_event(seq, PaneFrameKindV1::End {}))
                .unwrap_err();
            assert!(error.contains("not dense"));
        }
        let (stream, _peer) = UnixStream::pair().unwrap();
        let (v, _sink) = view(80, 24);
        let mut session = bare_session(
            stream,
            PaneStream::V1 {
                next_seq: 0,
                cut: 0,
            },
            Some(v),
        );
        let error = session
            .consume_pane_event(Event::NodePty {
                agent_id: AgentId("root".into()),
                seq: 0,
                mono_ns: 0,
                bytes: "wrong stream".into(),
            })
            .unwrap_err();
        assert!(error.contains("mixed legacy"));
    }

    #[test]
    fn pane_v1_refuses_end_before_the_advertised_replay_cut() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let (v, _sink) = view(80, 24);
        let mut session = bare_session(
            stream,
            PaneStream::V1 {
                next_seq: 0,
                cut: 4,
            },
            Some(v),
        );
        let error = session
            .consume_pane_event(pane_event(0, PaneFrameKindV1::End {}))
            .unwrap_err();
        assert!(
            error.contains("before the advertised replay cut 4"),
            "{error}"
        );
    }

    #[test]
    fn a_terminal_node_state_does_not_truncate_pane_v1_before_end() {
        use marion_core::contract::ExitStatus;
        use marion_core::node::{NodeState, ReapState};

        let (stream, _peer) = UnixStream::pair().unwrap();
        let (v, _sink) = view(80, 24);
        let mut session = bare_session(
            stream,
            PaneStream::V1 {
                next_seq: 0,
                cut: 0,
            },
            Some(v),
        );
        assert_eq!(
            session
                .consume_pane_event(Event::NodeState {
                    agent_id: AgentId("root".into()),
                    state: NodeState::Exited(ExitStatus::Ok),
                    reap_state: ReapState::Live,
                    ts: marion_core::encoding::SystemTime::from_unix_millis(1),
                })
                .unwrap(),
            PaneProgress::Continue
        );
    }

    #[test]
    fn a_frame_split_across_a_socket_timeout_is_retained_and_decoded_once() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (v, _sink) = view(80, 24);
        let mut session = bare_session(
            client,
            PaneStream::V1 {
                next_seq: 0,
                cut: 0,
            },
            Some(v),
        );
        let bytes = vec![0x00, 0xff, 0x80, b'x'];
        let frame = Frame::Notification(marion_proto::Notification::new(pane_event(
            0,
            PaneFrameKindV1::Output {
                bytes: marion_proto::OpaquePaneBytesV1::new(bytes.clone()),
            },
        )));
        let line = frame.to_line().into_bytes();
        let middle = line.len() / 2;
        server.write_all(&line[..middle]).unwrap();
        server.flush().unwrap();
        assert!(session.next_frame().unwrap().is_none());
        server.write_all(&line[middle..]).unwrap();
        server.flush().unwrap();
        let Some(Frame::Notification(note)) = session.next_frame().unwrap() else {
            panic!("the completed frame was not decoded")
        };
        let Event::NodePaneFrame(frame) = note.event else {
            panic!("wrong event")
        };
        let PaneFrameKindV1::Output { bytes: got } = frame.frame else {
            panic!("wrong pane frame")
        };
        assert_eq!(got.as_bytes(), bytes);
        assert!(session.inbound.is_empty());
    }

    #[test]
    fn inbound_frame_bound_rejects_one_over_even_when_newline_is_present() {
        for (bytes, rejected) in [
            (crate::serve::MAX_FRAME_BYTES, false),
            (crate::serve::MAX_FRAME_BYTES + 1, true),
        ] {
            let (client, server) = UnixStream::pair().unwrap();
            let mut session = bare_session(client, PaneStream::Negotiating, None);
            session.inbound = vec![b'x'; bytes];
            session.inbound.push(b'\n');
            if rejected {
                assert!(session.next_frame().unwrap_err().contains("larger"));
            } else {
                // Exactly at the transport limit reaches parsing; it is invalid JSON, not oversize.
                let error = session.next_frame().unwrap_err();
                assert!(
                    !error.contains("larger"),
                    "exact boundary was rejected: {error}"
                );
            }
            let _ = server.shutdown(std::net::Shutdown::Both);
        }
    }

    #[test]
    fn ready_is_written_only_after_the_attach_response_and_uses_its_exact_boundary() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            let Frame::Request(request) = Frame::from_line(&line).unwrap() else {
                panic!("expected attach request")
            };
            let Call::NodeAttach(params) = request.call else {
                panic!("expected node/attach")
            };
            assert!(params.pane_stream.is_some());
            server
                .write_all(
                    pane_attach_response(marion_proto::result::PaneAttach {
                        cols: 80,
                        rows: 24,
                        writable: false,
                        held_by: Some(7),
                        pane_ready: Some(marion_proto::result::PaneReadyDescriptorV1 {
                            token: pane_token(),
                            cut: 4,
                        }),
                    })
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Input(input) = Frame::from_line(&line).unwrap() else {
                panic!("first post-response frame was not Ready")
            };
            let Input::NodePaneReady(ready) = input.input else {
                panic!("first post-response frame was not node/pane-ready")
            };
            assert_eq!(ready.agent_id, AgentId("root".into()));
            assert_eq!(ready.token, pane_token());
            assert_eq!(ready.cut, 4);
        });
        let session = Session::open_for_test(
            client,
            AgentId("root".into()),
            Sink::default(),
            -1,
            (80, 24),
        )
        .unwrap();
        assert!(matches!(
            session.pane_stream,
            PaneStream::V1 {
                next_seq: 0,
                cut: 4
            }
        ));
        drop(session);
        server_thread.join().unwrap();
    }

    #[test]
    fn invalid_params_is_the_only_legacy_retry_and_uses_a_new_response_id() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            let Frame::Request(first) = Frame::from_line(&line).unwrap() else {
                panic!("expected first attach")
            };
            assert_eq!(first.id, RequestId::Number(1));
            server
                .write_all(
                    Frame::Response(marion_proto::Response::err(
                        RequestId::Number(1),
                        marion_proto::RpcError::invalid_params("unknown field pane_stream"),
                    ))
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Request(second) = Frame::from_line(&line).unwrap() else {
                panic!("expected legacy retry")
            };
            assert_eq!(second.id, RequestId::Number(2));
            let Call::NodeAttach(params) = second.call else {
                panic!("expected node/attach")
            };
            assert!(params.pane_stream.is_none());
            server
                .write_all(
                    pane_attach_response_with_id(
                        RequestId::Number(2),
                        marion_proto::result::PaneAttach {
                            cols: 80,
                            rows: 24,
                            writable: false,
                            held_by: Some(7),
                            pane_ready: None,
                        },
                    )
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
        });
        let session = Session::open_for_test(
            client,
            AgentId("root".into()),
            Sink::default(),
            -1,
            (80, 24),
        )
        .unwrap();
        assert_eq!(session.pane_stream, PaneStream::Legacy);
        drop(session);
        server_thread.join().unwrap();
    }

    #[test]
    fn writable_legacy_retry_keeps_incremental_utf8_on_node_pty_write() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (keyboard_input, mut keyboard_writer) = UnixStream::pair().unwrap();
        let (attached_tx, attached_rx) = std::sync::mpsc::channel();
        let (prefix_tx, prefix_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();

            lines.read_line(&mut line).unwrap();
            let Frame::Request(first) = Frame::from_line(&line).unwrap() else {
                panic!("expected pane-v1 attach request")
            };
            let Call::NodeAttach(first_params) = first.call else {
                panic!("expected node/attach")
            };
            assert!(first_params.pane_stream.is_some());
            server
                .write_all(
                    Frame::Response(marion_proto::Response::err(
                        RequestId::Number(1),
                        marion_proto::RpcError::invalid_params("unknown field pane_stream"),
                    ))
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();

            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Request(second) = Frame::from_line(&line).unwrap() else {
                panic!("expected legacy attach retry")
            };
            let Call::NodeAttach(second_params) = second.call else {
                panic!("expected node/attach")
            };
            assert_eq!(second.id, RequestId::Number(2));
            assert!(second_params.pane_stream.is_none());
            server
                .write_all(
                    pane_attach_response_with_id(
                        RequestId::Number(2),
                        marion_proto::result::PaneAttach {
                            cols: 80,
                            rows: 24,
                            writable: true,
                            held_by: None,
                            pane_ready: None,
                        },
                    )
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();

            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Input(initial) = Frame::from_line(&line).unwrap() else {
                panic!("expected initial geometry")
            };
            assert!(matches!(initial.input, Input::NodeResize { .. }));
            attached_tx.send(()).unwrap();

            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Input(prefix) = Frame::from_line(&line).unwrap() else {
                panic!("expected legacy keyboard input")
            };
            let Input::NodePtyWrite { agent_id, bytes } = prefix.input else {
                panic!("legacy keyboard input used node/pane-write")
            };
            assert_eq!(agent_id, AgentId("root".into()));
            assert_eq!(bytes, "a");
            prefix_tx.send(()).unwrap();

            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Input(suffix) = Frame::from_line(&line).unwrap() else {
                panic!("expected the completed UTF-8 scalar")
            };
            let Input::NodePtyWrite { agent_id, bytes } = suffix.input else {
                panic!("legacy keyboard input used node/pane-write")
            };
            assert_eq!(agent_id, AgentId("root".into()));
            assert_eq!(bytes, "😀b");
        });

        let session = Session::open_for_test(
            client,
            AgentId("root".into()),
            Sink::default(),
            keyboard_input.as_raw_fd(),
            (80, 24),
        )
        .unwrap();
        attached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        keyboard_writer.write_all(b"a\xf0").unwrap();
        keyboard_writer.flush().unwrap();
        prefix_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        keyboard_writer.write_all(b"\x9f\x98\x80b").unwrap();
        keyboard_writer.flush().unwrap();
        server_thread.join().unwrap();
        drop(session);
    }

    #[test]
    fn writable_pane_v1_sends_arbitrary_keyboard_bytes_on_node_pane_write() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (keyboard_input, mut keyboard_writer) = UnixStream::pair().unwrap();
        let (attached_tx, attached_rx) = std::sync::mpsc::channel();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            let Frame::Request(request) = Frame::from_line(&line).unwrap() else {
                panic!("expected pane-v1 attach request")
            };
            let Call::NodeAttach(params) = request.call else {
                panic!("expected node/attach")
            };
            assert!(params.pane_stream.is_some());
            server
                .write_all(
                    pane_attach_response(marion_proto::result::PaneAttach {
                        cols: 80,
                        rows: 24,
                        writable: true,
                        held_by: None,
                        pane_ready: Some(marion_proto::result::PaneReadyDescriptorV1 {
                            token: pane_token(),
                            cut: 0,
                        }),
                    })
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();

            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Input(ready) = Frame::from_line(&line).unwrap() else {
                panic!("expected pane Ready")
            };
            assert!(matches!(ready.input, Input::NodePaneReady(_)));
            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Input(initial) = Frame::from_line(&line).unwrap() else {
                panic!("expected initial geometry")
            };
            assert!(matches!(initial.input, Input::NodeResize { .. }));
            attached_tx.send(()).unwrap();

            line.clear();
            lines.read_line(&mut line).unwrap();
            let Frame::Input(input) = Frame::from_line(&line).unwrap() else {
                panic!("expected pane-v1 keyboard input")
            };
            let Input::NodePaneWrite(write) = input.input else {
                panic!("pane-v1 keyboard input did not use node/pane-write")
            };
            assert_eq!(write.agent_id, AgentId("root".into()));
            assert_eq!(write.bytes.as_bytes(), [0x00, 0xff, 0x80, b'x']);
        });

        let session = Session::open_for_test(
            client,
            AgentId("root".into()),
            Sink::default(),
            keyboard_input.as_raw_fd(),
            (80, 24),
        )
        .unwrap();
        attached_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        keyboard_writer
            .write_all(&[0x00, 0xff, 0x80, b'x'])
            .unwrap();
        keyboard_writer.flush().unwrap();
        server_thread.join().unwrap();
        drop(session);
    }

    #[test]
    fn legacy_retry_refuses_an_unsolicited_v1_ready_descriptor() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            server
                .write_all(
                    Frame::Response(marion_proto::Response::err(
                        RequestId::Number(1),
                        marion_proto::RpcError::invalid_params("unknown field pane_stream"),
                    ))
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
            line.clear();
            lines.read_line(&mut line).unwrap();
            server
                .write_all(
                    pane_attach_response_with_id(
                        RequestId::Number(2),
                        marion_proto::result::PaneAttach {
                            cols: 80,
                            rows: 24,
                            writable: false,
                            held_by: Some(7),
                            pane_ready: Some(marion_proto::result::PaneReadyDescriptorV1 {
                                token: pane_token(),
                                cut: 0,
                            }),
                        },
                    )
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
        });
        let error = Session::open_for_test(
            client,
            AgentId("root".into()),
            Sink::default(),
            -1,
            (80, 24),
        )
        .err()
        .expect("an unsolicited v1 descriptor must be refused");
        assert!(error.contains("unsolicited"), "{error}");
        server_thread.join().unwrap();
    }

    #[test]
    fn legacy_retry_preserves_same_agent_pty_that_precedes_its_response() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            server
                .write_all(
                    Frame::Response(marion_proto::Response::err(
                        RequestId::Number(1),
                        marion_proto::RpcError::invalid_params("unknown field pane_stream"),
                    ))
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
            line.clear();
            lines.read_line(&mut line).unwrap();
            let prefix = Frame::Notification(marion_proto::Notification::new(Event::NodePty {
                agent_id: AgentId("root".into()),
                seq: 0,
                mono_ns: 0,
                bytes: "legacy-prefix".into(),
            }));
            let response = pane_attach_response_with_id(
                RequestId::Number(2),
                marion_proto::result::PaneAttach {
                    cols: 80,
                    rows: 24,
                    writable: false,
                    held_by: Some(7),
                    pane_ready: None,
                },
            );
            server
                .write_all(format!("{}{}", prefix.to_line(), response.to_line()).as_bytes())
                .unwrap();
            server.flush().unwrap();
        });
        let session = Session::open_for_test(
            client,
            AgentId("root".into()),
            Sink::default(),
            -1,
            (80, 24),
        )
        .unwrap();
        assert_eq!(
            session.view.as_ref().unwrap().term.viewport_lines()[0],
            "legacy-prefix"
        );
        assert!(session.legacy_prefix.is_empty());
        drop(session);
        server_thread.join().unwrap();
    }

    #[test]
    fn legacy_retry_refuses_a_pane_v1_frame_before_its_response() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            lines.read_line(&mut line).unwrap();
            server
                .write_all(
                    Frame::Response(marion_proto::Response::err(
                        RequestId::Number(1),
                        marion_proto::RpcError::invalid_params("unknown field pane_stream"),
                    ))
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
            line.clear();
            lines.read_line(&mut line).unwrap();
            server
                .write_all(
                    Frame::Notification(marion_proto::Notification::new(pane_event(
                        0,
                        PaneFrameKindV1::End {},
                    )))
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server.flush().unwrap();
        });
        let error = Session::open_for_test(
            client,
            AgentId("root".into()),
            Sink::default(),
            -1,
            (80, 24),
        )
        .err()
        .expect("hybrid legacy/v1 response prefix must be refused");
        assert!(error.contains("explicit legacy"), "{error}");
        server_thread.join().unwrap();
    }

    #[test]
    fn pane_v1_transport_loss_before_end_is_visible() {
        let (client, server) = UnixStream::pair().unwrap();
        let (v, _sink) = view(80, 24);
        let mut session = bare_session(
            client,
            PaneStream::V1 {
                next_seq: 0,
                cut: 0,
            },
            Some(v),
        );
        drop(server);
        let error = session.pump().unwrap_err();
        assert!(error.contains("before its terminal End"), "{error}");
    }

    #[test]
    fn legacy_terminal_state_flushes_the_final_dirty_output() {
        use marion_core::contract::ExitStatus;
        use marion_core::node::{NodeState, ReapState};

        let (stream, _peer) = UnixStream::pair().unwrap();
        let (v, sink) = view(80, 24);
        let mut session = bare_session(stream, PaneStream::Legacy, Some(v));
        session
            .consume_pane_event(Event::NodePty {
                agent_id: AgentId("root".into()),
                seq: 0,
                mono_ns: 0,
                bytes: "final-dirty-tail".into(),
            })
            .unwrap();
        assert!(written(&sink).is_empty(), "output painted before an edge");
        assert_eq!(
            session
                .consume_pane_event(Event::NodeState {
                    agent_id: AgentId("root".into()),
                    state: NodeState::Exited(ExitStatus::Ok),
                    reap_state: ReapState::Live,
                    ts: marion_core::encoding::SystemTime::from_unix_millis(1),
                })
                .unwrap(),
            PaneProgress::End
        );
        assert!(
            !written(&sink).is_empty(),
            "terminal state dropped the final paint"
        );
    }

    #[test]
    fn serialized_writer_holds_one_whole_frame_until_flush() {
        #[derive(Clone, Default)]
        struct YieldingWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for YieldingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let n = bytes.len().min(7);
                self.0.lock().unwrap().extend_from_slice(&bytes[..n]);
                std::thread::yield_now();
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let sink = YieldingWriter::default();
        let bytes = Arc::clone(&sink.0);
        let writer = Arc::new(std::sync::Mutex::new(sink));
        let lines = (0..2)
            .map(|n| {
                Frame::Input(ClientNotification::new(Input::NodeResize {
                    agent_id: AgentId(format!("agent-{n}")),
                    cols: 80 + n,
                    rows: 24,
                }))
                .to_line()
            })
            .collect::<Vec<_>>();
        let threads = lines
            .clone()
            .into_iter()
            .map(|line| {
                let writer = Arc::clone(&writer);
                std::thread::spawn(move || write_serialized(&writer, line.as_bytes()).unwrap())
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let decoded = output
            .lines()
            .map(|line| Frame::from_line(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            decoded.len(),
            2,
            "whole JSON frames interleaved: {output:?}"
        );
        for expected in lines {
            assert!(output.contains(expected.trim_end()));
        }
    }

    /// Mutation: accept or discard a pane frame while waiting for the response that advertises
    /// its token. Such a frame can never be replayed after Ready, so continuing would turn a
    /// protocol-order violation into silent terminal-byte loss.
    #[test]
    fn a_same_agent_pane_frame_before_the_attach_response_is_rejected() {
        let (client, mut server) = UnixStream::pair().expect("an attach socket pair");
        let server_thread = std::thread::spawn(move || {
            let mut request = String::new();
            BufReader::new(server.try_clone().unwrap())
                .read_line(&mut request)
                .expect("the attach request");
            let early = Frame::Notification(marion_proto::Notification::new(Event::NodePaneFrame(
                marion_proto::PaneFrameV1::new(
                    AgentId("root".into()),
                    0,
                    marion_proto::PaneFrameKindV1::Output {
                        bytes: marion_proto::OpaquePaneBytesV1::new(b"lost"),
                    },
                ),
            )));
            let response = pane_attach_response(marion_proto::result::PaneAttach {
                cols: 80,
                rows: 24,
                writable: false,
                held_by: Some(9),
                pane_ready: Some(marion_proto::result::PaneReadyDescriptorV1 {
                    token: pane_token(),
                    cut: 1,
                }),
            });
            server
                .write_all(format!("{}{}", early.to_line(), response.to_line()).as_bytes())
                .unwrap();
            server.flush().unwrap();
        });

        let error = Session::open(client, AgentId("root".into()))
            .err()
            .expect("pre-response pane bytes must refuse the attach");
        assert!(
            error.contains("before") && error.contains("node/attach"),
            "the refusal did not name the broken order: {error}"
        );
        server_thread
            .join()
            .expect("the paired server did not panic");
    }

    /// A `View` over a sink, with the guard's terminal side inert: `Screen::enter` on a non-tty fd
    /// installs no raw mode, which is exactly the state a test wants. The preamble is discarded so
    /// each assertion is about this view's own later writes.
    fn view(cols: u16, rows: u16) -> (View, Sink) {
        let sink = Sink::default();
        let screen = Screen::enter(sink.clone(), -1, &Sticky::initial(cols, rows))
            .expect("a sink is always enterable");
        let v = View::enter_split(screen, cols, rows, cols, rows).expect("a view over a sink");
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
        v.feed("\u{1b}[?1049h\u{1b}[?1000h\u{1b}[?1002h\u{1b}[?1003h\u{1b}[?1006h")
            .unwrap();
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
        v.feed("\u{1b}[?1006h").unwrap();
        sink.0.lock().unwrap().clear();

        v.feed("some ordinary output with no private modes in it")
            .unwrap();
        assert_eq!(
            written(&sink),
            "",
            "a plain paint put mode sequences on the operator's terminal"
        );

        v.feed("\u{1b}[?1006h").unwrap();
        assert_eq!(
            written(&sink),
            "",
            "a mode the operator's terminal is already in was re-asserted"
        );

        v.feed("\u{1b}[?1006l").unwrap();
        assert_eq!(
            written(&sink),
            "\u{1b}[?1006l",
            "the node turned tracking off and the operator's terminal kept reporting"
        );
    }
}
