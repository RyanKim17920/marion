//! Transparent client relay for a claimed native facade.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use marion_core::contract::AgentId;
use marion_proto::{
    Call, ClientNotification, Event, Frame, Input, MethodResult, NodePaneReadyV1, NodePaneWriteV1,
    RequestId,
};
use marion_tui::{Action, Keys};

type Refusal = String;

const POLL: std::time::Duration = std::time::Duration::from_millis(50);
static RESIZED: AtomicBool = AtomicBool::new(false);
static RESIZE_SIGNAL_OWNER: Mutex<()> = Mutex::new(());
#[cfg(test)]
static AFTER_RESIZE_SIGNAL_ACQUIRE: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

unsafe extern "C" {
    #[cfg(test)]
    fn signal(sig: std::ffi::c_int, handler: usize) -> usize;
    fn sigaction(sig: std::ffi::c_int, action: *const Sigaction, prior: *mut Sigaction) -> i32;
}

const SIGWINCH: std::ffi::c_int = 28;

#[cfg(target_os = "macos")]
type SigSet = u32;
#[cfg(target_os = "macos")]
const fn empty_sigset() -> SigSet {
    0
}

#[cfg(target_os = "linux")]
type SigSet = [usize; 128 / std::mem::size_of::<usize>()];
#[cfg(target_os = "linux")]
const fn empty_sigset() -> SigSet {
    [0; 128 / std::mem::size_of::<usize>()]
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct Sigaction {
    handler: usize,
    mask: SigSet,
    flags: i32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct Sigaction {
    handler: usize,
    mask: SigSet,
    flags: i32,
    restorer: usize,
}

impl Sigaction {
    const fn zeroed() -> Self {
        Self {
            handler: 0,
            mask: empty_sigset(),
            flags: 0,
            #[cfg(target_os = "linux")]
            restorer: 0,
        }
    }

    fn resize_handler() -> Self {
        Self {
            handler: on_winch as *const () as usize,
            mask: empty_sigset(),
            // The relay retries interrupted reads itself. Zero avoids importing a platform-specific
            // SA_RESTART value (Darwin and Linux deliberately assign different bits).
            flags: 0,
            #[cfg(target_os = "linux")]
            restorer: 0,
        }
    }
}

extern "C" fn on_winch(_: std::ffi::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

/// Exclusive ownership of marion's process-global resize handler for one native relay session.
struct ResizeSignalGuard {
    prior: Sigaction,
    _owner: MutexGuard<'static, ()>,
}

impl ResizeSignalGuard {
    fn acquire() -> Result<Self, Refusal> {
        let owner = match RESIZE_SIGNAL_OWNER.try_lock() {
            Ok(owner) => owner,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return Err("another native relay already owns SIGWINCH".into());
            }
        };
        // Each relay begins with only its explicit initial geometry. No resize edge from a prior
        // owner may leak into this session.
        RESIZED.store(false, Ordering::SeqCst);
        let action = Sigaction::resize_handler();
        let mut prior = Sigaction::zeroed();
        // SAFETY: both values exactly match libc's supported Darwin/Linux `struct sigaction`
        // layout. This single call atomically installs marion's handler and captures the prior
        // handler, mask, flags, and restorer, leaving no query/install clobber window.
        if unsafe { sigaction(SIGWINCH, &action, &mut prior) } != 0 {
            let error = std::io::Error::last_os_error();
            RESIZED.store(false, Ordering::SeqCst);
            return Err(format!("installing the process SIGWINCH action: {error}"));
        }
        Ok(Self {
            prior,
            _owner: owner,
        })
    }
}

impl Drop for ResizeSignalGuard {
    fn drop(&mut self) {
        // SAFETY: `prior` is the unmodified action libc returned for this exact signal. Restoration
        // happens while `_owner` still excludes another in-process native relay.
        let _ = unsafe { sigaction(SIGWINCH, &self.prior, std::ptr::null_mut()) };
        // Restore first, then clear: a later SIGWINCH can no longer set marion's flag, so no edge
        // can appear between cleanup and the next session's acquisition.
        RESIZED.store(false, Ordering::SeqCst);
    }
}

struct OwnedFdReader(std::os::fd::OwnedFd);

impl Read for OwnedFdReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        rustix::io::read(&self.0, bytes).map_err(Into::into)
    }
}

struct OwnedFdWriter(std::os::fd::OwnedFd);

impl Write for OwnedFdWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        rustix::io::write(&self.0, bytes).map_err(Into::into)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Claim the authenticated launch on its issuing socket and transparently relay its pane until
/// End or operator detach. The terminal guard remains live across every protocol and I/O failure.
#[expect(
    dead_code,
    reason = "compiled for review and tests while production descriptors and dispatch stay dark"
)]
pub(crate) fn run(handoff: crate::native_bootstrap::NativeFacadeHandoff) -> Result<(), Refusal> {
    let claimed = handoff
        .claim()
        .map_err(|error| format!("claiming the native launch: {error}"))?;
    let (agent_id, tty, stream) = claimed.into_parts();
    let mut terminal = tty
        .enter_native_relay()
        .map_err(|error| format!("entering native terminal relay mode: {error}"))?;
    relay_claimed(agent_id, &mut terminal, stream)
}

pub(crate) fn relay_claimed(
    agent_id: AgentId,
    terminal: &mut crate::native_tty::NativeRelayTerminal,
    stream: UnixStream,
) -> Result<(), Refusal> {
    let primary = (|| {
        let input_fd = {
            use std::os::fd::AsRawFd;
            terminal.stdin().as_raw_fd()
        };
        let stdin = rustix::io::fcntl_dupfd_cloexec(terminal.stdin(), 3)
            .map_err(|error| format!("retaining native terminal input: {error}"))?;
        let stdout = rustix::io::fcntl_dupfd_cloexec(terminal.stdout(), 3)
            .map_err(|error| format!("retaining native terminal output: {error}"))?;
        let mut session = RawPaneSession::open_with_io(
            stream,
            agent_id,
            OwnedFdReader(stdin),
            Some(input_fd),
            OwnedFdWriter(stdout),
            None,
        )?;
        #[cfg(test)]
        if std::env::var_os("MARION_NATIVE_RELAY_PROBE").is_some() {
            eprintln!("NATIVE_RELAY_READY");
        }
        let result = session.pump();
        drop(session);
        result
    })();
    finish_terminal_relay(terminal, primary)
}

pub(crate) fn finish_terminal_relay(
    terminal: &mut crate::native_tty::NativeRelayTerminal,
    primary: Result<(), Refusal>,
) -> Result<(), Refusal> {
    match (primary, terminal.restore_result()) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(format!(
            "native relay cleanup stage `terminal restoration` failed: {cleanup}"
        )),
        (Err(primary), Err(cleanup)) => Err(format!(
            "{primary}; native relay cleanup stage `terminal restoration` failed: {cleanup}"
        )),
    }
}

fn write_serialized<W: Write>(
    writer: &Arc<std::sync::Mutex<W>>,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut writer = writer.lock().unwrap_or_else(|error| error.into_inner());
    writer.write_all(bytes)?;
    writer.flush()
}

/// A pane-v1 session whose display is the caller's retained stdout descriptor itself.
///
/// Unlike [`crate::attach`], this owns no grid, alternate screen, sticky-mode mirror, or painter:
/// an Output frame is one exact byte slice written to `output`.
struct RawPaneSession<W: Write> {
    stream: UnixStream,
    writer: Arc<std::sync::Mutex<UnixStream>>,
    inbound: Vec<u8>,
    id: AgentId,
    output: W,
    next_seq: u64,
    cut: u64,
    input_fd: Option<std::os::fd::RawFd>,
    resize_signal: Option<ResizeSignalGuard>,
    leaving: Arc<AtomicBool>,
    keyboard_failure: Arc<std::sync::Mutex<Option<String>>>,
    keyboard: Option<std::thread::JoinHandle<()>>,
}

impl<W: Write> RawPaneSession<W> {
    fn open_with_io<R: Read + Send + 'static>(
        stream: UnixStream,
        id: AgentId,
        input: R,
        input_fd: Option<std::os::fd::RawFd>,
        output: W,
        geometry: Option<(u16, u16)>,
    ) -> Result<Self, Refusal> {
        let resize_signal = geometry
            .is_none()
            .then(ResizeSignalGuard::acquire)
            .transpose()?;
        #[cfg(test)]
        if let Some(hook) = AFTER_RESIZE_SIGNAL_ACQUIRE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            hook();
        }
        let geometry = match (geometry, input_fd) {
            (Some(geometry), _) => geometry,
            (None, Some(input_fd)) => {
                marion_tui::guard::window_size(input_fd).ok_or_else(|| {
                    "reading native terminal geometry after arming resize tracking".to_string()
                })?
            }
            (None, None) => {
                return Err("native resize tracking requires a terminal input descriptor".into());
            }
        };
        stream
            .set_read_timeout(Some(POLL))
            .map_err(|error| format!("bounding native pane reads: {error}"))?;
        let writer_stream = stream
            .try_clone()
            .map_err(|error| format!("cloning the native pane socket: {error}"))?;
        writer_stream
            .set_write_timeout(Some(POLL))
            .map_err(|error| format!("bounding native pane writes: {error}"))?;
        let writer = Arc::new(std::sync::Mutex::new(writer_stream));
        let mut session = Self {
            stream,
            writer,
            inbound: Vec::new(),
            id,
            output,
            next_seq: 0,
            cut: 0,
            input_fd,
            resize_signal,
            leaving: Arc::new(AtomicBool::new(false)),
            keyboard_failure: Arc::new(std::sync::Mutex::new(None)),
            keyboard: None,
        };
        session.attach(geometry)?;
        session.start_keyboard(input)?;
        Ok(session)
    }

    #[cfg(test)]
    fn open_for_test<R: Read + Send + 'static>(
        stream: UnixStream,
        id: AgentId,
        input: R,
        output: W,
        geometry: (u16, u16),
    ) -> Result<Self, Refusal> {
        Self::open_with_io(stream, id, input, None, output, Some(geometry))
    }

    fn attach(&mut self, (cols, rows): (u16, u16)) -> Result<(), Refusal> {
        self.write_frame(
            &Frame::Request(marion_proto::Request::new(
                RequestId::Number(1),
                Call::NodeAttach(marion_proto::params::NodeAttachParams {
                    agent_id: self.id.clone(),
                    pane_stream: Some(marion_proto::params::PaneStreamCapabilityV1::new()),
                }),
            )),
            "sending native node/attach",
        )?;
        let response = self.await_attach_response()?;
        let body = match response.outcome {
            marion_proto::Outcome::Result(body) => body,
            marion_proto::Outcome::Error(error) => {
                return Err(format!(
                    "the supervisor refused the native pane attach: {}",
                    error.message
                ));
            }
        };
        let MethodResult::NodeAttach(attached) = marion_proto::Method::NodeAttach
            .decode_result(&body)
            .map_err(|error| format!("the native node/attach answer did not decode: {error}"))?
        else {
            return Err("the supervisor answered native node/attach with another result".into());
        };
        let pane = attached
            .pane
            .ok_or_else(|| "the claimed native node has no display plane".to_string())?;
        if !pane.writable {
            return Err("the claimed native connection did not retain its writer lease".into());
        }
        let ready = pane.pane_ready.ok_or_else(|| {
            "the native pane attach accepted pane-v1 but omitted its Ready descriptor".to_string()
        })?;
        self.cut = ready.cut;

        // The claim's writer lease already exists. Queue the caller's authoritative geometry
        // before opening the replay gate so every later output is ordered behind that resize.
        self.send_size(cols, rows)?;
        self.reject_buffered_pre_ready_pane_frames()?;
        self.write_frame(
            &Frame::Input(ClientNotification::new(Input::NodePaneReady(
                NodePaneReadyV1 {
                    agent_id: self.id.clone(),
                    token: ready.token,
                    cut: ready.cut,
                },
            ))),
            "sending native node/pane-ready",
        )
    }

    fn await_attach_response(&mut self) -> Result<marion_proto::Response, Refusal> {
        loop {
            match self.next_frame()? {
                Some(Frame::Response(response)) if response.id == RequestId::Number(1) => {
                    return Ok(response);
                }
                Some(Frame::Response(response)) => {
                    return Err(format!(
                        "the supervisor answered native node/attach with response id {:?}",
                        response.id
                    ));
                }
                Some(Frame::Notification(note))
                    if matches!(&note.event,
                        Event::NodePaneFrame(frame) if frame.agent_id == self.id)
                        || matches!(&note.event,
                            Event::NodePty { agent_id, .. } if agent_id == &self.id) =>
                {
                    return Err(format!(
                        "the supervisor sent node `{}` a pane frame before its native attach response",
                        self.id.0
                    ));
                }
                Some(Frame::Notification(_)) | None => continue,
                Some(other) => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame during native attach: {other:?}"
                    ));
                }
            }
        }
    }

    /// Reject a pane frame which arrived in the same socket read as the attach response. The
    /// server must not open this stream until Ready; silently accepting an already-buffered frame
    /// would make the boundary unenforceable on a fast local socket.
    fn reject_buffered_pre_ready_pane_frames(&mut self) -> Result<(), Refusal> {
        while let Some(end) = self.inbound.iter().position(|byte| *byte == b'\n') {
            if end > crate::serve::MAX_FRAME_BYTES {
                return Err("a pre-Ready native pane frame exceeded the frame bound".into());
            }
            let mut line = self.inbound.drain(..=end).collect::<Vec<_>>();
            line.pop();
            let line = std::str::from_utf8(&line)
                .map_err(|_| "a pre-Ready native protocol frame was not UTF-8".to_string())?;
            let frame = Frame::from_line(line)
                .map_err(|error| format!("a pre-Ready native frame did not decode: {error}"))?;
            match frame {
                Frame::Notification(note)
                    if matches!(&note.event,
                        Event::NodePaneFrame(frame) if frame.agent_id == self.id)
                        || matches!(&note.event,
                            Event::NodePty { agent_id, .. } if agent_id == &self.id) =>
                {
                    return Err(format!(
                        "the supervisor sent node `{}` a pane frame before native Ready",
                        self.id.0
                    ));
                }
                Frame::Notification(_) => {}
                other => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame before native Ready: {other:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    fn start_keyboard<R: Read + Send + 'static>(&mut self, mut input: R) -> Result<(), Refusal> {
        let leaving = Arc::clone(&self.leaving);
        let failure = Arc::clone(&self.keyboard_failure);
        let writer = Arc::clone(&self.writer);
        let id = self.id.clone();
        self.keyboard = Some(
            std::thread::Builder::new()
                .name("marion-native-keys".into())
                .spawn(move || {
                    let mut keys = Keys::new();
                    let mut bytes = [0u8; 4096];
                    while !leaving.load(Ordering::SeqCst) {
                        let count = match input.read(&mut bytes) {
                            Ok(0) => {
                                leaving.store(true, Ordering::SeqCst);
                                return;
                            }
                            Ok(count) => count,
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::Interrupted
                                ) =>
                            {
                                std::thread::sleep(POLL);
                                continue;
                            }
                            Err(error) => {
                                *failure.lock().unwrap_or_else(|error| error.into_inner()) =
                                    Some(format!("reading native pane input: {error}"));
                                leaving.store(true, Ordering::SeqCst);
                                return;
                            }
                        };
                        for action in keys.feed(&bytes[..count]) {
                            match action {
                                Action::Detach => {
                                    leaving.store(true, Ordering::SeqCst);
                                    return;
                                }
                                Action::Forward(bytes) => {
                                    let frame = Frame::Input(ClientNotification::new(
                                        Input::NodePaneWrite(NodePaneWriteV1 {
                                            agent_id: id.clone(),
                                            bytes: marion_proto::OpaquePaneBytesV1::new(bytes),
                                        }),
                                    ));
                                    if let Err(error) =
                                        write_serialized(&writer, frame.to_line().as_bytes())
                                    {
                                        *failure
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner()) = Some(
                                            format!("sending native pane keyboard input: {error}"),
                                        );
                                        leaving.store(true, Ordering::SeqCst);
                                        return;
                                    }
                                }
                            }
                        }
                    }
                })
                .map_err(|error| format!("starting native pane keyboard reader: {error}"))?,
        );
        Ok(())
    }

    fn next_frame(&mut self) -> Result<Option<Frame>, Refusal> {
        loop {
            if let Some(end) = self.inbound.iter().position(|byte| *byte == b'\n') {
                if end > crate::serve::MAX_FRAME_BYTES {
                    return Err(format!(
                        "the supervisor sent a native pane frame larger than {} bytes",
                        crate::serve::MAX_FRAME_BYTES
                    ));
                }
                let mut line = self.inbound.drain(..=end).collect::<Vec<_>>();
                line.pop();
                let line = std::str::from_utf8(&line).map_err(|_| {
                    "the supervisor sent a non-UTF-8 native protocol frame".to_string()
                })?;
                return Frame::from_line(line).map(Some).map_err(|error| {
                    format!("the supervisor sent an unreadable native pane frame: {error}")
                });
            }
            if self.inbound.len() > crate::serve::MAX_FRAME_BYTES {
                return Err(format!(
                    "the supervisor sent an unterminated native pane frame larger than {} bytes",
                    crate::serve::MAX_FRAME_BYTES
                ));
            }
            let mut bytes = [0u8; 8192];
            match self.stream.read(&mut bytes) {
                Ok(0) if self.inbound.is_empty() => {
                    return Err("the native pane socket closed before End".into());
                }
                Ok(0) => {
                    return Err(format!(
                        "the native pane socket closed with {} unfinished protocol bytes",
                        self.inbound.len()
                    ));
                }
                Ok(count) => self.inbound.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(format!("reading the native pane socket: {error}")),
            }
        }
    }

    fn pump(&mut self) -> Result<(), Refusal> {
        loop {
            if self.leaving.load(Ordering::SeqCst) {
                if let Some(error) = self
                    .keyboard_failure
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    return Err(error);
                }
                return Ok(());
            }
            self.forward_resize()?;
            match self.next_frame()? {
                Some(Frame::Notification(note)) => {
                    let decoded = crate::pane_client::decode_pane_v1_event(
                        &self.id,
                        self.next_seq,
                        self.cut,
                        note.event,
                    )?;
                    self.next_seq = decoded.next_seq;
                    match decoded.action {
                        crate::pane_client::PaneV1Action::Output(bytes) => {
                            self.output
                                .write_all(bytes.as_bytes())
                                .and_then(|()| self.output.flush())
                                .map_err(|error| {
                                    format!("writing native pane output to the terminal: {error}")
                                })?;
                        }
                        crate::pane_client::PaneV1Action::Resize { .. }
                        | crate::pane_client::PaneV1Action::Ignore => {}
                        crate::pane_client::PaneV1Action::End => return Ok(()),
                    }
                }
                Some(other) => {
                    return Err(format!(
                        "the supervisor sent an unexpected frame during native relay: {other:?}"
                    ));
                }
                None => {}
            }
        }
    }

    fn forward_resize(&mut self) -> Result<(), Refusal> {
        if !RESIZED.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let Some(input_fd) = self.input_fd else {
            return Ok(());
        };
        let Some((cols, rows)) = marion_tui::guard::window_size(input_fd) else {
            return Ok(());
        };
        self.send_size(cols, rows)
    }

    fn send_size(&self, cols: u16, rows: u16) -> Result<(), Refusal> {
        self.write_frame(
            &Frame::Input(ClientNotification::new(Input::NodeResize {
                agent_id: self.id.clone(),
                cols,
                rows,
            })),
            "sending native node/resize",
        )
    }

    fn write_frame(&self, frame: &Frame, action: &str) -> Result<(), Refusal> {
        write_serialized(&self.writer, frame.to_line().as_bytes())
            .map_err(|error| format!("{action}: {error}"))
    }
}

impl<W: Write> Drop for RawPaneSession<W> {
    fn drop(&mut self) {
        self.leaving.store(true, Ordering::SeqCst);
        if let Some(keyboard) = self.keyboard.take() {
            let _ = keyboard.join();
        }
        drop(self.resize_signal.take());
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use marion_core::contract::AgentId;
    use marion_proto::{Call, Event, Frame, Input, MethodResult, PaneFrameKindV1, RequestId};

    use super::{AFTER_RESIZE_SIGNAL_ACQUIRE, RESIZED, RawPaneSession, SIGWINCH, signal};

    const SIGNAL_RESTORE_PROBE: &str = "MARION_NATIVE_SIGNAL_RESTORE_PROBE";
    const SIGNAL_CONTENTION_PROBE: &str = "MARION_NATIVE_SIGNAL_CONTENTION_PROBE";
    const SIGNAL_RESET_PROBE: &str = "MARION_NATIVE_SIGNAL_RESET_PROBE";
    const SIG_ERR: usize = usize::MAX;
    static SENTINEL_HITS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" {
        fn raise(signal: std::ffi::c_int) -> std::ffi::c_int;
    }

    extern "C" fn sentinel_winch(_: std::ffi::c_int) {
        SENTINEL_HITS.fetch_add(1, Ordering::SeqCst);
    }

    struct RestoreSignal(usize);

    impl Drop for RestoreSignal {
        fn drop(&mut self) {
            // SAFETY: this writes back the handler returned by `signal` for the same signal.
            unsafe { signal(SIGWINCH, self.0) };
        }
    }

    fn install_sentinel() -> RestoreSignal {
        SENTINEL_HITS.store(0, Ordering::SeqCst);
        // SAFETY: the handler performs one lock-free atomic increment.
        let prior = unsafe { signal(SIGWINCH, sentinel_winch as *const () as usize) };
        assert_ne!(prior, SIG_ERR, "installing the SIGWINCH test sentinel");
        RestoreSignal(prior)
    }

    fn raise_winch() {
        // SAFETY: SIGWINCH is handled by either the relay's atomic-only handler or the test's
        // atomic-only sentinel throughout these isolated child probes.
        assert_eq!(unsafe { raise(SIGWINCH) }, 0);
    }

    fn run_isolated_signal_probe(variable: &str, test: &str) -> bool {
        if std::env::var_os(variable).is_some() {
            return true;
        }
        let name = format!(
            "{}::{test}",
            module_path!().split_once("::").expect("crate::module").1
        );
        let probe = std::process::Command::new(
            std::env::current_exe().expect("the unit-test binary has a path"),
        )
        .args(["--exact", "--nocapture", "--test-threads", "1", &name])
        .env(variable, "1")
        .output()
        .expect("the isolated signal probe runs");
        assert!(
            probe.status.success(),
            "the isolated signal probe failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&probe.stdout),
            String::from_utf8_lossy(&probe.stderr)
        );
        false
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct IdleInput;

    impl Read for IdleInput {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }

    struct FailingOutput;

    impl Write for FailingOutput {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("sentinel terminal write failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct OneRead {
        bytes: Option<Vec<u8>>,
        hold: Arc<AtomicBool>,
    }

    impl Read for OneRead {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if let Some(bytes) = self.bytes.take() {
                output[..bytes.len()].copy_from_slice(&bytes);
                return Ok(bytes.len());
            }
            while !self.hold.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }

    fn pane_token() -> marion_proto::PaneReadyTokenV1 {
        marion_proto::PaneReadyTokenV1::new([0x5a; 32])
    }

    fn attach_response() -> Frame {
        use marion_core::encoding::Duration;
        use marion_core::harness::Harness;
        use marion_core::node::{NodeState, ReapState};
        use marion_proto::model::{AttachMode, NodeSummary, ReplayPoint};

        Frame::Response(marion_proto::Response::ok(
            RequestId::Number(1),
            &MethodResult::NodeAttach(marion_proto::result::NodeAttachResult {
                node: NodeSummary {
                    agent_id: AgentId("native".into()),
                    parent_id: None,
                    name: None,
                    agent_type: "synthetic-native".into(),
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
                pane: Some(marion_proto::result::PaneAttach {
                    cols: 80,
                    rows: 24,
                    writable: true,
                    held_by: None,
                    pane_ready: Some(marion_proto::result::PaneReadyDescriptorV1 {
                        token: pane_token(),
                        cut: 0,
                    }),
                }),
            }),
        ))
    }

    fn pane_frame(seq: u64, frame: PaneFrameKindV1) -> Frame {
        Frame::Notification(marion_proto::Notification::new(Event::NodePaneFrame(
            marion_proto::PaneFrameV1::new(AgentId("native".into()), seq, frame),
        )))
    }

    fn read_frame(lines: &mut BufReader<UnixStream>) -> Frame {
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        Frame::from_line(&line).unwrap()
    }

    fn complete_attach(server: &mut UnixStream) {
        let mut lines = BufReader::new(server.try_clone().unwrap());
        assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
        server
            .write_all(attach_response().to_line().as_bytes())
            .unwrap();
        server.flush().unwrap();
        assert!(matches!(
            read_frame(&mut lines),
            Frame::Input(note) if matches!(note.input, Input::NodeResize { .. })
        ));
        assert!(matches!(
            read_frame(&mut lines),
            Frame::Input(note) if matches!(note.input, Input::NodePaneReady(_))
        ));
    }

    #[derive(Clone, Copy, Debug)]
    enum SignalExit {
        End,
        Detach,
        ReadFailure,
        WriteFailure,
        ResizeFailure,
    }

    type SignalRelay = (
        RawPaneSession<Box<dyn Write>>,
        Option<mpsc::SyncSender<()>>,
        std::thread::JoinHandle<()>,
        Option<std::os::fd::OwnedFd>,
    );

    fn relay_for_signal_exit(exit: SignalExit) -> SignalRelay {
        let (client, mut server) = UnixStream::pair().unwrap();
        let (attached_tx, attached_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let server_thread = std::thread::spawn(move || {
            complete_attach(&mut server);
            attached_tx.send(()).unwrap();
            match exit {
                SignalExit::End => {
                    server
                        .write_all(pane_frame(0, PaneFrameKindV1::End {}).to_line().as_bytes())
                        .unwrap();
                    server.flush().unwrap();
                }
                SignalExit::Detach => {
                    release_rx
                        .recv_timeout(Duration::from_secs(2))
                        .expect("detach returns while the pane socket remains open");
                }
                SignalExit::ReadFailure | SignalExit::ResizeFailure => {}
                SignalExit::WriteFailure => {
                    server
                        .write_all(
                            pane_frame(
                                0,
                                PaneFrameKindV1::Output {
                                    bytes: marion_proto::OpaquePaneBytesV1::new(b"output"),
                                },
                            )
                            .to_line()
                            .as_bytes(),
                        )
                        .unwrap();
                    server.flush().unwrap();
                }
            }
        });

        let input: Box<dyn Read + Send> = match exit {
            SignalExit::Detach => Box::new(std::io::Cursor::new(vec![0x1d, b'd'])),
            _ => Box::new(IdleInput),
        };
        let output: Box<dyn Write> = match exit {
            SignalExit::WriteFailure => Box::new(FailingOutput),
            _ => Box::new(Sink::default()),
        };
        let resize_tty = crate::pty::PtyMaster::open(crate::pty::WinSize::new(80, 24))
            .unwrap()
            .open_slave()
            .unwrap();
        let input_fd = Some(resize_tty.as_raw_fd());
        let session = RawPaneSession::open_with_io(
            client,
            AgentId("native".into()),
            input,
            input_fd,
            output,
            None,
        )
        .expect("the signal-owning native relay attaches");
        attached_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the server observed Ready");
        (
            session,
            matches!(exit, SignalExit::Detach).then_some(release_tx),
            server_thread,
            Some(resize_tty),
        )
    }

    #[test]
    fn native_relay_restores_the_prior_sigwinch_handler_after_every_exit() {
        if !run_isolated_signal_probe(
            SIGNAL_RESTORE_PROBE,
            "native_relay_restores_the_prior_sigwinch_handler_after_every_exit",
        ) {
            return;
        }
        let _restore_original = install_sentinel();

        for exit in [
            SignalExit::End,
            SignalExit::Detach,
            SignalExit::ReadFailure,
            SignalExit::WriteFailure,
            SignalExit::ResizeFailure,
        ] {
            SENTINEL_HITS.store(0, Ordering::SeqCst);
            RESIZED.store(false, Ordering::SeqCst);
            let (mut session, detach_release, server_thread, _resize_tty) =
                relay_for_signal_exit(exit);

            raise_winch();
            assert!(
                RESIZED.load(Ordering::SeqCst),
                "the relay did not own SIGWINCH during {exit:?}"
            );
            assert_eq!(SENTINEL_HITS.load(Ordering::SeqCst), 0);
            if !matches!(exit, SignalExit::ResizeFailure) {
                RESIZED.store(false, Ordering::SeqCst);
            }

            let outcome = session.pump();
            match exit {
                SignalExit::End | SignalExit::Detach => assert!(outcome.is_ok(), "{outcome:?}"),
                SignalExit::ReadFailure => assert!(
                    outcome.unwrap_err().contains("closed before End"),
                    "wrong read failure"
                ),
                SignalExit::WriteFailure => assert!(
                    outcome.unwrap_err().contains("writing native pane output"),
                    "wrong write failure"
                ),
                SignalExit::ResizeFailure => assert!(
                    outcome.unwrap_err().contains("sending native node/resize"),
                    "wrong resize failure"
                ),
            }
            if let Some(release) = detach_release {
                release.send(()).unwrap();
            }
            drop(session);
            server_thread.join().unwrap();

            SENTINEL_HITS.store(0, Ordering::SeqCst);
            RESIZED.store(false, Ordering::SeqCst);
            raise_winch();
            assert_eq!(
                SENTINEL_HITS.load(Ordering::SeqCst),
                1,
                "the exact prior SIGWINCH handler was not restored after {exit:?}"
            );
            assert!(
                !RESIZED.load(Ordering::SeqCst),
                "the relay's handler remained installed after {exit:?}"
            );
        }
    }

    #[test]
    fn native_relay_refuses_overlapping_process_signal_ownership_without_waiting() {
        if !run_isolated_signal_probe(
            SIGNAL_CONTENTION_PROBE,
            "native_relay_refuses_overlapping_process_signal_ownership_without_waiting",
        ) {
            return;
        }
        let _restore_original = install_sentinel();
        let (first, first_release, first_server, _first_tty) =
            relay_for_signal_exit(SignalExit::Detach);

        let (second_client, mut second_server) = UnixStream::pair().unwrap();
        second_server
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let second_server = std::thread::spawn(move || {
            let mut line = String::new();
            let mut lines = BufReader::new(second_server.try_clone().unwrap());
            if lines.read_line(&mut line).is_ok_and(|count| count > 0) {
                second_server
                    .write_all(attach_response().to_line().as_bytes())
                    .unwrap();
                second_server.flush().unwrap();
                let _ = read_frame(&mut lines);
                let _ = read_frame(&mut lines);
            }
        });

        let started = Instant::now();
        let second = RawPaneSession::open_with_io(
            second_client,
            AgentId("native".into()),
            IdleInput,
            None,
            Sink::default(),
            None,
        );
        let elapsed = started.elapsed();
        let error = match second {
            Ok(session) => {
                drop(session);
                "overlapping relay unexpectedly acquired SIGWINCH".to_string()
            }
            Err(error) => error,
        };

        first_release.unwrap().send(()).unwrap();
        drop(first);
        first_server.join().unwrap();
        second_server.join().unwrap();
        assert!(error.contains("already owns SIGWINCH"), "{error}");
        assert!(
            elapsed < Duration::from_millis(100),
            "signal contention waited for {elapsed:?} instead of refusing"
        );
    }

    #[test]
    fn native_relay_resets_process_resize_state_when_a_session_ends() {
        if !run_isolated_signal_probe(
            SIGNAL_RESET_PROBE,
            "native_relay_resets_process_resize_state_when_a_session_ends",
        ) {
            return;
        }
        let _restore_original = install_sentinel();
        RESIZED.store(false, Ordering::SeqCst);
        let (session, release, server_thread, _resize_tty) =
            relay_for_signal_exit(SignalExit::Detach);
        RESIZED.store(true, Ordering::SeqCst);
        release.unwrap().send(()).unwrap();
        drop(session);
        server_thread.join().unwrap();

        assert!(
            !RESIZED.load(Ordering::SeqCst),
            "a later native relay would inherit the prior session's resize edge"
        );
    }

    #[test]
    fn resize_arriving_before_signal_acquisition_survives_session_initialization() {
        if !run_isolated_signal_probe(
            SIGNAL_RESET_PROBE,
            "resize_arriving_before_signal_acquisition_survives_session_initialization",
        ) {
            return;
        }
        let _restore_original = install_sentinel();
        let resize_tty = crate::pty::PtyMaster::open(crate::pty::WinSize::new(80, 24)).unwrap();
        let input = resize_tty.open_slave().unwrap();
        let input_fd = input.as_raw_fd();
        let (client, mut server) = UnixStream::pair().unwrap();
        *AFTER_RESIZE_SIGNAL_ACQUIRE
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Box::new(move || {
            resize_tty
                .set_size(crate::pty::WinSize::new(132, 47))
                .unwrap();
            raise_winch();
        }));
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
            server
                .write_all(attach_response().to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();
            let Frame::Input(note) = read_frame(&mut lines) else {
                panic!("native relay omitted initial geometry")
            };
            assert!(
                matches!(
                    note.input,
                    Input::NodeResize {
                        cols: 132,
                        rows: 47,
                        ..
                    }
                ),
                "resize in the geometry-to-handler seam was lost: {:?}",
                note.input
            );
            assert!(matches!(read_frame(&mut lines), Frame::Input(_)));
        });

        let session = RawPaneSession::open_with_io(
            client,
            AgentId("native".into()),
            IdleInput,
            Some(input_fd),
            Sink::default(),
            None,
        )
        .expect("the native relay attaches");
        assert!(
            RESIZED.load(Ordering::SeqCst),
            "the resize edge arriving after handler installation was erased"
        );
        drop(session);
        server_thread.join().unwrap();
    }

    #[test]
    fn raw_pane_v1_writes_invalid_utf8_tail_verbatim_without_a_screen_preamble() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            let Frame::Request(request) = read_frame(&mut lines) else {
                panic!("relay did not attach")
            };
            assert!(matches!(request.call, Call::NodeAttach(_)));
            server
                .write_all(attach_response().to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();

            assert!(matches!(
                read_frame(&mut lines),
                Frame::Input(note) if matches!(note.input, marion_proto::Input::NodeResize { cols: 111, rows: 37, .. })
            ));
            assert!(matches!(
                read_frame(&mut lines),
                Frame::Input(note) if matches!(note.input, marion_proto::Input::NodePaneReady(_))
            ));

            let tail = [0xff, 0x00, 0x9b, b'3', b'1', b'm', b'Z'];
            server
                .write_all(
                    pane_frame(
                        0,
                        PaneFrameKindV1::Output {
                            bytes: marion_proto::OpaquePaneBytesV1::new(tail),
                        },
                    )
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server
                .write_all(
                    pane_frame(
                        1,
                        PaneFrameKindV1::Resize {
                            cols: 111,
                            rows: 37,
                        },
                    )
                    .to_line()
                    .as_bytes(),
                )
                .unwrap();
            server
                .write_all(pane_frame(2, PaneFrameKindV1::End {}).to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();
        });

        let sink = Sink::default();
        let mut session = RawPaneSession::open_for_test(
            client,
            AgentId("native".into()),
            IdleInput,
            sink.clone(),
            (111, 37),
        )
        .expect("the pane-v1 relay attaches");
        session.pump().expect("the relay reaches End");
        server_thread.join().unwrap();

        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            &[0xff, 0x00, 0x9b, b'3', b'1', b'm', b'Z']
        );
    }

    #[test]
    fn raw_pane_v1_forwards_invalid_utf8_keyboard_bytes_without_text_conversion() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let hold = Arc::new(AtomicBool::new(false));
        let release = Arc::clone(&hold);
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
            server
                .write_all(attach_response().to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();
            assert!(matches!(read_frame(&mut lines), Frame::Input(_))); // initial Resize
            assert!(matches!(read_frame(&mut lines), Frame::Input(_))); // Ready
            let Frame::Input(note) = read_frame(&mut lines) else {
                panic!("native keyboard did not send an input notification")
            };
            let Input::NodePaneWrite(write) = note.input else {
                panic!("native keyboard used a lossy legacy input method")
            };
            assert_eq!(write.bytes.as_bytes(), &[0xff, 0x00, 0x80, b'X']);
            server
                .write_all(pane_frame(0, PaneFrameKindV1::End {}).to_line().as_bytes())
                .unwrap();
            server.flush().unwrap();
            release.store(true, Ordering::SeqCst);
        });

        let mut session = RawPaneSession::open_for_test(
            client,
            AgentId("native".into()),
            OneRead {
                bytes: Some(vec![0xff, 0x00, 0x80, b'X']),
                hold,
            },
            Sink::default(),
            (80, 24),
        )
        .unwrap();
        session.pump().unwrap();
        server_thread.join().unwrap();
    }

    #[test]
    fn raw_pane_v1_rejects_a_frame_coalesced_before_ready() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = std::thread::spawn(move || {
            let mut lines = BufReader::new(server.try_clone().unwrap());
            assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
            let mut bytes = attach_response().to_line();
            bytes.push_str(&pane_frame(0, PaneFrameKindV1::End {}).to_line());
            server.write_all(bytes.as_bytes()).unwrap();
            server.flush().unwrap();
            let _ = read_frame(&mut lines);
        });

        let error = match RawPaneSession::open_for_test(
            client,
            AgentId("native".into()),
            IdleInput,
            Sink::default(),
            (80, 24),
        ) {
            Ok(_) => panic!("a pane frame before Ready must fail closed"),
            Err(error) => error,
        };
        assert!(error.contains("before native Ready"), "{error}");
        server_thread.join().unwrap();
    }

    #[test]
    fn raw_pane_v1_rejects_sequence_gap_end_before_cut_and_socket_loss() {
        enum Failure {
            Gap,
            EarlyEnd,
            Disconnect,
        }

        for (failure, expected) in [
            (Failure::Gap, "not dense"),
            (Failure::EarlyEnd, "before the advertised replay cut"),
            (Failure::Disconnect, "closed before End"),
        ] {
            let (client, mut server) = UnixStream::pair().unwrap();
            let server_thread = std::thread::spawn(move || {
                let mut lines = BufReader::new(server.try_clone().unwrap());
                assert!(matches!(read_frame(&mut lines), Frame::Request(_)));
                let mut response = attach_response();
                if matches!(failure, Failure::EarlyEnd) {
                    let Frame::Response(ref mut response) = response else {
                        unreachable!()
                    };
                    let MethodResult::NodeAttach(mut attached) = marion_proto::Method::NodeAttach
                        .decode_result(match &response.outcome {
                            marion_proto::Outcome::Result(body) => body,
                            _ => unreachable!(),
                        })
                        .unwrap()
                    else {
                        unreachable!()
                    };
                    attached
                        .pane
                        .as_mut()
                        .unwrap()
                        .pane_ready
                        .as_mut()
                        .unwrap()
                        .cut = 4;
                    *response = marion_proto::Response::ok(
                        RequestId::Number(1),
                        &MethodResult::NodeAttach(attached),
                    );
                }
                server.write_all(response.to_line().as_bytes()).unwrap();
                server.flush().unwrap();
                let _ = read_frame(&mut lines);
                let _ = read_frame(&mut lines);
                match failure {
                    Failure::Gap => server
                        .write_all(pane_frame(2, PaneFrameKindV1::End {}).to_line().as_bytes())
                        .unwrap(),
                    Failure::EarlyEnd => server
                        .write_all(pane_frame(0, PaneFrameKindV1::End {}).to_line().as_bytes())
                        .unwrap(),
                    Failure::Disconnect => return,
                }
                server.flush().unwrap();
            });
            let mut session = RawPaneSession::open_for_test(
                client,
                AgentId("native".into()),
                IdleInput,
                Sink::default(),
                (80, 24),
            )
            .unwrap();
            let error = session.pump().unwrap_err();
            assert!(error.contains(expected), "{error}");
            server_thread.join().unwrap();
        }
    }
}
