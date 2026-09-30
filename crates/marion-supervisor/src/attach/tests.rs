//! Tests for `attach.rs`, moved out of it unchanged.

use super::*;
use marion_core::proto::PaneFrameKindV1;

/// **A keystroke that wakes the keyboard wait after the session ended is neither read nor
/// forwarded.** The wait is held open by the fake; the session ends (`leaving`) while it is;
/// then input arrives and the wait returns ready. The worker must stop without reading, so the
/// byte stays in the operator's terminal and nothing reaches the supervisor.
///
/// Mutation: drop the `leaving` check between wait and read and `k` is read and sent.
#[test]
fn a_keystroke_that_arrives_after_the_session_ends_is_not_forwarded() {
    struct HeldWait {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
        reads: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl KeyboardInput for HeldWait {
        fn wait(&mut self, _: &crate::wake::Flag) -> std::io::Result<bool> {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            Ok(true)
        }
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            buf[0] = b'k';
            Ok(Some(1))
        }
    }
    let (client, mut server) = UnixStream::pair().unwrap();
    let (entered, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release) = std::sync::mpsc::sync_channel(0);
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let leaving = Arc::new(crate::wake::Flag::new());
    let failure = Arc::new(std::sync::Mutex::new(None));
    let mut input = HeldWait {
        entered,
        release,
        reads: Arc::clone(&reads),
    };
    let worker = {
        let (leaving, failure) = (Arc::clone(&leaving), Arc::clone(&failure));
        let writer = Arc::new(std::sync::Mutex::new(client));
        std::thread::spawn(move || {
            pump_keyboard(
                &mut input,
                AgentId("root".into()),
                true,
                writer,
                leaving,
                failure,
            );
        })
    };
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the worker waits for input");
    leaving.store(true, Ordering::SeqCst);
    release_tx.send(()).unwrap();
    worker.join().expect("the worker stops cleanly");

    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "the keystroke stays unread"
    );
    let mut sent = Vec::new();
    server.read_to_end(&mut sent).unwrap();
    assert!(
        sent.is_empty(),
        "nothing was forwarded: {:?}",
        String::from_utf8_lossy(&sent)
    );
    assert_eq!(*failure.lock().unwrap(), None, "a detach is not a failure");
}
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
        "the interactive client attached without asking for pane-stream v1"
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
                bytes: marion_core::proto::OpaquePaneBytesV1::new(b"unbracketed tail"),
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

    let master =
        crate::pty::PtyMaster::open(crate::pty::WinSize::new(COLS, ROWS)).expect("an operator pty");
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
                pane_attach_response(marion_core::proto::result::PaneAttach {
                    cols: COLS,
                    rows: ROWS,
                    writable: true,
                    held_by: None,
                    ended: false,
                    pane_ready: Some(marion_core::proto::result::PaneReadyDescriptorV1 {
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
                    bytes: marion_core::proto::OpaquePaneBytesV1::new(screenful),
                },
            ),
            (1, PaneFrameKindV1::End {}),
        ] {
            server
                .write_all(
                    Frame::Notification(marion_core::proto::Notification::new(pane_event(
                        seq, kind,
                    )))
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
                pane_attach_response(marion_core::proto::result::PaneAttach {
                    cols: 80,
                    rows: 24,
                    writable: true,
                    held_by: None,
                    ended: false,
                    pane_ready: Some(marion_core::proto::result::PaneReadyDescriptorV1 {
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

/// The hint names the full id, since `marion attach` resolves nothing shorter, and the tree.
#[test]
fn the_reattach_hint_is_one_line_naming_the_way_back() {
    let hint = reattach_hint("01a0ca8b-2cf8-766b-82c0-a64a086b5688");
    assert!(!hint.contains('\n'), "{hint}");
    assert!(
        hint.contains("`marion attach 01a0ca8b-2cf8-766b-82c0-a64a086b5688`"),
        "{hint}"
    );
    assert!(hint.contains("`marion ls`"), "{hint}");
}

/// A supervisor that takes the attach request and never answers must not hold the operator:
/// no keyboard can run before the answer says which pane protocol to encode, so the wait for
/// it is bounded and ends in a refusal the tree shows as its notice.
#[test]
fn an_attach_the_supervisor_never_answers_is_refused_within_its_bound() {
    let (client, server) = UnixStream::pair().unwrap();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let server_thread = std::thread::spawn(move || {
        let mut lines = BufReader::new(server);
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        // Holds the connection open, answering nothing, until the test is done.
        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
    });
    ATTACH_ANSWER_BOUND_FOR_TEST
        .with(|bound| bound.set(Some(std::time::Duration::from_millis(300))));
    let started = std::time::Instant::now();
    let opened = Session::open_for_test(
        client,
        AgentId("root".into()),
        Sink::default(),
        -1,
        (80, 24),
    );
    let waited = started.elapsed();
    let _ = release_tx.send(());
    server_thread.join().unwrap();
    let Err(error) = opened else {
        panic!("an unanswered attach opened a session")
    };
    assert!(error.contains("without answering node/attach"), "{error}");
    assert!(
        waited < std::time::Duration::from_secs(5),
        "the refusal took {waited:?}"
    );
}

/// **A read-only attach can still be left.** `marion tree` → Enter on a native root attaches
/// read-only, because the facade's own connection holds the keyboard; the keyboard reader was
/// started only for a writable attach, so nothing read `^] d` and the client sat in its socket
/// read until killed. The reader now runs for every attach: a read-only one forwards nothing —
/// the supervisor closes on an unleased keystroke — and detaches on `^] d` even when the
/// supervisor never sends another frame.
#[test]
fn a_read_only_attach_to_a_silent_node_still_detaches_on_the_prefix() {
    use std::os::fd::AsRawFd;
    let master =
        crate::pty::PtyMaster::open(crate::pty::WinSize::new(80, 24)).expect("an operator pty");
    let slave = master.open_slave().expect("the operator's tty");

    let (client, mut server) = UnixStream::pair().unwrap();
    let server_thread = std::thread::spawn(move || {
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut lines = BufReader::new(server.try_clone().unwrap());
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        server
            .write_all(
                pane_attach_response(marion_core::proto::result::PaneAttach {
                    cols: 80,
                    rows: 24,
                    writable: false,
                    held_by: Some(7),
                    ended: false,
                    pane_ready: Some(marion_core::proto::result::PaneReadyDescriptorV1 {
                        token: pane_token(),
                        cut: 0,
                    }),
                })
                .to_line()
                .as_bytes(),
            )
            .unwrap();
        server.flush().unwrap();
        // Silent from here on: whatever the client sends is collected until it hangs up.
        let mut sent = Vec::new();
        loop {
            line.clear();
            if lines.read_line(&mut line).unwrap() == 0 {
                return sent;
            }
            sent.push(Frame::from_line(&line).unwrap());
        }
    });

    let session = Session::open_for_test(
        client,
        AgentId("root".into()),
        Sink::default(),
        slave.as_raw_fd(),
        (80, 24),
    )
    .expect("a read-only attach opens");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut session = session;
        let pumped = session.pump();
        drop(session);
        let _ = done_tx.send(pumped);
    });
    master.write_all(b"x").unwrap();
    master
        .write_all(&[marion_tui::keys::PREFIX, marion_tui::keys::DETACH_KEY])
        .unwrap();
    let pumped = done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("a read-only attach did not detach on ^] d");
    let left = pumped.unwrap_or_else(|e| panic!("the detach was reported as a failure: {e}"));
    assert_eq!(left, Leave::Detached, "a ^] d was not reported as a detach");
    let sent = server_thread.join().unwrap();
    assert!(
        !sent.iter().any(|frame| matches!(
            frame,
            Frame::Input(note) if matches!(
                note.input,
                Input::NodePaneWrite(_)
            )
        )),
        "a read-only attach sent the supervisor a keystroke: {sent:?}"
    );
    drop(master);
}

fn pane_token() -> marion_core::proto::PaneReadyTokenV1 {
    marion_core::proto::PaneReadyTokenV1::new([0x5a; 32])
}

fn pane_attach_response(pane: marion_core::proto::result::PaneAttach) -> Frame {
    pane_attach_response_with_id(RequestId::Number(1), pane)
}

fn pane_attach_response_with_id(
    id: RequestId,
    pane: marion_core::proto::result::PaneAttach,
) -> Frame {
    use marion_core::encoding::Duration;
    use marion_core::harness::Harness;
    use marion_core::node::{NodeState, ReapState};
    use marion_core::proto::model::{AttachMode, NodeSummary, ReplayPoint};

    Frame::Response(marion_core::proto::Response::ok(
        id,
        &MethodResult::NodeAttach(marion_core::proto::result::NodeAttachResult {
            node: NodeSummary {
                widened: vec![],
                budget: None,
                changed: None,
                review_of: None,
                review: None,
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
                started_at: None,
                ended_at: None,
                tokens: None,
                attention: None,
                endpoint: None,
                race: None,
                cancel: None,
                workflow: None,
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
    let writer = stream.try_clone().unwrap();
    writer.set_write_timeout(Some(WRITE_BOUND)).unwrap();
    Session {
        stream,
        writer: Arc::new(std::sync::Mutex::new(writer)),
        inbound: Vec::new(),
        id: AgentId("root".into()),
        input_fd: -1,
        view,
        writable: false,
        pane_stream,
        leaving: Arc::new(crate::wake::Flag::new()),
        keyboard_failure: Arc::new(std::sync::Mutex::new(None)),
        keyboard: None,
    }
}

fn pane_event(seq: u64, frame: PaneFrameKindV1) -> Event {
    Event::NodePaneFrame(marion_core::proto::PaneFrameV1::new(
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
                    bytes: marion_core::proto::OpaquePaneBytesV1::new(raw),
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
    let mut lossy = Term::with_options(marion_term::Size::new(80, 24), marion_tui::grid_options());
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
fn pane_v1_rejects_sequence_gaps_and_duplicates() {
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
    let frame = Frame::Notification(marion_core::proto::Notification::new(pane_event(
        0,
        PaneFrameKindV1::Output {
            bytes: marion_core::proto::OpaquePaneBytesV1::new(bytes.clone()),
        },
    )));
    let line = frame.to_line().into_bytes();
    let middle = line.len() / 2;
    server.write_all(&line[..middle]).unwrap();
    server.flush().unwrap();
    let soon = std::time::Instant::now() + std::time::Duration::from_millis(50);
    assert!(session.next_frame(Some(soon)).unwrap().is_none());
    server.write_all(&line[middle..]).unwrap();
    server.flush().unwrap();
    let bound = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let Some(Frame::Notification(note)) = session.next_frame(Some(bound)).unwrap() else {
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
            assert!(session.next_frame(None).unwrap_err().contains("larger"));
        } else {
            // Exactly at the transport limit reaches parsing; it is invalid JSON, not oversize.
            let error = session.next_frame(None).unwrap_err();
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
                pane_attach_response(marion_core::proto::result::PaneAttach {
                    cols: 80,
                    rows: 24,
                    writable: false,
                    held_by: Some(7),
                    ended: false,
                    pane_ready: Some(marion_core::proto::result::PaneReadyDescriptorV1 {
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

/// **A refused pane-v1 attach is reported, never retried under a weaker protocol.** The client
/// and the supervisor ship as a pair, so an `invalid params` answer is a real refusal and not an
/// older supervisor that might accept a request without the capability.
///
/// Mutation: retry the attach without `pane_stream` after `INVALID_PARAMS`. The fixture then
/// sees a second request and this fails.
#[test]
fn an_invalid_params_answer_is_refused_and_not_retried() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_thread = std::thread::spawn(move || {
        let mut lines = BufReader::new(server.try_clone().unwrap());
        let mut line = String::new();
        lines.read_line(&mut line).unwrap();
        let Frame::Request(first) = Frame::from_line(&line).unwrap() else {
            panic!("expected the attach request")
        };
        assert_eq!(first.id, RequestId::Number(1));
        server
            .write_all(
                Frame::Response(marion_core::proto::Response::err(
                    RequestId::Number(1),
                    marion_core::proto::RpcError::invalid_params("unknown field pane_stream"),
                ))
                .to_line()
                .as_bytes(),
            )
            .unwrap();
        server.flush().unwrap();
        // Whatever the client sends after the refusal, until it hangs up.
        let mut after = Vec::new();
        loop {
            line.clear();
            if lines.read_line(&mut line).unwrap() == 0 {
                return after;
            }
            after.push(line.clone());
        }
    });
    let error = Session::open_for_test(
        client,
        AgentId("root".into()),
        Sink::default(),
        -1,
        (80, 24),
    )
    .err()
    .expect("a refused attach must not open a session");
    assert!(error.contains("refused the attach"), "{error}");
    let after = server_thread.join().unwrap();
    assert!(after.is_empty(), "the refusal was retried: {after:?}");
}

/// A pane answer with no Ready descriptor is refused rather than rendered.
#[test]
fn a_pane_answer_without_its_ready_descriptor_is_refused() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let server_thread = std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(server.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        server
            .write_all(
                pane_attach_response(marion_core::proto::result::PaneAttach {
                    cols: 80,
                    rows: 24,
                    writable: false,
                    held_by: Some(7),
                    ended: false,
                    pane_ready: None,
                })
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
    .expect("a pane without a Ready boundary must be refused");
    assert!(error.contains("omitted its Ready"), "{error}");
    server_thread.join().unwrap();
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
                pane_attach_response(marion_core::proto::result::PaneAttach {
                    cols: 80,
                    rows: 24,
                    writable: true,
                    held_by: None,
                    ended: false,
                    pane_ready: Some(marion_core::proto::result::PaneReadyDescriptorV1 {
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
fn the_end_frame_flushes_the_final_dirty_output() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let (v, sink) = view(80, 24);
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
                bytes: marion_core::proto::OpaquePaneBytesV1::new(b"final-dirty-tail"),
            },
        ))
        .unwrap();
    assert!(written(&sink).is_empty(), "output painted before an edge");
    assert_eq!(
        session
            .consume_pane_event(pane_event(1, PaneFrameKindV1::End {}))
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
        let early = Frame::Notification(marion_core::proto::Notification::new(
            Event::NodePaneFrame(marion_core::proto::PaneFrameV1::new(
                AgentId("root".into()),
                0,
                marion_core::proto::PaneFrameKindV1::Output {
                    bytes: marion_core::proto::OpaquePaneBytesV1::new(b"lost"),
                },
            )),
        ));
        let response = pane_attach_response(marion_core::proto::result::PaneAttach {
            cols: 80,
            rows: 24,
            writable: false,
            held_by: Some(9),
            ended: false,
            pane_ready: Some(marion_core::proto::result::PaneReadyDescriptorV1 {
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
